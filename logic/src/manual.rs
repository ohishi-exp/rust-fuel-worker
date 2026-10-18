//! fuel_intank の手入力(純粋ロジック): 伝票番号の候補・手入力の本文の検査と行の組み立て・recheck。
//! 移植元は CakePHP の FuelIntankController の addManual・addNew・recheck と FuelIntankTable の
//! getManualCandidate・beforeMarshal (is_inputForm・is_insert の分岐)・beforeSave (dtako_row_id の照合)。
//!
//! 値はすべて `crate::api::bind` の名前付き引数で入れる(文字列は `Token` / `Digits` だけ)。

use std::collections::BTreeSet;

use chrono::{Months, NaiveDateTime};
use kintai_mysql::bind::{Digits, Token, Value};
use kintai_mysql::response::ResultSet;
use serde_json::{json, Map, Value as Json};

use crate::api::{bind, format_datetime, parse_datetime, parse_object, require_row, ApiError};
use crate::intank::START_CARD_ID;
use crate::route::IntankKey;

/// 候補を探す範囲(getManualCandidate: 直近 2 か月の行)。
const CANDIDATE_MONTHS: u32 = 2;

/// 直近の行の `slip_num + 1` のうち、まだどの行にも使われていない番号(getManualCandidate)。列: slip_num
/// (slip_num が NULL の行は `NOT IN` を空にしてしまうので副問い合わせから外す)
pub const CANDIDATES_SQL: &str = "SELECT DISTINCT slip_num + 1 FROM fuel_intank \
     WHERE `datetime` >= :since AND slip_num IS NOT NULL \
     AND slip_num + 1 NOT IN (SELECT slip_num FROM fuel_intank WHERE slip_num IS NOT NULL) \
     ORDER BY 1 ASC";

/// 最新の行(addManual の lastInserted)。列: datetime, fuel_card_id, slip_num
pub const LAST_INSERTED_SQL: &str = "SELECT `datetime`, fuel_card_id, slip_num FROM fuel_intank \
     ORDER BY `datetime` DESC LIMIT 1";

/// 手入力の行の追加(manual = 1)
pub const INSERT_SQL: &str = "INSERT INTO fuel_intank \
     (`datetime`, fuel_card_id, slip_num, slip_num_parent, quantity, `type`, detail, dtako_row_id, manual) \
     VALUES (:datetime, :fuel_card_id, :slip_num, :slip_num_parent, :quantity, :type, :detail, \
     :dtako_row_id, 1)";

/// 手入力の行の上書き(manual が真の行だけ。CSV 由来の行は手入力で上書きしない)
pub const UPDATE_SQL: &str = "UPDATE fuel_intank SET slip_num = :slip_num, \
     slip_num_parent = :slip_num_parent, quantity = :quantity, `type` = :type, detail = :detail, \
     dtako_row_id = :dtako_row_id, manual = 1 \
     WHERE `datetime` = :datetime AND fuel_card_id = :fuel_card_id AND manual <> 0";

/// recheck: dtako_row_id を照合し直した値にして manual を NULL にする
pub const RECHECK_SQL: &str = "UPDATE fuel_intank SET dtako_row_id = :dtako_row_id, manual = NULL \
     WHERE `datetime` = :datetime AND fuel_card_id = :fuel_card_id";

/// start 行の detail(CakePHP の is_insert の分岐が入れる値)。
pub const START_DETAIL: &str = "EP900-34";

/// 伝票番号・カード番号の上限(18 桁。`api::parse_id` と同じ)。
const ID_MAX: u64 = 999_999_999_999_999_999;
/// 給油量の上限(L)。×100 で u64 にしても溢れない範囲に抑える。
const QUANTITY_MAX: f64 = 1_000_000.0;

pub fn candidates_sql(now: NaiveDateTime) -> Result<String, ApiError> {
    let since = now - Months::new(CANDIDATE_MONTHS);
    bind(CANDIDATES_SQL, &[("since", Value::DateTime(since))])
}

