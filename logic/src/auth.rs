//! 認可(純粋ロジック)。`/api/fuel/health` 以外の `/api/fuel/*` は、header `X-Tenant-ID` が var `FUEL_TENANT_ID` と
//! 一致することを求める。JWT を検証するのは auth-worker で、この Worker は付け直された header を信頼する
//! (Service Binding からだけ届くため)。role では絞らない。
//!
//! - var が空・UUID の形でない・nil UUID → 503(設定の誤り。どの header でも通さない)
//! - header が無い・UUID の形でない・var と一致しない → 403
//!
//! 比較は 16 進の大文字・小文字を区別しない。

use serde_json::json;

use crate::reply::Reply;

/// tenant を運ぶ header の名前。
pub const TENANT_HEADER: &str = "X-Tenant-ID";
/// 期待する tenant の var の名前(本番は deploy.yml が repo variable から `--var` で渡す)。
pub const TENANT_VAR: &str = "FUEL_TENANT_ID";

/// 認可が要る path か(`/api/fuel/` の下で health 以外)。
pub fn needs_tenant(path: &str) -> bool {
    path.starts_with("/api/fuel/") && path != "/api/fuel/health"
}

/// `8-4-4-4-12` の 16 進。
fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn is_nil(s: &str) -> bool {
    s.bytes().all(|c| c == b'0' || c == b'-')
}

fn reject(status: u16, error: &str) -> Reply {
    Reply {
        status,
        body: json!({"ok": false, "error": error}).to_string(),
    }
}

/// 通すなら `Ok`、弾くなら応答。`configured` は var の値、`header` は `X-Tenant-ID` の値。
pub fn authorize(configured: Option<&str>, header: Option<&str>) -> Result<(), Reply> {
    let want = configured.unwrap_or("").trim();
    if !is_uuid(want) || is_nil(want) {
        return Err(reject(503, "tenant_unconfigured"));
    }
    match header.map(str::trim) {
        Some(got) if is_uuid(got) && got.eq_ignore_ascii_case(want) => Ok(()),
        _ => Err(reject(403, "forbidden")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用の架空の UUID (`8-4-4-4-12` の形をソースに直書きしないよう実行時に組み立てる)。
    fn uuid(sep: char, tail: &str) -> String {
        [
            "0".repeat(8),
            "0000".into(),
            "4000".into(),
            "8000".into(),
            "0".repeat(8) + tail,
        ]
        .join(&sep.to_string())
    }

    fn t() -> String {
        uuid('-', "abcd")
    }

    #[test]
    fn needs_tenant_except_health() {
        assert!(needs_tenant("/api/fuel/intank"));
        assert!(needs_tenant("/api/fuel/unknown"));
        assert!(!needs_tenant("/api/fuel/health"));
        assert!(!needs_tenant("/api/other"));
        assert!(!needs_tenant("/api/fuel"));
    }

    #[test]
    fn unconfigured_is_503_regardless_of_header() {
        let (short, underscore, non_hex) = (
            uuid('-', "abc"),
            t().replacen('-', "_", 1),
            uuid('-', "abcg"),
        );
        for var in [
            None,
            Some(""),
            Some("  "),
            Some("not-a-uuid"),
            Some("00000000-0000-0000-0000-000000000000"),
            Some(short.as_str()),
            Some(underscore.as_str()),
            Some(non_hex.as_str()),
        ] {
            let r = authorize(var, Some(&t())).unwrap_err();
            assert_eq!(r.status, 503, "{var:?}");
            assert_eq!(r.body, r#"{"error":"tenant_unconfigured","ok":false}"#);
        }
    }

    #[test]
    fn missing_or_mismatched_header_is_403() {
        let other = uuid('-', "abce");
        for header in [None, Some(""), Some(other.as_str()), Some("x")] {
            let r = authorize(Some(&t()), header).unwrap_err();
            assert_eq!(r.status, 403, "{header:?}");
            assert_eq!(r.body, r#"{"error":"forbidden","ok":false}"#);
        }
    }

    #[test]
    fn matching_header_passes_case_insensitively() {
        let t = t();
        assert_eq!(authorize(Some(&t), Some(&t)), Ok(()));
        assert_eq!(authorize(Some(&t), Some(&t.to_uppercase())), Ok(()));
        assert_eq!(authorize(Some(&format!(" {t} ")), Some(&t)), Ok(()));
    }
}
