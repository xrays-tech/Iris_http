#!/usr/bin/env bash
#
# P7 OpenStack + firewall orchestration end-to-end test (plan/10 §端到端).
#
# Boots the upstream stub, `scripts/mock_openstack.py` (the control-plane
# contract) and `iris` against the committed `tests/fixtures/e2e-p7` config
# (one openstack site `rent` with a `firewall_dir`), then drives the whole
# pipeline over the HTTP + TLS listeners with `curl`:
#
#   * 200 through the resolved floating address,
#   * 403 for a port that is not open, an un-whitelisted / blacklisted client,
#     an instance without a role file,
#   * 404 for an unknown instance,
#   * 502 for a forced Nova 500 / 403, a failed Keystone auth and an instance
#     with no address,
#   * floating-down → fixed-up fallback still serving 200,
#   * a single `/v3/auth/tokens` call reused across lookups.
#
# Re-run from the repository root:
#     scripts/e2e/07-openstack.sh
#
# The ports are fixed because they are baked into the committed config fixture
# (auth `api_url`, the role files' `allowed_ports`, and the mock's fixed port):
#   upstream stub 18101 · control-plane mock 18102 · HTTP 18111 · TLS 18112.
# Output is deterministic: every assertion prints what it observed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

UP1=18101
OS_PORT=18102
HTTP_PORT=18111
TLS_PORT=18112
CONFIG_DIR="tests/fixtures/e2e-p7"
SERVERS_DIR="tests/fixtures/e2e-p7/servers"
LOG_DIR="target/e2e-p7"
PIDFILE="$LOG_DIR/iris.pid"
RECORD="$LOG_DIR/os-record.jsonl"
BASE="rent.do.top"

mkdir -p "$LOG_DIR"

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

# --- mock control plane -----------------------------------------------------
start_mock() {
  # "$@" = extra mock flags (e.g. --auth-fail, --servers-status 500).
  python3 scripts/mock_openstack.py \
    --port "$OS_PORT" \
    --servers-dir "$SERVERS_DIR" \
    --record "$RECORD" \
    "$@" > "$LOG_DIR/mock.log" 2>&1 &
  OS_PID=$!
}

stop_mock() {
  if [ -n "$OS_PID" ]; then
    kill "$OS_PID" 2>/dev/null || true
    wait "$OS_PID" 2>/dev/null || true
    OS_PID=""
  fi
}

restart_mock() {
  stop_mock
  rm -f "$RECORD"
  start_mock "$@"
  # Wait until the mock answers (a 404 for the probe name is a valid reply).
  for _ in $(seq 1 50); do
    if curl -s -o /dev/null --max-time 1 \
      "http://127.0.0.1:$OS_PORT/v2.1/servers?name=__probe__" 2>/dev/null; then
      break
    fi
    sleep 0.1
  done
}

# --- assertions -------------------------------------------------------------
status_for() {
  local host="$1"
  curl -s -o /dev/null -w '%{http_code}' --max-time 20 -H "Host: $host" \
    "http://127.0.0.1:$HTTP_PORT/" 2>/dev/null || true
}

assert_code() {
  local label="$1" expect="$2" host="$3"
  local code
  code="$(status_for "$host")"
  printf '    %-46s %s\n' "$label" "HTTP $code"
  if [ "$code" != "$expect" ]; then
    echo "ASSERT FAIL: $label — expected $expect, got $code"
    exit 1
  fi
}

upstream_port_for() {
  local host="$1"
  { curl -sS -D - -o /dev/null --max-time 20 -H "Host: $host" \
      "http://127.0.0.1:$HTTP_PORT/" 2>/dev/null || true; } \
    | tr -d '\r' | awk 'tolower($1)=="x-upstream-port:"{print $2}'
}

# --- upstream stub ----------------------------------------------------------
echo "[stub] starting upstream on $UP1"
python3 tests/stubs/upstream.py --port "$UP1" > "$LOG_DIR/stub-$UP1.log" 2>&1 &
STUB_PID=$!

# --- phase A: auth failure --------------------------------------------------
echo
echo "=== phase A: Keystone auth failure → 502 ==="
restart_mock --auth-fail

