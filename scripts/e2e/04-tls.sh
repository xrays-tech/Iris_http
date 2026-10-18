#!/usr/bin/env bash
#
# P4 TLS/SNI + HTTP/2 end-to-end test (plan/07 §端到端).
#
# Boots the upstream stub, writes a throwaway config (one HTTPS forward site,
# one plain-HTTP forward site, one openstack wildcard site) into
# `target/e2e-p4/`, runs `iris` on a plain + a TLS listener, and drives the TLS
# listener with `openssl s_client` / `curl`:
#
#   * per-SNI certificate selection (forward exact + openstack wildcard),
#   * ALPN HTTP/2 negotiation,
#   * handshake failure for an unknown SNI (no HTTP status possible),
#   * 421 Misdirected Request for a Host that does not belong to the SNI.
#
# Re-run from the repository root:
#     scripts/e2e/04-tls.sh
#
# The script builds the `iris` binary itself. Override the ports with
# `IRIS_E2E_HTTP_PORT=<port>` / `IRIS_E2E_TLS_PORT=<port>`. All output is
# deterministic: every assertion prints what it observed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

UP1="${IRIS_E2E_UP1:-18091}"
HTTP_PORT="${IRIS_E2E_HTTP_PORT:-18092}"
TLS_PORT="${IRIS_E2E_TLS_PORT:-18443}"
CONFIG_DIR="target/e2e-p4/config"
LOG_DIR="target/e2e-p4"
PIDFILE="$LOG_DIR/iris.pid"
CERTS="$ROOT/tests/fixtures/certs"

mkdir -p "$LOG_DIR"

echo "[build] cargo build --bin iris"
cargo build --bin iris --quiet
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
IRIS_BIN="$TARGET_DIR/debug/iris"
echo "[build] binary: $IRIS_BIN"

STUB_PID=""
IRIS_PID=""

