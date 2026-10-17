#!/usr/bin/env bash
#
# FIX #2 — health gating for a site added/changed by a hot reload
# (reload.md §3, plan/11 §端到端).
#
# Every forward engine shares ONE long-lived `HealthRegistry` driven by a single
# `HealthCheckService` registered at startup. A site added or changed by a reload
# therefore contributes its backends to the already-running probe with no
# re-attachment step, so an unhealthy backend is excluded immediately.
#
# The proof uses an HTTP health check (`http_path: /`, 2xx = healthy) and makes
# backend B *unhealthy while still connectable* (it answers 503): a
# connect-refused backend would be masked by the proxy's retry-on-connect, so a
# 503 backend is required to observe the selection itself.
#
# Scenario:
#   1. boot with a single healthy forward site `seed` (no health check);
#   2. hot-reload a config that ADDS site `gated` (backends A + B, 1s HTTP check)
#      while B always answers 503 -> `gated` must only ever select A;
#   3. restart B as healthy and hot-reload `gated` with a CHANGED health interval
#      -> the interval change re-probes and B is selected again;
#   4. `seed` must keep working untouched throughout.
#
# Re-run from the repository root:
#     scripts/e2e/11-health-reload-new-site.sh
#
# Ports are fixed and hermetic (nothing committed is mutated):
#   upstream A 18351 · upstream B 18352 · HTTP 18361 · metrics 18392.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

UP_A=18351
UP_B=18352
HTTP_PORT=18361
METRICS_PORT=18392

WORK="target/e2e-p11"
CFG_DIR="$WORK/config"
SITES_DIR="$CFG_DIR/sites"
LOG_DIR="$WORK/logs"
PIDFILE="$WORK/iris.pid"

rm -rf "$WORK"
mkdir -p "$SITES_DIR" "$LOG_DIR"

echo "[build] cargo build --bin iris"
cargo build --bin iris --quiet
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
IRIS_BIN="$TARGET_DIR/debug/iris"
echo "[build] binary: $IRIS_BIN"

STUB_A_PID=""
STUB_B_PID=""
IRIS_PID=""

cleanup() {
  if [ -n "$IRIS_PID" ]; then kill "$IRIS_PID" 2>/dev/null || true; fi
  if [ -n "$STUB_A_PID" ]; then kill "$STUB_A_PID" 2>/dev/null || true; fi
  if [ -n "$STUB_B_PID" ]; then kill "$STUB_B_PID" 2>/dev/null || true; fi
}
trap cleanup EXIT

# --- hermetic fixtures ------------------------------------------------------

# Config A: a single healthy forward site with no health check.
cat > "$SITES_DIR/001-seed.yaml" <<YAML
name: seed
mode: forward
domain: seed.test
http: true
https: false
backends:
  - addr: 127.0.0.1:$UP_A
YAML

# Config B/C: the health-checked site added by the reload. A is healthy; B
# always answers 503. `$1` is the health-check interval.
write_gated_site() { # $1=interval_secs
  cat > "$SITES_DIR/002-gated.yaml" <<YAML
name: gated
mode: forward
domain: gated.test
http: true
https: false
backends:
  - addr: 127.0.0.1:$UP_A
  - addr: 127.0.0.1:$UP_B
load_balancer: round_robin
health_check:
  kind: http
  http_path: /
  interval_secs: $1
  timeout_secs: 1
YAML
}

# --- helpers ----------------------------------------------------------------

# Fetch one request and echo the X-Upstream-Port header (any status; empty on a
# connection failure).
request_port() { # $1=Host
  local host="$1"
  { curl -sS -D - -o /dev/null -H "Host: $host" \
      "http://127.0.0.1:$HTTP_PORT/" 2>/dev/null || true; } \
    | tr -d '\r' | awk 'tolower($1)=="x-upstream-port:"{print $2}'
}

# Issue `n` requests for `host` and print "<count> <port>" lines.
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

# --- start upstreams + iris -------------------------------------------------

start_stub_b() { # remaining args are extra upstream.py flags
  python3 tests/stubs/upstream.py --port "$UP_B" "$@" \
    > "$LOG_DIR/stub-$UP_B.log" 2>&1 &
  STUB_B_PID=$!
}

