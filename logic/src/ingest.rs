//! 取り込みの SQL と、既存キーのスキップ (純粋ロジック)。CakePHP の FuelIntankController::uploadImport を写している。
//!
//! - 既存の (datetime, fuel_card_id) を 1 本の SELECT で引き ([`existing_sql`])、当たった行はスキップする
//! - 残りを 1 本の複数行 INSERT で入れる ([`insert_sql`]。manual = NULL)
//! - 同じ CSV の中で同じキーが 2 回出たら、2 回目はスキップに数える (PHP は主キーの重複で保存ごと落ちる)
//!
//! 何度流しても同じ結果になる (既存キーのスキップで担保する)。
//! 値はすべて `crate::api::bind` (`kintai_mysql::bind::expand`) で入れる。行の数だけ並ぶ `(…)` は、1 行ぶんの雛形を行ごとに expand して
//! `, ` でつなぐ (expand は名前の照合が引数の数の 2 乗なので、全行を 1 回で展開しない)。

use std::collections::HashSet;

use chrono::NaiveDateTime;
use kintai_mysql::bind::{Digits, Token, Value};

use crate::api::{bind, format_datetime, parse_datetime, ApiError};
use crate::csv::Row;

const EXISTING_HEAD: &str =
    "SELECT `datetime`, fuel_card_id FROM fuel_intank WHERE (`datetime`, fuel_card_id) IN (";
const KEY_TUPLE: &str = "(:datetime, :fuel_card_id)";
const EXISTING_TAIL: &str = ")";

const INSERT_HEAD: &str = "INSERT INTO fuel_intank \
(`datetime`, fuel_card_id, slip_num, slip_num_parent, quantity, `type`, detail, dtako_row_id, manual) VALUES ";
const INSERT_TUPLE: &str =
    "(:datetime, :fuel_card_id, :slip_num, :slip_num_parent, :quantity, :type, \
:detail, :dtako_row_id, NULL)";

/// 画面が取り込み直後に開く start 行 (CakePHP の `fuel_intank_latest_srch_data`)。
pub const LATEST_START_SQL: &str = "SELECT `datetime`, fuel_card_id FROM fuel_intank \
WHERE `type` = 'start' ORDER BY `datetime` DESC LIMIT 1";

/// `fuel_intank` の主キー (datetime, fuel_card_id)。
pub type Key = (NaiveDateTime, u64);

pub fn key(row: &Row) -> Key {
    (row.datetime, row.fuel_card_id)
}

fn opt_uint(n: Option<u64>) -> Value {
    n.map_or(Value::Null, Value::UInt)
}

/// 雛形 `tuple` を items ごとに expand して `, ` でつなぐ。
fn tuples<'a>(
    tuple: &str,
    items: impl Iterator<Item = Vec<(&'a str, Value)>>,
) -> Result<String, ApiError> {
    let parts = items
        .map(|params| bind(tuple, &params))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(parts.join(", "))
}

/// 既存キーを引く SELECT (行が無ければ `None`)。
pub fn existing_sql(rows: &[Row]) -> Result<Option<String>, ApiError> {
    let mut seen = HashSet::new();
    let keys: Vec<Key> = rows.iter().map(key).filter(|k| seen.insert(*k)).collect();
    if keys.is_empty() {
        return Ok(None);
    }
    tuples(
        KEY_TUPLE,
        keys.iter().map(|&(dt, card)| {
            vec![
                ("datetime", Value::DateTime(dt)),
                ("fuel_card_id", Value::UInt(card)),
            ]
        }),
    )
    .map(|body| Some([EXISTING_HEAD, &body, EXISTING_TAIL].concat()))
}

/// [`existing_sql`] の結果 (datetime, fuel_card_id の文字列) をキーの集合にする。読めない行は捨てる。
pub fn existing_keys<'a>(
    pairs: impl IntoIterator<Item = (Option<&'a str>, Option<&'a str>)>,
) -> HashSet<Key> {
    pairs
        .into_iter()
        .filter_map(|(dt, card)| Some((parse_datetime(dt?)?, card?.parse().ok()?)))
        .collect()
}

/// 入れる行とスキップの数。
#[derive(Debug, PartialEq, Eq)]
pub struct Plan<'a> {
    pub fresh: Vec<&'a Row>,
    pub skipped: usize,
}

/// 既存キーと、同じ CSV の中で先に出たキーをスキップする。
pub fn plan<'a>(rows: &'a [Row], existing: &HashSet<Key>) -> Plan<'a> {
    let mut seen = existing.clone();
    let fresh: Vec<&Row> = rows.iter().filter(|r| seen.insert(key(r))).collect();
    Plan {
        skipped: rows.len() - fresh.len(),
        fresh,
    }
}

/// 複数行の INSERT (行が無ければ `None`)。`rows` は行と照合で決まった dtako_row_id。
pub fn insert_sql(rows: &[(&Row, Option<Digits>)]) -> Result<Option<String>, ApiError> {
    if rows.is_empty() {
        return Ok(None);
    }
    tuples(
        INSERT_TUPLE,
        rows.iter().map(|(r, dtako)| {
            vec![
                ("datetime", Value::DateTime(r.datetime)),
                ("fuel_card_id", Value::UInt(r.fuel_card_id)),
                ("slip_num", opt_uint(r.slip_num)),
                ("slip_num_parent", opt_uint(r.slip_num_parent)),
                ("quantity", opt_uint(r.quantity)),
                (
                    "type",
                    Token::new(r.kind.as_str()).map_or(Value::Null, Value::Token),
                ),
                ("detail", r.detail.map_or(Value::Null, Value::Token)),
                ("dtako_row_id", dtako.map_or(Value::Null, Value::Digits)),
            ]
        }),
    )
    .map(|body| Some([INSERT_HEAD, &body].concat()))
}

