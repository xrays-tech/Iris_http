#!/usr/bin/env bash
#
# P8 reload / metrics / lifecycle end-to-end test (plan/11 §端到端, reload.md).
#
# Boots the upstream stub, scripts/mock_openstack.py and `iris` against a
# throwaway config under target/e2e-p8, then exercises the SIGHUP hot-reload
# path via `iris reload --pidfile`, asserting:
#
#   * a base forward site (`alpha.test`) and an openstack site
#     (`web-<port>.rent.do.top`) route with 200;
#   * `/healthz` is `ok` and `/metrics` is reachable and carries
#     `config_reload_total`;
#   * config B (adds forward site `beta.test` + edits the openstack site) →
#     reload → both new and old sites route, `/version` fingerprint CHANGED,
#     `config_reload_total{ok="true"} 1`;
#   * config C (an invalid YAML site file) → reload → the old config keeps
#     serving (fingerprint UNCHANGED, previously-working route still 200),
#     `config_reload_total{ok="false"} 1`;
#   * config D (removes `alpha.test`) → reload → its host now 404, `beta.test`
#     still 200.
#
# Re-run from the repository root:
#     scripts/e2e/08-reload.sh
#
# Ports are fixed (baked into nothing committed — every fixture is generated
# here so the test is hermetic):
#   upstream stub 18201 · control-plane mock 18202 · HTTP 18211 · TLS 18212 ·
#   metrics/version 18291.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

UP=18201
OS_PORT=18202
HTTP_PORT=18211
TLS_PORT=18212
METRICS_PORT=18291
BASE="rent.do.top"

WORK="target/e2e-p8"
CFG_DIR="$WORK/config"
SITES_DIR="$CFG_DIR/sites"
ENVS_DIR="$CFG_DIR/openstack"
FW_DIR="$WORK/firewall"
SERVERS_DIR="$WORK/servers"
LOG_DIR="$WORK/logs"
PIDFILE="$WORK/iris.pid"
RECORD="$WORK/os-record.jsonl"

rm -rf "$WORK"
mkdir -p "$SITES_DIR" "$ENVS_DIR" "$FW_DIR" "$SERVERS_DIR" "$LOG_DIR"

echo "[build] cargo build --bin iris"
cargo build --bin iris --quiet
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
IRIS_BIN="$TARGET_DIR/debug/iris"
echo "[build] binary: $IRIS_BIN"

STUB_PID=""
IRIS_PID=""
OS_PID=""

cleanup() {
  if [ -n "$IRIS_PID" ]; then kill "$IRIS_PID" 2>/dev/null || true; fi
  if [ -n "$OS_PID" ]; then kill "$OS_PID" 2>/dev/null || true; fi
  if [ -n "$STUB_PID" ]; then kill "$STUB_PID" 2>/dev/null || true; fi
}
trap cleanup EXIT

# --- hermetic fixtures ------------------------------------------------------

write_forward_site() { # $1=file stem  $2=site name  $3=domain
  cat > "$SITES_DIR/$1.yaml" <<YAML
name: $2
mode: forward
domain: $3
http: true
https: false
backends:
  - addr: 127.0.0.1:$UP
YAML
}

write_openstack_site() { # $1=cache_ttl_secs
  cat > "$ENVS_DIR/prod.yaml" <<YAML
timeout_secs: 5
cache_ttl_secs: $1
auth:
  api_url: http://127.0.0.1:$OS_PORT/v3
  approach: application_credential
  application_credential_id: e2e-id
  application_credential_secret: e2e-secret
nova: {}
YAML
  cat > "$SITES_DIR/002-openstack.yaml" <<YAML
name: rent
mode: openstack
base_domain: $BASE
http: true
https: false
firewall_dir: $FW_DIR
openstack_env: prod
YAML
}

# Nova reply for instance `web`: floating 127.0.0.1 (the upstream stub).
cat > "$SERVERS_DIR/web.json" <<'JSON'
{
  "servers": [
    {
      "id": "11111111-1111-1111-1111-111111111111",
      "name": "web",
      "addresses": {
        "net-a": [
          { "version": 4, "addr": "127.0.0.1", "OS-EXT-IPS:type": "floating" }
        ]
      }
    }
  ]
}
JSON