echo "[stub] starting upstream A (healthy) and B (always 503)"
python3 tests/stubs/upstream.py --port "$UP_A" > "$LOG_DIR/stub-$UP_A.log" 2>&1 &
STUB_A_PID=$!
start_stub_b --status 503

echo "[iris] http=127.0.0.1:$HTTP_PORT metrics=127.0.0.1:$METRICS_PORT"
IRIS_HTTP_LISTEN_ADDR="127.0.0.1:$HTTP_PORT" \
  "$IRIS_BIN" run --config-dir "$CFG_DIR" --pidfile "$PIDFILE" \
  --metrics-listen "127.0.0.1:$METRICS_PORT" > "$LOG_DIR/iris.log" 2>&1 &
IRIS_PID=$!

ready=0
for _ in $(seq 1 60); do
  if curl -sf -o /dev/null -H 'Host: seed.test' "http://127.0.0.1:$HTTP_PORT/"; then
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

echo
echo "=== baseline (config A): site 'seed' serves ==="
code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -H 'Host: seed.test' "http://127.0.0.1:$HTTP_PORT/")"
printf '    %-54s %s\n' "seed.test" "HTTP $code"
if [ "$code" != "200" ]; then
  echo "ASSERT FAIL: the baseline site did not serve (seed.test => $code)"
  exit 1
fi

# --- reload 1: add the health-checked site ----------------------------------

echo
echo "=== hot reload #1 (add health-checked site 'gated'; B answers 503) ==="
write_gated_site 1
"$IRIS_BIN" reload --pidfile "$PIDFILE"
wait_for_metric 'config_reload_total{ok="true"} 1' 'config_reload_total{ok="true"} 1'

# The new site must be routable...
code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -H 'Host: gated.test' "http://127.0.0.1:$HTTP_PORT/")"
printf '    %-54s %s\n' "gated.test routable" "HTTP $code"
if [ "$code" != "200" ]; then
  echo "ASSERT FAIL: the reloaded site did not route (gated.test => $code)"
  exit 1
fi

# ...and its unhealthy backend must be excluded once the probe has run.
sleep 5
echo "=== post-reload: unhealthy backend $UP_B must not be selected for 'gated' ==="
out="$(summary gated.test 8)"
printf '%s\n' "$out" | sed 's/^/    /'
if ! grep -q "$UP_A" "$LOG_DIR/dist.txt"; then
  echo "ASSERT FAIL: the healthy backend $UP_A was never selected"
  exit 1
fi
if grep -q "$UP_B" "$LOG_DIR/dist.txt"; then
  echo "ASSERT FAIL: the newly added site's unhealthy backend $UP_B was selected (health gating lost)"
  exit 1
fi
printf '    %-54s %s\n' "backend $UP_B excluded on the new site" "yes"

# --- reload 2: recover B and change the health interval ---------------------

echo
echo "[health] restarting backend $UP_B as HEALTHY (200)"
kill "$STUB_B_PID" 2>/dev/null || true
wait "$STUB_B_PID" 2>/dev/null || true
STUB_B_PID=""
sleep 0.3
start_stub_b

echo "=== hot reload #2 (change 'gated' health interval 1 -> 2) ==="
write_gated_site 2
"$IRIS_BIN" reload --pidfile "$PIDFILE"
wait_for_metric 'config_reload_total{ok="true"} 2' 'config_reload_total{ok="true"} 2'

sleep 5
echo "=== post-reload: the changed interval re-probes; $UP_B recovers ==="
out="$(summary gated.test 8)"
printf '%s\n' "$out" | sed 's/^/    /'
if ! grep -q "$UP_B" "$LOG_DIR/dist.txt"; then
  echo "ASSERT FAIL: the recovered backend $UP_B was not selected after the interval change"
  exit 1
fi
printf '    %-54s %s\n' "backend $UP_B recovered after the interval change" "yes"

# --- the untouched site still serves ----------------------------------------

code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -H 'Host: seed.test' "http://127.0.0.1:$HTTP_PORT/")"
printf '    %-54s %s\n' "seed.test still routes" "HTTP $code"
if [ "$code" != "200" ]; then
  echo "ASSERT FAIL: the untouched site broke after the reloads (seed.test => $code)"
  exit 1
fi

echo
echo "ALL HEALTH-RELOAD-NEW-SITE E2E ASSERTIONS PASSED"
