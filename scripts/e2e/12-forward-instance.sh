#!/usr/bin/env bash
#
# C3 — forward sites whose backends reference OpenStack instances by name/UUID
# (plan/14 §C3). Exercises the whole new path over the HTTP listener with curl:
#
#   * two instance backends weighted 2:1 -> X-Upstream-Port distribution,
#   * name + UUID mixed on one site (asserted from the mock control-plane record),
#   * a name backend whose Nova fixture is deleted / never resolves -> 502
#     (the site has NO health_check, so the failure surfaces at request time),
#   * single-flight + cache: many concurrent requests to an unresolvable
#     instance collapse to exactly one Nova lookup, and a warm positive
#     resolution is never re-fetched,
#   * one Keystone token reused across every lookup,
#   * an instance backend removed by hot reload stops being selected,
#   * the instance health probe (tcp) drops a dead backend and re-admits it
#     once it recovers.
#
# Re-run from the repository root:
#     scripts/e2e/12-forward-instance.sh
#
# Ports are fixed and hermetic (everything lives under target/e2e-p12):
#   upstream A 18401 · upstream B 18402 · control-plane mock 18403
#   HTTP 18411 · metrics 18412.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

UP_A=18401
UP_B=18402
OS_PORT=18403
HTTP_PORT=18411
METRICS_PORT=18412

UUID="22222222-2222-2222-2222-222222222222"
BASE="apps.test"
SOLO="solo.test"

WORK="target/e2e-p12"
CFG_DIR="$WORK/config"
SITES_DIR="$CFG_DIR/sites"
ENVS_DIR="$CFG_DIR/openstack"
SERVERS_DIR="$WORK/servers"
LOG_DIR="$WORK/logs"
PIDFILE="$WORK/iris.pid"
RECORD="$WORK/os-record.jsonl"

rm -rf "$WORK"
mkdir -p "$SITES_DIR" "$ENVS_DIR" "$SERVERS_DIR" "$LOG_DIR"

echo "[build] cargo build --bin iris"
cargo build --bin iris --quiet
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
IRIS_BIN="$TARGET_DIR/debug/iris"
echo "[build] binary: $IRIS_BIN"

STUB_A_PID=""
STUB_B_PID=""
OS_PID=""
IRIS_PID=""

cleanup() {
  if [ -n "$IRIS_PID" ]; then kill "$IRIS_PID" 2>/dev/null || true; fi
  if [ -n "$STUB_B_PID" ]; then kill "$STUB_B_PID" 2>/dev/null || true; fi
  if [ -n "$STUB_A_PID" ]; then kill "$STUB_A_PID" 2>/dev/null || true; fi
  if [ -n "$OS_PID" ]; then kill "$OS_PID" 2>/dev/null || true; fi
}
trap cleanup EXIT

# --- hermetic fixtures ------------------------------------------------------

# Control-plane environment: points Keystone at the mock; the resolver's cache
# TTL is the default large value so a warm resolution is never re-fetched.
cat > "$ENVS_DIR/prod.yaml" <<YAML
timeout_secs: 5
cache_ttl_secs: 86400
auth:
  api_url: http://127.0.0.1:$OS_PORT/v3
  approach: application_credential
  application_credential_id: e2e-id
  application_credential_secret: e2e-secret
nova: {}
YAML

# Nova name fixture: `appA` resolves to the loopback floating address.
cat > "$SERVERS_DIR/appA.json" <<JSON
{
  "servers": [
    {
      "id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
      "name": "appA",
      "addresses": {
        "net-a": [
          { "version": 4, "addr": "127.0.0.1", "OS-EXT-IPS:type": "floating" }
        ]
      }
    }
  ]
}
JSON

# Nova show-by-id fixture: the UUID backend resolves through the single-server
# envelope (`GET /v2.1/servers/<uuid>`).
cat > "$SERVERS_DIR/$UUID.json" <<JSON
{
  "server": {
    "id": "$UUID",
    "name": "appUuid",
    "addresses": {
      "net-a": [
        { "version": 4, "addr": "127.0.0.1", "OS-EXT-IPS:type": "floating" }
      ]
    }
  }
}
JSON

# The `solo` site's instance fixture exists at boot; the delete-fixture phase
# removes it so the (health-check-free) site must answer 502 at request time.
cat > "$SERVERS_DIR/gone.json" <<JSON
{
  "servers": [
    {
      "id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
      "name": "gone",
      "addresses": {
        "net-a": [
          { "version": 4, "addr": "127.0.0.1", "OS-EXT-IPS:type": "floating" }
        ]
      }
    }
  ]
}
JSON