cleanup() {
  if [ -n "$IRIS_PID" ]; then kill "$IRIS_PID" 2>/dev/null || true; fi
  if [ -n "$STUB_PID" ]; then kill "$STUB_PID" 2>/dev/null || true; fi
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Throwaway config
# ---------------------------------------------------------------------------
mkdir -p "$CONFIG_DIR/sites" "$CONFIG_DIR/openstack" "$CONFIG_DIR/firewall"

# openstack sites require a firewall_dir (P16); the wildcard site below is only
# used for SNI certificate selection, but the role dir must still exist.
cat > "$CONFIG_DIR/firewall/INS001.role" <<EOF
allowed_ports:
  - 80
EOF

cat > "$CONFIG_DIR/openstack/prod.yaml" <<EOF
timeout_secs: 5
cache_ttl_secs: 86400
auth:
  api_url: https://controller.example.com:5000/v3
  approach: application_credential
  application_credential_id: e2e-id
  application_credential_secret: e2e-secret
nova: {}
EOF

cat > "$CONFIG_DIR/sites/001-forward-https.yaml" <<EOF
name: www-example
mode: forward
domain: www.example.com
http: true
https: true
cert_file: $CERTS/www.example.com.crt
key_file: $CERTS/www.example.com.key
backends:
  - addr: 127.0.0.1:$UP1
EOF

cat > "$CONFIG_DIR/sites/002-forward-http-only.yaml" <<EOF
name: plain
mode: forward
domain: plain.example.com
http: true
https: false
backends:
  - addr: 127.0.0.1:$UP1
EOF

cat > "$CONFIG_DIR/sites/003-openstack-wildcard.yaml" <<EOF
name: rent
mode: openstack
base_domain: rent.do.top
http: true
https: true
cert_file: $CERTS/rent.do.top.crt
key_file: $CERTS/rent.do.top.key
firewall_dir: $CONFIG_DIR/firewall
openstack_env: prod
EOF

cat > "$CONFIG_DIR/sites/004-forward-https-only.yaml" <<EOF
name: secure
mode: forward
domain: secure.example.com
http: false
https: true
cert_file: $CERTS/www.example.com.crt
key_file: $CERTS/www.example.com.key
backends:
  - addr: 127.0.0.1:$UP1
EOF

echo "[stub] starting upstream on $UP1"
python3 tests/stubs/upstream.py --port "$UP1" > "$LOG_DIR/stub-$UP1.log" 2>&1 &
STUB_PID=$!

echo "[iris] http=127.0.0.1:$HTTP_PORT https=127.0.0.1:$TLS_PORT (config-dir=$CONFIG_DIR)"
IRIS_HTTP_LISTEN_ADDR="127.0.0.1:$HTTP_PORT" \
  IRIS_HTTP_LISTEN_TLS_ADDR="127.0.0.1:$TLS_PORT" \
  "$IRIS_BIN" run --config-dir "$CONFIG_DIR" --pidfile "$PIDFILE" \
  > "$LOG_DIR/iris.log" 2>&1 &
IRIS_PID=$!

ready=0
for _ in $(seq 1 50); do
  if curl -sf -o /dev/null -H 'Host: www.example.com' "http://127.0.0.1:$HTTP_PORT/"; then
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

# Print the subject of the certificate served for `sni`.
served_subject() {
  local sni="$1"
  { openssl s_client -connect "127.0.0.1:$TLS_PORT" -servername "$sni" </dev/null 2>/dev/null || true; } \
    | openssl x509 -noout -subject 2>/dev/null
}

echo
echo "=== per-SNI certificate: forward exact (SNI=www.example.com) ==="
subj="$(served_subject www.example.com)"
echo "    $subj"
assert_contains "$subj" "www.example.com"

echo
echo "=== ALPN negotiation (SNI=www.example.com, -alpn h2) ==="
alpn="$({ openssl s_client -connect "127.0.0.1:$TLS_PORT" -servername www.example.com -alpn h2 </dev/null 2>/dev/null || true; } \
  | tr -d '\r' | grep -i 'ALPN protocol' || true)"
echo "    $alpn"
assert_contains "$alpn" "h2"

echo
echo "=== per-SNI certificate: openstack wildcard (SNI=INS001-80.rent.do.top) ==="
subj="$(served_subject INS001-80.rent.do.top)"
echo "    $subj"
assert_contains "$subj" "rent.do.top"
san="$({ openssl s_client -connect "127.0.0.1:$TLS_PORT" -servername INS001-80.rent.do.top </dev/null 2>/dev/null || true; } \
  | openssl x509 -noout -ext subjectAltName 2>/dev/null || true)"
echo "    SAN:$(printf '%s' "$san" | tr '\n' ' ')"
assert_contains "$san" "*.rent.do.top"

echo
echo "=== unknown SNI (SNI=nope.example.com) — expect handshake failure ==="
fail_out="$({ openssl s_client -connect "127.0.0.1:$TLS_PORT" -servername nope.example.com </dev/null 2>&1 || true; } || true)"
if printf '%s' "$fail_out" | grep -q 'BEGIN CERTIFICATE'; then
  echo "ASSERT FAIL: a certificate was presented for an unmapped SNI"
  printf '%s\n' "$fail_out" | sed 's/^/    /'
  exit 1
fi
echo "    no certificate presented (handshake aborted, no HTTP status possible)"

echo
echo "=== HTTPS forward happy path over HTTP/2 (SNI=Host=www.example.com) ==="
resp="$(curl -sk -o /dev/null -w '%{http_code} %{http_version}' \
  --connect-to "www.example.com:443:127.0.0.1:$TLS_PORT" \
  "https://www.example.com/" || true)"
echo "    HTTP $resp"
code="${resp%% *}"
if [ "$code" != "200" ]; then
  echo "ASSERT FAIL: expected 200, got $resp"
  exit 1
fi

echo
echo "=== 421 (SNI=www.example.com, Host=plain.example.com http-only) ==="
code="$(curl -sk -o /dev/null -w '%{http_code}' \
  --connect-to "www.example.com:443:127.0.0.1:$TLS_PORT" \
  -H 'Host: plain.example.com' \
  "https://www.example.com/" || true)"
echo "    HTTP $code"
if [ "$code" != "421" ]; then
  echo "ASSERT FAIL: expected 421, got $code"
  exit 1
fi

echo
echo "=== HTTP listener still plain (Host=www.example.com over :$HTTP_PORT) ==="
code="$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: www.example.com' "http://127.0.0.1:$HTTP_PORT/" || true)"
echo "    HTTP $code"
if [ "$code" != "200" ]; then
  echo "ASSERT FAIL: expected 200, got $code"
  exit 1
fi

echo
echo "=== cleartext + http:false (Host=secure.example.com over :$HTTP_PORT) — expect 404 (P16) ==="
code="$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: secure.example.com' "http://127.0.0.1:$HTTP_PORT/" || true)"
echo "    HTTP $code"
if [ "$code" != "404" ]; then
  echo "ASSERT FAIL: an http:false site must not be served on the cleartext listener (got $code)"
  exit 1
fi

echo
echo "ALL P4 TLS/SNI E2E ASSERTIONS PASSED"
