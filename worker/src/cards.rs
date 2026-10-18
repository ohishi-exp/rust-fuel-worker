//! fuel_cards の口(`/api/fuel/cards`・`/api/fuel/cards/{id}`)と select 用の一覧(`/api/fuel/cars`・
//! `/api/fuel/unregistered-cards`)。SQL と JSON の組み立ては `fuel_logic::cards`、ここは DB とのやり取りの順番だけ。
//! 入力の検査は接続を開く前に済ませる(400 で DB に繋がない)。

use fuel_logic::access::Access;
use fuel_logic::api::{ok, require_row, with_status, ApiError};
use fuel_logic::cards::{
    car_exists_sql, cars_json, check_car_exists, delete_sql, get_sql, insert_sql, item_json,
    list_json, parse_create, parse_patch, unregistered_json, update_sql, CARS_SQL, LIST_SQL,
    UNREGISTERED_SQL,
};
use fuel_logic::intank::{by_card_sql, rows_json};
use fuel_logic::reply::Reply;
use serde_json::Map;
use worker::Env;

use crate::db::query_rows;
use crate::with_session;

pub async fn list(env: &Env) -> Result<Reply, ApiError> {
    with_session(env, Access::Read, async |s| {
        Ok(ok(list_json(&query_rows(s, LIST_SQL).await?)))
    })
    .await
}

/// card とその給油行。
pub async fn get(env: &Env, id: u64) -> Result<Reply, ApiError> {
    with_session(env, Access::Read, async |s| {
        let card = query_rows(s, &get_sql(id)?).await?;
        require_row(&card)?;
        let intank = rows_json(&query_rows(s, &by_card_sql(id)?).await?);
        Ok(ok(item_json(&card, Some(intank))?))
    })
    .await
}

/// 追加。応答は 201 と追加した card。
pub async fn create(env: &Env, body: &str) -> Result<Reply, ApiError> {
    let (id, car) = parse_create(body)?;
    with_session(env, Access::Write, async |s| {
        check_car_exists(&query_rows(s, &car_exists_sql(car)?).await?)?;
        query_rows(s, &insert_sql(id, car)?)
            .await
            .map_err(ApiError::from_write)?;
        Ok(with_status(
            201,
            item_json(&query_rows(s, &get_sql(id)?).await?, None)?,
        ))
    })
    .await
}

/// car_id・end_datetime の変更。応答は変えた後の card。
pub async fn patch(env: &Env, id: u64, body: &str) -> Result<Reply, ApiError> {
    let patch = parse_patch(body)?;
    with_session(env, Access::Write, async |s| {
        require_row(&query_rows(s, &get_sql(id)?).await?)?;
        if let Some(car) = patch.car_id {
            check_car_exists(&query_rows(s, &car_exists_sql(car)?).await?)?;
        }
        query_rows(s, &update_sql(id, patch)?)
            .await
            .map_err(ApiError::from_write)?;
        Ok(ok(item_json(&query_rows(s, &get_sql(id)?).await?, None)?))
    })
    .await
}

pub async fn delete(env: &Env, id: u64) -> Result<Reply, ApiError> {
    with_session(env, Access::Write, async |s| {
        require_row(&query_rows(s, &get_sql(id)?).await?)?;
        query_rows(s, &delete_sql(id)?)
            .await
            .map_err(ApiError::from_write)?;
        Ok(ok(Map::new()))
    })
    .await
}

pub async fn cars(env: &Env) -> Result<Reply, ApiError> {
    with_session(env, Access::Read, async |s| {
        Ok(ok(cars_json(&query_rows(s, CARS_SQL).await?)))
    })
    .await
}

pub async fn unregistered(env: &Env) -> Result<Reply, ApiError> {
    with_session(env, Access::Read, async |s| {
        Ok(ok(unregistered_json(
            &query_rows(s, UNREGISTERED_SQL).await?,
        )))
    })
    .await
}