# Baseline `apps` site: a name backend (weight 2) and a UUID backend (weight 1),
# weighted round robin, with a 1s TCP health check.
write_apps_site() { # $1=json backend block
  cat > "$SITES_DIR/001-apps.yaml" <<YAML
name: apps
mode: forward
domain: $BASE
http: true
https: false
openstack_env: prod
backends:
$1
load_balancer: weighted_round_robin
health_check:
  kind: tcp
  interval_secs: 1
  timeout_secs: 1
YAML
}

APPS_TWO="  - { instance: appA, port: $UP_A, weight: 2 }
  - { instance: $UUID, port: $UP_B, weight: 1 }"
APPS_ONE="  - { instance: appA, port: $UP_A, weight: 1 }"
write_apps_site "$APPS_TWO"

# `solo` site: a single instance backend and NO health check, so a resolution
# failure is a request-time 502 rather than a silent de-selection.
cat > "$SITES_DIR/002-solo.yaml" <<YAML
name: solo
mode: forward
domain: $SOLO
http: true
https: false
openstack_env: prod
backends:
  - { instance: gone, port: $UP_A }
YAML

# --- helpers ----------------------------------------------------------------

# Fetch one request and echo the X-Upstream-Port header (empty on a failure).
request_port() { # $1=Host
  local host="$1"
  { curl -sS -D - -o /dev/null --max-time 10 -H "Host: $host" \
      "http://127.0.0.1:$HTTP_PORT/" 2>/dev/null || true; } \
    | tr -d '\r' | awk 'tolower($1)=="x-upstream-port:"{print $2}'
}

status_for() { # $1=Host
  curl -s -o /dev/null -w '%{http_code}' --max-time 10 \
    -H "Host: $1" "http://127.0.0.1:$HTTP_PORT/" 2>/dev/null || true
}

# Issue `n` sequential requests for `host`; print "<count> <port>" lines and
# leave the raw ports in $LOG_DIR/dist.txt.
summary() { # $1=Host $2=n
  local host="$1" n="$2" i p
  : > "$LOG_DIR/dist.txt"
  for ((i = 0; i < n; i++)); do
    p="$(request_port "$host")"
    [ -n "$p" ] && echo "$p" >> "$LOG_DIR/dist.txt"
  done
  sort "$LOG_DIR/dist.txt" | uniq -c
}

metrics() { curl -s --max-time 5 "http://127.0.0.1:$METRICS_PORT/metrics" 2>/dev/null || true; }

wait_for_metric() { # $1=exact line $2=label
  for _ in $(seq 1 50); do
    if metrics | grep -qF "$1"; then
      printf '    %-54s %s\n' "$2" "observed"
      return 0
    fi
    sleep 0.2
  done
  echo "ASSERT FAIL: metric '$1' never appeared ($2)"
  echo "---- /metrics dump ----"; metrics; echo "-----------------------"
  exit 1
}

# Count request-record lines matching an ERE (0 when the file is missing).
record_count() { # $1=ERE
  [ -f "$RECORD" ] || { echo 0; return; }
  awk -v pat="$1" '$0 ~ pat {n++} END {print n+0}' "$RECORD"
}

# --- start the control plane, upstreams and iris ----------------------------

echo "[mock] control plane on 127.0.0.1:$OS_PORT (servers=$SERVERS_DIR)"
python3 scripts/mock_openstack.py \
  --port "$OS_PORT" --servers-dir "$SERVERS_DIR" --record "$RECORD" \
  > "$LOG_DIR/mock.log" 2>&1 &
OS_PID=$!
for _ in $(seq 1 50); do
  if curl -s -o /dev/null --max-time 1 \
    "http://127.0.0.1:$OS_PORT/v2.1/servers?name=__probe__" 2>/dev/null; then
    break
  fi
  sleep 0.1
done

start_stub_b() { # remaining args are extra upstream.py flags
  python3 tests/stubs/upstream.py --port "$UP_B" "$@" \
    > "$LOG_DIR/stub-$UP_B.log" 2>&1 &
  STUB_B_PID=$!
}

