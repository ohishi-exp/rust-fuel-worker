//! `/api/fuel/*` の経路判定(純粋ロジック)。
//! rust-ichibanboshi の workers/kintai/worker/src/probe.rs の route・reply_for_route を写して口を足している。
//!
//! | path | method |
//! |---|---|
//! | `/api/fuel/health` | GET |
//! | `/api/fuel/starts` | GET |
//! | `/api/fuel/intank` | GET (`?start=<datetime>` 任意) |
//! | `/api/fuel/intank/{datetime};{fuel_card_id}` | PATCH・DELETE |
//! | `/api/fuel/unregistered-cards` | GET |
//! | `/api/fuel/cars` | GET |
//! | `/api/fuel/cards` | GET・POST |
//! | `/api/fuel/cards/{id}` | GET・PATCH・DELETE |
//! | `/api/fuel/uploads` | POST (UTRAN.CSV の取り込み。`crate::inbox`) |
//! | `/api/fuel/uploads/{sha256}` | GET |
//! | `/machine/fuel-utran/uploads` | POST (USB アプリからの UTRAN.CSV。X-Tenant-ID ではなく共有 secret と machine credential で通す。`crate::machine`) |
//!
//! path が合って method が違えば 405、path の値 (id・日時) が読めなければ 400、それ以外の path は 404。

use chrono::NaiveDateTime;

use crate::api::{parse_datetime, parse_id, percent_decode};
use crate::inbox::{parse_sha256, Sha256Hex};
use crate::reply::Reply;

/// USB アプリからの取り込みの口 (auth-worker の machine-relay が中継する。`/api/fuel/` の外)。
pub const MACHINE_UPLOAD_PATH: &str = "/machine/fuel-utran/uploads";

/// fuel_intank の主キー (datetime, fuel_card_id)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntankKey {
    pub datetime: NaiveDateTime,
    pub fuel_card_id: u64,
}

/// 経路判定の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `GET /api/fuel/health` — 繋いで `SELECT 1` を流す
    Health,
    Starts,
    IntankList,
    IntankPatch(IntankKey),
    IntankDelete(IntankKey),
    UnregisteredCards,
    Cars,
    CardsList,
    CardCreate,
    CardGet(u64),
    CardPatch(u64),
    CardDelete(u64),
    /// `POST /api/fuel/uploads`
    Upload,
    /// `GET /api/fuel/uploads/{sha256}`
    Download(Sha256Hex),
    /// `POST /machine/fuel-utran/uploads`
    MachineUpload,
    /// path の値が読めない (400)
    BadPath,
    NotFound,
    MethodNotAllowed,
}

/// path の `{datetime};{fuel_card_id}`(URL エンコード可。`+` も空白に戻す)。
pub fn parse_intank_key(segment: &str) -> Option<IntankKey> {
    let decoded = percent_decode(segment, true)?;
    let (datetime, card) = decoded.split_once(';')?;
    Some(IntankKey {
        datetime: parse_datetime(datetime)?,
        fuel_card_id: parse_id(card)?,
    })
}

/// method が `want` なら `route`、違えば 405。
fn only(method: &str, want: &str, route: Route) -> Route {
    if method == want {
        route
    } else {
        Route::MethodNotAllowed
    }
}

fn intank_item(method: &str, segment: &str) -> Route {
    let make: fn(IntankKey) -> Route = match method {
        "PATCH" => Route::IntankPatch,
        "DELETE" => Route::IntankDelete,
        _ => return Route::MethodNotAllowed,
    };
    parse_intank_key(segment).map_or(Route::BadPath, make)
}

fn card_item(method: &str, segment: &str) -> Route {
    let make: fn(u64) -> Route = match method {
        "GET" => Route::CardGet,
        "PATCH" => Route::CardPatch,
        "DELETE" => Route::CardDelete,
        _ => return Route::MethodNotAllowed,
    };
    parse_id(segment).map_or(Route::BadPath, make)
}

pub fn route(method: &str, path: &str) -> Route {
    if path == MACHINE_UPLOAD_PATH {
        return only(method, "POST", Route::MachineUpload);
    }
    let Some(rest) = path.strip_prefix("/api/fuel/") else {
        return Route::NotFound;
    };
    let segments: Vec<&str> = rest.split('/').collect();
    match segments.as_slice() {
        ["health"] => only(method, "GET", Route::Health),
        ["starts"] => only(method, "GET", Route::Starts),
        ["intank"] => only(method, "GET", Route::IntankList),
        ["intank", key] => intank_item(method, key),
        ["unregistered-cards"] => only(method, "GET", Route::UnregisteredCards),
        ["cars"] => only(method, "GET", Route::Cars),
        ["cards"] => match method {
            "GET" => Route::CardsList,
            "POST" => Route::CardCreate,
            _ => Route::MethodNotAllowed,
        },
        ["cards", id] => card_item(method, id),
        ["uploads"] => only(method, "POST", Route::Upload),
        ["uploads", sha] => match parse_sha256(sha) {
            Some(sha) => only(method, "GET", Route::Download(sha)),
            None => Route::BadPath,
        },
        _ => Route::NotFound,
    }
}

