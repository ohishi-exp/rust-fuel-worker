//! fuel_intank の口(`GET /api/fuel/starts`・`GET /api/fuel/intank`・`PATCH`/`DELETE /api/fuel/intank/{key}`)。
//! SQL と JSON の組み立ては `fuel_logic::intank`、ここは DB とのやり取りの順番だけ。
//! 入力の検査は接続を開く前に済ませる(400 で DB に繋がない)。

use std::collections::HashMap;

use fuel_logic::access::Access;
use fuel_logic::api::{ok, parse_datetime, query_param, require_row, ApiError};
use fuel_logic::intank::{
    array_json, by_start_json, by_start_sql, check_deletable, delete_sql, follow_ups, item_json,
    list_json, parse_fuel_rows, parse_patch, parse_start, patch_sql, row_sql, start_row_sql,
    starts_json, sum_json, LATEST_SQL, STARTS_SQL,
};
use fuel_logic::reply::Reply;
use fuel_logic::route::IntankKey;
use kintai_mysql::response::ResultSet;
use serde_json::{Map, Value as Json};
use worker::Env;

use crate::db::{query_rows, Session};
use crate::with_session;

pub async fn starts(env: &Env) -> Result<Reply, ApiError> {
    with_session(env, Access::Read, async |s| {
        Ok(ok(starts_json(&query_rows(s, STARTS_SQL).await?)))
    })
    .await
}

/// query 無しは最新 100 行、`?start=<datetime>` はその start 行の伝票の給油行と集計。
pub async fn list(env: &Env, query: Option<&str>) -> Result<Reply, ApiError> {
    let start = match query_param(query, "start")? {
        Some(v) => Some(parse_datetime(&v).ok_or(ApiError::BadRequest("start"))?),
        None => None,
    };
    with_session(env, Access::Read, async |s| match start {
        Some(dt) => by_start(s, dt).await,
        None => Ok(ok(list_json(&query_rows(s, LATEST_SQL).await?))),
    })
    .await
}

async fn by_start(s: &mut Session, datetime: chrono::NaiveDateTime) -> Result<Reply, ApiError> {
    let start = parse_start(&query_rows(s, &start_row_sql(datetime)?).await?)?;
    let rows = parse_fuel_rows(&query_rows(s, &by_start_sql(&start)?).await?)?;
    // 同じ運行・同じ車輌の日は同じ SQL になるので 1 回だけ流す
    let mut cache = HashMap::new();
    let mut results = Vec::with_capacity(rows.len());
    for up in follow_ups(&start, &rows)? {
        let sum = cached(s, &mut cache, up.sum, sum_json).await?;
        let array = cached(s, &mut cache, up.array, array_json).await?;
        results.push((sum, array));
    }
    Ok(ok(by_start_json(start, rows, results)))
}

async fn cached(
    s: &mut Session,
    cache: &mut HashMap<String, Json>,
    sql: Option<String>,
    to_json: fn(&ResultSet) -> Json,
) -> Result<Option<Json>, ApiError> {
    let Some(sql) = sql else { return Ok(None) };
    if let Some(v) = cache.get(&sql) {
        return Ok(Some(v.clone()));
    }
    let v = to_json(&query_rows(s, &sql).await?);
    cache.insert(sql, v.clone());
    Ok(Some(v))
}

/// dtako_row_id の付け替え。応答は付け替えた後の行。
pub async fn patch(env: &Env, key: IntankKey, body: &str) -> Result<Reply, ApiError> {
    let dtako_row_id = parse_patch(body)?;
    with_session(env, Access::Write, async |s| {
        require_row(&query_rows(s, &row_sql(key)?).await?)?;
        query_rows(s, &patch_sql(key, dtako_row_id)?)
            .await
            .map_err(ApiError::from_write)?;
        Ok(ok(item_json(&query_rows(s, &row_sql(key)?).await?)?))
    })
    .await
}

/// manual が真の行だけ消す(そうでなければ 409)。
pub async fn delete(env: &Env, key: IntankKey) -> Result<Reply, ApiError> {
    with_session(env, Access::Write, async |s| {
        check_deletable(&query_rows(s, &row_sql(key)?).await?)?;
        query_rows(s, &delete_sql(key)?)
            .await
            .map_err(ApiError::from_write)?;
        Ok(ok(Map::new()))
    })
    .await
}
