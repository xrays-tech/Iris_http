#!/usr/bin/env bash
#
# Regenerate the self-signed TLS test certificates used by P4 unit tests and
# `scripts/e2e/04-tls.sh`.
#
# The generated PEM files are committed so tests are reproducible without a
# build-time dependency on the `openssl` CLI; run this script only when the
# fixtures must change.
#
# File names are fixed because `tests/fixtures/sites/*.yaml` reference them:
#   www.example.com.crt/.key  -> forward site `www.example.com`
#   rent.do.top.crt/.key      -> openstack wildcard `*.rent.do.top`
#
# Requires an OpenSSL CLI that supports `-addext` (OpenSSL 1.1.1+).
#
# Re-run from the repository root:
#     scripts/gen-test-certs.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/tests/fixtures/certs"
DAYS=3650

mkdir -p "$OUT"

# gen <file-stem> <CN> <subjectAltName>
gen() {
  local stem="$1" cn="$2" san="$3"
  openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days "$DAYS" \
    -keyout "$OUT/$stem.key" -out "$OUT/$stem.crt" \
    -subj "/CN=$cn" -addext "subjectAltName=$san"
  echo "[cert] wrote $OUT/$stem.crt ($cn; SAN=$san)"
}

gen www.example.com www.example.com "DNS:www.example.com"
gen rent.do.top      rent.do.top      "DNS:*.rent.do.top,DNS:rent.do.top"

echo "done: regenerated self-signed test certificates in $OUT"
