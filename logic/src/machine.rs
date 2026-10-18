//! USB 常駐アプリからの UTRAN.CSV の受け口 `POST /machine/fuel-utran/uploads` の判定 (純粋ロジック)。
//!
//! 経路: USB アプリ → auth-worker `/m/fuel-utran/uploads` (machine credential の Basic) → machine-relay
//! (tenant を決めない・`X-Tenant-*` を剥がす・`X-Internal-Shared-Secret` と `Authorization` を付けて中継) → この口。
//! path が `/api/fuel/` の外なので router の X-Tenant-ID 認可はかからない。代わりに次の順で通す:
//!
//! 1. header `X-Internal-Shared-Secret` を secret `INTERNAL_SHARED_SECRET` と定数時間で照合 ([`secrets_match`])。
//!    無い・不一致は 401、secret が読めなければ 503
//! 2. body を読む (上限は `crate::inbox::MAX_BYTES`。超えたら 413) → sha256
//! 3. auth-worker の RPC `issueTicket(Authorization, "fuel-utran", sha256)` ([`parse_issued`])。
//!    Authorization が無い・戻りが null は 401、RPC の失敗は 503
//! 4. 戻りの `tenantId` を `crate::auth::authorize` で var `FUEL_TENANT_ID` と突き合わせる (403 / 503)
//! 5. 以降は `POST /api/fuel/uploads` と同じ (R2 に put → 取り込み)。customMetadata は [`metadata`]
//!
//! Authorization・ticket・secret・username は応答にもログにも出さない (応答は [`reject`] の `error` だけ)。

use crate::inbox::error_body;
use crate::reply::Reply;

/// 共有 secret を運ぶ header (auth-worker の machine-relay が付ける)。
pub const SHARED_SECRET_HEADER: &str = "X-Internal-Shared-Secret";
/// 共有 secret の Secrets Store の binding。
pub const SHARED_SECRET_BINDING: &str = "INTERNAL_SHARED_SECRET";
/// auth-worker の `MachineCredentialEntrypoint` の Service Binding。
pub const MCRED_BINDING: &str = "AUTH_MCRED";
/// `issueTicket` の purpose。
pub const PURPOSE: &str = "fuel-utran";

/// header の値と secret が一致するか。どちらかが無い・空なら偽。
/// 比較は定数時間 (長さが違っても長い方の全バイトを見てから偽にする。nuxt-leave の `isRelayed` と同じ形)。
pub fn secrets_match(header: Option<&str>, secret: &str) -> bool {
    let (a, b) = (header.unwrap_or("").as_bytes(), secret.as_bytes());
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let mut diff = (a.len() ^ b.len()) as u64;
    for i in 0..a.len().max(b.len()) {
        diff |= u64::from(a[i % a.len()] ^ b[i % b.len()]);
    }
    diff == 0
}

/// `issueTicket` が通したときの戻り。値は応答にもログにも出さない (Debug も中身を伏せる)。
#[derive(PartialEq, Eq)]
pub struct Ticket {
    pub ticket: String,
    pub tenant_id: String,
    pub username: String,
}

impl std::fmt::Debug for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ticket(..)")
    }
}

/// `issueTicket` の戻りの解釈。
#[derive(Debug, PartialEq, Eq)]
pub enum Issued {
    Ticket(Ticket),
    /// null (Basic が通らない)。401
    Denied,
    /// 想定の形でない。503 (RPC の失敗と同じ扱い)
    Malformed,
}