echo "[stub] starting upstream A ($UP_A) and B ($UP_B), both healthy"
python3 tests/stubs/upstream.py --port "$UP_A" > "$LOG_DIR/stub-$UP_A.log" 2>&1 &
STUB_A_PID=$!
start_stub_b

echo "[iris] http=127.0.0.1:$HTTP_PORT metrics=127.0.0.1:$METRICS_PORT"
IRIS_HTTP_LISTEN_ADDR="127.0.0.1:$HTTP_PORT" \
  "$IRIS_BIN" run --config-dir "$CFG_DIR" --pidfile "$PIDFILE" \
  --metrics-listen "127.0.0.1:$METRICS_PORT" > "$LOG_DIR/iris.log" 2>&1 &
IRIS_PID=$!

ready=0
for _ in $(seq 1 60); do
  if curl -sf -o /dev/null --max-time 2 -H "Host: $BASE" "http://127.0.0.1:$HTTP_PORT/"; then
    ready=1
    break
  fi
  sleep 0.2
done
if [ "$ready" != "1" ]; then
  echo "FAIL: iris did not become ready; log follows"
  cat "$LOG_DIR/iris.log"
  exit 1
fi

# Give the instance health probe a couple of ticks so both backends are known
# healthy before the baseline measurement.
sleep 2

# --- phase 1: weighted distribution across two instance backends ------------

echo
echo "=== phase 1: weighted 2:1 distribution over two instance backends ==="
out="$(summary "$BASE" 30)"
printf '%s\n' "$out" | sed 's/^/    /'
if ! grep -q "$UP_A" "$LOG_DIR/dist.txt"; then
  echo "ASSERT FAIL: the name backend $UP_A was never selected"
  exit 1
fi
if ! grep -q "$UP_B" "$LOG_DIR/dist.txt"; then
  echo "ASSERT FAIL: the UUID backend $UP_B was never selected"
  exit 1
fi
n_a="$(grep -c "$UP_A" "$LOG_DIR/dist.txt" || true)"
n_b="$(grep -c "$UP_B" "$LOG_DIR/dist.txt" || true)"
printf '    %-54s A=%s B=%s\n' "weighted distribution (2:1)" "$n_a" "$n_b"
if [ "$n_a" -le "$n_b" ]; then
  echo "ASSERT FAIL: weight 2:1 not honoured (A=$n_a B=$n_b)"
  exit 1
fi

# --- phase 2: name + UUID mixed, one token, cached resolutions --------------

echo
echo "=== phase 2: name + UUID mixed, token reuse, cache ==="
appA_calls="$(record_count 'name=appA')"
# Real Nova's list endpoint omits `addresses`, so a cold name resolution costs
# exactly 2 Nova requests: the name list (locate the id) + show-by-id (detail).
appA_detail_calls="$(record_count '/servers/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa')"
uuid_calls="$(record_count "/servers/$UUID")"
auth_calls="$(record_count '/v3/auth/tokens')"
printf '    %-54s %s\n' "Nova name list (name=appA)" "$appA_calls"
printf '    %-54s %s\n' "Nova show-by-id (appA detail)" "$appA_detail_calls"
printf '    %-54s %s\n' "Nova show-by-id (/servers/<uuid>)" "$uuid_calls"
printf '    %-54s %s\n' "Keystone POST /v3/auth/tokens" "$auth_calls"
if [ "$appA_calls" != "1" ]; then
  echo "ASSERT FAIL: expected exactly one Nova name list for appA, got $appA_calls"
  exit 1
fi
if [ "$appA_detail_calls" != "1" ]; then
  echo "ASSERT FAIL: expected exactly one show-by-id detail for appA, got $appA_detail_calls"
  exit 1
fi
if [ "$uuid_calls" -lt 1 ]; then
  echo "ASSERT FAIL: the UUID backend never hit Nova show-by-id"
  exit 1
fi
if [ "$auth_calls" != "1" ]; then
  echo "ASSERT FAIL: expected exactly one Keystone token POST, got $auth_calls"
  exit 1
fi

# A warm resolution must be reused: many more requests add no Nova traffic.
before=$((appA_calls + appA_detail_calls + uuid_calls))
summary "$BASE" 12 > /dev/null
after="$(( $(record_count 'name=appA') \
  + $(record_count '/servers/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa') \
  + $(record_count "/servers/$UUID") ))"
printf '    %-54s %s -> %s\n' "Nova lookups before/after 12 warm requests" "$before" "$after"
if [ "$after" != "$before" ]; then
  echo "ASSERT FAIL: a warm resolution was re-fetched ($before -> $after)"
  exit 1
