#!/usr/bin/env bash
# fuel Worker (rust-fuel-worker) が外から届かないこと・社内への口を本番の外へ漏らさないことを wrangler.toml で検査する
# (rust-ichibanboshi の workers/kintai/scripts/check-exposure.sh を写している)。
#   (a) トップレベルに workers_dev = false と preview_urls = false が **明示** されている
#   (b) route / routes が無い (custom_domain も routes の中に書く)
#   (c) env 表が無い (トップレベルだけで運用する。env を足すと上の検査を env ごとにやり直す必要がある)
#   (d) トップレベルに vpc_services の KINTAI_MARIADB_VPC (MariaDB への口) があり、vpc_services がトップレベル以外
#       (env.* を含む表の奥) のどこにも無い
#   (e) wrangler.toml のどこにも `LOCAL_` で始まる var (キー) が無い (ローカルで VPC や Secrets Store を迂回する口を作らない)
#   (f) vpc_services の service_id がプレースホルダのままなら warning (fail にはしない)
#   (g) トップレベルの secrets_store_secrets に KINTAI_MARIADB (読み取り。勤怠 Worker と共用) と FUEL_MARIADB_WRITE
#       (書き込み) が 1 本ずつ (secret_name も同名) あり、それ以外は INTERNAL_SHARED_SECRET だけ。secrets_store_secrets が
#       トップレベル以外のどこにも無い
#   (h) r2_buckets がトップレベル以外のどこにも無い (R2 への口をトップレベルの外へ置かない)
#   (i) トップレベルの [vars] に FUEL_TENANT_ID があり、値が空 (本番の tenant の UUID は deploy.yml が --var で渡す。
#       git に焼かない)
#   (j) トップレベルの services は AUTH_MCRED (service = auth-worker・entrypoint = MachineCredentialEntrypoint) と
#       AUTH_WORKER (service = auth-worker・entrypoint なし。inform の LINE WORKS 送信が内部用 proxy を叩く) の 2 本ちょうどで、
#       services がトップレベル以外のどこにも無い (ほかの worker・ほかの entrypoint への binding は置かない)
#   (k) トップレベルの secrets_store_secrets に INTERNAL_SHARED_SECRET (secret_name も同名) がある (USB の口の中継の確認)
#   (l) トップレベルの [vars] に FUEL_INFORM_TARGET があり、値が空 (inform の宛先の UUID は git に書かない)
# 1 つでも違えば exit 1。CI で毎回走らせる。陰性対照は scripts/check-exposure-test.sh。
#
#   bash scripts/check-exposure.sh [wrangler.toml]   (既定 worker/wrangler.toml)
set -euo pipefail

if [ "$#" -gt 1 ]; then
  echo "usage: $0 [wrangler.toml]" >&2
  exit 2
fi
target="${1:-$(dirname "$0")/../worker/wrangler.toml}"

python3 - "$target" <<'PY'
import sys
import tomllib

PLACEHOLDER = "00000000-0000-0000-0000-000000000000"
path = sys.argv[1]
with open(path, "rb") as f:
    cfg = tomllib.load(f)

errors = []


def err(msg):
    errors.append(msg)
    print(f"::error file={path}::{msg}")


def nested(node, key, prefix=""):
    """トップレベル以外 (表・表の配列の奥) にある key を探す。"""
    if isinstance(node, dict):
        for k, v in node.items():
            name = f"{prefix}.{k}" if prefix else k
            if k == key and prefix:
                yield name
            yield from nested(v, key, name)
    elif isinstance(node, list):
        for i, v in enumerate(node):
            yield from nested(v, key, f"{prefix}[{i}]")


def local_keys(node, prefix=""):
    if isinstance(node, dict):
        for k, v in node.items():
            name = f"{prefix}.{k}" if prefix else k
            if k.startswith("LOCAL_"):
                yield name
            yield from local_keys(v, name)
    elif isinstance(node, list):
        for i, v in enumerate(node):
            yield from local_keys(v, f"{prefix}[{i}]")


# (a) トップレベル: 明示的に false (省略は既定値に頼ることになるので許さない)
for key in ("workers_dev", "preview_urls"):
    if cfg.get(key) is not False:
        err(f"トップレベルに {key} = false がない (外から直接届く)")

# (b) route / routes
for key in ("route", "routes"):
    if key in cfg:
        err(f"トップレベルに {key} がある (外から直接届く)")

# (c) env 表
if "env" in cfg:
    err("env 表がある (この Worker はトップレベルだけで運用する)")

# (d) MariaDB への口 KINTAI_MARIADB_VPC はトップレベル。vpc_services の表はトップレベル以外に置かない
vpcs = cfg.get("vpc_services")
if not isinstance(vpcs, list) or not any(isinstance(v, dict) and v.get("binding") == "KINTAI_MARIADB_VPC" for v in vpcs):
    err("トップレベルに vpc_services の KINTAI_MARIADB_VPC が無い (MariaDB への口)")
for name in nested(cfg, "vpc_services"):
    err(f"{name} がある (vpc_services はトップレベルにだけ置く。env 等へ置くと社内への口が漏れる)")

