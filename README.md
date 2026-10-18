# rust-fuel-worker

インタンク給油 (`fuel_intank` / `fuel_cards`) の Rust Worker。CakePHP の `/fuel-intank`・`/fuel-cards` (社内 MariaDB) の移行先。
Refs yhonda-ohishi/nginx#814

この repo は土台 (PR1) だけ。CSV 取り込み・参照 API・auth-worker の振り分けは後続の PR で足す。

## 構成

| パス | 内容 |
|---|---|
| `logic/` (`fuel-logic`) | native で `cargo test` できる純粋ロジック。経路判定・失敗の段・応答の写像。行カバレッジ 100% gate (`coverage_100.toml`) |
| `worker/` (`fuel-worker`) | Cloudflare Worker (wasm32、`worker` 0.8.7)。Workers VPC の TCP で社内 MariaDB に繋ぐ |
| `scripts/` | `check-exposure.sh` (外から届かないことの検査) とその陰性対照、`check_coverage_100.sh` |

MariaDB への接続部品は [ohishi-exp/rust-ichibanboshi](https://github.com/ohishi-exp/rust-ichibanboshi) の共有 crate (`vpc-mysql`・`kintai-mysql`) を rev 固定の git 依存で使う。
勤怠 Worker と同じ VPC Service (`KINTAI_MARIADB_VPC`) を共用し、資格情報だけ別の secret (`FUEL_MARIADB`) を使う。

## 口

Service Binding 専用 (route・workers.dev・preview なし)。

- `GET /api/fuel/health` — MariaDB に繋いで `SELECT 1` を流す。成功は 200 `{"ok":true}`、失敗は 503 `{"ok":false,"stage":"secret|connect|handshake|auth|query"}`。
  エラーの文言・宛先・ユーザー名は応答にもログにも出さない (段と種別だけ)
- それ以外の path は 404 (health の method 違いは 405)

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

初回は Secrets Store の `FUEL_MARIADB` (JSON。キー user / password / database) の投入と、R2 バケット `fuel-inbox` の作成が要る
(未投入でも deploy はでき、health は 503 `stage=secret` を返す)。
