//! fuel_intank の参照・編集(純粋ロジック): SQL 定数・引数の展開・結果セットの JSON 化・本文の検査。
//! 移植元は CakePHP の FuelIntankController の index・edit・delete と FuelIntankTable の
//! findWtSumFuelDtakoFuel・WtSumFuelDtakoFuel・beforeDelete、DtakoFuelTable の getSum・getFuelArray。
//!
//! 値はすべて `kintai_mysql::bind::expand` の名前付き引数で入れる(日時は `Value::DateTime`、文字列は
//! `Value::Digits` / `Value::Token` だけ)。SQL に値を文字列で連結しない。
//!
//! fuel_intank の行の JSON(`row_json`): `datetime`・`fuel_card_id`・`slip_num`・`slip_num_parent`・
//! `quantity`(×100 の整数のまま)・`qt`(= quantity / 100)・`type`・`detail`・`dtako_row_id`・`manual`・
//! `id_st`(`{datetime};{fuel_card_id}`。編集・削除の path に使う)。

use std::collections::BTreeMap;

use chrono::{NaiveDateTime, NaiveTime, TimeDelta};
use kintai_mysql::bind::{Digits, Token, Value};
use kintai_mysql::response::ResultSet;
use serde_json::{json, Map, Value as Json};

use crate::api::{bind, flag, num, parse_datetime, parse_object, require_row, text, ApiError};
use crate::route::IntankKey;

/// start 行の fuel_card_id(CSV の Z 行。CakePHP の beforeMarshal が 9999 を入れる)。
pub const START_CARD_ID: u64 = 9999;

/// srch_data_list: start 行を新しい順に。列: slip_num, datetime, fuel_card_id
pub const STARTS_SQL: &str = "SELECT slip_num, datetime, fuel_card_id FROM fuel_intank \
     WHERE type = 'start' ORDER BY datetime DESC";

/// 一覧の既定: 最新 100 行。列は `row_json` の 9 列
pub const LATEST_SQL: &str = "SELECT datetime, fuel_card_id, slip_num, slip_num_parent, quantity, \
     type, detail, dtako_row_id, manual FROM fuel_intank ORDER BY datetime DESC LIMIT 100";

/// `?start=` の start 行。列: slip_num, datetime, fuel_card_id
pub const START_ROW_SQL: &str = "SELECT slip_num, datetime, fuel_card_id FROM fuel_intank \
     WHERE datetime = :datetime AND fuel_card_id = 9999";

/// start 行の伝票番号を親に持つ給油行(findWtSumFuelDtakoFuel)。列: `row_json` の 9 列、
/// fuel_cards の id・car_id、その car の id・name、dtako_rows の id・車輌CC、その car の name、出庫日時・帰庫日時、
/// 対象乗務員の id・name(計 20 列)。結合はどれも LEFT(CakePHP の belongsTo の既定)
pub const BY_START_SQL: &str =
    "SELECT fi.datetime, fi.fuel_card_id, fi.slip_num, fi.slip_num_parent, \
     fi.quantity, fi.type, fi.detail, fi.dtako_row_id, fi.manual, fc.id, fc.car_id, c.id, c.name, \
     dr.id, dr.`車輌CC`, drc.name, dr.`出庫日時`, dr.`帰庫日時`, d.id, d.name \
     FROM fuel_intank fi \
     LEFT JOIN fuel_cards fc ON fc.id = fi.fuel_card_id \
     LEFT JOIN cars c ON c.id = fc.car_id \
     LEFT JOIN dtako_rows dr ON dr.id = fi.dtako_row_id \
     LEFT JOIN cars drc ON drc.id = dr.`車輌CC` \
     LEFT JOIN drivers d ON d.id = dr.`対象乗務員CD` \
     WHERE fi.slip_num_parent = :slip_num AND fi.type = 'fuel' AND fi.fuel_card_id <> 9999 \
     ORDER BY fi.datetime ASC";

