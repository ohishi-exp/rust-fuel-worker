//! 失敗の段と応答の写像(純粋ロジック)。
//! rust-ichibanboshi の workers/kintai/worker/src/probe.rs の Stage・Failure・reply_for・log_line を縮めて写している。

/// どこで失敗したか。応答とログに出すのはこの名前だけ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// 資格情報の secret が無い・読めない・キー欠け・空
    Secret,
    /// TCP が開けない・時間切れ
    Connect,
    /// Initial Handshake が読めない
    Handshake,
    /// 認証が通らない
    Auth,
    /// クエリが失敗した・時間切れ・結果が想定と違う
    Query,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Secret => "secret",
            Stage::Connect => "connect",
            Stage::Handshake => "handshake",
            Stage::Auth => "auth",
            Stage::Query => "query",
        }
    }
}

/// 失敗(段と種別の名前だけ。サーバーの文言・宛先・ユーザー名は持たない)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub stage: Stage,
    pub kind: String,
}

impl Failure {
    pub fn new(stage: Stage, kind: impl Into<String>) -> Self {
        Self {
            stage,
            kind: kind.into(),
        }
    }
}

/// 応答の status と JSON 本文。
#[derive(Debug, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

/// health の結果の応答。成功は 200 `{"ok":true}`、失敗は 503 `{"ok":false,"stage":…}`(種別 kind は応答に出さない)。
pub fn reply_for(outcome: Result<(), Failure>) -> Reply {
    match outcome {
        Ok(()) => Reply {
            status: 200,
            body: r#"{"ok":true}"#.to_string(),
        },
        Err(f) => Reply {
            status: 503,
            body: serde_json::json!({"ok": false, "stage": f.stage.as_str()}).to_string(),
        },
    }
}

/// 失敗ログの 1 行(段・種別・所要ミリ秒だけ)。
pub fn log_line(f: &Failure, elapsed_ms: u64) -> String {
    let (stage, kind) = (f.stage.as_str(), &f.kind);
    format!("fuel health: failed at {stage} ({elapsed_ms} ms) kind={kind}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_names() {
        let names: Vec<_> = [
            Stage::Secret,
            Stage::Connect,
            Stage::Handshake,
            Stage::Auth,
            Stage::Query,
        ]
        .map(Stage::as_str)
        .to_vec();
        assert_eq!(names, ["secret", "connect", "handshake", "auth", "query"]);
    }

    #[test]
    fn reply_for_ok_is_200() {
        let r = reply_for(Ok(()));
        assert_eq!((r.status, r.body.as_str()), (200, r#"{"ok":true}"#));
    }

    #[test]
    fn reply_for_failure_is_503_with_stage_only() {
        for (stage, name) in [
            (Stage::Secret, "secret"),
            (Stage::Connect, "connect"),
            (Stage::Handshake, "handshake"),
            (Stage::Auth, "auth"),
            (Stage::Query, "query"),
        ] {
            let r = reply_for(Err(Failure::new(stage, "some_kind")));
            assert_eq!(r.status, 503);
            assert_eq!(r.body, format!(r#"{{"ok":false,"stage":"{name}"}}"#));
            assert!(!r.body.contains("some_kind"));
        }
    }

    #[test]
    fn log_line_has_stage_kind_and_elapsed_only() {
        let f = Failure::new(Stage::Auth, "denied");
        assert_eq!(
            log_line(&f, 12),
            "fuel health: failed at auth (12 ms) kind=denied"
        );
    }
}
