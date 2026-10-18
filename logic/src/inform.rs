//! 給油データの届き具合の通知 (純粋ロジック)。移植元は CakePHP の FuelIntankController::inform
//! (checkfuel.sh が毎日叩いていた)。
//!
//! fuel_intank の最新の datetime が「前日の 12:00 (JST)」より前なら LINE WORKS に 1 通送る。送り方は
//! nuxt-dtako-admin の dtako-scraper-relay (lineworks-notify.ts) と同じ: auth-worker の
//! `/alc-internal-proxy/api/internal/lineworks/send` を Service Binding (`AUTH_WORKER`) で叩き、ヘッダは
//! `X-Alc-Proxy-Secret` (= `INTERNAL_SHARED_SECRET`) だけ、本文は `{channel_id | recipient_id, text}`。
//! 宛先は var `FUEL_INFORM_TARGET` (`{"channel_id":"<uuid>"}` か `{"recipient_id":"<uuid>"}` の JSON 1 件。
//! rust-alc-api の lineworks_channels / notify_recipients の行 id)。未設定・不正なら送らない (fail-closed)。

use chrono::{DateTime, Duration, NaiveDateTime, NaiveTime};
use kintai_mysql::response::ResultSet;
use serde_json::{json, Value as Json};

use crate::api::{format_datetime, parse_datetime};

/// inform を走らせる cron (毎日 13:00 JST = 04:00 UTC)。wrangler.toml の `[triggers] crons` と同じ文字列。
pub const INFORM_CRON: &str = "0 4 * * *";

/// 最新の給油データの日時。列: max(datetime)
pub const LATEST_SQL: &str = "SELECT MAX(`datetime`) FROM fuel_intank";

/// auth-worker への Service Binding (既定の fetch の entrypoint)。
pub const AUTH_WORKER_BINDING: &str = "AUTH_WORKER";
/// Service Binding の fetch の URL (host は binding が無視する。path が auth-worker の内部用 proxy の口)。
pub const SEND_URL: &str =
    "https://auth-worker.internal/alc-internal-proxy/api/internal/lineworks/send";
/// 内部用 proxy に呼び手の Worker であることを示すヘッダ (値は `INTERNAL_SHARED_SECRET`)。
pub const PROXY_SECRET_HEADER: &str = "X-Alc-Proxy-Secret";
/// 宛先の var。
pub const TARGET_VAR: &str = "FUEL_INFORM_TARGET";

/// JST (UTC+9)。
const JST_OFFSET: Duration = Duration::hours(9);

/// UNIX ミリ秒を JST の日時にする (DB の datetime は JST の naive)。
pub fn jst_now(unix_ms: u64) -> NaiveDateTime {
    let utc = DateTime::from_timestamp_millis(unix_ms as i64).unwrap_or_default();
    utc.naive_utc() + JST_OFFSET
}

/// 前日の 12:00 (JST)。最新の行がこれより前なら通知する。
pub fn threshold(now: NaiveDateTime) -> NaiveDateTime {
    let noon = NaiveTime::from_hms_opt(12, 0, 0).unwrap_or_default();
    (now.date() - Duration::days(1)).and_time(noon)
}

/// 最新の行の日時 (`LATEST_SQL` の結果)。行が無い・NULL・読めないは `None`。
pub fn latest(rs: &ResultSet) -> Option<NaiveDateTime> {
    rs.text(0, 0).and_then(parse_datetime)
}

/// 送る文面。届いていれば `None`。1 行も無いときも送る (CakePHP は例外にしてメールしていた)。
pub fn message(latest: Option<NaiveDateTime>, now: NaiveDateTime) -> Option<String> {
    match latest {
        Some(l) if l >= threshold(now) => None,
        Some(l) => Some(text(&format_datetime(l))),
        None => Some(text("なし")),
    }
}

fn text(latest: &str) -> String {
    format!("給油データが前日分まで届いていません (最新: {latest})")
}

/// 届いていたときのログの 1 行。
pub fn ok_line(latest: NaiveDateTime) -> String {
    format!("fuel inform: ok latest={}", format_datetime(latest))
}

/// 宛先 (どちらも rust-alc-api の DB の行 id。LINE WORKS の channelId ではない)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Channel(String),
    Recipient(String),
}

