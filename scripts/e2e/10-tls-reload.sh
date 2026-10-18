#!/usr/bin/env bash
#
# FIX #1 — TLS certificate hot-reload (reload.md §3/§4).
#
# Proves that a certificate added by a hot reload is served on the very next
# handshake: the TLS `SniCertSource` reads the SAME `Snapshot` the proxy and the
# reload path use, so there is no private certificate store that could keep
# serving the old table forever.
#
# Scenario:
#   1. boot with one HTTPS forward site (`www.example.com` cert) and no
#      openstack wildcard certificate configured;
#   2. assert a handshake for `INS001-80.rent.do.top` ABORTS (no certificate);
#   3. hot-reload a config that ADDS the SSL openstack site (`*.rent.do.top`);
#   4. assert the same SNI now SUCCEEDS and serves CN/SAN `*.rent.do.top`.
#
# Re-run from the repository root:
#     scripts/e2e/10-tls-reload.sh
#
# Ports are fixed and hermetic (nothing committed is mutated):
#   upstream 18451 · HTTP 18452 · TLS 18453 · metrics 18491.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

UP=18451
HTTP_PORT=18452
TLS_PORT=18453
METRICS_PORT=18491
CERTS="$ROOT/tests/fixtures/certs"

WORK="target/e2e-p10"
CFG_DIR="$WORK/config"
SITES_DIR="$CFG_DIR/sites"
ENVS_DIR="$CFG_DIR/openstack"
LOG_DIR="$WORK/logs"
PIDFILE="$WORK/iris.pid"

rm -rf "$WORK"
mkdir -p "$SITES_DIR" "$ENVS_DIR" "$LOG_DIR"

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

# --- hermetic fixtures ------------------------------------------------------

# Config A: a single HTTPS forward site. The openstack wildcard is absent.
cat > "$SITES_DIR/001-forward-https.yaml" <<YAML
name: www-example
mode: forward
domain: www.example.com
http: true
https: true
cert_file: $CERTS/www.example.com.crt
key_file: $CERTS/www.example.com.key
backends:
  - addr: 127.0.0.1:$UP
YAML

# Added by the reload: the SSL openstack wildcard site.
write_openstack_site() {
  cat > "$ENVS_DIR/prod.yaml" <<YAML
timeout_secs: 5
cache_ttl_secs: 86400
auth:
  api_url: https://controller.example.com:5000/v3
  approach: application_credential
  application_credential_id: e2e-id
  application_credential_secret: e2e-secret
nova: {}
YAML
  cat > "$SITES_DIR/003-openstack-wildcard.yaml" <<YAML
name: rent
mode: openstack
base_domain: rent.do.top
http: true
https: true
cert_file: $CERTS/rent.do.top.crt
key_file: $CERTS/rent.do.top.key
openstack_env: prod
YAML
}

# --- helpers ----------------------------------------------------------------

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

# Whether a certificate is presented for `sni` (i.e. the handshake succeeded).
cert_present() { # $1=sni
  { openssl s_client -connect "127.0.0.1:$TLS_PORT" -servername "$1" </dev/null 2>/dev/null || true; } \
    | grep -q 'BEGIN CERTIFICATE'
}

# The subject of the certificate served for `sni` (empty when the handshake aborts).
served_subject() { # $1=sni
  { openssl s_client -connect "127.0.0.1:$TLS_PORT" -servername "$1" </dev/null 2>/dev/null || true; } \
    | openssl x509 -noout -subject 2>/dev/null || true
}

served_san() { # $1=sni
  { openssl s_client -connect "127.0.0.1:$TLS_PORT" -servername "$1" </dev/null 2>/dev/null || true; } \
    | openssl x509 -noout -ext subjectAltName 2>/dev/null || true
}

# --- start upstream + iris --------------------------------------------------

echo "[stub] starting upstream on $UP"
python3 tests/stubs/upstream.py --port "$UP" > "$LOG_DIR/stub-$UP.log" 2>&1 &
STUB_PID=$!

echo "[iris] http=127.0.0.1:$HTTP_PORT https=127.0.0.1:$TLS_PORT metrics=127.0.0.1:$METRICS_PORT"
IRIS_HTTP_LISTEN_ADDR="127.0.0.1:$HTTP_PORT" \
  IRIS_HTTP_LISTEN_TLS_ADDR="127.0.0.1:$TLS_PORT" \
  "$IRIS_BIN" run --config-dir "$CFG_DIR" --pidfile "$PIDFILE" \
  --metrics-listen "127.0.0.1:$METRICS_PORT" > "$LOG_DIR/iris.log" 2>&1 &
IRIS_PID=$!

ready=0
for _ in $(seq 1 60); do
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

# --- baseline: forward cert served, openstack SNI aborts --------------------

echo
echo "=== baseline (config A): forward cert present, openstack SNI unmapped ==="
if ! cert_present www.example.com; then
  echo "ASSERT FAIL: the forward certificate was not served at startup"
  exit 1
fi
printf '    %-54s %s\n' "SNI www.example.com" "certificate presented"

if cert_present INS001-80.rent.do.top; then
  echo "ASSERT FAIL: a certificate was presented for an unmapped SNI BEFORE the reload"
  exit 1
fi
printf '    %-54s %s\n' "SNI INS001-80.rent.do.top (before reload)" "handshake aborted (no cert)"

# --- hot reload: add the openstack wildcard site ----------------------------

echo
echo "=== hot reload (config B: add openstack wildcard site 'rent') ==="
write_openstack_site
"$IRIS_BIN" reload --pidfile "$PIDFILE"
wait_for_metric 'config_reload_total{ok="true"} 1' 'config_reload_total{ok="true"} 1'

echo
echo "=== post-reload: the new certificate is served on the next handshake ==="
if ! cert_present INS001-80.rent.do.top; then
  echo "ASSERT FAIL: the reloaded certificate was NOT served for INS001-80.rent.do.top"
  echo "---- iris log (tail) ----"; tail -20 "$LOG_DIR/iris.log"; echo "------------------------"
  exit 1
fi
subj="$(served_subject INS001-80.rent.do.top)"
printf '    %-54s %s\n' "SNI INS001-80.rent.do.top subject" "$subj"
case "$subj" in
  *rent.do.top*) ;;
  *)
    echo "ASSERT FAIL: expected the rent.do.top certificate, got: $subj"
    exit 1
    ;;
esac

san="$(served_san INS001-80.rent.do.top)"
printf '    %-54s %s\n' "SNI INS001-80.rent.do.top SAN" "$(printf '%s' "$san" | tr '\n' ' ')"
case "$san" in
  *'*.rent.do.top'*) ;;
  *)
    echo "ASSERT FAIL: expected SAN *.rent.do.top, got: $san"
    exit 1
    ;;
esac

# The pre-existing forward certificate must still be served after the reload.
if ! cert_present www.example.com; then
  echo "ASSERT FAIL: the forward certificate broke after the reload"
  exit 1
fi
printf '    %-54s %s\n' "SNI www.example.com (after reload)" "certificate presented"

echo
echo "ALL TLS-RELOAD E2E ASSERTIONS PASSED"