# (e) LOCAL_ で始まるキー
for name in local_keys(cfg):
    err(f"{name} がある (LOCAL_ で始まる var は作らない。ローカルで VPC / Secrets Store を迂回する口になる)")

# (f) service_id のプレースホルダ (warning のみ)
for v in vpcs or []:
    if isinstance(v, dict) and v.get("service_id") == PLACEHOLDER:
        print(f"::warning file={path}::vpc_services {v.get('binding')} の service_id がプレースホルダのまま (VPC Service 作成後に入れる)")

# (g) 資格情報の secret は読み取り (KINTAI_MARIADB) と書き込み (FUEL_MARIADB_WRITE) の 2 本だけ (+ INTERNAL_SHARED_SECRET)
secrets = cfg.get("secrets_store_secrets")
secrets_list = secrets if isinstance(secrets, list) else []
for want in ("KINTAI_MARIADB", "FUEL_MARIADB_WRITE"):
    found = [s for s in secrets_list if isinstance(s, dict) and s.get("binding") == want]
    if len(found) != 1:
        err(f"トップレベルに secrets_store_secrets の {want} が無い (または複数ある。MariaDB の資格情報)")
    elif found[0].get("secret_name") != want:
        err(f"{want} の secret_name が {want} でない")
for s in secrets_list:
    if isinstance(s, dict) and s.get("binding") not in ("KINTAI_MARIADB", "FUEL_MARIADB_WRITE", "INTERNAL_SHARED_SECRET"):
        err(f"secrets_store_secrets に想定外の binding {s.get('binding')} がある (KINTAI_MARIADB・FUEL_MARIADB_WRITE・INTERNAL_SHARED_SECRET だけ)")
for name in nested(cfg, "secrets_store_secrets"):
    err(f"{name} がある (secrets_store_secrets はトップレベルにだけ置く)")

# (h) r2_buckets はトップレベルにだけ
for name in nested(cfg, "r2_buckets"):
    err(f"{name} がある (r2_buckets はトップレベルにだけ置く)")

# (i) 認可の tenant は空で置く (値は deploy 時に --var で渡す)
vars_ = cfg.get("vars")
if not isinstance(vars_, dict) or "FUEL_TENANT_ID" not in vars_:
    err("トップレベルの [vars] に FUEL_TENANT_ID が無い (認可で突き合わせる tenant)")
elif vars_["FUEL_TENANT_ID"] != "":
    err("[vars] の FUEL_TENANT_ID が空でない (tenant の UUID は git に書かず deploy.yml の --var で渡す)")

# (j) services はトップレベルの AUTH_MCRED (entrypoint 付き) と AUTH_WORKER (entrypoint なし) の 2 本だけ
MCRED = {"binding": "AUTH_MCRED", "service": "auth-worker", "entrypoint": "MachineCredentialEntrypoint"}
AUTH_WORKER = {"binding": "AUTH_WORKER", "service": "auth-worker"}
services = cfg.get("services")
if services is not None and not isinstance(services, list):
    err("services が表の配列でない")
    services = []
services = services or []
if not any(s == MCRED for s in services):
    err("トップレベルに services の AUTH_MCRED (auth-worker / entrypoint MachineCredentialEntrypoint) が無い")
if sum(1 for s in services if s == AUTH_WORKER) != 1:
    err("トップレベルに services の AUTH_WORKER (auth-worker / entrypoint なし) が 1 本ちょうど無い")
for s in services:
    if s != MCRED and s != AUTH_WORKER:
        err(f"services に {s.get('binding') if isinstance(s, dict) else s} がある (置けるのは AUTH_MCRED の entrypoint 付きと AUTH_WORKER だけ)")
for name in nested(cfg, "services"):
    err(f"{name} がある (services はトップレベルにだけ置く)")

# (k) USB の口の共有 secret
shared = [s for s in secrets if isinstance(s, dict) and s.get("binding") == "INTERNAL_SHARED_SECRET"] if isinstance(secrets, list) else []
if not shared:
    err("トップレベルに secrets_store_secrets の INTERNAL_SHARED_SECRET が無い (USB の口の中継の確認)")
elif shared[0].get("secret_name") != "INTERNAL_SHARED_SECRET":
    err("INTERNAL_SHARED_SECRET の secret_name が INTERNAL_SHARED_SECRET でない")

# (l) inform の宛先は空で置く
if not isinstance(vars_, dict) or "FUEL_INFORM_TARGET" not in vars_:
    err("トップレベルの [vars] に FUEL_INFORM_TARGET が無い (inform の LINE WORKS の宛先)")
elif vars_["FUEL_INFORM_TARGET"] != "":
    err("[vars] の FUEL_INFORM_TARGET が空でない (宛先の UUID は git に書かない)")

if errors:
    sys.exit(1)
print(f"OK: {path} は workers_dev / preview_urls = false・route 無し・env 無し・vpc_services / secrets_store_secrets / r2_buckets / services はトップレベル・services は AUTH_MCRED (entrypoint 付き) と AUTH_WORKER だけ・LOCAL_* 無し・FUEL_TENANT_ID と FUEL_INFORM_TARGET は空")
PY
