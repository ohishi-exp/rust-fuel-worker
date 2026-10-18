//! 参照・編集の口に共通の部品(純粋ロジック): 失敗の応答・結果セットのセルの JSON 化・path と query と本文の読み取り。
//!
//! 応答の形: 成功は `{"ok":true,…}`、入力の誤りは 400 `{"ok":false,"error":<code>}`、無い行は 404、
//! 状態が合わない操作は 409、DB に届かない・クエリが失敗したは 503 `{"ok":false,"stage":…}`(health と同じ)、
//! SQL 定数の展開の失敗(コードの誤り)は 500。サーバーの文言・SQL の本文は応答にもログにも出さない。

use chrono::NaiveDateTime;
use kintai_mysql::bind::{expand, BindError, Value};
use kintai_mysql::response::ResultSet;
use serde_json::{json, Map, Value as Json};

use crate::reply::{Failure, Reply};

/// 日時の書式(応答・path・query・本文で共通)。
pub const DATETIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// 主キーの重複 (ER_DUP_ENTRY)。`kintai_mysql::Error::kind` が `server:<番号>` で返す。
const DUPLICATE_KIND: &str = "server:1062";
/// 外部キーで参照されている行の削除・更新 (ER_ROW_IS_REFERENCED_2)。
const REFERENCED_KIND: &str = "server:1451";

/// 口の失敗。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// 400。値は応答の `error`(入力のどこが悪いか)
    BadRequest(&'static str),
    /// 404
    NotFound,
    /// 409。値は応答の `error`
    Conflict(&'static str),
    /// 503。DB の段の失敗
    Db(Failure),
    /// 500。値はログに出す種別(SQL 定数の展開の失敗・DB の値が想定の形でない)
    Internal(String),
}

impl From<BindError> for ApiError {
    fn from(e: BindError) -> Self {
        ApiError::Internal(e.kind().to_string())
    }
}

impl From<Failure> for ApiError {
    fn from(f: Failure) -> Self {
        ApiError::Db(f)
    }
}

impl ApiError {
    /// 書き込みの失敗のうち、主キーの重複と外部キーの参照だけを 409 にする。
    pub fn from_write(f: Failure) -> Self {
        match f.kind.as_str() {
            DUPLICATE_KIND => ApiError::Conflict("duplicate"),
            REFERENCED_KIND => ApiError::Conflict("referenced"),
            _ => ApiError::Db(f),
        }
    }

    pub fn reply(&self) -> Reply {
        let (status, body) = match self {
            ApiError::BadRequest(code) => (400, json!({"ok": false, "error": code})),
            ApiError::NotFound => (404, json!({"ok": false, "error": "not_found"})),
            ApiError::Conflict(code) => (409, json!({"ok": false, "error": code})),
            ApiError::Db(f) => (503, json!({"ok": false, "stage": f.stage.as_str()})),
            ApiError::Internal(_) => (500, json!({"ok": false, "error": "internal"})),
        };
        Reply {
            status,
            body: body.to_string(),
        }
    }

    /// 失敗ログの 1 行(口の名前・段・種別・所要ミリ秒だけ)。利用者の入力の誤り (4xx) はログに出さない。
    pub fn log_line(&self, name: &str, elapsed_ms: u64) -> Option<String> {
        let (stage, kind) = match self {
            ApiError::Db(f) => (f.stage.as_str(), f.kind.as_str()),
            ApiError::Internal(kind) => ("internal", kind.as_str()),
            _ => return None,
        };
        Some(format!(
            "fuel {name}: failed at {stage} ({elapsed_ms} ms) kind={kind}"
        ))
    }
}

/// 200 `{"ok":true, …body}`。
pub fn ok(body: Map<String, Json>) -> Reply {
    with_status(200, body)
}

/// `status` で `{"ok":true, …body}`。
pub fn with_status(status: u16, mut body: Map<String, Json>) -> Reply {
    body.insert("ok".to_string(), Json::Bool(true));
    Reply {
        status,
        body: Json::Object(body).to_string(),
    }
}

/// SQL 定数の名前付き引数を展開する(展開の失敗は SQL 定数と引数の食い違い = コードの誤りなので 500)。
pub fn bind(sql: &str, params: &[(&str, Value)]) -> Result<String, ApiError> {
    Ok(expand(sql, params)?)
}

/// 1 行も無ければ 404。
pub fn require_row(rs: &ResultSet) -> Result<(), ApiError> {
    if rs.rows.is_empty() {
        Err(ApiError::NotFound)
    } else {
        Ok(())
    }
}

/// セルを JSON の数にする(整数・小数として読めなければ文字列のまま、NULL は null)。
pub fn num(cell: Option<&str>) -> Json {
    let Some(s) = cell else { return Json::Null };
    if let Ok(n) = s.parse::<i64>() {
        return Json::from(n);
    }
    match s.parse::<f64>().ok().and_then(serde_json::Number::from_f64) {
        Some(n) => Json::Number(n),
        None => Json::String(s.to_string()),
    }
}

/// セルを JSON の文字列にする(NULL は null)。
pub fn text(cell: Option<&str>) -> Json {
    cell.map_or(Json::Null, |s| Json::String(s.to_string()))
}

/// tinyint(1) のセルを JSON の真偽にする(`0` は false、それ以外は true、NULL は null)。
pub fn flag(cell: Option<&str>) -> Json {
    cell.map_or(Json::Null, |s| Json::Bool(s != "0"))
}

/// `YYYY-MM-DD HH:MM:SS` だけを受ける(19 文字ちょうど)。
pub fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    if s.len() != 19 {
        return None;
    }
    NaiveDateTime::parse_from_str(s, DATETIME_FORMAT).ok()
}