/// `GET /api/fuel/manual-candidates` の本文。`last` は `LAST_INSERTED_SQL` の結果。
/// 最新の行の `slip_num + 1` が候補に無ければ足す(addManual)。候補は昇順・重複なし。
pub fn candidates_json(candidates: &ResultSet, last: &ResultSet) -> Map<String, Json> {
    let mut set: BTreeSet<u64> = (0..candidates.rows.len())
        .filter_map(|i| candidates.text(i, 0)?.parse().ok())
        .collect();
    let last_slip = last.text(0, 2).and_then(|s| s.parse::<u64>().ok());
    if let Some(next) = last_slip.and_then(|s| s.checked_add(1)) {
        set.insert(next);
    }
    let last_inserted = match last.rows.is_empty() {
        true => Json::Null,
        false => json!({
            "datetime": crate::api::text(last.text(0, 0)),
            "fuel_card_id": crate::api::num(last.text(0, 1)),
            "slip_num": crate::api::num(last.text(0, 2)),
        }),
    };
    let mut m = Map::new();
    m.insert(
        "candidates".into(),
        json!(set.into_iter().collect::<Vec<_>>()),
    );
    m.insert("last_inserted".into(), last_inserted);
    m
}

/// 手入力の行(addNew の本文を beforeMarshal の規則で組み立てたもの)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManualRow {
    pub key: IntankKey,
    pub slip_num: u64,
    pub slip_num_parent: Option<u64>,
    /// 0.01 L 単位。start 行は `None`
    pub quantity: Option<u64>,
}

impl ManualRow {
    /// fuel_card_id が 9999 なら start 行(dtako の照合をしない)。
    pub fn is_start(&self) -> bool {
        self.key.fuel_card_id == START_CARD_ID
    }

    fn kind(&self) -> &'static str {
        match self.is_start() {
            true => "start",
            false => "fuel",
        }
    }
}

/// JSON の 0 以上の整数(18 桁まで)。
fn uint(v: &Json) -> Option<u64> {
    v.as_u64().filter(|n| *n <= ID_MAX)
}

/// 無い・null は `None`、0 以上の整数は `Some`、それ以外は 400。
fn opt_uint(obj: &Map<String, Json>, name: &'static str) -> Result<Option<u64>, ApiError> {
    match obj.get(name) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => uint(v).map(Some).ok_or(ApiError::BadRequest(name)),
    }
}

/// 給油量(L、小数 2 桁)を 0.01 L の整数にする(四捨五入)。
fn quantity(v: &Json) -> Option<u64> {
    let q = v.as_f64().filter(|q| (0.0..QUANTITY_MAX).contains(q))?;
    Some((q * 100.0).round() as u64)
}

/// `POST /api/fuel/intank` の本文:
/// `{"datetime": "YYYY-MM-DD HH:MM:SS", "slip_num": <整数>, "fuel_card_id"?: <整数>|null,
///   "quantity_decimal"?: <数>, "slip_num_parent"?: <整数>|null}`。
///
/// fuel_card_id がある → fuel 行(quantity_decimal は必須。quantity = round(quantity_decimal × 100))。
/// fuel_card_id が無い・null → start 行(fuel_card_id 9999・quantity と slip_num_parent は NULL。
/// quantity_decimal・slip_num_parent は送られても使わない)。fuel_card_id の 0 と 9999 は 400。
pub fn parse_create(body: &str) -> Result<ManualRow, ApiError> {
    let allowed = [
        "datetime",
        "slip_num",
        "fuel_card_id",
        "quantity_decimal",
        "slip_num_parent",
    ];
    let obj = parse_object(body, &allowed)?;
    let datetime = obj
        .get("datetime")
        .and_then(Json::as_str)
        .and_then(parse_datetime)
        .ok_or(ApiError::BadRequest("datetime"))?;
    let slip_num = obj
        .get("slip_num")
        .and_then(uint)
        .ok_or(ApiError::BadRequest("slip_num"))?;
    let card = opt_uint(&obj, "fuel_card_id")?;
    let Some(card) = card else {
        return Ok(ManualRow {
            key: IntankKey {
                datetime,
                fuel_card_id: START_CARD_ID,
            },
            slip_num,
            slip_num_parent: None,
            quantity: None,
        });
    };
    if card == 0 || card == START_CARD_ID {
        return Err(ApiError::BadRequest("fuel_card_id"));
    }
    let quantity = obj
        .get("quantity_decimal")
        .and_then(quantity)
        .ok_or(ApiError::BadRequest("quantity_decimal"))?;
    Ok(ManualRow {
        key: IntankKey {
            datetime,
            fuel_card_id: card,
        },
        slip_num,
        slip_num_parent: opt_uint(&obj, "slip_num_parent")?,
        quantity: Some(quantity),
    })
}