/// 取り込み直後に開く start 行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatestStart {
    pub datetime: NaiveDateTime,
    pub fuel_card_id: u64,
}

/// [`LATEST_START_SQL`] の結果の 1 行目。無い・読めなければ `None`。
pub fn latest_start(datetime: Option<&str>, fuel_card_id: Option<&str>) -> Option<LatestStart> {
    Some(LatestStart {
        datetime: parse_datetime(datetime?)?,
        fuel_card_id: fuel_card_id?.parse().ok()?,
    })
}

/// 取り込みの結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub inserted: usize,
    pub skipped: usize,
    pub latest_start: Option<LatestStart>,
}

impl Summary {
    /// `{inserted, skipped, latest_start: {datetime, fuel_card_id} | null}`
    pub fn to_json(&self) -> serde_json::Value {
        let latest = self.latest_start.map(|l| {
            serde_json::json!({
                "datetime": format_datetime(l.datetime),
                "fuel_card_id": l.fuel_card_id,
            })
        });
        serde_json::json!({
            "inserted": self.inserted,
            "skipped": self.skipped,
            "latest_start": latest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csv::read;

    const CSV: &str = "Z,71043,43007,231220,0733,EP900,34,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0\n\
1,0827,6698,5523,3,43007,00019,0057,0,0,001,0003,0,00042001,0,0,0,A,0,0,0,231220\n\
1,0844,6699,5524,3,43007,00019,0218,0,0,001,0003,0,00048300,0,0,0,A,0,0,0,231220\n";

    fn rows() -> Vec<Row> {
        read(CSV.as_bytes()).unwrap()
    }

    fn dt(s: &str) -> NaiveDateTime {
        parse_datetime(s).unwrap()
    }

    #[test]
    fn existing_sql_has_one_tuple_per_unique_key() {
        let mut rows = rows();
        rows.push(rows[1].clone());
        let sql = existing_sql(&rows).unwrap().unwrap();
        assert_eq!(
            sql,
            "SELECT `datetime`, fuel_card_id FROM fuel_intank WHERE (`datetime`, fuel_card_id) IN (\
('2023-12-20 07:33:00', 9999), ('2023-12-20 08:27:00', 57), ('2023-12-20 08:44:00', 218))"
        );
        assert_eq!(existing_sql(&[]), Ok(None));
    }

    #[test]
    fn existing_keys_drops_unreadable() {
        let keys = existing_keys([
            (Some("2023-12-20 08:27:00"), Some("57")),
            (None, Some("1")),
            (Some("2023-12-20 08:27:00"), None),
            (Some("x"), Some("1")),
            (Some("2023-12-20 08:27:00"), Some("x")),
        ]);
        assert_eq!(keys, HashSet::from([(dt("2023-12-20 08:27:00"), 57)]));
    }

    #[test]
    fn plan_skips_existing_and_repeated() {
        let mut rows = rows();
        rows.push(rows[2].clone());
        let existing = HashSet::from([key(&rows[1])]);
        let p = plan(&rows, &existing);
        assert_eq!(p.skipped, 2);
        assert_eq!(p.fresh, [&rows[0], &rows[2]]);
        // 全部既存なら 0 行
        let all: HashSet<Key> = rows.iter().map(key).collect();
        assert_eq!(plan(&rows, &all).fresh.len(), 0);
    }

    #[test]
    fn insert_sql_binds_every_column() {
        let rows = rows();
        let dtako = Digits::new("23122006000000000571");
        let sql = insert_sql(&[(&rows[0], None), (&rows[1], dtako)])
            .unwrap()
            .unwrap();
        assert_eq!(
            sql,
            "INSERT INTO fuel_intank (`datetime`, fuel_card_id, slip_num, slip_num_parent, quantity, \
`type`, detail, dtako_row_id, manual) VALUES \
('2023-12-20 07:33:00', 9999, 5522, NULL, NULL, 'start', 'EP900-34', NULL, NULL), \
('2023-12-20 08:27:00', 57, 5523, 5522, 42001, 'fuel', NULL, '23122006000000000571', NULL)"
        );
        assert_eq!(insert_sql(&[]), Ok(None));
    }

    #[test]
    fn latest_start_and_summary_json() {
        let l = latest_start(Some("2023-12-20 07:33:00"), Some("9999")).unwrap();
        let s = Summary {
            inserted: 3,
            skipped: 1,
            latest_start: Some(l),
        };
        assert_eq!(
            s.to_json().to_string(),
            r#"{"inserted":3,"latest_start":{"datetime":"2023-12-20 07:33:00","fuel_card_id":9999},"skipped":1}"#
        );
        let none = Summary {
            latest_start: None,
            ..s
        };
        assert_eq!(none.to_json()["latest_start"], serde_json::Value::Null);
        assert!(latest_start(None, Some("1")).is_none());
        assert!(latest_start(Some("2023-12-20 07:33:00"), None).is_none());
        assert!(latest_start(Some("x"), Some("1")).is_none());
        assert!(latest_start(Some("2023-12-20 07:33:00"), Some("x")).is_none());
    }
}