/// DtakoFuelTable::getSum: 給油の日の 0 時〜翌 0 時、かつ start 行の時刻〜最後の行 +1 時間の、その運行の
/// 自社・軽油の給油量の合計。列: sum
pub const SUM_SQL: &str = "SELECT SUM(quantity) FROM dtako_fuel \
     WHERE fuel_kubun = 1 AND fuel_shubetsu = 1 AND fuel_bunrui = 1 \
     AND fuel_datetime >= :day_start AND fuel_datetime < :day_end \
     AND fuel_datetime >= :start_time AND fuel_datetime <= :end_time \
     AND dtako_row_id IN (:dtako_row_id) \
     GROUP BY date_format(fuel_datetime, '%y%m%d') LIMIT 1";

/// DtakoFuelTable::getFuelArray: その車輌の給油の日の自社・軽油の給油。列: dtako_row_id, fuel_datetime, quantity
pub const FUEL_ARRAY_SQL: &str = "SELECT dtako_row_id, fuel_datetime, quantity FROM dtako_fuel \
     WHERE sharyo_cc = :car_id AND fuel_kubun = 1 AND fuel_shubetsu = 1 AND fuel_bunrui = 1 \
     AND fuel_datetime >= :day_start AND fuel_datetime < :day_end \
     ORDER BY fuel_datetime ASC";

/// 主キーで 1 行。列は `row_json` の 9 列
pub const ROW_SQL: &str = "SELECT datetime, fuel_card_id, slip_num, slip_num_parent, quantity, \
     type, detail, dtako_row_id, manual FROM fuel_intank \
     WHERE datetime = :datetime AND fuel_card_id = :fuel_card_id";

/// その card の給油行(FuelCards の view の contain FuelIntank)。列は `row_json` の 9 列
pub const BY_CARD_SQL: &str =
    "SELECT datetime, fuel_card_id, slip_num, slip_num_parent, quantity, \
     type, detail, dtako_row_id, manual FROM fuel_intank \
     WHERE fuel_card_id = :fuel_card_id ORDER BY datetime ASC";

/// dtako_row_id の付け替え(NULL も入れられる)
pub const PATCH_SQL: &str = "UPDATE fuel_intank SET dtako_row_id = :dtako_row_id \
     WHERE datetime = :datetime AND fuel_card_id = :fuel_card_id";

/// 削除(beforeDelete: manual が真の行だけ)
pub const DELETE_SQL: &str = "DELETE FROM fuel_intank \
     WHERE datetime = :datetime AND fuel_card_id = :fuel_card_id AND manual <> 0";

/// dtako_row_id の最大の長さ(validationDefault の maxLength)。
const DTAKO_ROW_ID_MAX: usize = 24;