/// `{ticket, tenantId, username}` (どれも空でない文字列) なら通す。null は拒否、それ以外の形は想定外。
pub fn parse_issued(value: &serde_json::Value) -> Issued {
    if value.is_null() {
        return Issued::Denied;
    }
    let field = |k: &str| {
        value
            .get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    match (field("ticket"), field("tenantId"), field("username")) {
        (Some(ticket), Some(tenant_id), Some(username)) => Issued::Ticket(Ticket {
            ticket,
            tenant_id,
            username,
        }),
        _ => Issued::Malformed,
    }
}

/// Authorization header の値 (空白だけ・無いなら `None` = 401。中身の検証は auth-worker がする)。
pub fn authorization(header: Option<&str>) -> Option<&str> {
    header.filter(|h| !h.trim().is_empty())
}

/// 弾くときの応答 (`{"ok":false,"error":…}`)。
pub fn reject(status: u16, error: &str) -> Reply {
    Reply {
        status,
        body: error_body(error),
    }
}

/// 共有 secret が無い・一致しない。
pub fn unauthorized() -> Reply {
    reject(401, "unauthorized")
}

/// secret が読めない・RPC が失敗した (`what` は `secret` か `auth`)。
pub fn unavailable(what: &str) -> Reply {
    reject(503, &[what, "_unavailable"].concat())
}

/// R2 の customMetadata (`tenant`・`source`=`usb`・`uploaded_at`・`ticket`・`machine_user`)。
pub fn metadata(t: &Ticket, uploaded_at: String) -> Vec<(String, String)> {
    vec![
        ("tenant".into(), t.tenant_id.clone()),
        ("source".into(), "usb".into()),
        ("uploaded_at".into(), uploaded_at),
        ("ticket".into(), t.ticket.clone()),
        ("machine_user".into(), t.username.clone()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secrets_match_only_equal_non_empty() {
        assert!(secrets_match(Some("s3cret"), "s3cret"));
        assert!(!secrets_match(Some("s3cret"), "s3creT"));
        assert!(!secrets_match(Some("s3cre"), "s3cret"));
        assert!(!secrets_match(Some("s3cret!"), "s3cret"));
        // 長さの違いを繰り返しで埋めても一致にならない
        assert!(!secrets_match(Some("abab"), "ab"));
        assert!(!secrets_match(None, "s3cret"));
        assert!(!secrets_match(Some(""), "s3cret"));
        assert!(!secrets_match(Some(""), ""));
        assert!(!secrets_match(Some("x"), ""));
    }

    fn ok() -> serde_json::Value {
        json!({"ticket": "tk", "tenantId": "tn", "username": "u"})
    }

    fn ticket() -> Ticket {
        Ticket {
            ticket: "tk".into(),
            tenant_id: "tn".into(),
            username: "u".into(),
        }
    }

    #[test]
    fn parse_issued_shapes() {
        assert_eq!(parse_issued(&ok()), Issued::Ticket(ticket()));
        assert_eq!(parse_issued(&serde_json::Value::Null), Issued::Denied);
        for k in ["ticket", "tenantId", "username"] {
            let mut v = ok();
            v[k] = json!("");
            assert_eq!(parse_issued(&v), Issued::Malformed, "{k} empty");
            v[k] = json!(1);
            assert_eq!(parse_issued(&v), Issued::Malformed, "{k} number");
        }
        assert_eq!(parse_issued(&json!({})), Issued::Malformed);
        assert_eq!(parse_issued(&json!("x")), Issued::Malformed);
    }

    #[test]
    fn ticket_debug_hides_values() {
        let shown = format!("{:?}", Issued::Ticket(ticket()));
        assert_eq!(shown, "Ticket(Ticket(..))");
    }

    #[test]
    fn authorization_must_be_non_blank() {
        assert_eq!(authorization(Some("Basic x")), Some("Basic x"));
        assert_eq!(authorization(Some("  ")), None);
        assert_eq!(authorization(None), None);
    }

    #[test]
    fn replies() {
        let r = unauthorized();
        assert_eq!(r.status, 401);
        assert_eq!(r.body, r#"{"error":"unauthorized","ok":false}"#);
        let r = unavailable("secret");
        assert_eq!(r.status, 503);
        assert_eq!(r.body, r#"{"error":"secret_unavailable","ok":false}"#);
        assert_eq!(
            unavailable("auth").body,
            r#"{"error":"auth_unavailable","ok":false}"#
        );
        assert_eq!(reject(413, "too_large").status, 413);
        assert_eq!(
            (
                SHARED_SECRET_HEADER,
                SHARED_SECRET_BINDING,
                MCRED_BINDING,
                PURPOSE
            ),
            (
                "X-Internal-Shared-Secret",
                "INTERNAL_SHARED_SECRET",
                "AUTH_MCRED",
                "fuel-utran"
            )
        );
    }

    #[test]
    fn metadata_for_usb() {
        let m = metadata(&ticket(), "2023-12-20T15:30:00Z".into());
        let pairs: Vec<(&str, &str)> = m.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            pairs,
            [
                ("tenant", "tn"),
                ("source", "usb"),
                ("uploaded_at", "2023-12-20T15:30:00Z"),
                ("ticket", "tk"),
                ("machine_user", "u"),
            ]
        );
    }
}
