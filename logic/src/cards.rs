//! fuel_cards の参照・編集と select 用の一覧(純粋ロジック): SQL 定数・引数の展開・結果セットの JSON 化・本文の検査。
//! 移植元は CakePHP の FuelCardsController の全部と FuelCardsTable(validationDefault・buildRules)、
//! FuelIntankController::index の Cars_list・fuelCardsAdd。
//!
//! card の JSON(`card_json`): `id`・`car_id`・`end_datetime`・`car`(`{id, name}`。cars に無ければ null)。
//! car_id は `Value::Token` で受ける(英数字だけ・6 文字以下。それ以外は 400)。cars に無い car_id も 400
//! (buildRules の existsIn と同じ)。

use chrono::NaiveDateTime;
use kintai_mysql::bind::{Token, Value};
use kintai_mysql::response::ResultSet;
use serde_json::{json, Map, Value as Json};

use crate::api::{bind, num, parse_datetime, parse_object, require_row, text, ApiError};

/// card の一覧(cars を結合、id 昇順・car_id 昇順)。列: fc.id, fc.car_id, fc.end_datetime, c.id, c.name
pub const LIST_SQL: &str = "SELECT fc.id, fc.car_id, fc.end_datetime, c.id, c.name \
     FROM fuel_cards fc LEFT JOIN cars c ON c.id = fc.car_id \
     ORDER BY fc.id ASC, fc.car_id ASC";

/// card 1 枚。列は `LIST_SQL` と同じ
pub const GET_SQL: &str = "SELECT fc.id, fc.car_id, fc.end_datetime, c.id, c.name \
     FROM fuel_cards fc LEFT JOIN cars c ON c.id = fc.car_id \
     WHERE fc.id = :id ORDER BY fc.car_id ASC LIMIT 1";

/// car_id が cars に在るか(existsIn)。列: id
pub const CAR_EXISTS_SQL: &str = "SELECT id FROM cars WHERE id = :car_id";

pub const INSERT_SQL: &str = "INSERT INTO fuel_cards (id, car_id) VALUES (:id, :car_id)";

/// 渡された項目だけを変える(`:set_car` / `:set_end` が 1 の項目だけ。end_datetime は NULL にもできる)
pub const UPDATE_SQL: &str = "UPDATE fuel_cards \
     SET car_id = IF(:set_car = 1, :car_id, car_id), \
     end_datetime = IF(:set_end = 1, :end_datetime, end_datetime) \
     WHERE id = :id";

pub const DELETE_SQL: &str = "DELETE FROM fuel_cards WHERE id = :id";

/// select 用の車輌(Cars の find("list")。id 昇順)。列: id, name
pub const CARS_SQL: &str = "SELECT id, name FROM cars ORDER BY id ASC";

/// fuel_intank にあって fuel_cards に無い fuel_card_id(9999 を除く。fuelCardsAdd)。列: fuel_card_id
pub const UNREGISTERED_SQL: &str = "SELECT DISTINCT fi.fuel_card_id FROM fuel_intank fi \
     LEFT JOIN fuel_cards fc ON fc.id = fi.fuel_card_id \
     WHERE fc.id IS NULL AND fi.fuel_card_id <> 9999 \
     ORDER BY fi.fuel_card_id ASC";

/// car_id の最大の長さ(validationDefault の maxLength)。
const CAR_ID_MAX: usize = 6;

fn card_json(rs: &ResultSet, i: usize) -> Json {
    let cell = |c: usize| rs.text(i, c);
    let car = match cell(3) {
        Some(_) => json!({"id": text(cell(3)), "name": text(cell(4))}),
        None => Json::Null,
    };
    json!({
        "id": num(cell(0)),
        "car_id": text(cell(1)),
        "end_datetime": text(cell(2)),
        "car": car,
    })
}

/// `GET /api/fuel/cards` の本文。
pub fn list_json(rs: &ResultSet) -> Map<String, Json> {
    let mut m = Map::new();
    let cards = (0..rs.rows.len()).map(|i| card_json(rs, i)).collect();
    m.insert("cards".into(), Json::Array(cards));
    m
}

pub fn get_sql(id: u64) -> Result<String, ApiError> {
    bind(GET_SQL, &[("id", Value::UInt(id))])
}

/// `GET /api/fuel/cards/{id}` の本文(card が無ければ 404)。`intank` は給油行の配列。
pub fn item_json(rs: &ResultSet, intank: Option<Json>) -> Result<Map<String, Json>, ApiError> {
    require_row(rs)?;
    let mut m = Map::new();
    m.insert("card".into(), card_json(rs, 0));
    if let Some(rows) = intank {
        m.insert("intank".into(), rows);
    }
    Ok(m)
}

/// car_id(英数字だけ・1〜6 文字の文字列)。
fn parse_car_id(v: &Json) -> Result<Token, ApiError> {
    match v {
        Json::String(s)
            if s.len() <= CAR_ID_MAX && s.bytes().all(|b| b.is_ascii_alphanumeric()) =>
        {
            Token::new(s).ok_or(ApiError::BadRequest("car_id"))
        }
        _ => Err(ApiError::BadRequest("car_id")),
    }
}