/// UUID の形 (8-4-4-4-12 の 16 進)。
fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    let lens = parts.iter().map(|p| p.len()).collect::<Vec<_>>();
    lens == [8, 4, 4, 4, 12]
        && parts
            .iter()
            .all(|p| p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// `FUEL_INFORM_TARGET` を読む。未設定・空・JSON でない・両方ある・両方無い・UUID でないは `Err` (ログの種別)。
pub fn parse_target(var: Option<&str>) -> Result<Target, &'static str> {
    let raw = var
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or("unset")?;
    let Ok(Json::Object(obj)) = serde_json::from_str::<Json>(raw) else {
        return Err("invalid_json");
    };
    let get = |k: &str| {
        obj.get(k)
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let target = match (get("channel_id"), get("recipient_id")) {
        (Some(c), None) => Target::Channel(c.to_string()),
        (None, Some(r)) => Target::Recipient(r.to_string()),
        (Some(_), Some(_)) => return Err("both"),
        (None, None) => return Err("none"),
    };
    match &target {
        Target::Channel(id) | Target::Recipient(id) if is_uuid(id) => Ok(target),
        _ => Err("not_uuid"),
    }
}

/// `POST /api/internal/lineworks/send` の本文 (指定した側のキーだけを出す)。
pub fn send_body(target: &Target, text: &str) -> String {
    let body = match target {
        Target::Channel(id) => json!({"channel_id": id, "text": text}),
        Target::Recipient(id) => json!({"recipient_id": id, "text": text}),
    };
    body.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests::{dt, rs};

    const UUID: &str = "0f8b6c2e-1d3a-4b5c-9e7f-a1b2c3d4e5f6";

    #[test]
    fn jst_and_threshold() {
        // 2024-10-02 04:00:00 UTC = 13:00 JST
        assert_eq!(jst_now(1_727_841_600_000), dt("2024-10-02 13:00:00"));
        assert_eq!(
            threshold(dt("2024-10-02 13:00:00")),
            dt("2024-10-01 12:00:00")
        );
        assert_eq!(
            threshold(dt("2024-03-01 00:00:00")),
            dt("2024-02-29 12:00:00")
        );
    }

    #[test]
    fn decide_message() {
        let now = dt("2024-10-02 13:00:00");
        assert_eq!(message(Some(dt("2024-10-01 12:00:00")), now), None);
        assert_eq!(message(Some(dt("2024-10-02 08:00:00")), now), None);
        assert_eq!(
            message(Some(dt("2024-10-01 11:59:59")), now).unwrap(),
            "給油データが前日分まで届いていません (最新: 2024-10-01 11:59:59)"
        );
        assert_eq!(
            message(None, now).unwrap(),
            "給油データが前日分まで届いていません (最新: なし)"
        );
        assert_eq!(
            ok_line(dt("2024-10-02 08:00:00")),
            "fuel inform: ok latest=2024-10-02 08:00:00"
        );
    }

    #[test]
    fn latest_cell() {
        assert_eq!(
            latest(&rs(&[&[Some("2024-10-01 12:00:00")]])),
            Some(dt("2024-10-01 12:00:00"))
        );
        assert_eq!(latest(&rs(&[&[None]])), None);
        assert_eq!(latest(&rs(&[])), None);
        assert_eq!(latest(&rs(&[&[Some("x")]])), None);
        assert!(LATEST_SQL.starts_with("SELECT MAX("));
    }

    #[test]
    fn targets() {
        let ch = format!(r#"{{"channel_id":" {UUID} "}}"#);
        assert_eq!(parse_target(Some(&ch)), Ok(Target::Channel(UUID.into())));
        let re = format!(r#"{{"recipient_id":"{UUID}","channel_id":""}}"#);
        assert_eq!(parse_target(Some(&re)), Ok(Target::Recipient(UUID.into())));
        let both = format!(r#"{{"recipient_id":"{UUID}","channel_id":"{UUID}"}}"#);
        let cases = [
            (None, "unset"),
            (Some("  "), "unset"),
            (Some("x"), "invalid_json"),
            (Some("[]"), "invalid_json"),
            (Some(both.as_str()), "both"),
            (Some(r#"{"channel_id":1}"#), "none"),
            (Some(r#"{"channel_id":"abc"}"#), "not_uuid"),
            (
                Some(r#"{"recipient_id":"0f8b6c2e-1d3a-4b5c-9e7f-a1b2c3d4e5fg"}"#),
                "not_uuid",
            ),
            (
                Some(r#"{"recipient_id":"0f8b6c2e1d3a-4b5c-9e7f-a1b2c3d4e5f6"}"#),
                "not_uuid",
            ),
        ];
        for (var, want) in cases {
            assert_eq!(parse_target(var), Err(want), "{var:?}");
        }
    }

    #[test]
    fn bodies() {
        let c =
            serde_json::from_str::<Json>(&send_body(&Target::Channel(UUID.into()), "t")).unwrap();
        assert_eq!(c, json!({"channel_id": UUID, "text": "t"}));
        let r =
            serde_json::from_str::<Json>(&send_body(&Target::Recipient(UUID.into()), "t")).unwrap();
        assert_eq!(r, json!({"recipient_id": UUID, "text": "t"}));
        assert_eq!(INFORM_CRON, "0 4 * * *");
        assert!(SEND_URL.ends_with("/alc-internal-proxy/api/internal/lineworks/send"));
        assert_eq!(
            (AUTH_WORKER_BINDING, PROXY_SECRET_HEADER, TARGET_VAR),
            ("AUTH_WORKER", "X-Alc-Proxy-Secret", "FUEL_INFORM_TARGET")
        );
    }
}
