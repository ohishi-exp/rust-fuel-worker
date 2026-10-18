//! UTRAN.CSV の受け取り置き場 (R2 binding `FUEL_INBOX`) の口と定時実行。
//!
//! - `POST /api/fuel/uploads`: CSV を読んで (読めなければ 400、5 MB を超えたら 413)、`utran/<sha256>.csv` に
//!   customMetadata (`tenant`・`source`・`uploaded_at`) を付けて put し、そのまま取り込む。
//!   応答は `{ok, key, inserted, skipped, latest_start}`。同じ内容の再アップロードは同じ key で、取り込みはスキップで終わる
//! - `GET /api/fuel/uploads/{sha256}`: R2 にある間だけ CSV をそのまま返す (無ければ 404)
//! - 定時実行 (`[triggers] crons` の `*/10 * * * *`): `utran/` の下を全部取り込み直す。取り込みは何度流しても同じ結果になるので、
//!   取り込み済みの目印は持たない (オブジェクトは R2 の lifecycle で 1 日で消える)
//!
//! USB アプリからの口 (`crate::machine`) も、認証のあと [`store_and_ingest`] で同じ put と取り込みを通す。
//!
//! 認可 (X-Tenant-ID) は router がかける。ログには件数・段・種別だけを出す (CSV の中身・テナントは出さない)。

use std::collections::HashMap;

use fuel_logic::api::ApiError;
use fuel_logic::auth::TENANT_HEADER;
use fuel_logic::csv::{read, Row};
use fuel_logic::inbox::{
    content_disposition, csv_error_body, error_body, is_inbox_key, object_key, ok_body, sha256_hex,
    source, uploaded_at, Sha256Hex, MAX_BYTES, PREFIX, SOURCE_HEADER,
};
use fuel_logic::ingest::Summary;
use fuel_logic::reply::Reply;
use fuel_logic::route::Route;
use worker::{
    console_error, console_log, Bucket, Date, Env, Headers, HttpMetadata, Request, Response,
};

use crate::ingest::ingest;
use crate::respond;

const BUCKET_BINDING: &str = "FUEL_INBOX";

fn json(status: u16, body: String) -> worker::Result<Response> {
    respond(Reply { status, body })
}

/// `/api/fuel/uploads` の下の口 (経路と認可は lib.rs の router が済ませている)。
pub(crate) async fn handle(route: Route, req: Request, env: &Env) -> worker::Result<Response> {
    match route {
        Route::Download(sha) => download(sha, env).await,
        _ => upload(req, env).await,
    }
}

async fn upload(mut req: Request, env: &Env) -> worker::Result<Response> {
    let Some(source) = source(req.headers().get(SOURCE_HEADER)?.as_deref()) else {
        return json(400, error_body("bad_source"));
    };
    let tenant = req.headers().get(TENANT_HEADER)?.unwrap_or_default();
    let Some(bytes) = read_body(&mut req).await? else {
        return json(413, error_body("too_large"));
    };
    let sha = sha256_hex(&bytes);
    let meta = HashMap::from([
        ("tenant".to_string(), tenant),
        ("source".to_string(), source.to_string()),
        (
            "uploaded_at".to_string(),
            uploaded_at(Date::now().as_millis()),
        ),
    ]);
    store_and_ingest(env, bytes, &sha, meta).await
}

/// body を読む。宣言の長さか実際の長さが [`MAX_BYTES`] を超えたら `None` (413)。
pub(crate) async fn read_body(req: &mut Request) -> worker::Result<Option<Vec<u8>>> {
    let declared = req.headers().get("content-length")?;
    if declared
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|n| n > MAX_BYTES)
    {
        return Ok(None);
    }
    let bytes = req.bytes().await?;
    Ok((bytes.len() <= MAX_BYTES).then_some(bytes))
}

