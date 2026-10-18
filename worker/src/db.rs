//! 社内 MariaDB への 1 接続(Workers VPC の TCP + MySQL プロトコル。1 リクエスト 1 接続)。
//!
//! 口は 3 つ: [`open_session`](資格情報を読んで接続 → handshake → 認証 → 打ち切り時間の設定)・
//! [`query_rows`](COM_QUERY を 1 本流して結果セットを読む)・[`close`](COM_QUIT を送って閉じる)。
//! SQL は呼び手が `fuel_logic::api::bind`(`kintai_mysql::bind::expand`)で値を入れてから渡す。
//!
//! 資格情報は Secrets Store の binding `FUEL_MARIADB` から読む。エラーの文言・宛先・ユーザー名は返さない
//! (段と種別だけ)。

use std::future::Future;
use std::pin::pin;
use std::time::Duration;

use fuel_logic::reply::{Failure, Stage};
use futures_util::future::{select, Either};
use kintai_mysql::response::ResultSet;
use kintai_mysql::retry::{should_retry, Phase, RETRY_DELAY_MS};
use vpc_mysql::{open as open_socket, parse_creds, Creds};
use worker::{console_log, Delay, Env};

pub use vpc_mysql::Session;

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

/// 資格情報を読んで 1 接続を開く。資格情報が無い・読めないは `Stage::Secret`。
pub async fn open_session(env: &Env) -> Result<Session, Failure> {
    let creds = load_creds(env)
        .await
        .ok_or(Failure::new(Stage::Secret, "missing"))?;
    open(env, &creds).await
}

/// SQL を 1 本流して結果セットを読む(結果を返さない文は空の結果)。失敗は `Stage::Query`。
pub async fn query_rows(session: &mut Session, sql: &str) -> Result<ResultSet, Failure> {
    step(Stage::Query, QUERY_TIMEOUT, session.query(sql)).await
}

/// COM_QUIT を送って閉じる(失敗しても結果は変わらないので捨てる)。
pub async fn close(session: Session) {
    session.quit().await;
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
    query_rows(session, SET_STATEMENT_TIME).await.map(|_| ())
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