fn key_params(key: IntankKey) -> [(&'static str, Value); 2] {
    [
        ("datetime", Value::DateTime(key.datetime)),
        ("fuel_card_id", Value::UInt(key.fuel_card_id)),
    ]
}

/// 9 列(`row_json` の列順)を `offset` から読んで JSON にする。
fn row_json_at(rs: &ResultSet, i: usize, offset: usize) -> Map<String, Json> {
    let cell = |c: usize| rs.text(i, offset + c);
    let qt = cell(4)
        .and_then(|q| q.parse::<i64>().ok())
        .map_or(Json::Null, |q| json!(q as f64 / 100.0));
    let id_st = match (cell(0), cell(1)) {
        (Some(d), Some(c)) => Json::String(format!("{d};{c}")),
        _ => Json::Null,
    };
    let mut m = Map::new();
    m.insert("datetime".into(), text(cell(0)));
    m.insert("fuel_card_id".into(), num(cell(1)));
    m.insert("slip_num".into(), num(cell(2)));
    m.insert("slip_num_parent".into(), num(cell(3)));
    m.insert("quantity".into(), num(cell(4)));
    m.insert("qt".into(), qt);
    m.insert("type".into(), text(cell(5)));
    m.insert("detail".into(), text(cell(6)));
    m.insert("dtako_row_id".into(), text(cell(7)));
    m.insert("manual".into(), flag(cell(8)));
    m.insert("id_st".into(), id_st);
    m
}

/// `row_json` の 9 列だけの結果セットを行の配列にする。
pub fn rows_json(rs: &ResultSet) -> Json {
    Json::Array(
        (0..rs.rows.len())
            .map(|i| Json::Object(row_json_at(rs, i, 0)))
            .collect(),
    )
}

/// `GET /api/fuel/starts` の本文。
pub fn starts_json(rs: &ResultSet) -> Map<String, Json> {
    let rows = (0..rs.rows.len())
        .map(|i| {
            json!({
                "slip_num": num(rs.text(i, 0)),
                "datetime": text(rs.text(i, 1)),
                "fuel_card_id": num(rs.text(i, 2)),
            })
        })
        .collect();
    let mut m = Map::new();
    m.insert("rows".into(), Json::Array(rows));
    m
}

/// `GET /api/fuel/intank`(query 無し)・card の給油行の本文。
pub fn list_json(rs: &ResultSet) -> Map<String, Json> {
    let mut m = Map::new();
    m.insert("rows".into(), rows_json(rs));
    m
}

pub fn start_row_sql(datetime: NaiveDateTime) -> Result<String, ApiError> {
    bind(START_ROW_SQL, &[("datetime", Value::DateTime(datetime))])
}

/// `?start=` の start 行。
#[derive(Debug, Clone, PartialEq)]
pub struct Start {
    slip_num: Digits,
    datetime: NaiveDateTime,
    body: Json,
}

/// start 行を読む。無ければ 404。
pub fn parse_start(rs: &ResultSet) -> Result<Start, ApiError> {
    require_row(rs)?;
    let slip_num = rs
        .text(0, 0)
        .and_then(Digits::new)
        .ok_or(ApiError::Internal("start_slip_num".into()))?;
    let datetime = rs
        .text(0, 1)
        .and_then(parse_datetime)
        .ok_or(ApiError::Internal("start_datetime".into()))?;
    let body = json!({
        "slip_num": num(rs.text(0, 0)),
        "datetime": text(rs.text(0, 1)),
        "fuel_card_id": num(rs.text(0, 2)),
    });
    Ok(Start {
        slip_num,
        datetime,
        body,
    })
}

pub fn by_start_sql(start: &Start) -> Result<String, ApiError> {
    bind(BY_START_SQL, &[("slip_num", Value::Digits(start.slip_num))])
}

/// 給油行 1 行(JSON の本体と、後の集計に要る値)。
#[derive(Debug, Clone, PartialEq)]
pub struct FuelRow {
    body: Map<String, Json>,
    datetime: NaiveDateTime,
    has_card: bool,
    car_id: Option<String>,
    dtako_row_id: Option<String>,
}

fn object_or_null(present: bool, value: Json) -> Json {
    if present {
        value
    } else {
        Json::Null
    }
}

/// `BY_START_SQL` の 20 列を読む。
pub fn parse_fuel_rows(rs: &ResultSet) -> Result<Vec<FuelRow>, ApiError> {
    (0..rs.rows.len())
        .map(|i| {
            let cell = |c: usize| rs.text(i, c);
            let datetime = cell(0)
                .and_then(parse_datetime)
                .ok_or(ApiError::Internal("fuel_datetime".into()))?;
            let mut body = row_json_at(rs, i, 0);
            let has_card = cell(9).is_some();
            let car = json!({"id": text(cell(11)), "name": text(cell(12))});
            body.insert("car".into(), object_or_null(cell(11).is_some(), car));
            let dtako_row = json!({
                "id": text(cell(13)),
                "car_cc": text(cell(14)),
                "car_name": text(cell(15)),
                "departed_at": text(cell(16)),
                "returned_at": text(cell(17)),
                "driver_id": num(cell(18)),
                "driver_name": text(cell(19)),
            });
            body.insert(
                "dtako_row".into(),
                object_or_null(cell(13).is_some(), dtako_row),
            );
            Ok(FuelRow {
                body,
                datetime,
                has_card,
                car_id: cell(10).map(str::to_string),
                dtako_row_id: cell(7).map(str::to_string),
            })
        })
        .collect()
}

/// 集計の終わり(WtSumFuelDtakoFuel: 最後の行の時刻 +1 時間)。行が無ければ `None`。
pub fn end_time(rows: &[FuelRow]) -> Option<NaiveDateTime> {
    rows.last().map(|r| r.datetime + TimeDelta::hours(1))
}

/// 給油の日の 0 時と翌日の 0 時。
fn day_range(dt: NaiveDateTime) -> (Value, Value) {
    let start = dt.date().and_time(NaiveTime::MIN);
    (
        Value::DateTime(start),
        Value::DateTime(start + TimeDelta::days(1)),
    )
}

/// 1 行ぶんの追加のクエリ。fuel_card が無い行は両方 `None`(CakePHP も集計しない)。
/// dtako_row_id が無い(数字でない)行の合計は `None`(`IN (NULL)` は 1 行も当たらないので結果は同じ null)。
/// card の car_id が英数字・`-`・`_` でなければ給油の一覧も `None`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowUp {
    pub sum: Option<String>,
    pub array: Option<String>,
}