# `web` may reach the upstream stub's port only.
cat > "$FW_DIR/web.role" <<YAML
allowed_ports:
  - $UP
YAML

# --- config set A (baseline) ------------------------------------------------
write_forward_site 001-forward-alpha alpha alpha.test
write_openstack_site 86400

# --- helpers ----------------------------------------------------------------

status_for() { # $1=Host
  curl -s -o /dev/null -w '%{http_code}' --max-time 20 -H "Host: $1" \
    "http://127.0.0.1:$HTTP_PORT/" 2>/dev/null || true
}

assert_code() { # $1=label $2=expect $3=Host
  local code
  code="$(status_for "$3")"
  printf '    %-54s %s\n' "$1" "HTTP $code"
  if [ "$code" != "$2" ]; then
    echo "ASSERT FAIL: $1 — expected $2, got $code"
    exit 1
  fi
}

metrics() { curl -s --max-time 5 "http://127.0.0.1:$METRICS_PORT/metrics" 2>/dev/null || true; }

wait_for_metric() { # $1=exact line  $2=label
  local tries="${3:-50}"
  for _ in $(seq 1 "$tries"); do
    if metrics | grep -qF "$1"; then
      printf '    %-54s %s\n' "$2" "observed"
      return 0
    fi
    sleep 0.2
  done
  echo "ASSERT FAIL: metric '$1' never appeared ($2)"
  echo "---- /metrics dump ----"
  metrics
  echo "-----------------------"
  exit 1
}

version_fp() {
  curl -s --max-time 5 "http://127.0.0.1:$METRICS_PORT/version" 2>/dev/null \
    | sed -n 's/.*config_hash=//p' | tr -d '[:space:]'
}

reload() { # SIGHUP via the CLI against the running pidfile
  echo "    \$ iris reload --pidfile $PIDFILE"
  "$IRIS_BIN" reload --pidfile "$PIDFILE"
}

# --- start upstream + control plane ----------------------------------------
echo "[stub] starting upstream on $UP"
python3 tests/stubs/upstream.py --port "$UP" > "$LOG_DIR/stub.log" 2>&1 &
STUB_PID=$!

echo "[mock] starting control-plane mock on $OS_PORT"
python3 scripts/mock_openstack.py --port "$OS_PORT" \
  --servers-dir "$SERVERS_DIR" --record "$RECORD" > "$LOG_DIR/mock.log" 2>&1 &
OS_PID=$!
for _ in $(seq 1 50); do
  curl -s -o /dev/null --max-time 1 "http://127.0.0.1:$OS_PORT/v2.1/servers?name=__probe__" 2>/dev/null && break
  sleep 0.1
done

# --- start iris (config A) --------------------------------------------------
echo "[iris] http=127.0.0.1:$HTTP_PORT https=127.0.0.1:$TLS_PORT metrics=127.0.0.1:$METRICS_PORT"
IRIS_HTTP_LISTEN_ADDR="127.0.0.1:$HTTP_PORT" \
  IRIS_HTTP_LISTEN_TLS_ADDR="127.0.0.1:$TLS_PORT" \
  "$IRIS_BIN" run --config-dir "$CFG_DIR" --pidfile "$PIDFILE" \
  --metrics-listen "127.0.0.1:$METRICS_PORT" > "$LOG_DIR/iris.log" 2>&1 &
IRIS_PID=$!

ready=0
for _ in $(seq 1 60); do
  code="$(status_for alpha.test)"
  if [ -n "$code" ] && [ "$code" != "000" ]; then ready=1; break; fi
  sleep 0.2
done
if [ "$ready" != "1" ]; then
  echo "FAIL: iris did not become ready; log follows"
  cat "$LOG_DIR/iris.log"
  exit 1
fi

echo
echo "=== phase A: baseline routing (config A) ==="
assert_code "forward alpha.test → 200" "200" "alpha.test"
assert_code "openstack web-${UP}.${BASE} → 200" "200" "web-${UP}.${BASE}"

