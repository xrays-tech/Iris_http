#!/usr/bin/env bash
#
# P3 forward-lane end-to-end test (plan/06 §端到端).
#
# Boots the upstream stub on two ports, runs `iris` against the committed
# `tests/fixtures/e2e-p3` config (round-robin / weighted / random / ketama +
# a TCP health-checked site), then drives it through `Host`-header based
# virtual hosts with curl.
#
# Re-run from the repository root:
#     scripts/e2e/03-forward.sh
#
# The script builds the `iris` binary itself. Override the proxy port with
# `IRIS_E2E_PROXY_PORT=<port>`. Output is deterministic: every assertion is
# printed with its observed distribution.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

UP1="${IRIS_E2E_UP1:-18081}"
UP2="${IRIS_E2E_UP2:-18082}"
PROXY_PORT="${IRIS_E2E_PROXY_PORT:-18080}"
CONFIG_DIR="tests/fixtures/e2e-p3"
LOG_DIR="target/e2e-p3"
PIDFILE="$LOG_DIR/iris.pid"

mkdir -p "$LOG_DIR"

echo "[build] cargo build --bin iris"
cargo build --bin iris --quiet
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
IRIS_BIN="$TARGET_DIR/debug/iris"
echo "[build] binary: $IRIS_BIN"

STUB1_PID=""
STUB2_PID=""
IRIS_PID=""

cleanup() {
  if [ -n "$IRIS_PID" ]; then kill "$IRIS_PID" 2>/dev/null || true; fi
  if [ -n "$STUB1_PID" ]; then kill "$STUB1_PID" 2>/dev/null || true; fi
  if [ -n "$STUB2_PID" ]; then kill "$STUB2_PID" 2>/dev/null || true; fi
}
trap cleanup EXIT

start_stub2() {
  python3 tests/stubs/upstream.py --port "$UP2" > "$LOG_DIR/stub-$UP2.log" 2>&1 &
  STUB2_PID=$!
}

echo "[stub] starting upstreams on $UP1 and $UP2"
python3 tests/stubs/upstream.py --port "$UP1" > "$LOG_DIR/stub-$UP1.log" 2>&1 &
STUB1_PID=$!
start_stub2

echo "[iris] listening on 127.0.0.1:$PROXY_PORT (config-dir=$CONFIG_DIR)"
IRIS_HTTP_LISTEN_ADDR="127.0.0.1:$PROXY_PORT" "$IRIS_BIN" run \
  --config-dir "$CONFIG_DIR" --pidfile "$PIDFILE" \
  > "$LOG_DIR/iris.log" 2>&1 &
IRIS_PID=$!

ready=0
for _ in $(seq 1 50); do
  if curl -sf -o /dev/null -H 'Host: rr.test' "http://127.0.0.1:$PROXY_PORT/"; then
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

# Fetch one request and echo the X-Upstream-Port header (empty on failure).
request_port() {
  local host="$1"
  { curl -sS -D - -o /dev/null -H "Host: $host" "http://127.0.0.1:$PROXY_PORT/" 2>/dev/null || true; } \
    | tr -d '\r' | awk 'tolower($1)=="x-upstream-port:"{print $2}'
}

# Issue `n` requests for `host` and print "<count> <port>" lines.
summary() {
  local host="$1" n="$2" i p
  : > "$LOG_DIR/dist.txt"
  for ((i = 0; i < n; i++)); do
    p="$(request_port "$host")"
    [ -n "$p" ] && echo "$p" >> "$LOG_DIR/dist.txt"
  done
  sort "$LOG_DIR/dist.txt" | uniq -c
}

assert_contains() {
  case "$1" in
    *"$2"*) return 0 ;;
    *)
      echo "ASSERT FAIL: expected '$2' in:"
      printf '%s\n' "$1"
      return 1
      ;;
  esac
}

echo
echo "=== round_robin (rr.test) — 6 requests, expect a 3/3 split ==="
out="$(summary rr.test 6)"
printf '%s\n' "$out" | sed 's/^/    /'
assert_contains "$out" "3 $UP1"
assert_contains "$out" "3 $UP2"

echo
echo "=== weighted_round_robin (weighted.test) — 8 requests, weight 3:1 ==="
out="$(summary weighted.test 8)"
printf '%s\n' "$out" | sed 's/^/    /'
assert_contains "$out" "6 $UP1"
assert_contains "$out" "2 $UP2"

echo
echo "=== random (random.test) — 40 requests, expect both backends ==="
out="$(summary random.test 40)"
printf '%s\n' "$out" | sed 's/^/    /'
assert_contains "$out" "$UP1"
assert_contains "$out" "$UP2"

echo
echo "=== ketama (ketama.test) — 10 requests from one client, expect a single backend ==="
out="$(summary ketama.test 10)"
printf '%s\n' "$out" | sed 's/^/    /'
distinct="$(sort -u "$LOG_DIR/dist.txt" | wc -l | tr -d ' ')"
if [ "$distinct" != "1" ]; then
  echo "ASSERT FAIL: ketama should be sticky, saw $distinct distinct backends"
  exit 1
fi

echo
echo "=== health_check (health.test) — baseline both backends up ==="
out="$(summary health.test 6)"
printf '%s\n' "$out" | sed 's/^/    /'
assert_contains "$out" "$UP1"
assert_contains "$out" "$UP2"

echo
echo "[health] stopping backend $UP2"
kill "$STUB2_PID" 2>/dev/null || true
wait "$STUB2_PID" 2>/dev/null || true
STUB2_PID=""
sleep 4
out="$(summary health.test 8)"
printf '%s\n' "$out" | sed 's/^/    /'
assert_contains "$out" "$UP1"
if grep -q "$UP2" "$LOG_DIR/dist.txt"; then
  echo "ASSERT FAIL: backend $UP2 still selected while down"
  exit 1
fi

echo "[health] restarting backend $UP2"
start_stub2
sleep 4
out="$(summary health.test 8)"
printf '%s\n' "$out" | sed 's/^/    /'
assert_contains "$out" "$UP2"

echo
echo "=== unknown host (unknown.test) — expect 404 ==="
code="$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: unknown.test' "http://127.0.0.1:$PROXY_PORT/")"
echo "    HTTP $code"
if [ "$code" != "404" ]; then
  echo "ASSERT FAIL: expected 404, got $code"
  exit 1
fi

echo
echo "ALL P3 FORWARD E2E ASSERTIONS PASSED"