/// CSV を読んで (読めなければ 400)、`utran/<sha256>.csv` に `meta` を customMetadata として put し、取り込む。
/// `POST /api/fuel/uploads` と USB の口 (`crate::machine`) の共通部分。
pub(crate) async fn store_and_ingest(
    env: &Env,
    bytes: Vec<u8>,
    sha256: &str,
    meta: HashMap<String, String>,
) -> worker::Result<Response> {
    let rows = match read(&bytes) {
        Ok(rows) if rows.is_empty() => return json(400, error_body("empty")),
        Ok(rows) => rows,
        Err(e) => return json(400, csv_error_body(&e)),
    };

    let key = object_key(sha256);
    let csv = HttpMetadata {
        content_type: Some("text/csv".to_string()),
        ..Default::default()
    };
    let put = match bucket(env) {
        Some(b) => {
            b.put(&key, bytes)
                .custom_metadata(meta)
                .http_metadata(csv)
                .execute()
                .await
        }
        None => return json(502, error_body("r2")),
    };
    if put.is_err() {
        console_error!("fuel upload: r2 put failed");
        return json(502, error_body("r2"));
    }

    match timed_ingest(env, &rows).await {
        Ok(summary) => json(200, ok_body(&key, &summary)),
        Err(e) => respond(e.reply()),
    }
}

async fn download(sha: Sha256Hex, env: &Env) -> worker::Result<Response> {
    let Some(bucket) = bucket(env) else {
        return json(502, error_body("r2"));
    };
    let object = match bucket.get(object_key(sha.as_str())).execute().await {
        Ok(Some(object)) => object,
        Ok(None) => return json(404, error_body("not_found")),
        Err(_) => return json(502, error_body("r2")),
    };
    let bytes = match object.body() {
        Some(body) => body.bytes().await,
        None => return json(502, error_body("r2")),
    };
    let Ok(bytes) = bytes else {
        return json(502, error_body("r2"));
    };
    let headers = Headers::new();
    headers.set("content-type", "text/csv; charset=Shift_JIS")?;
    headers.set(
        "content-disposition",
        &content_disposition(Date::now().as_millis()),
    )?;
    Ok(Response::from_bytes(bytes)?.with_headers(headers))
}

fn bucket(env: &Env) -> Option<Bucket> {
    env.bucket(BUCKET_BINDING).ok()
}

/// 取り込み (所要ミリ秒と結果の件数・段だけをログに出す)。
async fn timed_ingest(env: &Env, rows: &[Row]) -> Result<Summary, ApiError> {
    let started = Date::now().as_millis();
    let outcome = ingest(env, rows).await;
    let ms = Date::now().as_millis().saturating_sub(started);
    match &outcome {
        Ok(s) => console_log!(
            "fuel uploads: ok ({ms} ms) inserted={} skipped={}",
            s.inserted,
            s.skipped
        ),
        Err(e) => {
            if let Some(line) = e.log_line("uploads", ms) {
                console_error!("{line}");
            }
        }
    }
    outcome
}

/// `utran/` の下を全部取り込み直す。1 つ失敗しても残りは続ける (定時実行は `crate::scheduled` から)。
pub(crate) async fn sweep(env: &Env) {
    let Some(bucket) = bucket(env) else {
        console_error!("fuel sweep: no bucket");
        return;
    };
    let (mut files, mut failed) = (0usize, 0usize);
    let mut cursor: Option<String> = None;
    loop {
        let mut list = bucket.list().prefix(PREFIX);
        if let Some(c) = cursor.take() {
            list = list.cursor(c);
        }
        let Ok(page) = list.execute().await else {
            console_error!("fuel sweep: r2 list failed");
            break;
        };
        for object in page.objects() {
            let key = object.key();
            if !is_inbox_key(&key) {
                continue;
            }
            files += 1;
            if !sweep_one(&bucket, env, &key).await {
                failed += 1;
            }
        }
        cursor = page.cursor().filter(|_| page.truncated());
        if cursor.is_none() {
            break;
        }
    }
    console_log!("fuel sweep: files={files} failed={failed}");
}

/// 1 つ取り込み直す。読めない・取り込めなければ `false`。
async fn sweep_one(bucket: &Bucket, env: &Env, key: &str) -> bool {
    let Ok(Some(object)) = bucket.get(key).execute().await else {
        console_error!("fuel sweep: r2 get failed");
        return false;
    };
    let Some(body) = object.body() else {
        return false;
    };
    let Ok(bytes) = body.bytes().await else {
        return false;
    };
    match read(&bytes) {
        Ok(rows) => timed_ingest(env, &rows).await.is_ok(),
        Err(e) => {
            console_error!("fuel sweep: csv line {} {}", e.line, e.reason);
            false
        }
    }
}
