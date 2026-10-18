//! `GET /api/fuel/health` の経路判定(純粋ロジック)。
//! rust-ichibanboshi の workers/kintai/worker/src/probe.rs の route・reply_for_route を縮めて写している。

use crate::reply::Reply;

/// 経路判定の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `GET /api/fuel/health` — 繋いで `SELECT 1` を流す
    Health,
    NotFound,
    MethodNotAllowed,
}

/// 口は `GET /api/fuel/health` の 1 本。path が合って method が違えば 405、それ以外の path は 404。
pub fn route(method: &str, path: &str) -> Route {
    match (path, method) {
        ("/api/fuel/health", "GET") => Route::Health,
        ("/api/fuel/health", _) => Route::MethodNotAllowed,
        _ => Route::NotFound,
    }
}

/// 経路判定で弾いた行き先の応答。口に当たったら `None`(先へ進む)。
pub fn reply_for_route(route: Route) -> Option<Reply> {
    let status = match route {
        Route::NotFound => 404,
        Route::MethodNotAllowed => 405,
        Route::Health => return None,
    };
    Some(Reply {
        status,
        body: r#"{"ok":false}"#.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_picks_health_only_for_get() {
        assert_eq!(route("GET", "/api/fuel/health"), Route::Health);
        assert_eq!(route("POST", "/api/fuel/health"), Route::MethodNotAllowed);
        assert_eq!(route("GET", "/api/fuel/other"), Route::NotFound);
        assert_eq!(route("GET", "/"), Route::NotFound);
    }

    #[test]
    fn reply_for_route_rejects_unknown_only() {
        assert_eq!(reply_for_route(Route::Health), None);
        let nf = reply_for_route(Route::NotFound).unwrap();
        assert_eq!((nf.status, nf.body.as_str()), (404, r#"{"ok":false}"#));
        let na = reply_for_route(Route::MethodNotAllowed).unwrap();
        assert_eq!(na.status, 405);
    }
}
