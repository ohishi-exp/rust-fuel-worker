#!/usr/bin/env bash
# check-exposure.sh の陰性対照。wrangler.toml を tomllib で読んだ dict を 1 か所ずつ崩して TOML に書き戻し、
# (a)〜(k) それぞれで exit 1 になること、元のまま・書き戻しただけなら exit 0 になることを確かめる。
# 4 つ目の引数を渡した行は、出力にその文字列 (どの検査が落としたか) があることも確かめる。
# 文字列の特定の表の直前に行を挿す作りにはしない (表の中身に紛れて別の表のキーになる — rust-alc-api#698)。
# CI で check-exposure.sh の直後に走る。
#
#   bash scripts/check-exposure-test.sh [wrangler.toml]   (既定 worker/wrangler.toml)
set -euo pipefail
cd "$(dirname "$0")/.."

BASE="${1:-worker/wrangler.toml}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
fail=0

expect() { # $1 = 期待する exit, $2 = label, $3 = wrangler.toml, $4 = 出力に在るべき文字列 (任意)
  local want="$1" label="$2" got
  if bash scripts/check-exposure.sh "$3" >"$tmp/out" 2>&1; then got=0; else got=1; fi
  if [ "$got" != "$want" ]; then
    echo "FAIL ${label}: exit ${got}, want ${want}"; sed 's/^/     /' "$tmp/out"; fail=1
  elif [ -n "${4:-}" ] && ! grep -qF -- "$4" "$tmp/out"; then
    echo "FAIL ${label}: 出力に「$4」が無い"; sed 's/^/     /' "$tmp/out"; fail=1
  else
    echo "ok   ${label} (exit ${got})"
  fi
}