echo
echo "=== phase A: health / metrics / version endpoints ==="
hz="$(curl -s --max-time 5 "http://127.0.0.1:$METRICS_PORT/healthz" | tr -d '\n')"
printf '    %-54s %s\n' "GET /healthz" "$hz"
if [ "$hz" != "ok" ]; then echo "ASSERT FAIL: /healthz expected 'ok', got '$hz'"; exit 1; fi

if ! metrics | grep -qF "config_reload_total"; then
  echo "ASSERT FAIL: /metrics does not expose config_reload_total"
  exit 1
fi
printf '    %-54s %s\n' "GET /metrics exposes config_reload_total" "yes"
unknown="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "http://127.0.0.1:$METRICS_PORT/nope")"
printf '    %-54s %s\n' "GET /nope" "HTTP $unknown"
if [ "$unknown" != "404" ]; then echo "ASSERT FAIL: expected 404 for unknown path"; exit 1; fi

fp_a="$(version_fp)"
printf '    %-54s %s\n' "fingerprint A" "$fp_a"

echo
echo "=== phase B: reload config B (add beta.test, edit openstack site) ==="
write_forward_site 003-forward-beta beta beta.test
write_openstack_site 43200
reload
wait_for_metric 'config_reload_total{ok="true"} 1' "config_reload_total{ok=\"true\"} 1"

assert_code "new forward beta.test → 200" "200" "beta.test"
assert_code "existing forward alpha.test → 200" "200" "alpha.test"
assert_code "openstack web-${UP}.${BASE} still → 200" "200" "web-${UP}.${BASE}"

fp_b="$(version_fp)"
printf '    %-54s %s\n' "fingerprint B" "$fp_b"
if [ -z "$fp_b" ] || [ "$fp_b" = "$fp_a" ]; then
  echo "ASSERT FAIL: fingerprint must change after a successful reload ($fp_a → $fp_b)"
  exit 1
fi
printf '    %-54s %s\n' "fingerprint changed A→B" "yes"

echo
echo "=== phase C: reload a broken config (invalid YAML) — keeps old ==="
cat > "$SITES_DIR/999-bad.yaml" <<'YAML'
name: [this is not valid yaml
YAML
reload
wait_for_metric 'config_reload_total{ok="false"} 1' "config_reload_total{ok=\"false\"} 1"

fp_c="$(version_fp)"
printf '    %-54s %s\n' "fingerprint C" "$fp_c"
if [ "$fp_c" != "$fp_b" ]; then
  echo "ASSERT FAIL: a rejected reload must not change the fingerprint ($fp_b → $fp_c)"
  exit 1
fi
printf '    %-54s %s\n' "fingerprint unchanged B==C" "yes"
assert_code "previously-working beta.test still → 200" "200" "beta.test"
if ! metrics | grep -qF 'config_reload_total{ok="true"} 1'; then
  echo "ASSERT FAIL: ok=true counter must remain 1 after the rejected reload"
  exit 1
fi
printf '    %-54s %s\n' "ok=\"true\" counter still 1" "yes"

echo
echo "=== phase D: reload config D (remove alpha.test) ==="
rm -f "$SITES_DIR/999-bad.yaml" "$SITES_DIR/001-forward-alpha.yaml"
reload
wait_for_metric 'config_reload_total{ok="true"} 2' "config_reload_total{ok=\"true\"} 2"

assert_code "removed alpha.test → 404" "404" "alpha.test"
assert_code "kept beta.test → 200" "200" "beta.test"

fp_d="$(version_fp)"
printf '    %-54s %s\n' "fingerprint D" "$fp_d"
if [ "$fp_d" = "$fp_b" ]; then
  echo "ASSERT FAIL: removing a site must change the fingerprint"
  exit 1
fi
printf '    %-54s %s\n' "fingerprint changed B→D" "yes"

echo
echo "=== /version body ==="
curl -s --max-time 5 "http://127.0.0.1:$METRICS_PORT/version"
echo "=== /metrics (iris counters) ==="
metrics | grep -E '^(config_reload_total|firewall_denied_total)' || true

echo
echo "ALL P8 RELOAD / METRICS / LIFECYCLE E2E ASSERTIONS PASSED"
