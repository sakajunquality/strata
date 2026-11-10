#!/usr/bin/env bash
# Runs the server against an S3-compatible store (LocalStack in Docker) and checks
# that pushes, compaction, a restart and clones all work through the S3 API.
# Needs Docker and the AWS CLI. Uses LocalStack 3.8, the last line that runs
# without a licence token; set S3_IMAGE to try another server on port 4566.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
bin="$repo_root/target/debug/strata"
port="${PORT:-18082}"
s3_port="${S3_PORT:-14566}"
url="http://127.0.0.1:$port"
work="$(mktemp -d)"
container="strata-s3-$$"
pid=""

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=smoke GIT_AUTHOR_EMAIL=smoke@example.com
export GIT_COMMITTER_NAME=smoke GIT_COMMITTER_EMAIL=smoke@example.com
# Throwaway credentials for the local container only.
export AWS_ACCESS_KEY_ID=strata-smoke AWS_SECRET_ACCESS_KEY=strata-smoke-secret
export AWS_REGION=us-east-1 AWS_ENDPOINT="http://127.0.0.1:$s3_port" AWS_ALLOW_HTTP=true
unset AWS_PROFILE AWS_SESSION_TOKEN

s3() {
  if command -v aws >/dev/null; then
    aws --endpoint-url "$AWS_ENDPOINT" "$@"
  else
    mise exec -- aws --endpoint-url "$AWS_ENDPOINT" "$@"
  fi
}

stop() {
  [ -n "$pid" ] && kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null || true
  pid=""
}
cleanup() {
  stop
  docker rm -f "$container" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

start() {
  # Small blocks, so that reads take many ranged requests.
  "$bin" --listen "127.0.0.1:$port" --data-dir "$work/data" --scratch-dir "$work" \
    --object-store "s3://strata-smoke/repos" --compact-at 1000 --gc-grace-secs 0 >>"$work/server.log" 2>&1 &
  pid=$!
  for _ in $(seq 1 50); do
    curl -s -o /dev/null "$url/" && return
    sleep 0.1
  done
  echo "server did not start" >&2
  cat "$work/server.log" >&2
  exit 1
}

docker run -d --rm --name "$container" -p "127.0.0.1:$s3_port:4566" -e SERVICES=s3 "${S3_IMAGE:-localstack/localstack:3.8}" >/dev/null
ready=""
for _ in $(seq 1 150); do
  if s3 s3 mb s3://strata-smoke >/dev/null 2>&1; then
    ready=yes
    break
  fi
  sleep 0.4
done
if [ -z "$ready" ]; then
  echo "the S3 server did not become usable:" >&2
  docker logs "$container" 2>&1 | tail -n 20 >&2
  exit 1
fi

start
cd "$work"
curl -sf -X PUT "$url/api/repos/acme/demo" >/dev/null

git init -q -b main src
cd src
# 12 MiB that will not compress: forces a multipart upload and reads across many blocks.
head -c 12582912 /dev/urandom >big.bin
for i in $(seq 1 300); do echo "line $i of the file"; done >notes.txt
git add .
git commit -q -m first
git remote add origin "$url/acme/demo.git"
git push -q origin main
for n in 2 3 4; do
  echo "change $n" >>notes.txt
  git commit -q -am "change $n"
  git push -q origin main
done
cd "$work"

echo "objects in the bucket after 4 pushes:"
s3 s3 ls --recursive s3://strata-smoke | awk '{printf "  %10d  %s\n", $3, $4}'

echo "compaction: $(curl -sf -X POST "$url/api/repos/acme/demo/compact")"

# Nothing may be left in the process: everything below is read back from the bucket.
stop
start

git clone -q "$url/acme/demo.git" clone
git -C clone fsck --strict
[ "$(git -C clone rev-parse HEAD)" = "$(git -C src rev-parse HEAD)" ]
cmp src/big.bin clone/big.bin
git clone -q --depth 1 "$url/acme/demo.git" shallow
[ "$(git -C shallow rev-list --count HEAD)" = 1 ]

# A push after the restart, and a fetch of it.
cd src
echo "after restart" >>notes.txt
git commit -q -am "after restart"
git push -q origin main
cd "$work"
git -C clone pull -q --ff-only
[ "$(git -C clone rev-parse HEAD)" = "$(git -C src rev-parse HEAD)" ]

echo "objects in the bucket at the end (replaced packs are deleted after the next sweep):"
s3 s3 ls --recursive s3://strata-smoke | awk '{printf "  %10d  %s\n", $3, $4}'
# The one warning expected here is that this test runs without authentication.
if grep -E "ERROR|WARN" "$work/server.log" | grep -v -q "no admin token configured"; then
  echo "server log has warnings or errors:" >&2
  grep -E "ERROR|WARN" "$work/server.log" | grep -v "no admin token configured" >&2
  exit 1
fi
echo "OK"