fn opt(n: Option<u64>) -> Value {
    n.map_or(Value::Null, Value::UInt)
}

fn row_params(row: &ManualRow, dtako_row_id: Option<Digits>) -> Vec<(&'static str, Value)> {
    let detail = match row.is_start() {
        true => Token::new(START_DETAIL).map_or(Value::Null, Value::Token),
        false => Value::Null,
    };
    vec![
        ("datetime", Value::DateTime(row.key.datetime)),
        ("fuel_card_id", Value::UInt(row.key.fuel_card_id)),
        ("slip_num", Value::UInt(row.slip_num)),
        ("slip_num_parent", opt(row.slip_num_parent)),
        ("quantity", opt(row.quantity)),
        (
            "type",
            Token::new(row.kind()).map_or(Value::Null, Value::Token),
        ),
        ("detail", detail),
        (
            "dtako_row_id",
            dtako_row_id.map_or(Value::Null, Value::Digits),
        ),
    ]
}

/// 既にある行(`intank::ROW_SQL` の結果)に対して何をするか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    Insert,
    Update,
}

/// 同じ (datetime, fuel_card_id) が無ければ追加、manual の行なら上書き、manual でない行(CSV 由来)は 409。
pub fn decide_write(existing: &ResultSet) -> Result<Write, ApiError> {
    if existing.rows.is_empty() {
        return Ok(Write::Insert);
    }
    match existing.text(0, 8) {
        Some(m) if m != "0" => Ok(Write::Update),
        _ => Err(ApiError::Conflict("not_manual")),
    }
}

/// 追加・上書きの SQL(start 行の dtako_row_id は常に NULL)。
pub fn write_sql(
    write: Write,
    row: &ManualRow,
    dtako_row_id: Option<Digits>,
) -> Result<String, ApiError> {
    let id = dtako_row_id.filter(|_| !row.is_start());
    let sql = match write {
        Write::Insert => INSERT_SQL,
        Write::Update => UPDATE_SQL,
    };
    bind(sql, &row_params(row, id))
}

/// 追加は 201、上書きは 200。
pub fn write_status(write: Write) -> u16 {
    match write {
        Write::Insert => 201,
        Write::Update => 200,
    }
}

/// recheck で照合し直すか(`intank::ROW_SQL` の結果。無ければ 404)。
/// 照合するのは fuel の行で dtako_row_id がまだ無いときだけ(beforeSave の getDtakoRowID は既にある値を返す)。
/// 既にある値はそのまま `Keep` で返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recheck {
    /// 照合してその結果を入れる
    Match(IntankKey),
    /// この値のまま(start 行・既に付いている行)
    Keep(Option<Digits>),
}

pub fn decide_recheck(key: IntankKey, rs: &ResultSet) -> Result<Recheck, ApiError> {
    require_row(rs)?;
    if key.fuel_card_id == START_CARD_ID {
        return Ok(Recheck::Keep(None));
    }
    match rs.text(0, 7) {
        Some(id) => Ok(Recheck::Keep(Digits::new(id))),
        None => Ok(Recheck::Match(key)),
    }
}

pub fn recheck_sql(key: IntankKey, dtako_row_id: Option<Digits>) -> Result<String, ApiError> {
    bind(
        RECHECK_SQL,
        &[
            (
                "dtako_row_id",
                dtako_row_id.map_or(Value::Null, Value::Digits),
            ),
            ("datetime", Value::DateTime(key.datetime)),
            ("fuel_card_id", Value::UInt(key.fuel_card_id)),
        ],
    )
}

