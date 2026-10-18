# rust-fuel-worker

インタンク給油 (`fuel_intank` / `fuel_cards`) の Rust Worker。CakePHP の `/fuel-intank`・`/fuel-cards` (社内 MariaDB) の移行先。
Refs yhonda-ohishi/nginx#814

## 構成

| パス | 内容 |
|---|---|
| `logic/` (`fuel-logic`) | native で `cargo test` できる純粋ロジック。経路判定・失敗の段・応答の写像。行カバレッジ 100% gate (`coverage_100.toml`) |
| `worker/` (`fuel-worker`) | Cloudflare Worker (wasm32、`worker` 0.8.7)。Workers VPC の TCP で社内 MariaDB に繋ぐ |
| `scripts/` | `check-exposure.sh` (外から届かないことの検査) とその陰性対照、`check_coverage_100.sh` |

MariaDB への接続部品は [ohishi-exp/rust-ichibanboshi](https://github.com/ohishi-exp/rust-ichibanboshi) の共有 crate (`vpc-mysql`・`kintai-mysql`) を rev 固定の git 依存で使う。
勤怠 Worker と同じ VPC Service (`KINTAI_MARIADB_VPC`) を共用し、資格情報は書き込みと分かる形で 2 本に分ける。読み取りは勤怠 Worker と共用の Secrets Store の `KINTAI_MARIADB`、書き込みは fuel 専用の `FUEL_MARIADB_WRITE` (fuel_intank・fuel_cards の INSERT・UPDATE・DELETE と、照合のための dtako_rows・fuel_cards の SELECT)。口ごとの種別は呼び出し側が `Access::Read` / `Access::Write` で必ず明示する (既定値は無い。`logic/src/access.rs`)。書き込みの口は前段の SELECT も Write の接続で行う (1 リクエスト 1 接続)。

## 口

Service Binding 専用 (route・workers.dev・preview なし)。

- `GET /api/fuel/health` — MariaDB に繋いで `SELECT 1` を流す。成功は 200 `{"ok":true}`、失敗は 503 `{"ok":false,"stage":"secret|connect|handshake|auth|query"}`。
  エラーの文言・宛先・ユーザー名は応答にもログにも出さない (段と種別だけ)
- `/api/fuel/*` のそれ以外 — header `X-Tenant-ID` が var `FUEL_TENANT_ID` と一致するときだけ通す参照・編集と、UTRAN.CSV の取り込み (`POST /api/fuel/uploads`)
- `POST /machine/fuel-utran/uploads` — USB 常駐アプリからの UTRAN.CSV (下の「USB アップロード」)
- それ以外の path は 404 (method 違いは 405)

## USB アップロード

Windows に USB を挿すと常駐アプリが UTRAN.CSV を送る。経路:

```
USB アプリ ─(machine credential の Basic)→ auth-worker /m/fuel-utran/uploads
  → machine-relay (tenant を決めない・X-Tenant-* を剥がす・X-Internal-Shared-Secret と Authorization を付ける)
  → この Worker の POST /machine/fuel-utran/uploads
      1. X-Internal-Shared-Secret を INTERNAL_SHARED_SECRET (Secrets Store) と定数時間で照合 (無い・不一致 401 / 読めない 503)
      2. body (5 MB まで。超えたら 413) の sha256
      3. auth-worker の RPC issueTicket(Authorization, "fuel-utran", sha256) (binding AUTH_MCRED。null 401 / 失敗 503)
      4. 戻りの tenantId を FUEL_TENANT_ID と突き合わせ (違えば 403)
      5. POST /api/fuel/uploads と同じく R2 に put して取り込む (customMetadata は tenant・source=usb・uploaded_at・ticket・machine_user)
```

Authorization・ticket・secret・username は応答にもログにも出さない。判定は `logic/src/machine.rs`、I/O は `worker/src/machine.rs`。

## 開発

```bash
cargo test --locked -p fuel-logic
cargo llvm-cov --locked -p fuel-logic --text > cov.txt
bash scripts/check_coverage_100.sh --config coverage_100.toml --use-cache cov.txt
cargo clippy --locked -p fuel-worker --target wasm32-unknown-unknown --all-targets -- -D warnings
bash scripts/check-exposure.sh worker/wrangler.toml && bash scripts/check-exposure-test.sh
(cd worker && worker-build --release && npx wrangler@4.144.0 deploy --dry-run)
```

toolchain は Rust 1.92.0 (target wasm32-unknown-unknown)、`worker-build` 0.8.7。

## deploy

タグ `v*` の push で本番に `wrangler deploy` する (`.github/workflows/deploy.yml`。staging は無い)。merge では出ない。
deploy の前に CI (`ci.yml`) と同じ検査が通る必要がある。token は org の secret `CLOUDFLARE_API_TOKEN`。

初回は Secrets Store の `FUEL_MARIADB_WRITE` (JSON。キー user / password / database。`KINTAI_MARIADB` は勤怠 Worker が投入済み) の投入と、R2 バケット `fuel-inbox` の作成が要る
(未投入でも deploy はでき、該当の口は 503 `stage=secret` を返す)。
