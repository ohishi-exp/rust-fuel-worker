//! 給油データの届き具合の通知 (定時実行 `0 4 * * *` = 13:00 JST。判定と文面は `fuel_logic::inform`)。
//!
//! fuel_intank の最新の datetime を Read の接続で引き、前日の 12:00 (JST) より前なら LINE WORKS に 1 通送る。
//! 届いていればログに 1 行だけ出して終わる。送り方は nuxt-dtako-admin の dtako-scraper-relay (lineworks-notify.ts) と同じ:
//! auth-worker への Service Binding `AUTH_WORKER` で `/alc-internal-proxy/api/internal/lineworks/send` を POST し、
//! ヘッダは `X-Alc-Proxy-Secret` (= `INTERNAL_SHARED_SECRET`) だけ。宛先は var `FUEL_INFORM_TARGET`。
//!
//! 宛先の id・secret・応答の本文はログに出さない (段と種別と HTTP status だけ)。
//! このファイルは wasm 専用で、CI のテストと coverage の gate の外 (確かめるのは本番の実物)。

use fuel_logic::access::Access;
use fuel_logic::inform::{
    jst_now, latest, message, ok_line, parse_target, send_body, AUTH_WORKER_BINDING, LATEST_SQL,
    PROXY_SECRET_HEADER, SEND_URL, TARGET_VAR,
};
use worker::{console_error, console_log, Date, Env, Headers, Method, Request, RequestInit};

use crate::db::{close, open_session, query_rows};
use crate::machine::shared_secret;

pub(crate) async fn run(env: &Env) {
    let now = jst_now(Date::now().as_millis());
    let latest = match read_latest(env).await {
        Ok(l) => l,
        Err(stage) => return console_error!("fuel inform: failed at {stage}"),
    };
    let Some(text) = message(latest, now) else {
        if let Some(l) = latest {
            console_log!("{}", ok_line(l));
        }
        return;
    };
    let var = env.var(TARGET_VAR).ok().map(|v| v.to_string());
    let target = match parse_target(var.as_deref()) {
        Ok(t) => t,
        Err(kind) => return console_error!("fuel inform: not sent (target {kind})"),
    };
    let Some(secret) = shared_secret(env).await else {
        return console_error!("fuel inform: not sent (secret)");
    };
    match send(env, &secret, &send_body(&target, &text)).await {
        Ok(200..=299) => console_log!("fuel inform: sent"),
        Ok(status) => console_error!("fuel inform: send failed (HTTP {status})"),
        Err(stage) => console_error!("fuel inform: send failed at {stage}"),
    }
}

/// 最新の行の日時。接続・クエリの失敗は段の名前。
async fn read_latest(env: &Env) -> Result<Option<chrono::NaiveDateTime>, &'static str> {
    let mut session = open_session(env, Access::Read)
        .await
        .map_err(|f| f.stage.as_str())?;
    let result = query_rows(&mut session, LATEST_SQL).await;
    close(session).await;
    result.map(|rs| latest(&rs)).map_err(|f| f.stage.as_str())
}

/// auth-worker の内部用 proxy へ POST して HTTP status を返す。binding が無い・投げられないは段の名前。
async fn send(env: &Env, secret: &str, body: &str) -> Result<u16, &'static str> {
    let fetcher = env.service(AUTH_WORKER_BINDING).map_err(|_| "binding")?;
    let headers = Headers::new();
    headers
        .set(PROXY_SECRET_HEADER, secret)
        .map_err(|_| "request")?;
    headers
        .set("content-type", "application/json")
        .map_err(|_| "request")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(body.into()));
    let req = Request::new_with_init(SEND_URL, &init).map_err(|_| "request")?;
    let res = fetcher.fetch_request(req).await.map_err(|_| "fetch")?;
    Ok(res.status_code())
}