fi
if [ "$(record_count '/v3/auth/tokens')" != "1" ]; then
  echo "ASSERT FAIL: the token was re-fetched on the warm path"
  exit 1
fi

# --- phase 3: single-flight + 502 for an unresolvable instance --------------

echo
echo "=== phase 3: delete fixture -> 502 (solo has no health check) ==="
rm -f "$SERVERS_DIR/gone.json"
# Fire the requests concurrently: a cold, unresolvable key must collapse to a
# single Nova lookup (single-flight), and every request must be a 502.
side_before="$(record_count 'name=gone')"
: > "$LOG_DIR/solo-codes.txt"
req_pids=()
for _ in $(seq 1 10); do
  ( printf '%s\n' "$(status_for "$SOLO")" >> "$LOG_DIR/solo-codes.txt" ) &
  req_pids+=("$!")
done
for pid in "${req_pids[@]}"; do wait "$pid" 2>/dev/null || true; done
sort "$LOG_DIR/solo-codes.txt" | uniq -c | sed 's/^/    /'
if [ "$(grep -c '^502$' "$LOG_DIR/solo-codes.txt" || true)" != "10" ]; then
  echo "ASSERT FAIL: a deleted instance fixture must answer 502 for every request"
  exit 1
fi
gone_calls="$(record_count 'name=gone')"
printf '    %-54s %s\n' "Nova lookups for gone (10 concurrent reqs)" "$gone_calls"
if [ "$gone_calls" != $((side_before + 1)) ]; then
  echo "ASSERT FAIL: expected exactly one Nova lookup for 'gone' (single-flight)"
  exit 1
fi
if [ "$(grep -c "$UP_A" "$LOG_DIR/dist.txt" || true)" -lt 1 ]; then
  echo "ASSERT FAIL: the healthy site should be unaffected"
  exit 1
fi

# --- phase 4: instance health gating ----------------------------------------

echo
echo "=== phase 4: TCP health probe drops a dead instance backend ==="
kill "$STUB_B_PID" 2>/dev/null || true
wait "$STUB_B_PID" 2>/dev/null || true
STUB_B_PID=""
sleep 3
out="$(summary "$BASE" 8)"
printf '%s\n' "$out" | sed 's/^/    /'
if [ "$(grep -c "$UP_B" "$LOG_DIR/dist.txt" || true)" != "0" ]; then
  echo "ASSERT FAIL: the dead backend $UP_B was still selected"
  exit 1
fi
if ! grep -q "$UP_A" "$LOG_DIR/dist.txt"; then
  echo "ASSERT FAIL: the healthy backend $UP_A was never selected"
  exit 1
fi
printf '    %-54s %s\n' "dead backend $UP_B excluded" "yes"

echo "[health] restarting backend $UP_B as HEALTHY"
sleep 0.3
start_stub_b
sleep 3
out="$(summary "$BASE" 12)"
printf '%s\n' "$out" | sed 's/^/    /'
if ! grep -q "$UP_B" "$LOG_DIR/dist.txt"; then
  echo "ASSERT FAIL: the recovered backend $UP_B was not re-admitted"
  exit 1
fi
printf '    %-54s %s\n' "backend $UP_B re-admitted after recovery" "yes"

# --- phase 5: hot reload swaps the instance backend set ---------------------

echo
echo "=== phase 5: hot reload removes the UUID backend ==="
write_apps_site "$APPS_ONE"
"$IRIS_BIN" reload --pidfile "$PIDFILE"
wait_for_metric 'config_reload_total{ok="true"} 1' 'config_reload_total{ok="true"} 1'
sleep 2
out="$(summary "$BASE" 12)"
printf '%s\n' "$out" | sed 's/^/    /'
if [ "$(grep -c "$UP_B" "$LOG_DIR/dist.txt" || true)" != "0" ]; then
  echo "ASSERT FAIL: the removed UUID backend $UP_B was still selected after reload"
  exit 1
fi
if ! grep -q "$UP_A" "$LOG_DIR/dist.txt"; then
  echo "ASSERT FAIL: the retained backend $UP_A was never selected after reload"
  exit 1
fi
printf '    %-54s %s\n' "reloaded pool serves only the retained backend" "yes"

echo
echo "ALL C3 FORWARD-INSTANCE E2E ASSERTIONS PASSED"