# $1 = 期待する exit, $2 = label, $3 = python の式 (cfg を書き換える。空なら書き戻すだけ), $4 = expect の $4
mutate() {
  python3 - "$BASE" "$tmp/w.toml" "$3" <<'PY'
import copy
import json
import re
import sys
import tomllib

BARE = re.compile(r"^[A-Za-z0-9_-]+$")


def key(k):
    return k if BARE.match(k) else json.dumps(k, ensure_ascii=False)


def value(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return repr(v)
    if isinstance(v, str):
        return json.dumps(v, ensure_ascii=False)
    if isinstance(v, list):
        return "[" + ", ".join(value(x) for x in v) + "]"
    if isinstance(v, dict):
        return "{ " + ", ".join(f"{key(k)} = {value(x)}" for k, x in v.items()) + " }"
    raise TypeError(type(v))


def is_table_array(v):
    return isinstance(v, list) and v and all(isinstance(x, dict) for x in v)


def dump(d, prefix=()):
    out = []
    for k, v in d.items():
        if not isinstance(v, dict) and not is_table_array(v):
            out.append(f"{key(k)} = {value(v)}")
    for k, v in d.items():
        name = ".".join(key(p) for p in prefix + (k,))
        if isinstance(v, dict):
            out.append(f"\n[{name}]")
            out.append(dump(v, prefix + (k,)))
        elif is_table_array(v):
            for item in v:
                out.append(f"\n[[{name}]]")
                out.append(dump(item, prefix + (k,)))
    return "\n".join(x for x in out if x != "")


with open(sys.argv[1], "rb") as f:
    cfg = tomllib.load(f)
before = copy.deepcopy(cfg)
if sys.argv[3]:
    exec(sys.argv[3])
    assert cfg != before, "mutation did not change the config"
text = dump(cfg) + "\n"
# 書き戻しが正しいこと: 読み直すと意図した dict になる
assert tomllib.loads(text) == cfg, "round trip changed the config"
open(sys.argv[2], "w").write(text)
PY
  expect "$1" "$2" "$tmp/w.toml" "${4:-}"
}

expect 0 "${BASE} そのまま" "$BASE"
mutate 0 "書き戻しただけ (崩していない)" ''
# (a)
mutate 1 "(a) workers_dev = true" 'cfg["workers_dev"] = True'
mutate 1 "(a) workers_dev を消す (明示でない)" 'del cfg["workers_dev"]'
mutate 1 "(a) preview_urls = true" 'cfg["preview_urls"] = True'
mutate 1 "(a) preview_urls を消す (明示でない)" 'del cfg["preview_urls"]'
# (b)
mutate 1 "(b) route を足す" 'cfg["route"] = "fuel.example.com/*"'
mutate 1 "(b) routes (custom_domain) を足す" \
  'cfg["routes"] = [{"pattern": "fuel.example.com", "custom_domain": True}]'
# (c)
mutate 1 "(c) env 表を足す" 'cfg["env"] = {"staging": {"name": "rust-fuel-worker-staging", "workers_dev": False, "preview_urls": False}}'
# (d)
mutate 1 "(d) vpc_services を消す" 'del cfg["vpc_services"]' "vpc_services の KINTAI_MARIADB_VPC が無い"
mutate 1 "(d) KINTAI_MARIADB_VPC の binding 名を変える" 'cfg["vpc_services"][0]["binding"] = "OTHER_VPC"' \
  "vpc_services の KINTAI_MARIADB_VPC が無い"
mutate 1 "(d) vpc_services を env 側へ動かす" \
  'cfg["env"] = {"prod": {"vpc_services": cfg.pop("vpc_services")}}' "env.prod.vpc_services がある"
mutate 1 "(d) トップレベルに残したまま env にも置く" \
  'cfg["env"] = {"prod": {"vpc_services": [dict(cfg["vpc_services"][0])]}}' "env.prod.vpc_services がある"
mutate 1 "(d) env 以外の表の奥に vpc_services" 'cfg["observability"]["vpc_services"] = [dict(cfg["vpc_services"][0])]' \
  "observability.vpc_services がある"
# (e) LOCAL_ で始まる var はどこに置いても落ちる
mutate 1 "(e) vars に LOCAL_SQL_ADDR" 'cfg.setdefault("vars", {})["LOCAL_SQL_ADDR"] = "127.0.0.1:3306"' "LOCAL_SQL_ADDR がある"
mutate 1 "(e) vars に LOCAL_ で始まる任意の名前" 'cfg.setdefault("vars", {})["LOCAL_X"] = "1"' "LOCAL_X がある"
mutate 1 "(e) 表の配列の奥に LOCAL_" 'cfg["vpc_services"][0]["LOCAL_ADDR"] = "127.0.0.1:3306"' "LOCAL_ADDR がある"
# LOCAL_ で始まらないもの (前後の一致ではなく接頭辞の一致であること) は通る
mutate 0 "(e) vars に NOT_LOCAL_X (接頭辞でない)" 'cfg.setdefault("vars", {})["NOT_LOCAL_X"] = "1"'
# (f) は warning だけ: service_id がプレースホルダのままでも exit 0
mutate 0 "(f) service_id がプレースホルダ (warning のみ)" \
  'cfg["vpc_services"][0]["service_id"] = "00000000-0000-0000-0000-000000000000"' "プレースホルダのまま"
# (g) 資格情報の secret
kintai='next(s for s in cfg["secrets_store_secrets"] if s["binding"] == "KINTAI_MARIADB")'
write='next(s for s in cfg["secrets_store_secrets"] if s["binding"] == "FUEL_MARIADB_WRITE")'
mutate 1 "(g) secrets_store_secrets を消す" 'del cfg["secrets_store_secrets"]' "secrets_store_secrets の KINTAI_MARIADB が無い"
mutate 1 "(g) KINTAI_MARIADB を消す (読み取りが無い)" \
  'cfg["secrets_store_secrets"] = [s for s in cfg["secrets_store_secrets"] if s["binding"] != "KINTAI_MARIADB"]' \
  "secrets_store_secrets の KINTAI_MARIADB が無い"
mutate 1 "(g) FUEL_MARIADB_WRITE を消す (書き込みが無い)" \
  'cfg["secrets_store_secrets"] = [s for s in cfg["secrets_store_secrets"] if s["binding"] != "FUEL_MARIADB_WRITE"]' \
  "secrets_store_secrets の FUEL_MARIADB_WRITE が無い"
mutate 1 "(g) KINTAI_MARIADB の binding 名を変える" "${kintai}[\"binding\"] = \"OTHER\"" \
  "secrets_store_secrets の KINTAI_MARIADB が無い"
mutate 1 "(g) FUEL_MARIADB_WRITE の binding 名を変える" "${write}[\"binding\"] = \"OTHER\"" \
  "secrets_store_secrets の FUEL_MARIADB_WRITE が無い"
mutate 1 "(g) KINTAI_MARIADB の secret_name をずらす" "${kintai}[\"secret_name\"] = \"FUEL_MARIADB_WRITE\"" \
  "KINTAI_MARIADB の secret_name が KINTAI_MARIADB でない"
mutate 1 "(g) FUEL_MARIADB_WRITE の secret_name をずらす" "${write}[\"secret_name\"] = \"KINTAI_MARIADB\"" \
  "FUEL_MARIADB_WRITE の secret_name が FUEL_MARIADB_WRITE でない"
mutate 1 "(g) 想定外の secret (旧名) を足す" \
  'cfg["secrets_store_secrets"].append({"binding": "FUEL_MARIADB_OLD", "store_id": "x", "secret_name": "FUEL_MARIADB_OLD"})' \
  "想定外の binding FUEL_MARIADB_OLD がある"
mutate 1 "(g) secrets_store_secrets を env へ動かす" \
  'cfg["env"] = {"prod": {"secrets_store_secrets": cfg.pop("secrets_store_secrets")}}' "env.prod.secrets_store_secrets がある"
mutate 1 "(g) トップレベルに残したまま env にも置く" \
  'cfg["env"] = {"prod": {"secrets_store_secrets": [dict(cfg["secrets_store_secrets"][0])]}}' "env.prod.secrets_store_secrets がある"
# (h) r2_buckets はトップレベルにだけ (トップレベルに在る分は通る)
mutate 1 "(h) r2_buckets を env へ置く" \
  'cfg["env"] = {"prod": {"r2_buckets": [dict(cfg["r2_buckets"][0])]}}' "env.prod.r2_buckets がある"
mutate 1 "(h) env 以外の表の奥に r2_buckets" 'cfg["observability"]["r2_buckets"] = [dict(cfg["r2_buckets"][0])]' \
  "observability.r2_buckets がある"
# (i) 認可の tenant は空で置く
mutate 1 "(i) FUEL_TENANT_ID に UUID を書く" \
  'cfg["vars"]["FUEL_TENANT_ID"] = "-".join(["0" * 8, "0000", "4000", "8000", "0" * 8 + "abcd"])' "FUEL_TENANT_ID が空でない"
mutate 1 "(i) FUEL_TENANT_ID を消す" 'del cfg["vars"]["FUEL_TENANT_ID"]' "FUEL_TENANT_ID が無い"
mutate 1 "(i) vars 表を消す" 'del cfg["vars"]' "FUEL_TENANT_ID が無い"
# (j) services はトップレベルの AUTH_MCRED (entrypoint 付き) だけ
mcred='next(s for s in cfg["services"] if s["binding"] == "AUTH_MCRED")'
mutate 1 "(j) services を消す" 'del cfg["services"]' "services の AUTH_MCRED"
mutate 1 "(j) AUTH_MCRED の entrypoint を消す" "del ${mcred}[\"entrypoint\"]" "services の AUTH_MCRED"
mutate 1 "(j) AUTH_MCRED の entrypoint を変える" "${mcred}[\"entrypoint\"] = \"InternalEntrypoint\"" "services の AUTH_MCRED"
mutate 1 "(j) AUTH_MCRED の service を変える" "${mcred}[\"service\"] = \"other-worker\"" "services の AUTH_MCRED"
mutate 1 "(j) entrypoint の無い services を足す" \
  'cfg["services"].append({"binding": "AUTH_WORKER", "service": "auth-worker"})' "services に AUTH_WORKER がある"
mutate 1 "(j) services を env へ置く" \
  'cfg["env"] = {"prod": {"services": [dict(cfg["services"][0])]}}' "env.prod.services がある"
mutate 1 "(j) env 以外の表の奥に services" 'cfg["observability"]["services"] = [dict(cfg["services"][0])]' \
  "observability.services がある"
# (k) USB の口の共有 secret
shared='next(s for s in cfg["secrets_store_secrets"] if s["binding"] == "INTERNAL_SHARED_SECRET")'
mutate 1 "(k) INTERNAL_SHARED_SECRET を消す" \
  'cfg["secrets_store_secrets"] = [s for s in cfg["secrets_store_secrets"] if s["binding"] != "INTERNAL_SHARED_SECRET"]' \
  "secrets_store_secrets の INTERNAL_SHARED_SECRET が無い"
mutate 1 "(k) INTERNAL_SHARED_SECRET の secret_name を変える" "${shared}[\"secret_name\"] = \"OTHER\"" \
  "secret_name が INTERNAL_SHARED_SECRET でない"

if bash scripts/check-exposure.sh "$tmp/none.toml" >/dev/null 2>&1; then
  echo "FAIL 存在しない toml: exit 0, want 非 0"; fail=1
else
  echo "ok   存在しない toml (非 0)"
fi

exit "$fail"