pub fn follow_ups(start: &Start, rows: &[FuelRow]) -> Result<Vec<FollowUp>, ApiError> {
    let Some(end) = end_time(rows) else {
        return Ok(Vec::new());
    };
    rows.iter()
        .map(|row| {
            if !row.has_card {
                return Ok(FollowUp {
                    sum: None,
                    array: None,
                });
            }
            let (day_start, day_end) = day_range(row.datetime);
            let sum_params = row.dtako_row_id.as_deref().and_then(Digits::new).map(|id| {
                [
                    ("day_start", day_start),
                    ("day_end", day_end),
                    ("start_time", Value::DateTime(start.datetime)),
                    ("end_time", Value::DateTime(end)),
                    ("dtako_row_id", Value::Digits(id)),
                ]
            });
            let array_params = row.car_id.as_deref().and_then(Token::new).map(|car| {
                [
                    ("car_id", Value::Token(car)),
                    ("day_start", day_start),
                    ("day_end", day_end),
                ]
            });
            Ok(FollowUp {
                sum: sum_params.map(|p| bind(SUM_SQL, &p)).transpose()?,
                array: array_params.map(|p| bind(FUEL_ARRAY_SQL, &p)).transpose()?,
            })
        })
        .collect()
}

/// `SUM_SQL` の結果(行が無ければ null)。
pub fn sum_json(rs: &ResultSet) -> Json {
    num(rs.text(0, 0))
}

/// 小数 2 桁以上・3 桁まで・3 桁ごとに `,`(CakePHP の `Number::format($q, ["places" => 2])`)。
fn format_quantity(q: &str) -> String {
    let Ok(x) = q.parse::<f64>() else {
        return q.to_string();
    };
    let mut s = format!("{x:.3}");
    if s.ends_with('0') {
        s.pop();
    }
    let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
    let (sign, digits) = int.strip_prefix('-').map_or(("", int), |d| ("-", d));
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{sign}{grouped}.{frac}")
}

/// getFuelArray の 1 件の表示 (`yyyyMMddHHmm_<給油量>`)。
fn fuel_state(datetime: &str, quantity: &str) -> String {
    let stamp: String = datetime
        .chars()
        .filter(char::is_ascii_digit)
        .take(12)
        .collect();
    format!("{stamp}_{}", format_quantity(quantity))
}

/// `FUEL_ARRAY_SQL` の結果を dtako_row_id ごとの表示の配列にする(CakePHP の
/// `combine("fuel_state", "fuel_state", "dtako_row_id")`。dtako_row_id が NULL の給油は `""` に、同じ表示は 1 つに)。
pub fn array_json(rs: &ResultSet) -> Json {
    let mut groups: BTreeMap<String, Vec<Json>> = BTreeMap::new();
    for i in 0..rs.rows.len() {
        let state = fuel_state(rs.text(i, 1).unwrap_or(""), rs.text(i, 2).unwrap_or(""));
        let list = groups
            .entry(rs.text(i, 0).unwrap_or("").to_string())
            .or_default();
        if !list.iter().any(|s| s == state.as_str()) {
            list.push(Json::String(state));
        }
    }
    Json::Object(
        groups
            .into_iter()
            .map(|(k, v)| (k, Json::Array(v)))
            .collect(),
    )
}