/// `POST /api/fuel/cards` の本文: `{"id": <正の整数>, "car_id": "<英数字 6 文字以下>"}`。
pub fn parse_create(body: &str) -> Result<(u64, Token), ApiError> {
    let obj = parse_object(body, &["id", "car_id"])?;
    let id = obj
        .get("id")
        .and_then(Json::as_u64)
        .filter(|id| *id > 0)
        .ok_or(ApiError::BadRequest("id"))?;
    let car = parse_car_id(obj.get("car_id").unwrap_or(&Json::Null))?;
    Ok((id, car))
}

/// PATCH の中身。`end_datetime` の `Some(None)` は NULL にする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardPatch {
    pub car_id: Option<Token>,
    pub end_datetime: Option<Option<NaiveDateTime>>,
}

/// `PATCH /api/fuel/cards/{id}` の本文: `{"car_id"?: "…", "end_datetime"?: "YYYY-MM-DD HH:MM:SS" | "" | null}`。
/// どちらも無ければ 400。
pub fn parse_patch(body: &str) -> Result<CardPatch, ApiError> {
    let obj = parse_object(body, &["car_id", "end_datetime"])?;
    let car_id = obj.get("car_id").map(parse_car_id).transpose()?;
    let end_datetime = match obj.get("end_datetime") {
        None => None,
        Some(Json::Null) => Some(None),
        Some(Json::String(s)) if s.is_empty() => Some(None),
        Some(Json::String(s)) => Some(Some(
            parse_datetime(s).ok_or(ApiError::BadRequest("end_datetime"))?,
        )),
        Some(_) => return Err(ApiError::BadRequest("end_datetime")),
    };
    if car_id.is_none() && end_datetime.is_none() {
        return Err(ApiError::BadRequest("empty_patch"));
    }
    Ok(CardPatch {
        car_id,
        end_datetime,
    })
}

pub fn car_exists_sql(car: Token) -> Result<String, ApiError> {
    bind(CAR_EXISTS_SQL, &[("car_id", Value::Token(car))])
}

/// `CAR_EXISTS_SQL` の結果。無ければ 400。
pub fn check_car_exists(rs: &ResultSet) -> Result<(), ApiError> {
    require_row(rs).map_err(|_| ApiError::BadRequest("car_id_not_found"))
}

pub fn insert_sql(id: u64, car: Token) -> Result<String, ApiError> {
    bind(
        INSERT_SQL,
        &[("id", Value::UInt(id)), ("car_id", Value::Token(car))],
    )
}

pub fn update_sql(id: u64, patch: CardPatch) -> Result<String, ApiError> {
    let flag = |set: bool| Value::UInt(u64::from(set));
    let end = patch
        .end_datetime
        .flatten()
        .map_or(Value::Null, Value::DateTime);
    bind(
        UPDATE_SQL,
        &[
            ("set_car", flag(patch.car_id.is_some())),
            ("car_id", patch.car_id.map_or(Value::Null, Value::Token)),
            ("set_end", flag(patch.end_datetime.is_some())),
            ("end_datetime", end),
            ("id", Value::UInt(id)),
        ],
    )
}

pub fn delete_sql(id: u64) -> Result<String, ApiError> {
    bind(DELETE_SQL, &[("id", Value::UInt(id))])
}

/// `GET /api/fuel/cars` の本文。
pub fn cars_json(rs: &ResultSet) -> Map<String, Json> {
    let cars = (0..rs.rows.len())
        .map(|i| json!({"id": text(rs.text(i, 0)), "name": text(rs.text(i, 1))}))
        .collect();
    let mut m = Map::new();
    m.insert("cars".into(), Json::Array(cars));
    m
}

