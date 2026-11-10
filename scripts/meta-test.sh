#!/usr/bin/env bash
# Checks the shared metadata stores against real servers in Docker:
# PostgreSQL and the Cloud Spanner emulator. For each, runs the backend
# conformance tests, then two server processes sharing the database and one
# pack directory, with git talking to both.
# Usage: meta-test.sh [postgres|spanner]...   (default: both)
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
bin="$repo_root/target/debug/strata"
cargo="cargo"
command -v cargo >/dev/null || cargo="mise exec -- cargo"
work="$(mktemp -d)"
pg_port="${PG_PORT:-15432}"
spanner_grpc="${SPANNER_GRPC_PORT:-19010}"
spanner_rest="${SPANNER_REST_PORT:-19020}"
port_a="${PORT_A:-18083}"
port_b="${PORT_B:-18084}"
containers=()
pids=()

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=smoke GIT_AUTHOR_EMAIL=smoke@example.com
export GIT_COMMITTER_NAME=smoke GIT_COMMITTER_EMAIL=smoke@example.com

stop_nodes() {
  for pid in ${pids[@]+"${pids[@]}"}; do
    kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null || true
  done
  pids=()
}
cleanup() {
  stop_nodes
  for c in ${containers[@]+"${containers[@]}"}; do docker rm -f "$c" >/dev/null 2>&1 || true; done
  rm -rf "$work"
}
trap cleanup EXIT

start_node() { # port, log name; STRATA_META must be set
  "$bin" --listen "127.0.0.1:$1" --data-dir "$work/unused-$2" --scratch-dir "$work" \
    --object-store "file://$dir/objects" --compact-at 1000 >>"$dir/$2.log" 2>&1 &
  pids+=($!)
  for _ in $(seq 1 100); do
    curl -s -o /dev/null "http://127.0.0.1:$1/" && return
    sleep 0.1
  done
  echo "node $2 did not start" >&2
  cat "$dir/$2.log" >&2
  exit 1
}

# Two nodes, one database, one pack directory.
two_nodes() { # label
  dir="$work/$1"
  mkdir -p "$dir/objects"
  local a="http://127.0.0.1:$port_a" b="http://127.0.0.1:$port_b"
  start_node "$port_a" node-a
  start_node "$port_b" node-b
  cd "$dir"

  # Created through one node, pushed through the other.
  curl -sf -X PUT "$a/api/repos/acme/demo" >/dev/null
  git init -q -b main src
  cd src
  for i in $(seq 1 200); do echo "line $i"; done >notes.txt
  git add .
  git commit -q -m first
  git remote add a "$a/acme/demo.git"
  git remote add b "$b/acme/demo.git"
  git push -q b main
  cd "$dir"
  git clone -q "$a/acme/demo.git" clone
  [ "$(git -C clone rev-parse HEAD)" = "$(git -C src rev-parse HEAD)" ]

  # Alternating nodes: each must see what the other just committed.
  cd src
  for n in 1 2 3 4 5 6; do
    echo "change $n" >>notes.txt
    git commit -q -am "change $n"
    if [ $((n % 2)) = 0 ]; then git push -q a main; else git push -q b main; fi
  done
  # A push based on stale history is refused whichever node it reaches.
  git -C "$dir/clone" commit -q --allow-empty -m stale
  if git -C "$dir/clone" push -q "$b/acme/demo.git" main 2>/dev/null; then
    echo "a non-fast-forward push was accepted" >&2
    exit 1
  fi

  # Different branches pushed through both nodes at the same moment.
  git branch left && git branch right
  git push -q a left &
  local left=$!
  git push -q b right
  wait "$left"
  cd "$dir"
  [ "$(git ls-remote "$a/acme/demo.git" | wc -l | tr -d ' ')" = 4 ]
  [ "$(git ls-remote "$a/acme/demo.git")" = "$(git ls-remote "$b/acme/demo.git")" ]

  # Compacted by one node, read through the other.
  echo "  compaction via node b: $(curl -sf -X POST "$b/api/repos/acme/demo/compact")"
  git clone -q "$a/acme/demo.git" after
  git -C after fsck --strict
  [ "$(git -C after rev-parse HEAD)" = "$(git -C src rev-parse HEAD)" ]
  git clone -q --depth 1 "$b/acme/demo.git" shallow
  [ "$(git -C shallow rev-list --count HEAD)" = 1 ]

  # Both nodes restarted: nothing lived only in a process.
  stop_nodes
  start_node "$port_a" node-a
  git -C clone fetch -q origin
  [ "$(git -C clone rev-parse origin/main)" = "$(git -C src rev-parse HEAD)" ]
  stop_nodes

  if grep -hE "ERROR|WARN" "$dir"/node-*.log | grep -v "no admin token configured" >&2; then
    echo "server logs have warnings or errors" >&2
    exit 1
  fi
  cd "$repo_root"
}