/// `GET /api/fuel/intank?start=` の本文。`results` は `follow_ups` と同じ順の (合計, 給油の一覧)。
pub fn by_start_json(
    start: Start,
    rows: Vec<FuelRow>,
    results: Vec<(Option<Json>, Option<Json>)>,
) -> Map<String, Json> {
    let end = end_time(&rows).map_or(Json::Null, |e| Json::String(crate::api::format_datetime(e)));
    let out = rows
        .into_iter()
        .zip(results)
        .map(|(row, (sum, array))| {
            let mut body = row.body;
            body.insert("dtako_fuel_sum".into(), sum.unwrap_or(Json::Null));
            body.insert("dtako_fuel_array".into(), array.unwrap_or(Json::Null));
            Json::Object(body)
        })
        .collect();
    let mut m = Map::new();
    m.insert("start".into(), start.body);
    m.insert("end_time".into(), end);
    m.insert("rows".into(), Json::Array(out));
    m
}

pub fn row_sql(key: IntankKey) -> Result<String, ApiError> {
    bind(ROW_SQL, &key_params(key))
}

pub fn by_card_sql(card_id: u64) -> Result<String, ApiError> {
    bind(BY_CARD_SQL, &[("fuel_card_id", Value::UInt(card_id))])
}

/// `ROW_SQL` の 1 行の本文(無ければ 404)。
pub fn item_json(rs: &ResultSet) -> Result<Map<String, Json>, ApiError> {
    require_row(rs)?;
    let mut m = Map::new();
    m.insert("row".into(), Json::Object(row_json_at(rs, 0, 0)));
    Ok(m)
}

/// PATCH の本文: `{"dtako_row_id": "<数字 24 文字以下>" | "" | null}`(空と null は NULL にする)。
/// 当面ほかのキーは受けない。
pub fn parse_patch(body: &str) -> Result<Option<Digits>, ApiError> {
    let obj = parse_object(body, &["dtako_row_id"])?;
    match obj.get("dtako_row_id") {
        None => Err(ApiError::BadRequest("missing_dtako_row_id")),
        Some(Json::Null) => Ok(None),
        Some(Json::String(s)) if s.is_empty() => Ok(None),
        Some(Json::String(s)) if s.len() <= DTAKO_ROW_ID_MAX => Digits::new(s)
            .map(Some)
            .ok_or(ApiError::BadRequest("dtako_row_id")),
        Some(_) => Err(ApiError::BadRequest("dtako_row_id")),
    }
}

pub fn patch_sql(key: IntankKey, dtako_row_id: Option<Digits>) -> Result<String, ApiError> {
    let id = dtako_row_id.map_or(Value::Null, Value::Digits);
    let [a, b] = key_params(key);
    bind(PATCH_SQL, &[("dtako_row_id", id), a, b])
}

/// 削除してよいか(`ROW_SQL` の結果)。行が無ければ 404、manual が真でなければ 409。
pub fn check_deletable(rs: &ResultSet) -> Result<(), ApiError> {
    require_row(rs)?;
    match rs.text(0, 8) {
        Some(m) if m != "0" => Ok(()),
        _ => Err(ApiError::Conflict("not_manual")),
    }
}