/// ログ用の主キー(`{datetime};{fuel_card_id}`)。
pub fn key_label(key: IntankKey) -> String {
    format!("{};{}", format_datetime(key.datetime), key.fuel_card_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests::{dt, rs};
    use kintai_mysql::bind::names;

    const NOW: &str = "2024-12-31 10:00:00";
    const KEY_TEXT: &str = "2024-10-01 06:58:00";

    fn row9<'a>(dtako: Option<&'a str>, manual: Option<&'a str>) -> Vec<Option<&'a str>> {
        vec![
            Some(KEY_TEXT),
            Some("57"),
            Some("5"),
            Some("4"),
            Some("12345"),
            Some("fuel"),
            None,
            dtako,
            manual,
        ]
    }

    fn fuel_key() -> IntankKey {
        IntankKey {
            datetime: dt(KEY_TEXT),
            fuel_card_id: 57,
        }
    }

    #[test]
    fn sql_names() {
        let all = [
            "datetime",
            "fuel_card_id",
            "slip_num",
            "slip_num_parent",
            "quantity",
            "type",
            "detail",
            "dtako_row_id",
        ];
        let update = [
            "slip_num",
            "slip_num_parent",
            "quantity",
            "type",
            "detail",
            "dtako_row_id",
            "datetime",
            "fuel_card_id",
        ];
        let cases: [(&str, &[&str]); 5] = [
            (CANDIDATES_SQL, &["since"]),
            (LAST_INSERTED_SQL, &[]),
            (INSERT_SQL, &all),
            (UPDATE_SQL, &update),
            (RECHECK_SQL, &["dtako_row_id", "datetime", "fuel_card_id"]),
        ];
        for (sql, want) in cases {
            assert_eq!(names(sql).unwrap(), want.to_vec(), "{sql}");
        }
    }

    #[test]
    fn candidates_since_two_months() {
        let sql = candidates_sql(dt(NOW)).unwrap();
        assert!(sql.contains("WHERE `datetime` >= '2024-10-31 10:00:00' AND"));
        // 月末は月の長さに合わせて丸める (chrono の Months)
        let sql = candidates_sql(dt("2024-04-30 00:00:00")).unwrap();
        assert!(sql.contains(">= '2024-02-29 00:00:00'"));
    }

    #[test]
    fn candidates_add_last_and_sort() {
        let c = rs(&[&[Some("8")], &[Some("3")], &[None], &[Some("x")]]);
        let last = rs(&[&[Some(KEY_TEXT), Some("57"), Some("5")]]);
        let m = Json::Object(candidates_json(&c, &last));
        assert_eq!(
            m,
            json!({
                "candidates": [3, 6, 8],
                "last_inserted": {"datetime": KEY_TEXT, "fuel_card_id": 57, "slip_num": 5},
            })
        );
        // 最新の slip_num + 1 が既に候補なら足さない (重複しない)
        let last = rs(&[&[Some(KEY_TEXT), Some("57"), Some("7")]]);
        let m = Json::Object(candidates_json(&c, &last));
        assert_eq!(m["candidates"], json!([3, 8]));
    }

    #[test]
    fn candidates_without_last() {
        let m = Json::Object(candidates_json(&rs(&[]), &rs(&[])));
        assert_eq!(m, json!({"candidates": [], "last_inserted": null}));
        // 最新の行の slip_num が NULL・読めない・溢れるなら足さない
        let max = u64::MAX.to_string();
        for slip in [None, Some("x"), Some(max.as_str())] {
            let last = rs(&[&[Some(KEY_TEXT), Some("9999"), slip]]);
            let m = Json::Object(candidates_json(&rs(&[]), &last));
            assert_eq!(m["candidates"], json!([]));
        }
    }

    #[test]
    fn create_fuel_row() {
        let body = r#"{"datetime":"2024-10-01 06:58:00","slip_num":12,"fuel_card_id":57,
            "quantity_decimal":123.45,"slip_num_parent":11}"#;
        let row = parse_create(body).unwrap();
        assert_eq!(
            row,
            ManualRow {
                key: fuel_key(),
                slip_num: 12,
                slip_num_parent: Some(11),
                quantity: Some(12345),
            }
        );
        assert!(!row.is_start());
        // 0.01 単位に丸める (1.005 * 100 = 100.49999… も 0.1 + 0.2 も四捨五入で揃う)
        let q = |v: &str| {
            let b = format!(
                r#"{{"datetime":"{KEY_TEXT}","slip_num":1,"fuel_card_id":57,"quantity_decimal":{v}}}"#
            );
            parse_create(&b).map(|r| r.quantity)
        };
        assert_eq!(q("12.34"), Ok(Some(1234)));
        assert_eq!(q("0.30000000000000004"), Ok(Some(30)));
        assert_eq!(q("0"), Ok(Some(0)));
        assert_eq!(q("7"), Ok(Some(700)));
        for bad in ["-1", "1000000", "\"1.5\"", "null"] {
            let want = Err(ApiError::BadRequest("quantity_decimal"));
            assert_eq!(q(bad), want, "{bad}");
        }
        // slip_num_parent は省略・null で NULL
        let b = format!(
            r#"{{"datetime":"{KEY_TEXT}","slip_num":1,"fuel_card_id":57,"quantity_decimal":1,"slip_num_parent":null}}"#
        );
        assert_eq!(parse_create(&b).unwrap().slip_num_parent, None);
        let b = format!(
            r#"{{"datetime":"{KEY_TEXT}","slip_num":1,"fuel_card_id":57,"quantity_decimal":1,"slip_num_parent":"1"}}"#
        );
        assert_eq!(
            parse_create(&b),
            Err(ApiError::BadRequest("slip_num_parent"))
        );
    }

    #[test]
    fn create_start_row() {
        for body in [
            r#"{"datetime":"2024-10-01 06:58:00","slip_num":12}"#,
            r#"{"datetime":"2024-10-01 06:58:00","slip_num":12,"fuel_card_id":null,"quantity_decimal":5,"slip_num_parent":3}"#,
        ] {
            let row = parse_create(body).unwrap();
            assert_eq!(
                row,
                ManualRow {
                    key: IntankKey {
                        datetime: dt(KEY_TEXT),
                        fuel_card_id: 9999,
                    },
                    slip_num: 12,
                    slip_num_parent: None,
                    quantity: None,
                }
            );
            assert!(row.is_start());
        }
    }

    #[test]
    fn create_rejects() {
        let cases = [
            (r#"{"slip_num":1}"#, "datetime"),
            (r#"{"datetime":"2024-10-01","slip_num":1}"#, "datetime"),
            (r#"{"datetime":1,"slip_num":1}"#, "datetime"),
            (r#"{"datetime":"2024-10-01 06:58:00"}"#, "slip_num"),
            (
                r#"{"datetime":"2024-10-01 06:58:00","slip_num":-1}"#,
                "slip_num",
            ),
            (
                r#"{"datetime":"2024-10-01 06:58:00","slip_num":"1"}"#,
                "slip_num",
            ),
            (
                r#"{"datetime":"2024-10-01 06:58:00","slip_num":1.5}"#,
                "slip_num",
            ),
            (
                r#"{"datetime":"2024-10-01 06:58:00","slip_num":1000000000000000000}"#,
                "slip_num",
            ),
            (
                r#"{"datetime":"2024-10-01 06:58:00","slip_num":1,"fuel_card_id":"57"}"#,
                "fuel_card_id",
            ),
            (
                r#"{"datetime":"2024-10-01 06:58:00","slip_num":1,"fuel_card_id":0}"#,
                "fuel_card_id",
            ),
            (
                r#"{"datetime":"2024-10-01 06:58:00","slip_num":1,"fuel_card_id":9999,"quantity_decimal":1}"#,
                "fuel_card_id",
            ),
            (
                r#"{"datetime":"2024-10-01 06:58:00","slip_num":1,"fuel_card_id":57}"#,
                "quantity_decimal",
            ),
            (r#"{"manual":true}"#, "unknown_field"),
            ("[]", "invalid_json"),
        ];
        for (body, code) in cases {
            let got = parse_create(body);
            assert_eq!(got, Err(ApiError::BadRequest(code)), "{body}");
        }
    }

    #[test]
    fn write_decision() {
        assert_eq!(decide_write(&rs(&[])), Ok(Write::Insert));
        let manual = rs(&[&row9(None, Some("1"))]);
        assert_eq!(decide_write(&manual), Ok(Write::Update));
        for m in [Some("0"), None] {
            let csv = rs(&[&row9(None, m)]);
            assert_eq!(decide_write(&csv), Err(ApiError::Conflict("not_manual")));
        }
        assert_eq!(write_status(Write::Insert), 201);
        assert_eq!(write_status(Write::Update), 200);
    }

    #[test]
    fn write_sql_fuel_and_start() {
        let fuel = ManualRow {
            key: fuel_key(),
            slip_num: 12,
            slip_num_parent: Some(11),
            quantity: Some(12345),
        };
        let id = Digits::new("24100106000000000018031");
        assert_eq!(
            write_sql(Write::Insert, &fuel, id).unwrap(),
            "INSERT INTO fuel_intank \
             (`datetime`, fuel_card_id, slip_num, slip_num_parent, quantity, `type`, detail, dtako_row_id, manual) \
             VALUES ('2024-10-01 06:58:00', 57, 12, 11, 12345, 'fuel', NULL, \
             '24100106000000000018031', 1)"
        );
        assert_eq!(
            write_sql(Write::Update, &fuel, None).unwrap(),
            "UPDATE fuel_intank SET slip_num = 12, \
             slip_num_parent = 11, quantity = 12345, `type` = 'fuel', detail = NULL, \
             dtako_row_id = NULL, manual = 1 \
             WHERE `datetime` = '2024-10-01 06:58:00' AND fuel_card_id = 57 AND manual <> 0"
        );
        let start = ManualRow {
            key: IntankKey {
                datetime: dt(KEY_TEXT),
                fuel_card_id: 9999,
            },
            slip_num: 12,
            slip_num_parent: None,
            quantity: None,
        };
        // start 行は照合の値が渡されても NULL
        let sql = write_sql(Write::Insert, &start, id).unwrap();
        assert!(sql.ends_with(
            "VALUES ('2024-10-01 06:58:00', 9999, 12, NULL, NULL, 'start', 'EP900-34', \
             NULL, 1)"
        ));
    }

    #[test]
    fn recheck_decision() {
        let key = fuel_key();
        assert_eq!(decide_recheck(key, &rs(&[])), Err(ApiError::NotFound));
        let none = rs(&[&row9(None, Some("1"))]);
        assert_eq!(decide_recheck(key, &none), Ok(Recheck::Match(key)));
        let id = "24100106000000000018031";
        let some = rs(&[&row9(Some(id), Some("1"))]);
        assert_eq!(
            decide_recheck(key, &some),
            Ok(Recheck::Keep(Digits::new(id)))
        );
        let start = IntankKey {
            fuel_card_id: 9999,
            ..key
        };
        assert_eq!(decide_recheck(start, &none), Ok(Recheck::Keep(None)));
    }

    #[test]
    fn recheck_and_label() {
        assert_eq!(
            recheck_sql(fuel_key(), Digits::new("123")).unwrap(),
            "UPDATE fuel_intank SET dtako_row_id = '123', manual = NULL \
             WHERE `datetime` = '2024-10-01 06:58:00' AND fuel_card_id = 57"
        );
        assert!(recheck_sql(fuel_key(), None)
            .unwrap()
            .contains("SET dtako_row_id = NULL, manual = NULL"));
        assert_eq!(key_label(fuel_key()), "2024-10-01 06:58:00;57");
    }
}