echo "[iris] http=127.0.0.1:$HTTP_PORT https=127.0.0.1:$TLS_PORT (config-dir=$CONFIG_DIR)"
IRIS_HTTP_LISTEN_ADDR="127.0.0.1:$HTTP_PORT" \
  IRIS_HTTP_LISTEN_TLS_ADDR="127.0.0.1:$TLS_PORT" \
  "$IRIS_BIN" run --config-dir "$CONFIG_DIR" --pidfile "$PIDFILE" \
  > "$LOG_DIR/iris.log" 2>&1 &
IRIS_PID=$!

ready=0
for _ in $(seq 1 60); do
  code="$(status_for "web-${UP1}.${BASE}")"
  if [ -n "$code" ] && [ "$code" != "000" ]; then ready=1; break; fi
  sleep 0.2
done
if [ "$ready" != "1" ]; then
  echo "FAIL: iris did not become ready; log follows"
  cat "$LOG_DIR/iris.log"
  exit 1
fi

assert_code "auth failure → 502" "502" "web-${UP1}.${BASE}"

# --- phase B: normal control plane -----------------------------------------
echo
echo "=== phase B: normal control plane (token + firewall + discovery) ==="
restart_mock

assert_code "floating upstream happy path → 200" "200" "web-${UP1}.${BASE}"
uport="$(upstream_port_for "web-${UP1}.${BASE}")"
echo "    upstream port for web-${UP1}.${BASE}: ${uport:-<none>}"
if [ "$uport" != "$UP1" ]; then
  echo "ASSERT FAIL: expected upstream port $UP1, got '${uport:-<none>}'"
  exit 1
fi

assert_code "fixed-only instance (no floating) → 200" "200" "fixedonly-${UP1}.${BASE}"

echo
echo "=== phase B (firewall denials, uniform 403) ==="
assert_code "port not open (web-9) → 403" "403" "web-9.${BASE}"
assert_code "un-whitelisted client (locked) → 403" "403" "locked-${UP1}.${BASE}"
assert_code "blacklisted client (banned) → 403" "403" "banned-${UP1}.${BASE}"
assert_code "instance without a role file (ghost) → 403" "403" "ghost-${UP1}.${BASE}"

echo
echo "=== phase B (discovery errors) ==="
assert_code "unknown instance (missinginst) → 404" "404" "missinginst-${UP1}.${BASE}"
assert_code "instance with no address (noip) → 502" "502" "noip-${UP1}.${BASE}"

echo
echo "=== phase B (floating down, fixed up → fallback 200) ==="
assert_code "failover floating→fixed → 200" "200" "failover-${UP1}.${BASE}"
uport="$(upstream_port_for "failover-${UP1}.${BASE}")"
echo "    upstream port after fallback: ${uport:-<none>}"
if [ "$uport" != "$UP1" ]; then
  echo "ASSERT FAIL: fallback expected upstream port $UP1, got '${uport:-<none>}'"
  exit 1
fi

echo
echo "=== phase B (token reuse) ==="
auth_calls="$(awk '/\/v3\/auth\/tokens/{n++} END{print n+0}' "$RECORD")"
echo "    POST /v3/auth/tokens count: $auth_calls (record: $RECORD)"
if [ "$auth_calls" != "1" ]; then
  echo "ASSERT FAIL: expected exactly one token POST, got $auth_calls"
  exit 1
fi

echo
echo "=== phase B (HTTPS listener, wildcard SNI, -k) ==="
code="$(curl -sk -o /dev/null -w '%{http_code}' --max-time 20 \
  --connect-to "web-${UP1}.${BASE}:443:127.0.0.1:$TLS_PORT" \
  "https://web-${UP1}.${BASE}/" 2>/dev/null || true)"
echo "    HTTPS web-${UP1}.${BASE}: HTTP $code"
if [ "$code" != "200" ]; then
  echo "ASSERT FAIL: expected HTTPS 200, got $code"
  exit 1
fi

# --- phase C: forced control-plane failures --------------------------------
echo
echo "=== phase C: forced control-plane failures → 502 ==="
restart_mock --servers-status 500
assert_code "Nova forced 500 (probe) → 502" "502" "probe-${UP1}.${BASE}"

restart_mock --servers-status 403
assert_code "Nova forced 403 (probe2) → 502" "502" "probe2-${UP1}.${BASE}"

echo
echo "ALL P7 OPENSTACK + FIREWALL E2E ASSERTIONS PASSED"
