//! fuel Worker (Refs yhonda-ohishi/nginx#814)。CakePHP の /fuel-intank・/fuel-cards の移行先。
//!
//! 口(経路の表は `fuel_logic::route`):
//! - `GET /api/fuel/health`: 接続 → handshake → 認証 → `SET SESSION max_statement_time=60` → `SELECT 1` → `COM_QUIT`。
//!   成功は 200 `{"ok":true}`、失敗は 503 `{"ok":false,"stage":…}`。認可を求めない
//! - それ以外の `/api/fuel/*`: header `X-Tenant-ID` が var `FUEL_TENANT_ID` と一致することを求める
//!   (`fuel_logic::auth`)。参照(starts・intank・unregistered-cards・cars・cards)と編集(cards の追加・変更・削除、
//!   給油行の dtako_row_id の付け替えと手入力の行の削除)、UTRAN.CSV の取り込み (`inbox`。uploads)
//! - `POST /machine/fuel-utran/uploads`: USB 常駐アプリからの UTRAN.CSV (auth-worker の machine-relay が中継)。
//!   X-Tenant-ID ではなく共有 secret `X-Internal-Shared-Secret` と auth-worker の RPC `issueTicket` で通す (`machine`)
//! - それ以外の path は 404。1 リクエスト 1 接続(`db`)
//!
//! 到達面: fetch は Service Binding からだけ届く(route・workers.dev・preview 無し)。JWT は auth-worker が検証し、
//! この Worker は付け直された `X-Tenant-ID` を信頼する。エラーの文言・宛先・ユーザー名は応答にもログにも出さない
//! (段と種別だけ)。SQL の値はすべて `kintai_mysql::bind::expand` の名前付き引数で入れる。

mod cards;
mod db;
mod inbox;
mod ingest;
mod intank;
mod machine;

use fuel_logic::api::ApiError;
use fuel_logic::auth::{authorize, needs_tenant, TENANT_HEADER, TENANT_VAR};
use fuel_logic::reply::{log_line, reply_for, Failure, Reply, Stage};
use fuel_logic::route::{reply_for_route, route, Route};
use worker::{console_error, console_log, event, Context, Date, Env, Request, Response};

const HEALTH_SQL: &str = "SELECT 1";

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> worker::Result<Response> {
    let path = req.path();
    if needs_tenant(&path) {
        let configured = env.var(TENANT_VAR).ok().map(|v| v.to_string());
        let header = req.headers().get(TENANT_HEADER)?;
        if let Err(reply) = authorize(configured.as_deref(), header.as_deref()) {
            return respond(reply);
        }
    }
    let route = route(req.method().as_ref(), &path);
    if let Some(reply) = reply_for_route(route) {
        return respond(reply);
    }
    let reply = match route {
        Route::Health => run_health(&env).await,
        // 本文が CSV のバイト列 (SJIS) なので text() で読まない
        Route::Upload | Route::Download(_) => return inbox::handle(route, req, &env).await,
        Route::MachineUpload => return machine::upload(req, &env).await,
        _ => {
            let query = req.url()?.query().map(str::to_string);
            let body = req.text().await?;
            run_api(&env, route, query.as_deref(), &body).await
        }
    };
    respond(reply)
}

fn respond(reply: Reply) -> worker::Result<Response> {
    let headers = worker::Headers::new();
    headers.set("content-type", "application/json")?;
    Ok(Response::ok(reply.body)?
        .with_status(reply.status)
        .with_headers(headers))
}

async fn run_health(env: &Env) -> Reply {
    let started = Date::now().as_millis();
    let outcome = health(env).await;
    let ms = Date::now().as_millis().saturating_sub(started);
    match &outcome {
        Ok(()) => console_log!("fuel health: ok ({ms} ms)"),
        Err(f) => console_error!("{}", log_line(f, ms)),
    }
    reply_for(outcome)
}

async fn health(env: &Env) -> Result<(), Failure> {
    let mut session = db::open_session(env).await?;
    let result = db::query_rows(&mut session, HEALTH_SQL).await;
    db::close(session).await;
    match result?.text(0, 0) {
        Some("1") => Ok(()),
        _ => Err(Failure::new(Stage::Query, "unexpected_result")),
    }
}

/// ログに出す口の名前。
fn route_name(route: Route) -> &'static str {
    match route {
        Route::Starts => "starts",
        Route::IntankList | Route::IntankPatch(_) | Route::IntankDelete(_) => "intank",
        Route::UnregisteredCards => "unregistered-cards",
        Route::Cars => "cars",
        _ => "cards",
    }
}

async fn run_api(env: &Env, route: Route, query: Option<&str>, body: &str) -> Reply {
    let started = Date::now().as_millis();
    let outcome = api(env, route, query, body).await;
    let ms = Date::now().as_millis().saturating_sub(started);
    outcome.unwrap_or_else(|e| {
        if let Some(line) = e.log_line(route_name(route), ms) {
            console_error!("{line}");
        }
        e.reply()
    })
}

async fn api(env: &Env, route: Route, query: Option<&str>, body: &str) -> Result<Reply, ApiError> {
    match route {
        Route::Starts => intank::starts(env).await,
        Route::IntankList => intank::list(env, query).await,
        Route::IntankPatch(key) => intank::patch(env, key, body).await,
        Route::IntankDelete(key) => intank::delete(env, key).await,
        Route::UnregisteredCards => cards::unregistered(env).await,
        Route::Cars => cards::cars(env).await,
        Route::CardsList => cards::list(env).await,
        Route::CardCreate => cards::create(env, body).await,
        Route::CardGet(id) => cards::get(env, id).await,
        Route::CardPatch(id) => cards::patch(env, id, body).await,
        Route::CardDelete(id) => cards::delete(env, id).await,
        // health と弾いた行き先は fetch で先に返している
        Route::Health
        | Route::Upload
        | Route::Download(_)
        | Route::MachineUpload
        | Route::BadPath
        | Route::NotFound
        | Route::MethodNotAllowed => Err(ApiError::NotFound),
    }
}

/// 1 接続を開いて `f` を走らせ、結果に関わらず閉じる。
async fn with_session<T>(
    env: &Env,
    f: impl AsyncFnOnce(&mut db::Session) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let mut session = db::open_session(env).await?;
    let result = f(&mut session).await;
    db::close(session).await;
    result
}