/// `GET /api/fuel/unregistered-cards` の本文。
pub fn unregistered_json(rs: &ResultSet) -> Map<String, Json> {
    let ids = (0..rs.rows.len()).map(|i| num(rs.text(i, 0))).collect();
    let mut m = Map::new();
    m.insert("fuel_card_ids".into(), Json::Array(ids));
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests::{dt, rs};
    use kintai_mysql::bind::names;

    #[test]
    fn sql_constants_have_expected_names() {
        let cases: [(&str, &[&str]); 8] = [
            (GET_SQL, &["id"]),
            (CAR_EXISTS_SQL, &["car_id"]),
            (INSERT_SQL, &["id", "car_id"]),
            (
                UPDATE_SQL,
                &["set_car", "car_id", "set_end", "end_datetime", "id"],
            ),
            (DELETE_SQL, &["id"]),
            (CARS_SQL, &[]),
            (UNREGISTERED_SQL, &[]),
            (LIST_SQL, &[]),
        ];
        for (sql, want) in cases {
            assert_eq!(names(sql).unwrap(), want.to_vec(), "{sql}");
        }
    }

    #[test]
    fn list_and_item() {
        let data = rs(&[
            &[
                Some("1"),
                Some("307200"),
                None,
                Some("307200"),
                Some("長崎100か3072"),
            ],
            &[
                Some("2"),
                Some("999999"),
                Some("2024-01-01 00:00:00"),
                None,
                None,
            ],
        ]);
        assert_eq!(
            Json::Object(list_json(&data)),
            json!({"cards": [
                {"id": 1, "car_id": "307200", "end_datetime": null,
                 "car": {"id": "307200", "name": "長崎100か3072"}},
                {"id": 2, "car_id": "999999", "end_datetime": "2024-01-01 00:00:00", "car": null},
            ]})
        );
        assert_eq!(item_json(&rs(&[]), None), Err(ApiError::NotFound));
        let with = Json::Object(item_json(&data, Some(json!([]))).unwrap());
        assert_eq!(
            (&with["card"]["id"], &with["intank"]),
            (&json!(1), &json!([]))
        );
        let without = Json::Object(item_json(&data, None).unwrap());
        assert_eq!(without.get("intank"), None);
        assert!(get_sql(1).unwrap().contains("WHERE fc.id = 1 ORDER BY"));
    }

    #[test]
    fn create_body() {
        let (id, car) = parse_create(r#"{"id":12,"car_id":"3072ab"}"#).unwrap();
        assert_eq!((id, car.as_str()), (12, "3072ab"));
        for (body, err) in [
            (r#"{"car_id":"307200"}"#, "id"),
            (r#"{"id":0,"car_id":"307200"}"#, "id"),
            (r#"{"id":-1,"car_id":"307200"}"#, "id"),
            (r#"{"id":"12","car_id":"307200"}"#, "id"),
            (r#"{"id":12}"#, "car_id"),
            (r#"{"id":12,"car_id":""}"#, "car_id"),
            (r#"{"id":12,"car_id":"1234567"}"#, "car_id"),
            (r#"{"id":12,"car_id":"30-72"}"#, "car_id"),
            (r#"{"id":12,"car_id":307200}"#, "car_id"),
            (r#"{"id":12,"car_id":"a","x":1}"#, "unknown_field"),
        ] {
            assert_eq!(parse_create(body), Err(ApiError::BadRequest(err)), "{body}");
        }
    }

    #[test]
    fn patch_body() {
        let p = parse_patch(r#"{"car_id":"307200"}"#).unwrap();
        assert_eq!(
            (p.car_id.unwrap().as_str(), p.end_datetime),
            ("307200", None)
        );
        let p = parse_patch(r#"{"end_datetime":"2024-01-02 03:04:05"}"#).unwrap();
        assert_eq!(
            (p.car_id, p.end_datetime),
            (None, Some(Some(dt("2024-01-02 03:04:05"))))
        );
        assert_eq!(
            parse_patch(r#"{"end_datetime":null}"#)
                .unwrap()
                .end_datetime,
            Some(None)
        );
        assert_eq!(
            parse_patch(r#"{"end_datetime":""}"#).unwrap().end_datetime,
            Some(None)
        );
        for (body, err) in [
            ("{}", "empty_patch"),
            (r#"{"end_datetime":"2024-01-02"}"#, "end_datetime"),
            (r#"{"end_datetime":1}"#, "end_datetime"),
            (r#"{"car_id":"x y"}"#, "car_id"),
        ] {
            assert_eq!(parse_patch(body), Err(ApiError::BadRequest(err)), "{body}");
        }
    }

    #[test]
    fn write_sql() {
        let car = Token::new("307200").unwrap();
        assert_eq!(
            car_exists_sql(car).unwrap(),
            "SELECT id FROM cars WHERE id = '307200'"
        );
        assert_eq!(
            check_car_exists(&rs(&[])),
            Err(ApiError::BadRequest("car_id_not_found"))
        );
        assert_eq!(check_car_exists(&rs(&[&[Some("307200")]])), Ok(()));
        assert_eq!(
            insert_sql(12, car).unwrap(),
            "INSERT INTO fuel_cards (id, car_id) VALUES (12, '307200')"
        );
        let both = CardPatch {
            car_id: Some(car),
            end_datetime: Some(Some(dt("2024-01-02 03:04:05"))),
        };
        assert_eq!(
            update_sql(12, both).unwrap(),
            "UPDATE fuel_cards \
             SET car_id = IF(1 = 1, '307200', car_id), \
             end_datetime = IF(1 = 1, '2024-01-02 03:04:05', end_datetime) \
             WHERE id = 12"
        );
        let clear = CardPatch {
            car_id: None,
            end_datetime: Some(None),
        };
        assert!(update_sql(12, clear)
            .unwrap()
            .contains("SET car_id = IF(0 = 1, NULL, car_id), end_datetime = IF(1 = 1, NULL,"));
        assert_eq!(
            delete_sql(12).unwrap(),
            "DELETE FROM fuel_cards WHERE id = 12"
        );
    }

    #[test]
    fn select_lists() {
        let cars = cars_json(&rs(&[&[Some("307200"), Some("長崎100か3072")]]));
        assert_eq!(
            Json::Object(cars),
            json!({"cars": [{"id": "307200", "name": "長崎100か3072"}]})
        );
        let ids = unregistered_json(&rs(&[&[Some("12")], &[Some("40")]]));
        assert_eq!(Json::Object(ids), json!({"fuel_card_ids": [12, 40]}));
    }
}
