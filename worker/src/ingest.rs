//! UTRAN.CSV の行を社内 MariaDB の `fuel_intank` へ取り込む (CakePHP の FuelIntankController::uploadImport)。
//!
//! 1 接続で: 既存の (datetime, fuel_card_id) を 1 本の SELECT で引く → 残りの fuel 行ごとに dtako_row_id を照合する
//! (fuel_cards → dtako_rows。fuel_card_id ごとの car_id は 1 回だけ引く) → 1 本の複数行 INSERT → 最新の start 行を引く。
//! 何度流しても同じ結果になる (既存キーはスキップ)。SQL と判定は fuel-logic の `ingest`・`dtako_match`、
//! 値はすべて `fuel_logic::api::bind` で入れる。接続は `db` と `with_session`。

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use fuel_logic::access::Access;
use fuel_logic::api::{bind, ApiError};
use fuel_logic::csv::Row;
use fuel_logic::dtako_match::{
    card_params, decide, needs_match, span, span_params, Span, AFTER_SQL, CARD_SQL, WITHIN_SQL,
};
use fuel_logic::ingest::{
    existing_keys, existing_sql, insert_sql, latest_start, plan, Summary, LATEST_START_SQL,
};
use kintai_mysql::bind::{Digits, Value};
use kintai_mysql::response::ResultSet;
use worker::Env;

use crate::db::{query_rows, Session};
use crate::with_session;

/// 行を取り込む (1 リクエスト 1 接続)。失敗は段と種別だけ。
pub(crate) async fn ingest(env: &Env, rows: &[Row]) -> Result<Summary, ApiError> {
    with_session(env, Access::Write, async |s| run(s, rows).await).await
}

async fn query_bound(
    session: &mut Session,
    sql: &str,
    params: &[(&str, Value)],
) -> Result<ResultSet, ApiError> {
    Ok(query_rows(session, &bind(sql, params)?).await?)
}

async fn run(session: &mut Session, rows: &[Row]) -> Result<Summary, ApiError> {
    let existing = match existing_sql(rows)? {
        Some(sql) => {
            let set = query_rows(session, &sql).await?;
            existing_keys((0..set.rows.len()).map(|i| (set.text(i, 0), set.text(i, 1))))
        }
        None => HashSet::new(),
    };
    let plan = plan(rows, &existing);

    let mut cars = HashMap::new();
    let mut fresh = Vec::with_capacity(plan.fresh.len());
    for &row in &plan.fresh {
        let id = match needs_match(row) {
            true => dtako_row_id(session, row, &mut cars).await?,
            false => None,
        };
        fresh.push((row, id));
    }
    if let Some(sql) = insert_sql(&fresh)? {
        query_rows(session, &sql).await?;
    }

    let latest = query_rows(session, LATEST_START_SQL).await?;
    Ok(Summary {
        inserted: fresh.len(),
        skipped: plan.skipped,
        latest_start: latest_start(latest.text(0, 0), latest.text(0, 1)),
    })
}

fn first_span(set: &ResultSet) -> Option<Span> {
    span(set.text(0, 0), set.text(0, 1), set.text(0, 2))
}

/// 1 行の dtako_row_id。カードが無い・運行が無い・時間外は `None`。
async fn dtako_row_id(
    session: &mut Session,
    row: &Row,
    cars: &mut HashMap<u64, Option<String>>,
) -> Result<Option<Digits>, ApiError> {
    if let Entry::Vacant(slot) = cars.entry(row.fuel_card_id) {
        let set = query_bound(session, CARD_SQL, &card_params(row.fuel_card_id)).await?;
        slot.insert(set.text(0, 0).map(str::to_string));
    }
    let car = cars.get(&row.fuel_card_id).and_then(Option::as_deref);
    let Some(params) = span_params(car, row.datetime) else {
        return Ok(None);
    };
    let within = first_span(&query_bound(session, WITHIN_SQL, &params).await?);
    if let Some(id) = decide(row.datetime, within, None) {
        return Ok(Some(id));
    }
    let after = first_span(&query_bound(session, AFTER_SQL, &params).await?);
    Ok(decide(row.datetime, within, after))
}