/// 経路判定で弾いた行き先の応答。口に当たったら `None`(先へ進む)。
pub fn reply_for_route(route: Route) -> Option<Reply> {
    let (status, body) = match route {
        Route::NotFound => (404, r#"{"ok":false}"#),
        Route::MethodNotAllowed => (405, r#"{"ok":false}"#),
        Route::BadPath => (400, r#"{"ok":false,"error":"bad_path"}"#),
        _ => return None,
    };
    Some(Reply {
        status,
        body: body.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests::dt;

    fn key(d: &str, card: u64) -> IntankKey {
        IntankKey {
            datetime: dt(d),
            fuel_card_id: card,
        }
    }

    #[test]
    fn route_picks_health_only_for_get() {
        assert_eq!(route("GET", "/api/fuel/health"), Route::Health);
        assert_eq!(route("POST", "/api/fuel/health"), Route::MethodNotAllowed);
        assert_eq!(route("GET", "/api/fuel/other"), Route::NotFound);
        assert_eq!(route("GET", "/"), Route::NotFound);
    }

    #[test]
    fn get_only_collections() {
        for (path, want) in [
            ("/api/fuel/starts", Route::Starts),
            ("/api/fuel/intank", Route::IntankList),
            ("/api/fuel/unregistered-cards", Route::UnregisteredCards),
            ("/api/fuel/cars", Route::Cars),
        ] {
            assert_eq!(route("GET", path), want);
            assert_eq!(route("POST", path), Route::MethodNotAllowed);
        }
    }

    #[test]
    fn intank_items() {
        let k = key("2024-10-01 06:58:00", 1234);
        let enc = "/api/fuel/intank/2024-10-01%2006%3A58%3A00;1234";
        assert_eq!(route("PATCH", enc), Route::IntankPatch(k));
        assert_eq!(route("DELETE", enc), Route::IntankDelete(k));
        let plus = "/api/fuel/intank/2024-10-01+06:58:00%3B1234";
        assert_eq!(route("DELETE", plus), Route::IntankDelete(k));
        assert_eq!(route("GET", enc), Route::MethodNotAllowed);
        for bad in [
            "/api/fuel/intank/2024-10-01;1234",
            "/api/fuel/intank/2024-10-01%2006:58:00",
            "/api/fuel/intank/2024-10-01%2006:58:00;x",
            "/api/fuel/intank/%zz",
        ] {
            assert_eq!(route("PATCH", bad), Route::BadPath, "{bad}");
        }
        assert_eq!(route("PATCH", "/api/fuel/intank/a/b"), Route::NotFound);
    }

    #[test]
    fn cards() {
        assert_eq!(route("GET", "/api/fuel/cards"), Route::CardsList);
        assert_eq!(route("POST", "/api/fuel/cards"), Route::CardCreate);
        assert_eq!(route("PUT", "/api/fuel/cards"), Route::MethodNotAllowed);
        assert_eq!(route("GET", "/api/fuel/cards/12"), Route::CardGet(12));
        assert_eq!(route("PATCH", "/api/fuel/cards/12"), Route::CardPatch(12));
        assert_eq!(route("DELETE", "/api/fuel/cards/12"), Route::CardDelete(12));
        assert_eq!(route("POST", "/api/fuel/cards/12"), Route::MethodNotAllowed);
        assert_eq!(route("GET", "/api/fuel/cards/x"), Route::BadPath);
        assert_eq!(route("GET", "/api/fuel/cards/"), Route::BadPath);
    }

    #[test]
    fn uploads() {
        let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(route("POST", "/api/fuel/uploads"), Route::Upload);
        assert_eq!(route("GET", "/api/fuel/uploads"), Route::MethodNotAllowed);
        let path = format!("/api/fuel/uploads/{sha}");
        let want = Route::Download(parse_sha256(sha).unwrap());
        assert_eq!(route("GET", &path), want);
        assert_eq!(route("POST", &path), Route::MethodNotAllowed);
        assert_eq!(route("GET", "/api/fuel/uploads/abc"), Route::BadPath);
    }

    #[test]
    fn machine_upload() {
        let p = "/machine/fuel-utran/uploads";
        assert_eq!(route("POST", p), Route::MachineUpload);
        assert_eq!(route("GET", p), Route::MethodNotAllowed);
        assert_eq!(
            route("POST", "/machine/fuel-utran/uploads/x"),
            Route::NotFound
        );
        assert_eq!(route("POST", "/machine/other"), Route::NotFound);
        assert_eq!(reply_for_route(Route::MachineUpload), None);
        // /api/fuel/ の外なので X-Tenant-ID の認可はかからない (machine の順で通す)
        assert!(!crate::auth::needs_tenant(p));
    }

    #[test]
    fn reply_for_route_rejects_unknown_only() {
        assert_eq!(reply_for_route(Route::Health), None);
        assert_eq!(reply_for_route(Route::CardsList), None);
        let nf = reply_for_route(Route::NotFound).unwrap();
        assert_eq!((nf.status, nf.body.as_str()), (404, r#"{"ok":false}"#));
        let na = reply_for_route(Route::MethodNotAllowed).unwrap();
        assert_eq!(na.status, 405);
        let bp = reply_for_route(Route::BadPath).unwrap();
        assert_eq!(
            (bp.status, bp.body.as_str()),
            (400, r#"{"ok":false,"error":"bad_path"}"#)
        );
    }
}