pub fn format_datetime(dt: NaiveDateTime) -> String {
    dt.format(DATETIME_FORMAT).to_string()
}

/// 数字だけの id(1〜18 桁)。符号・空白・空は `None`。
pub fn parse_id(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 18 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `%XX` を戻す。`plus_as_space` なら `+` も空白に戻す。`%` の後が 16 進 2 桁でない・UTF-8 でなければ `None`。
pub fn percent_decode(s: &str, plus_as_space: bool) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' if plus_as_space => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// query の `name` の値(最初の 1 つ、デコード後)。無ければ `None`、デコードできなければ 400。
pub fn query_param(query: Option<&str>, name: &str) -> Result<Option<String>, ApiError> {
    for pair in query.unwrap_or("").split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == name {
            return percent_decode(v, true)
                .map(Some)
                .ok_or(ApiError::BadRequest("bad_query"));
        }
    }
    Ok(None)
}

/// 本文を JSON の object として読み、`allowed` に無いキーを 400 で弾く。
pub fn parse_object(body: &str, allowed: &[&str]) -> Result<Map<String, Json>, ApiError> {
    let Ok(Json::Object(obj)) = serde_json::from_str::<Json>(body) else {
        return Err(ApiError::BadRequest("invalid_json"));
    };
    if obj.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(ApiError::BadRequest("unknown_field"));
    }
    Ok(obj)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::reply::Stage;
    use kintai_mysql::response::Column;

    /// テスト用の結果セット(列の名前は使わないので空)。
    pub(crate) fn rs(rows: &[&[Option<&str>]]) -> ResultSet {
        let width = rows.first().map_or(0, |r| r.len());
        ResultSet {
            columns: (0..width)
                .map(|_| Column {
                    name: String::new(),
                    column_type: 0,
                    charset: 0,
                })
                .collect(),
            rows: rows
                .iter()
                .map(|r| r.iter().map(|c| c.map(|s| s.as_bytes().to_vec())).collect())
                .collect(),
        }
    }

    pub(crate) fn dt(s: &str) -> NaiveDateTime {
        parse_datetime(s).unwrap()
    }

    fn body(r: &Reply) -> Json {
        serde_json::from_str(&r.body).unwrap()
    }

    #[test]
    fn error_replies() {
        let cases = [
            (
                ApiError::BadRequest("x"),
                400,
                json!({"ok": false, "error": "x"}),
            ),
            (
                ApiError::NotFound,
                404,
                json!({"ok": false, "error": "not_found"}),
            ),
            (
                ApiError::Conflict("y"),
                409,
                json!({"ok": false, "error": "y"}),
            ),
            (
                ApiError::Db(Failure::new(Stage::Connect, "timeout")),
                503,
                json!({"ok": false, "stage": "connect"}),
            ),
            (
                ApiError::Internal("bind_unknown_name".into()),
                500,
                json!({"ok": false, "error": "internal"}),
            ),
        ];
        for (e, status, want) in cases {
            let r = e.reply();
            assert_eq!((r.status, body(&r)), (status, want));
        }
    }

    #[test]
    fn conversions_and_duplicate() {
        let e: ApiError = BindError::Comment.into();
        assert_eq!(e, ApiError::Internal("bind_comment".into()));
        let f = Failure::new(Stage::Query, "server:1146");
        assert_eq!(ApiError::from(f.clone()), ApiError::Db(f.clone()));
        assert_eq!(ApiError::from_write(f.clone()), ApiError::Db(f));
        let dup = Failure::new(Stage::Query, "server:1062");
        assert_eq!(ApiError::from_write(dup), ApiError::Conflict("duplicate"));
        let referenced = Failure::new(Stage::Query, "server:1451");
        assert_eq!(
            ApiError::from_write(referenced),
            ApiError::Conflict("referenced")
        );
    }

    #[test]
    fn bind_maps_errors_to_internal() {
        assert_eq!(
            bind("SELECT :a", &[("a", Value::UInt(1))]).unwrap(),
            "SELECT 1"
        );
        assert_eq!(
            bind("SELECT :a", &[]),
            Err(ApiError::Internal("bind_unknown_name".into()))
        );
    }

    #[test]
    fn log_line_only_for_server_side() {
        let db = ApiError::Db(Failure::new(Stage::Auth, "denied"));
        assert_eq!(
            db.log_line("cards", 7).unwrap(),
            "fuel cards: failed at auth (7 ms) kind=denied"
        );
        let internal = ApiError::Internal("bind_comment".into());
        assert_eq!(
            internal.log_line("intank", 1).unwrap(),
            "fuel intank: failed at internal (1 ms) kind=bind_comment"
        );
        assert_eq!(ApiError::NotFound.log_line("x", 0), None);
    }

    #[test]
    fn ok_replies() {
        let mut m = Map::new();
        m.insert("rows".into(), json!([]));
        let r = ok(m.clone());
        assert_eq!((r.status, body(&r)), (200, json!({"ok": true, "rows": []})));
        assert_eq!(with_status(201, m).status, 201);
    }

    #[test]
    fn require_row_is_404_when_empty() {
        assert_eq!(require_row(&rs(&[])), Err(ApiError::NotFound));
        assert_eq!(require_row(&rs(&[&[Some("1")]])), Ok(()));
    }

    #[test]
    fn cells() {
        assert_eq!(num(Some("12")), json!(12));
        assert_eq!(num(Some("12.5")), json!(12.5));
        assert_eq!(num(Some("abc")), json!("abc"));
        assert_eq!(num(Some("NaN")), json!("NaN"));
        assert_eq!(num(None), Json::Null);
        assert_eq!(text(Some("a")), json!("a"));
        assert_eq!(text(None), Json::Null);
        assert_eq!(flag(Some("0")), json!(false));
        assert_eq!(flag(Some("1")), json!(true));
        assert_eq!(flag(None), Json::Null);
    }

    #[test]
    fn datetimes_and_ids() {
        let d = dt("2024-10-01 12:30:00");
        assert_eq!(format_datetime(d), "2024-10-01 12:30:00");
        assert_eq!(parse_datetime("2024-10-01 12:30"), None);
        assert_eq!(parse_datetime("2024-13-01 12:30:00"), None);
        assert_eq!(parse_id("0042"), Some(42));
        for bad in ["", "+1", "-1", "1a", " 1", "1234567890123456789"] {
            assert_eq!(parse_id(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%20b%3B1", false).unwrap(), "a b;1");
        assert_eq!(percent_decode("a+b", false).unwrap(), "a+b");
        assert_eq!(percent_decode("a+b", true).unwrap(), "a b");
        assert_eq!(percent_decode("%E7%87%83", false).unwrap(), "燃");
        assert_eq!(percent_decode("%2", false), None);
        assert_eq!(percent_decode("%zz", false), None);
        assert_eq!(percent_decode("%FF", false), None);
        assert_eq!(percent_decode("%C3%", false), None);
        assert_eq!(percent_decode("%\u{e9}x", false), None);
    }

    #[test]
    fn query_params() {
        let q = Some("x=1&start=2024-10-01+06%3A58%3A00&start=other&flag");
        assert_eq!(
            query_param(q, "start").unwrap().as_deref(),
            Some("2024-10-01 06:58:00")
        );
        assert_eq!(query_param(q, "flag").unwrap().as_deref(), Some(""));
        assert_eq!(query_param(q, "none").unwrap(), None);
        assert_eq!(query_param(None, "start").unwrap(), None);
        assert_eq!(
            query_param(Some("start=%zz"), "start"),
            Err(ApiError::BadRequest("bad_query"))
        );
    }

    #[test]
    fn objects() {
        let obj = parse_object(r#"{"a":1}"#, &["a", "b"]).unwrap();
        assert_eq!(obj.get("a"), Some(&json!(1)));
        assert_eq!(
            parse_object("[1]", &["a"]),
            Err(ApiError::BadRequest("invalid_json"))
        );
        assert_eq!(
            parse_object("{", &["a"]),
            Err(ApiError::BadRequest("invalid_json"))
        );
        assert_eq!(
            parse_object(r#"{"c":1}"#, &["a"]),
            Err(ApiError::BadRequest("unknown_field"))
        );
    }
}
