//! fuel Worker (Refs yhonda-ohishi/nginx#814)。CakePHP の /fuel-intank・/fuel-cards の移行先。
//!
//! 口は `GET /api/fuel/health` の 1 本だけ(接続 → handshake → 認証 → `SET SESSION max_statement_time=60` →
//! `SELECT 1` → `COM_QUIT`)。成功は 200 `{"ok":true}`、失敗は 503 `{"ok":false,"stage":…}`。
//! それ以外の path は 404。1 リクエスト 1 接続。
//!
//! 到達面: fetch は Service Binding からだけ届く(route・workers.dev・preview 無し)。
//! 資格情報は Secrets Store の binding `FUEL_MARIADB` から読む。エラーの文言・宛先・ユーザー名は応答にもログにも出さない
//! (段と種別だけ)。社内 MariaDB へは SELECT だけ(SET SESSION はこの接続の打ち切り時間で、データは書かない)。

use std::future::Future;
use std::pin::pin;
use std::time::Duration;

use fuel_logic::reply::{log_line, reply_for, Failure, Reply, Stage};
use fuel_logic::route::{reply_for_route, route};
use futures_util::future::{select, Either};
use kintai_mysql::retry::{should_retry, Phase, RETRY_DELAY_MS};
use vpc_mysql::{open as open_socket, parse_creds, Creds, Session};
use worker::{console_error, console_log, event, Context, Date, Delay, Env, Request, Response};

/// TCP を開くまでの上限(勤怠 Worker と同じ)。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// handshake と認証の上限(それぞれ)。
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);
/// クエリの上限。サーバー側の max_statement_time (60 秒) より少し長くする。
const QUERY_TIMEOUT: Duration = Duration::from_secs(65);

/// 社内 MariaDB への Workers VPC の binding(勤怠 Worker と同じ VPC Service を共用する)。
const MARIADB_VPC_BINDING: &str = "KINTAI_MARIADB_VPC";
/// `connect()` に渡す名目のアドレス(VPC Service 側で宛先を固定するので使われない。社内のアドレスはコードに書かない)。
const MARIADB_NOMINAL_ADDR: &str = "fuel-mariadb:3306";
/// 資格情報の Secrets Store の binding。
const CREDS_BINDING: &str = "FUEL_MARIADB";

const SET_STATEMENT_TIME: &str = "SET SESSION max_statement_time=60";
const HEALTH_SQL: &str = "SELECT 1";

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> worker::Result<Response> {
    let route = route(req.method().as_ref(), &req.path());
    let reply = match reply_for_route(route) {
        Some(reply) => reply,
        None => run_health(&env).await,
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
    let creds = load_creds(env)
        .await
        .ok_or(Failure::new(Stage::Secret, "missing"))?;
    let mut session = open(env, &creds).await?;
    let result = step(Stage::Query, QUERY_TIMEOUT, session.query(HEALTH_SQL)).await;
    session.quit().await;
    match result?.text(0, 0) {
        Some("1") => Ok(()),
        _ => Err(Failure::new(Stage::Query, "unexpected_result")),
    }
}

/// やり直しの判断 (`kintai_mysql::retry`) に渡す段。資格情報の段は接続の外なので `None`。
fn phase(stage: Stage) -> Option<Phase> {
    match stage {
        Stage::Secret => None,
        Stage::Connect => Some(Phase::Connect),
        Stage::Handshake => Some(Phase::Handshake),
        Stage::Auth => Some(Phase::Auth),
        Stage::Query => Some(Phase::Query),
    }
}

/// 1 接続を開く(接続 → handshake → 認証 → 打ち切り時間の設定)。
/// 認証パケットを送る前 (connect・handshake の段) の失敗だけ、新しい接続で `kintai_mysql::retry` の上限までやり直す。
async fn open(env: &Env, creds: &Creds) -> Result<Session, Failure> {
    let mut retries = 0;
    loop {
        match open_once(env, creds).await {
            Err(f) if phase(f.stage).is_some_and(|p| should_retry(p, retries)) => {
                retries += 1;
                console_log!("fuel mariadb: retry {retries} after {}", f.stage.as_str());
                Delay::from(Duration::from_millis(RETRY_DELAY_MS)).await;
            }
            result => return result,
        }
    }
}

/// 1 回ぶん: 接続 → handshake → 認証 → 打ち切り時間の設定。途中で失敗したら COM_QUIT を送って閉じる。
async fn open_once(env: &Env, creds: &Creds) -> Result<Session, Failure> {
    let socket = step(Stage::Connect, CONNECT_TIMEOUT, async {
        open_socket(env, MARIADB_VPC_BINDING, MARIADB_NOMINAL_ADDR)
            .await
            .map_err(|_| "transport".to_string())
    })
    .await?;
    let mut session = Session::new(socket);
    match login(&mut session, creds).await {
        Ok(()) => Ok(session),
        Err(f) => {
            session.quit().await;
            Err(f)
        }
    }
}

/// handshake → 認証 → 打ち切り時間の設定。
async fn login(session: &mut Session, creds: &Creds) -> Result<(), Failure> {
    let (seq, hs) = step(Stage::Handshake, LOGIN_TIMEOUT, session.handshake()).await?;
    step(
        Stage::Auth,
        LOGIN_TIMEOUT,
        session.authenticate(seq, &hs, creds),
    )
    .await?;
    step(
        Stage::Query,
        QUERY_TIMEOUT,
        session.query(SET_STATEMENT_TIME),
    )
    .await
    .map(|_| ())
}

/// 1 段を `limit` で打ち切って走らせ、失敗に段を付ける。
async fn step<T>(
    stage: Stage,
    limit: Duration,
    fut: impl Future<Output = Result<T, String>>,
) -> Result<T, Failure> {
    match select(pin!(fut), pin!(Delay::from(limit))).await {
        Either::Left((result, _)) => result.map_err(|kind| Failure::new(stage, kind)),
        Either::Right(_) => Err(Failure::new(stage, "timeout")),
    }
}

/// 資格情報を読んで検証する。binding が無い・secret が未投入・JSON が不正は `None`(中身はどこにも出さない)。
async fn load_creds(env: &Env) -> Option<Creds> {
    let store = env.secret_store(CREDS_BINDING).ok()?;
    let json = store.get().await.ok().flatten()?;
    parse_creds(&json)
}
