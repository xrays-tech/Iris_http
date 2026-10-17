#!/usr/bin/env bash
#
# Health-check survival across a hot reload (plan/11 §端到端, reload.md §3).
#
# Regression proof for the reload x health-check interaction: `apply_new_config`
# must not replace the `Arc<LbEngine>` of a forward site whose pool-relevant
# configuration (backends / algorithm / health check) is unchanged, because the
# health-check background service registered at startup holds that exact `Arc`.
# If the engine were rebuilt, its pool would have no probe and an unhealthy
# backend would be selected again.
#
# The proof uses an HTTP health check (`http_path: /`, 2xx = healthy) and makes
# backend B *unhealthy while still connectable*: it keeps serving requests but
# answers `503`. A connect-refused backend would be masked by the proxy's
# retry-on-connect (a failed select is retried to the healthy backend), so a 503
# backend is required to observe the *selection* itself:
#   * health gating intact  -> B is never selected, every response is from A;
#   * health gating lost    -> B is selected and its 503s carry
#     `X-Upstream-Port: <B>`, which the assertion below catches.
#
# Scenario:
#   1. boot a forward site with two healthy backends and a 1s HTTP health check;
#   2. confirm round-robin reaches both backends;
#   3. hot-reload a config that ONLY adds an unrelated forward site (the health
#      site is bit-for-bit unchanged, so it must be reused);
#   4. restart backend B so it answers 503 and confirm, after the probe runs,
#      that B is no longer selected.
#
# Re-run from the repository root:
#     scripts/e2e/09-health-reload.sh
#
# Ports are fixed and hermetic (nothing committed is mutated):
#   upstream A 18301 · upstream B 18302 · HTTP 18311 · metrics 18391.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

UP_A=18301
UP_B=18302
HTTP_PORT=18311
METRICS_PORT=18391

WORK="target/e2e-p9"
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

# The health-checked forward site: two backends + a 1s HTTP probe on `/`.
cat > "$SITES_DIR/001-health.yaml" <<YAML
name: health
mode: forward
domain: health.test
http: true
https: false

backends:
  - addr: 127.0.0.1:$UP_A
  - addr: 127.0.0.1:$UP_B

load_balancer: round_robin
health_check:
  kind: http
  http_path: /
  interval_secs: 1
  timeout_secs: 1
YAML

# An unrelated forward site, added by the reload. It must not touch `health`.
write_other_site() {
  cat > "$SITES_DIR/002-other.yaml" <<YAML
name: other
mode: forward
domain: other.test
http: true
https: false

backends:
  - addr: 127.0.0.1:$UP_A
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

assert_contains() { # $1=haystack $2=needle
  case "$1" in
    *"$2"*) return 0 ;;
    *)
      echo "ASSERT FAIL: expected '$2' in:"
      printf '%s\n' "$1"
      return 1
      ;;
  esac
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

echo "[stub] starting upstreams on $UP_A and $UP_B (both healthy)"
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
  if curl -sf -o /dev/null -H 'Host: health.test' "http://127.0.0.1:$HTTP_PORT/"; then
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
echo "=== baseline (config A): both backends healthy ==="
out="$(summary health.test 6)"
printf '%s\n' "$out" | sed 's/^/    /'
assert_contains "$out" "$UP_A"
assert_contains "$out" "$UP_B"

echo
echo "=== hot reload (config B: add unrelated site 'other.test') ==="
write_other_site
"$IRIS_BIN" reload --pidfile "$PIDFILE"
wait_for_metric 'config_reload_total{ok="true"} 1' 'config_reload_total{ok="true"} 1'
code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -H 'Host: other.test' "http://127.0.0.1:$HTTP_PORT/")"
printf '    %-54s %s\n' "new site other.test" "HTTP $code"
if [ "$code" != "200" ]; then
  echo "ASSERT FAIL: the reload did not take effect (other.test => $code)"
  exit 1
fi

echo
echo "[health] restarting backend $UP_B as UNHEALTHY (still connectable, answers 503)"
kill "$STUB_B_PID" 2>/dev/null || true
wait "$STUB_B_PID" 2>/dev/null || true
STUB_B_PID=""
sleep 0.3
start_stub_b --status 503
sleep 5

echo "=== post-reload: unhealthy backend $UP_B must not be selected ==="
out="$(summary health.test 8)"
printf '%s\n' "$out" | sed 's/^/    /'
assert_contains "$out" "$UP_A"
if grep -q "$UP_B" "$LOG_DIR/dist.txt"; then
  echo "ASSERT FAIL: backend $UP_B still selected after the reload (health gating lost)"
  exit 1
fi
printf '    %-54s %s\n' "backend $UP_B excluded after reload" "yes"

# The reused engine must still serve, and the new site must be unaffected.
code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -H 'Host: other.test' "http://127.0.0.1:$HTTP_PORT/")"
printf '    %-54s %s\n' "other.test still routes" "HTTP $code"
if [ "$code" != "200" ]; then
  echo "ASSERT FAIL: unrelated site broke after the reload (other.test => $code)"
  exit 1
fi

echo
echo "ALL HEALTH-RELOAD E2E ASSERTIONS PASSED"