pub fn delete_sql(key: IntankKey) -> Result<String, ApiError> {
    bind(DELETE_SQL, &key_params(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests::{dt, rs};
    use kintai_mysql::bind::names;

    const KEY_TEXT: &str = "2024-10-01 06:58:00";

    fn key() -> IntankKey {
        IntankKey {
            datetime: dt(KEY_TEXT),
            fuel_card_id: 1234,
        }
    }

    fn row9(manual: Option<&str>) -> Vec<Option<&str>> {
        vec![
            Some(KEY_TEXT),
            Some("1234"),
            Some("5"),
            Some("4"),
            Some("12345"),
            Some("fuel"),
            None,
            Some("24100106000000000018031"),
            manual,
        ]
    }

    #[test]
    fn sql_constants_have_expected_names() {
        let cases: [(&str, &[&str]); 11] = [
            (STARTS_SQL, &[]),
            (LATEST_SQL, &[]),
            (START_ROW_SQL, &["datetime"]),
            (BY_START_SQL, &["slip_num"]),
            (
                SUM_SQL,
                &[
                    "day_start",
                    "day_end",
                    "start_time",
                    "end_time",
                    "dtako_row_id",
                ],
            ),
            (FUEL_ARRAY_SQL, &["car_id", "day_start", "day_end"]),
            (ROW_SQL, &["datetime", "fuel_card_id"]),
            (BY_CARD_SQL, &["fuel_card_id"]),
            (PATCH_SQL, &["dtako_row_id", "datetime", "fuel_card_id"]),
            (DELETE_SQL, &["datetime", "fuel_card_id"]),
            (crate::cards::LIST_SQL, &[]),
        ];
        for (sql, want) in cases {
            assert_eq!(names(sql).unwrap(), want.to_vec(), "{sql}");
        }
    }

    #[test]
    fn starts_and_list() {
        let s = starts_json(&rs(&[&[Some("12"), Some(KEY_TEXT), Some("9999")]]));
        assert_eq!(
            Json::Object(s),
            json!({"rows": [{"slip_num": 12, "datetime": KEY_TEXT, "fuel_card_id": 9999}]})
        );
        let l = list_json(&rs(&[&row9(Some("1"))]));
        assert_eq!(
            Json::Object(l),
            json!({"rows": [{
                "datetime": KEY_TEXT, "fuel_card_id": 1234, "slip_num": 5, "slip_num_parent": 4,
                "quantity": 12345, "qt": 123.45, "type": "fuel", "detail": null,
                "dtako_row_id": "24100106000000000018031", "manual": true,
                "id_st": "2024-10-01 06:58:00;1234",
            }]})
        );
    }

    #[test]
    fn row_without_quantity_or_key() {
        let r = rows_json(&rs(&[&[
            None,
            None,
            Some("1"),
            None,
            None,
            Some("start"),
            Some("EP900-34"),
            None,
            None,
        ]]));
        assert_eq!(r[0]["qt"], Json::Null);
        assert_eq!(r[0]["id_st"], Json::Null);
        assert_eq!(r[0]["manual"], Json::Null);
    }

    #[test]
    fn start_row() {
        assert_eq!(
            start_row_sql(dt(KEY_TEXT)).unwrap(),
            "SELECT slip_num, datetime, fuel_card_id FROM fuel_intank \
             WHERE datetime = '2024-10-01 06:58:00' AND fuel_card_id = 9999"
        );
        assert_eq!(parse_start(&rs(&[])), Err(ApiError::NotFound));
        assert_eq!(
            parse_start(&rs(&[&[None, Some(KEY_TEXT), Some("9999")]])),
            Err(ApiError::Internal("start_slip_num".into()))
        );
        assert_eq!(
            parse_start(&rs(&[&[Some("12"), Some("bad"), Some("9999")]])),
            Err(ApiError::Internal("start_datetime".into()))
        );
        let start = parse_start(&rs(&[&[Some("12"), Some(KEY_TEXT), Some("9999")]])).unwrap();
        let sql = by_start_sql(&start).unwrap();
        assert!(sql.contains("WHERE fi.slip_num_parent = '12' AND fi.type = 'fuel'"));
    }

    fn joined<'a>(
        datetime: &'a str,
        card: bool,
        car_id: Option<&'a str>,
        dtako: Option<&'a str>,
    ) -> Vec<Option<&'a str>> {
        let mut r = row9(None);
        r[0] = Some(datetime);
        r[7] = dtako;
        let on = |v: &'a str| if card { Some(v) } else { None };
        r.extend([on("1234"), car_id, car_id, car_id.map(|_| "長崎100か3072")]);
        match dtako {
            Some(id) => r.extend([
                Some(id),
                Some("307200"),
                Some("長崎100か3072"),
                Some("2024-10-01 05:00:00"),
                Some("2024-10-01 18:00:00"),
                Some("1120"),
                Some("長嶺　孝輔"),
            ]),
            None => r.extend([None; 7]),
        }
        r
    }

    #[test]
    fn by_start_flow() {
        let start = parse_start(&rs(&[&[Some("12"), Some(KEY_TEXT), Some("9999")]])).unwrap();
        let dtako = "24100105000000000307201";
        let rows_rs = rs(&[
            &joined("2024-10-01 07:00:00", true, Some("307200"), Some(dtako)),
            &joined("2024-10-01 08:00:00", true, Some("307200"), None),
            &joined("2024-10-01 09:00:00", true, Some("bad id"), Some("x")),
            &joined("2024-10-02 01:30:00", false, None, None),
        ]);
        let rows = parse_fuel_rows(&rows_rs).unwrap();
        assert_eq!(end_time(&rows), Some(dt("2024-10-02 02:30:00")));
        let ups = follow_ups(&start, &rows).unwrap();
        assert_eq!(
            ups[0].sum.as_deref().unwrap(),
            "SELECT SUM(quantity) FROM dtako_fuel \
             WHERE fuel_kubun = 1 AND fuel_shubetsu = 1 AND fuel_bunrui = 1 \
             AND fuel_datetime >= '2024-10-01 00:00:00' AND fuel_datetime < '2024-10-02 00:00:00' \
             AND fuel_datetime >= '2024-10-01 06:58:00' AND fuel_datetime <= '2024-10-02 02:30:00' \
             AND dtako_row_id IN ('24100105000000000307201') \
             GROUP BY date_format(fuel_datetime, '%y%m%d') LIMIT 1"
        );
        assert!(ups[0]
            .array
            .as_deref()
            .unwrap()
            .contains("WHERE sharyo_cc = '307200' AND"));
        assert_eq!(ups[1].sum, None);
        assert!(ups[1].array.is_some());
        assert_eq!(
            ups[2],
            FollowUp {
                sum: None,
                array: None
            }
        );
        assert_eq!(
            ups[3],
            FollowUp {
                sum: None,
                array: None
            }
        );

        let results = vec![
            (Some(json!(120.5)), Some(json!({"x": []}))),
            (None, Some(json!({}))),
            (None, None),
            (None, None),
        ];
        let out = Json::Object(by_start_json(start, rows, results));
        assert_eq!(
            out["start"],
            json!({"slip_num": 12, "datetime": KEY_TEXT, "fuel_card_id": 9999})
        );
        assert_eq!(out["end_time"], json!("2024-10-02 02:30:00"));
        let r0 = &out["rows"][0];
        assert_eq!(r0["car"], json!({"id": "307200", "name": "長崎100か3072"}));
        assert_eq!(
            r0["dtako_row"],
            json!({
                "id": dtako, "car_cc": "307200", "car_name": "長崎100か3072",
                "departed_at": "2024-10-01 05:00:00", "returned_at": "2024-10-01 18:00:00",
                "driver_id": 1120, "driver_name": "長嶺　孝輔",
            })
        );
        assert_eq!(r0["dtako_fuel_sum"], json!(120.5));
        assert_eq!(r0["dtako_fuel_array"], json!({"x": []}));
        let r3 = &out["rows"][3];
        assert_eq!(
            (&r3["car"], &r3["dtako_row"], &r3["dtako_fuel_sum"]),
            (&Json::Null, &Json::Null, &Json::Null)
        );
    }

    #[test]
    fn by_start_empty_and_bad_rows() {
        let start = parse_start(&rs(&[&[Some("12"), Some(KEY_TEXT), Some("9999")]])).unwrap();
        assert_eq!(follow_ups(&start, &[]).unwrap(), vec![]);
        let out = Json::Object(by_start_json(start, vec![], vec![]));
        assert_eq!((&out["end_time"], &out["rows"]), (&Json::Null, &json!([])));
        let mut bad = joined(KEY_TEXT, true, None, None);
        bad[0] = Some("bad");
        assert_eq!(
            parse_fuel_rows(&rs(&[&bad])),
            Err(ApiError::Internal("fuel_datetime".into()))
        );
    }

    #[test]
    fn sums_and_arrays() {
        assert_eq!(sum_json(&rs(&[&[Some("120.50")]])), json!(120.5));
        assert_eq!(sum_json(&rs(&[])), Json::Null);
        let a = array_json(&rs(&[
            &[Some("A"), Some("2024-10-01 07:05:09"), Some("1234.5")],
            &[Some("A"), Some("2024-10-01 07:05:09"), Some("1234.5")],
            &[Some("A"), Some("2024-10-01 08:00:00"), Some("20")],
            &[None, Some("2024-10-01 09:00:00"), Some("-1.2346")],
            &[Some("B"), None, Some("x")],
        ]));
        assert_eq!(
            a,
            json!({
                "": ["202410010900_-1.235"],
                "A": ["202410010705_1,234.50", "202410010800_20.00"],
                "B": ["_x"],
            })
        );
        assert_eq!(format_quantity("1234567.891"), "1,234,567.891");
        assert_eq!(format_quantity("0"), "0.00");
    }

    #[test]
    fn item_and_card_rows() {
        assert_eq!(item_json(&rs(&[])), Err(ApiError::NotFound));
        let m = Json::Object(item_json(&rs(&[&row9(Some("0"))])).unwrap());
        assert_eq!(m["row"]["manual"], json!(false));
        assert_eq!(
            row_sql(key()).unwrap(),
            "SELECT datetime, fuel_card_id, slip_num, slip_num_parent, quantity, \
             type, detail, dtako_row_id, manual FROM fuel_intank \
             WHERE datetime = '2024-10-01 06:58:00' AND fuel_card_id = 1234"
        );
        assert!(by_card_sql(7)
            .unwrap()
            .contains("WHERE fuel_card_id = 7 ORDER BY datetime ASC"));
    }

    #[test]
    fn patch_body() {
        let d = parse_patch(r#"{"dtako_row_id":"24100105000000000307201"}"#).unwrap();
        assert_eq!(d.unwrap().as_str(), "24100105000000000307201");
        assert_eq!(parse_patch(r#"{"dtako_row_id":null}"#), Ok(None));
        assert_eq!(parse_patch(r#"{"dtako_row_id":""}"#), Ok(None));
        for bad in [
            r#"{"dtako_row_id":"12a"}"#,
            r#"{"dtako_row_id":"1234567890123456789012345"}"#,
            r#"{"dtako_row_id":12}"#,
        ] {
            // 失敗メッセージの引数を独立した行にしない (1.92 の llvm-cov が未通過の行に数える)
            let got = parse_patch(bad);
            assert_eq!(got, Err(ApiError::BadRequest("dtako_row_id")), "{bad}");
        }
        assert_eq!(
            parse_patch("{}"),
            Err(ApiError::BadRequest("missing_dtako_row_id"))
        );
        assert_eq!(
            parse_patch(r#"{"manual":true}"#),
            Err(ApiError::BadRequest("unknown_field"))
        );
    }

    #[test]
    fn patch_and_delete_sql() {
        assert_eq!(
            patch_sql(key(), Digits::new("123")).unwrap(),
            "UPDATE fuel_intank SET dtako_row_id = '123' \
             WHERE datetime = '2024-10-01 06:58:00' AND fuel_card_id = 1234"
        );
        assert!(patch_sql(key(), None)
            .unwrap()
            .contains("SET dtako_row_id = NULL WHERE"));
        assert_eq!(
            delete_sql(key()).unwrap(),
            "DELETE FROM fuel_intank \
             WHERE datetime = '2024-10-01 06:58:00' AND fuel_card_id = 1234 AND manual <> 0"
        );
    }

    #[test]
    fn deletable_only_when_manual() {
        assert_eq!(check_deletable(&rs(&[])), Err(ApiError::NotFound));
        assert_eq!(check_deletable(&rs(&[&row9(Some("1"))])), Ok(()));
        for manual in [Some("0"), None] {
            assert_eq!(
                check_deletable(&rs(&[&row9(manual)])),
                Err(ApiError::Conflict("not_manual"))
            );
        }
    }
}
