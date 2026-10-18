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
#   (g) トップレベルの secrets_store_secrets に FUEL_MARIADB (secret_name も同名) があり、secrets_store_secrets が
#       トップレベル以外のどこにも無い
#   (h) r2_buckets がトップレベル以外のどこにも無い (R2 への口をトップレベルの外へ置かない)
#   (i) トップレベルの [vars] に FUEL_TENANT_ID があり、値が空 (本番の tenant の UUID は deploy.yml が --var で渡す。
#       git に焼かない)
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

# (g) 資格情報の secret FUEL_MARIADB はトップレベル
secrets = cfg.get("secrets_store_secrets")
fuel = [s for s in secrets if isinstance(s, dict) and s.get("binding") == "FUEL_MARIADB"] if isinstance(secrets, list) else []
if not fuel:
    err("トップレベルに secrets_store_secrets の FUEL_MARIADB が無い (MariaDB の資格情報)")
elif fuel[0].get("secret_name") != "FUEL_MARIADB":
    err("FUEL_MARIADB の secret_name が FUEL_MARIADB でない")
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

if errors:
    sys.exit(1)
print(f"OK: {path} は workers_dev / preview_urls = false・route 無し・env 無し・vpc_services / secrets_store_secrets / r2_buckets はトップレベル・LOCAL_* 無し・FUEL_TENANT_ID は空")
PY
