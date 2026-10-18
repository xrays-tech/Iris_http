#!/usr/bin/env bash
# Iris HTTP hotspot benchmark — baseline/after comparison (P15).
# Usage: scripts/bench/perf.sh [tag]    (tag usually "baseline" | "after")
# Requires: oha, python3, openssl-free iris release built via cargo build --release.
set -euo pipefail
ulimit -n 8192

TAG="${1:-$(date +%H%M%S)}"
ROOT_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
RELEASE="$HOME/.cargo/global-target/release/iris"
WORK="/tmp/iris-bench-p15"
RESULTS="$WORK/results-$TAG.txt"

echo "== P15 perf: tag=$TAG =="
# Pre-flight: a stale iris/mock/stub from a previous run would bind our ports and
# serve the benchmark (or fail its upstreams -> phantom 502s). Kill them first.
pkill -f "global-target/release/iris" 2>/dev/null || true
pkill -f "mock_openstack.py" 2>/dev/null || true
pkill -f "stubs/upstream.py" 2>/dev/null || true
sleep 1
echo "== building release =="
cargo build --release --manifest-path "$ROOT_DIR/Cargo.toml" -q

rm -rf "$WORK"; mkdir -p "$WORK/config/sites" "$WORK/config/openstack" "$WORK/nova"
exec >> >(tee -a "$RESULTS") 2>&1

# ---- Nova fixtures: appA (weight 2) + appB (weight 1), both floating on loopback
# NOTE: name= list queries must be wrapped in the {"servers": [...]} LIST envelope
# (decode_server_list expects a sequence); a single {"server":{...}} envelope here
# made the bench return 502 (transient decode failure storm).
cat > "$WORK/nova/appA.json" <<'EOF'
{
  "servers": [
    {
      "id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaa1",
      "name": "appA",
      "addresses": {
        "net": [
          { "version": 4, "addr": "127.0.0.1", "OS-EXT-IPS:type": "floating" }
        ]
      }
    }
  ]
}
EOF
cat > "$WORK/nova/appB.json" <<'EOF'
{
  "servers": [
    {
      "id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbb2",
      "name": "appB",
      "addresses": {
        "net": [
          { "version": 4, "addr": "127.0.0.1", "OS-EXT-IPS:type": "floating" }
        ]
      }
    }
  ]
}
EOF

# ---- OpenStack env file (auth url filled by mock port below)
cat > "$WORK/config/openstack/bench-env.yaml" <<'EOF'
timeout_secs: 10
cache_ttl_secs: 3600
auth:
  api_url: http://127.0.0.1:PORT/v3
  approach: password
  username: bench
  password: bench-secret
  project_name: bench
  project_domain_name: Default
  user_domain_name: Default
nova:
  all_tenants: true
EOF

# ---- forward sites
cat > "$WORK/config/sites/bench.yaml" <<'EOF'
name: bench
mode: forward
domain: bench.test
http: true
https: false
backends:
  - addr: 127.0.0.1:18099
load_balancer: round_robin
EOF
cat > "$WORK/config/sites/bench-ii.yaml" <<'EOF'
name: bench-ii
mode: forward
domain: bench-ii.test
http: true
https: false
openstack_env: bench-env
backends:
  - instance: appA
    port: 18099
    weight: 2
  - instance: appB
    port: 18098
    weight: 1
load_balancer: ketama
EOF

# ---- control plane
python3 "$ROOT_DIR/scripts/mock_openstack.py" --port 0 --servers-dir "$WORK/nova" --record "$WORK/ctrl.log" \
  > "$WORK/mock.out" 2>&1 &
MOCK_PID=$!
for _ in $(seq 1 100); do grep -q LISTENING "$WORK/mock.out" 2>/dev/null && break; sleep 0.1; done
MOCK_PORT="$(grep -o 'LISTENING [0-9]*' "$WORK/mock.out" | head -1 | awk '{print $2}')"
sed -i '' "s/:PORT/:$MOCK_PORT/" "$WORK/config/openstack/bench-env.yaml"
echo "== mock on :$MOCK_PORT (pid $MOCK_PID) =="