# Runs one backend's tests; a skipped test counts as a failure here.
conformance() { # module, VAR=value
  local out
  if ! out="$(cd "$repo_root" && env "$2" $cargo test -q -p strata-store "$1::" -- --nocapture 2>&1)" ||
    grep -q "skipped" <<<"$out" || ! grep -q "1 passed" <<<"$out"; then
    echo "$out" | tail -n 30 >&2
    exit 1
  fi
  echo "  $(grep "test result" <<<"$out")"
}

run_postgres() {
  local c="strata-pg-$$"
  containers+=("$c")
  docker run -d --rm --name "$c" -p "127.0.0.1:$pg_port:5432" -e POSTGRES_PASSWORD=strata-test "${PG_IMAGE:-postgres:17}" >/dev/null
  # The image restarts the server once while initialising; wait for the final one.
  for _ in $(seq 1 150); do
    if docker logs "$c" 2>&1 | grep -q "PostgreSQL init process complete" &&
      docker exec "$c" pg_isready -q -h 127.0.0.1 -U postgres 2>/dev/null; then break; fi
    sleep 0.4
  done
  local url="postgres://postgres:strata-test@127.0.0.1:$pg_port/postgres?sslmode=disable"
  echo "postgres: conformance tests"
  conformance postgres STRATA_TEST_POSTGRES="$url"
  echo "postgres: two nodes"
  STRATA_META="$url" two_nodes postgres
  docker exec "$c" psql -U postgres -Atc "SELECT 'rows: repos=' || (SELECT count(*) FROM repos) || ' refs=' || (SELECT count(*) FROM refs) || ' packs=' || (SELECT count(*) FROM packs) || ' garbage=' || (SELECT count(*) FROM garbage)" | sed 's/^/  /'
}

run_spanner() {
  local c="strata-spanner-$$"
  containers+=("$c")
  docker run -d --rm --name "$c" -p "127.0.0.1:$spanner_grpc:9010" -p "127.0.0.1:$spanner_rest:9020" \
    "${SPANNER_IMAGE:-gcr.io/cloud-spanner-emulator/emulator}" >/dev/null
  local rest="http://127.0.0.1:$spanner_rest/v1/projects/strata-test"
  local ready=""
  for _ in $(seq 1 150); do
    if curl -sf -X POST "$rest/instances" -H 'Content-Type: application/json' \
      -d '{"instanceId":"test","instance":{"config":"emulator-config","displayName":"test","nodeCount":1}}' >/dev/null 2>&1; then
      ready=yes
      break
    fi
    sleep 0.4
  done
  [ -n "$ready" ] || { echo "the Spanner emulator did not become usable" >&2; docker logs "$c" 2>&1 | tail -n 20 >&2; exit 1; }
  export SPANNER_EMULATOR_HOST="127.0.0.1:$spanner_grpc"
  local db
  for db in conformance nodes; do
    curl -sf -X POST "$rest/instances/test/databases" -H 'Content-Type: application/json' \
      -d "{\"createStatement\":\"CREATE DATABASE \`$db\`\"}" >/dev/null
  done
  local path="projects/strata-test/instances/test/databases"
  echo "spanner (emulator): conformance tests"
  conformance spanner STRATA_TEST_SPANNER="$path/conformance"
  echo "spanner (emulator): two nodes"
  STRATA_META="spanner://$path/nodes" two_nodes spanner
  unset SPANNER_EMULATOR_HOST
}

(cd "$repo_root" && $cargo build -q)
[ $# -gt 0 ] || set -- postgres spanner
for backend in "$@"; do
  case "$backend" in
    postgres) run_postgres ;;
    spanner) run_spanner ;;
    *) echo "unknown backend: $backend" >&2; exit 2 ;;
  esac
done
echo "OK"
