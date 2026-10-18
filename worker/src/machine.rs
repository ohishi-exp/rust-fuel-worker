//! USB 常駐アプリからの UTRAN.CSV の受け口 `POST /machine/fuel-utran/uploads` (判定は `fuel_logic::machine`)。
//!
//! auth-worker の machine-relay からだけ届く (共有 secret `X-Internal-Shared-Secret`)。tenant は X-Tenant-ID ではなく、
//! auth-worker の RPC `issueTicket` (Service Binding `AUTH_MCRED`、entrypoint `MachineCredentialEntrypoint`) が
//! machine credential の Basic から決めた `tenantId` を使う。通ったあとは `POST /api/fuel/uploads` と同じ
//! (`inbox::store_and_ingest`)。
//!
//! Authorization・ticket・secret・username は応答にもログにも出さない (ログは段の名前だけ)。
//! このファイルは wasm 専用で、CI のテストと coverage の gate の外 (確かめるのは本番の実物)。

use fuel_logic::auth::{authorize, TENANT_VAR};
use fuel_logic::inbox::{sha256_hex, uploaded_at};
use fuel_logic::machine::{
    authorization, metadata, parse_issued, reject, secrets_match, unauthorized, unavailable,
    Issued, MCRED_BINDING, PURPOSE, SHARED_SECRET_BINDING, SHARED_SECRET_HEADER,
};
use fuel_logic::reply::Reply;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use worker::{console_error, Date, Env, Request, Response};

use crate::inbox::{read_body, store_and_ingest};
use crate::respond;

#[wasm_bindgen]
extern "C" {
    /// auth-worker の named entrypoint `MachineCredentialEntrypoint` (binding `AUTH_MCRED`)
    #[wasm_bindgen(extends = js_sys::Object)]
    type MachineCredentialRpc;

    #[wasm_bindgen(method, catch, js_name = issueTicket)]
    fn issue_ticket(
        this: &MachineCredentialRpc,
        authorization: &str,
        purpose: &str,
        sha256: &str,
    ) -> Result<js_sys::Promise, JsValue>;
}

/// 段の名前だけをログに出して弾く。
fn deny(stage: &str, reply: Reply) -> worker::Result<Response> {
    console_error!("fuel machine upload: {stage}");
    respond(reply)
}

pub(crate) async fn upload(mut req: Request, env: &Env) -> worker::Result<Response> {
    let Some(secret) = shared_secret(env).await else {
        return deny("secret", unavailable("secret"));
    };
    let header = req.headers().get(SHARED_SECRET_HEADER)?;
    if !secrets_match(header.as_deref(), &secret) {
        return deny("not_relayed", unauthorized());
    }
    let authz = req.headers().get("Authorization")?;

    let Some(bytes) = read_body(&mut req).await? else {
        return respond(reject(413, "too_large"));
    };
    let sha = sha256_hex(&bytes);

    let Some(authz) = authorization(authz.as_deref()) else {
        return deny("no_authorization", unauthorized());
    };
    let ticket = match issue(env, authz, &sha).await.map(|v| parse_issued(&v)) {
        Some(Issued::Ticket(t)) => t,
        Some(Issued::Denied) => return deny("denied", unauthorized()),
        Some(Issued::Malformed) | None => return deny("rpc", unavailable("auth")),
    };

    let configured = env.var(TENANT_VAR).ok().map(|v| v.to_string());
    if let Err(reply) = authorize(configured.as_deref(), Some(&ticket.tenant_id)) {
        return deny("tenant", reply);
    }

    let meta = metadata(&ticket, uploaded_at(Date::now().as_millis()));
    store_and_ingest(env, bytes, &sha, meta.into_iter().collect()).await
}

/// 共有 secret (Secrets Store)。binding が無い・読めない・空なら `None` (503)。
async fn shared_secret(env: &Env) -> Option<String> {
    let store = env.secret_store(SHARED_SECRET_BINDING).ok()?;
    store.get().await.ok().flatten().filter(|s| !s.is_empty())
}

/// `issueTicket` を呼んで戻りを JSON にする。binding が無い・throw・変換できない値は `None` (503)。
/// null / undefined は `Value::Null` (401)。エラーの中身は捨てる。
async fn issue(env: &Env, authorization: &str, sha256: &str) -> Option<serde_json::Value> {
    let rpc = env
        .service(MCRED_BINDING)
        .ok()?
        .into_rpc::<MachineCredentialRpc>();
    let promise = rpc.issue_ticket(authorization, PURPOSE, sha256).ok()?;
    let value = JsFuture::from(promise).await.ok()?;
    if value.is_null() || value.is_undefined() {
        return Some(serde_json::Value::Null);
    }
    serde_wasm_bindgen::from_value(value).ok()
}