# ---- upstream stubs
python3 "$ROOT_DIR/tests/stubs/upstream.py" --port 18099 > "$WORK/stubA.out" 2>&1 &
PIDA=$!
python3 "$ROOT_DIR/tests/stubs/upstream.py" --port 18098 > "$WORK/stubB.out" 2>&1 &
PIDB=$!

# ---- iris
IRIS_HTTP_LISTEN_ADDR=127.0.0.1:18080 IRIS_HTTP_METRICS_ADDR=127.0.0.1:19091 \
  "$RELEASE" run --config-dir "$WORK/config" --pidfile "$WORK/iris.pid" > "$WORK/iris.out" 2>&1 &
IRIS_PID=$!
for _ in $(seq 1 100); do curl -fsS -o /dev/null http://127.0.0.1:19091/healthz 2>/dev/null && break; sleep 0.1; done
# Verify the freshly spawned process actually owns the listeners (a stale process
# would also answer /healthz while ours sits in an "in use, will try again" loop).
sleep 1
LISTENER_PID="$(lsof -nP -iTCP:18080 -sTCP:LISTEN -t 2>/dev/null | head -1 || true)"
if [ -z "${LISTENER_PID:-}" ] || [ "$LISTENER_PID" != "$IRIS_PID" ]; then
  echo "FATAL: listener :18080 owned by pid ${LISTENER_PID:-<none>}, expected $IRIS_PID (stale process?) — aborting"; exit 1
fi
if ! ps -p "$IRIS_PID" >/dev/null 2>&1; then echo "FATAL: iris (pid $IRIS_PID) exited"; tail -5 "$WORK/iris.out"; exit 1; fi
echo "== iris up (pid $IRIS_PID), warming resolvers =="
curl -s -H "Host: bench-ii.test" -o /dev/null http://127.0.0.1:18080/  || true
curl -s -H "Host: bench.test" -o /dev/null http://127.0.0.1:18080/  || true

run_oha() {
  local name="$1" concurrency="$2" host="$3"
  local out="/tmp/iris-bench-$TAG-$name-c$concurrency.txt"
  # stdout = JSON results only; stderr (e.g. macOS DNS resolver warning) stays out of the file.
  oha -z 6s -c "$concurrency" --output-format json -H "Host: $host" "http://127.0.0.1:18080/" > "$out" 2>"$out.err" || true
  # oha JSON: summary{requestsPerSec,successRate,...} + latencyPercentiles{p50,p90,p99,...}.
  local line
  line=$(python3 - "$out" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
s = d.get("summary") or {}
rps = s.get("requestsPerSec"); succ = s.get("successRate")
lp = d.get("latencyPercentiles") or {}
print(rps, succ, lp.get("p50"), lp.get("p90"), lp.get("p99"))
PY
  )
  local rps p50 p90 p99 succ
  read -r rps succ p50 p90 p99 <<< "$line"
  printf '%-16s c=%-4s rps=%-9s p50=%-9s p90=%-9s p99=%-9s succ=%-7s\n' \
    "$name" "$concurrency" "${rps:-?}" "${p50:-?}" "${p90:-?}" "${p99:-?}" "${succ:-?}"
}

echo "== scenarios (S1..S4) =="
run_oha S1-static-rr 50  bench.test
run_oha S2-static-rr 200 bench.test
run_oha S3-ketama-inst 50  bench-ii.test
run_oha S4-ketama-inst 200 bench-ii.test

kill "$IRIS_PID" "$PIDA" "$PIDB" "$MOCK_PID" 2>/dev/null || true
echo "== done; full oha outputs under /tmp/iris-bench-$TAG-*; summary: $RESULTS =="
