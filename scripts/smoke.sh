#!/usr/bin/env bash
# Smoke test against the real binary: push, restart the server, clone.
# Restarting proves that no state lives in the server process, and
# `git verify-pack` proves that stored packs are plain git packs.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
bin="$repo_root/target/debug/strata"
port="${PORT:-18080}"
url="http://127.0.0.1:$port"
work="$(mktemp -d)"
pid=""

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=smoke GIT_AUTHOR_EMAIL=smoke@example.com
export GIT_COMMITTER_NAME=smoke GIT_COMMITTER_EMAIL=smoke@example.com

start() {
  "$bin" --listen "127.0.0.1:$port" --data-dir "$work/data" >>"$work/server.log" 2>&1 &
  pid=$!
  for _ in $(seq 1 50); do
    curl -s -o /dev/null "$url/" && return
    sleep 0.1
  done
  echo "server did not start" >&2
  exit 1
}

stop() {
  [ -n "$pid" ] && kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null || true
  pid=""
}

trap 'stop; rm -rf "$work"' EXIT

start
cd "$work"

if git ls-remote "$url/acme/demo.git" >/dev/null 2>&1; then
  echo "FAIL: repository exists before it was created" >&2
  exit 1
fi
curl -sf -X PUT "$url/api/repos/acme/demo" >/dev/null

git init -q -b main src
cd src
for i in $(seq 1 300); do echo "line $i of the file"; done >notes.txt
git add .
git commit -q -m first
git remote add origin "$url/acme/demo.git"
git push -q origin main

# A one-line edit: git sends this as a thin pack with a delta against the first push.
for i in $(seq 1 300); do
  if [ "$i" = 150 ]; then echo "line one-fifty of the file"; else echo "line $i of the file"; fi
done >notes.txt
git commit -q -am second
git push -q origin main
cd "$work"

stop
start

git clone -q "$url/acme/demo.git" clone
git -C clone fsck --strict
[ "$(git -C clone rev-parse HEAD)" = "$(git -C src rev-parse HEAD)" ]

echo "stored objects:"
(cd data && find . -type f | sort)
for pack in $(find data/objects -name '*.pack'); do
  echo "--- $(basename "$pack")"
  git verify-pack -v "$pack" | tail -n 6
done
echo "OK"
