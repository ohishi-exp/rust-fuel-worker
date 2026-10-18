//! UTRAN.CSV の受け取り置き場 (R2 `FUEL_INBOX`) の口の判定と、key・メタデータ・応答の形 (純粋ロジック)。
//!
//! - `POST /api/fuel/uploads`: body は CSV のバイト列 (最大 [`MAX_BYTES`]。超えたら 413)。
//!   key は `utran/<sha256 の hex>.csv` ([`object_key`]) で、同じ内容は同じ key になる
//! - `GET /api/fuel/uploads/{sha256}`: R2 にある間だけ CSV をそのまま返す (CakePHP の downloadUtran。
//!   `Content-Disposition: attachment; filename=UTRAN<yyMMdd>.CSV`、日付は日本時間の今日)
//! - 定時実行は [`PREFIX`] の下を全部取り込み直す (取り込みは何度流しても同じ結果になる)
//!
//! 経路は `crate::route` (`Route::Upload`・`Route::Download`)、認可 (X-Tenant-ID) は router がかける。ここは見ない。

use chrono::{DateTime, Duration};
use sha2::{Digest, Sha256};

use crate::csv::ParseError;

/// 置き場の key の頭。
pub const PREFIX: &str = "utran/";
/// 受ける CSV の上限 (5 MB)。
pub const MAX_BYTES: usize = 5 * 1024 * 1024;
/// アップロード元の header (`usb` か `web`。無ければ `web`)。
pub const SOURCE_HEADER: &str = "X-Fuel-Source";

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// sha256 の hex 小文字 64 桁 (`GET /api/fuel/uploads/{sha256}` の path の値)。[`parse_sha256`] を通らなければ作れない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sha256Hex([u8; 64]);

impl Sha256Hex {
    pub fn as_str(&self) -> &str {
        // parse_sha256 が ASCII の 16 進だけを入れている
        std::str::from_utf8(&self.0).unwrap_or_default()
    }
}

/// 64 桁の hex 小文字でなければ `None`。
pub fn parse_sha256(s: &str) -> Option<Sha256Hex> {
    is_sha256_hex(s).then(|| Sha256Hex(s.as_bytes().try_into().unwrap_or([0; 64])))
}

/// 中身の sha256 (hex 小文字)。
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// R2 の key。
pub fn object_key(sha256: &str) -> String {
    [PREFIX, sha256, ".csv"].concat()
}

/// 置き場の CSV の key か (定時実行が取り込む対象)。
pub fn is_inbox_key(key: &str) -> bool {
    key.strip_prefix(PREFIX)
        .and_then(|k| k.strip_suffix(".csv"))
        .is_some_and(is_sha256_hex)
}

/// header `X-Fuel-Source` の値。無ければ `web`、`usb`・`web` 以外は `None` (400)。
pub fn source(header: Option<&str>) -> Option<&'static str> {
    match header {
        None => Some("web"),
        Some("web") => Some("web"),
        Some("usb") => Some("usb"),
        Some(_) => None,
    }
}

/// customMetadata `uploaded_at` (UTC の RFC 3339)。
pub fn uploaded_at(now_ms: u64) -> String {
    DateTime::from_timestamp_millis(now_ms as i64)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// ダウンロードの `Content-Disposition` (日付は日本時間の今日。CakePHP の downloadUtran と同じ名前)。
pub fn content_disposition(now_ms: u64) -> String {
    let jst =
        DateTime::from_timestamp_millis(now_ms as i64).unwrap_or_default() + Duration::hours(9);
    let day = jst.format("%y%m%d").to_string();
    ["attachment; filename=UTRAN", &day, ".CSV"].concat()
}

/// 失敗の応答の本文 (`{"ok":false,"error":…}`)。
pub fn error_body(error: &str) -> String {
    serde_json::json!({"ok": false, "error": error}).to_string()
}

/// CSV が読めなかったときの応答の本文 (行番号と理由だけ)。
pub fn csv_error_body(e: &ParseError) -> String {
    serde_json::json!({"ok": false, "error": "csv", "line": e.line, "reason": e.reason}).to_string()
}

/// 取り込めたときの応答の本文 (`{"ok":true,"key":…,"inserted":…,"skipped":…,"latest_start":…}`)。
pub fn ok_body(key: &str, summary: &crate::ingest::Summary) -> String {
    let mut v = summary.to_json();
    v["ok"] = true.into();
    v["key"] = key.into();
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn sha256_path_value() {
        assert_eq!(parse_sha256(SHA).unwrap().as_str(), SHA);
        assert!(parse_sha256("abc").is_none());
        assert!(parse_sha256(&SHA.to_uppercase()).is_none());
        assert!(parse_sha256(&SHA.replace('e', "g")).is_none());
    }

    #[test]
    fn key_is_content_hash() {
        assert_eq!(sha256_hex(b""), SHA);
        let key = object_key(SHA);
        assert_eq!(key, format!("utran/{SHA}.csv"));
        assert!(is_inbox_key(&key));
        assert!(!is_inbox_key("utran/abc.csv"));
        assert!(!is_inbox_key(&format!("other/{SHA}.csv")));
        assert!(!is_inbox_key(&format!("utran/{SHA}.txt")));
    }

    #[test]
    fn source_defaults_to_web() {
        assert_eq!(source(None), Some("web"));
        assert_eq!(source(Some("web")), Some("web"));
        assert_eq!(source(Some("usb")), Some("usb"));
        assert_eq!(source(Some("USB")), None);
    }

    #[test]
    fn times_and_names() {
        // 2023-12-20 15:30:00 UTC = 2023-12-21 00:30 JST
        let ms = 1_703_086_200_000;
        assert_eq!(uploaded_at(ms), "2023-12-20T15:30:00Z");
        assert_eq!(
            content_disposition(ms),
            "attachment; filename=UTRAN231221.CSV"
        );
        assert_eq!(MAX_BYTES, 5_242_880);
    }

    #[test]
    fn bodies() {
        assert_eq!(
            error_body("too_large"),
            r#"{"error":"too_large","ok":false}"#
        );
        let e = ParseError {
            line: 3,
            reason: "bad_datetime",
        };
        assert_eq!(
            csv_error_body(&e),
            r#"{"error":"csv","line":3,"ok":false,"reason":"bad_datetime"}"#
        );
        let s = crate::ingest::Summary {
            inserted: 1,
            skipped: 0,
            latest_start: None,
        };
        assert_eq!(
            ok_body("utran/x.csv", &s),
            r#"{"inserted":1,"key":"utran/x.csv","latest_start":null,"ok":true,"skipped":0}"#
        );
    }
}
