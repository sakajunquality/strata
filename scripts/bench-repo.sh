#!/usr/bin/env bash
# Times the server against a real repository: a push of all of a branch's
# history, clones of several kinds before and after compaction, and a few
# API calls. Prints wall-clock time, the server's CPU time so far and its
# resident memory after each step. Clones do not check out, so the times are
# the server's and the transfer's (and git indexing what it received).
#
#   git clone --bare https://github.com/git/git /tmp/git.git
#   cargo build --release && ./scripts/bench-repo.sh /tmp/git.git master
#
# Synthetic histories (scripts/bench.sh) are too regular to show much: this
# is what found the slow paths, and an object lost from bitmaps.
set -uo pipefail

source_repo="$(cd "${1:?usage: bench-repo.sh <bare repository> [branch]}" && pwd)"
branch="${2:-master}"
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
bin="${STRATA_BIN:-$repo_root/target/release/strata}"
port="${PORT:-18082}"
work="$(mktemp -d)"
url="http://127.0.0.1:$port/bench/real.git"
api="http://127.0.0.1:$port/api/repos/bench/real"
pid=""

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1

stop() {
  [ -n "$pid" ] && kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null || true
  pid=""
}
trap 'stop; rm -rf "$work"' EXIT

start() {
  "$bin" --listen "127.0.0.1:$port" --data-dir "$work/data" --auto-create --scratch-dir "$work" >>"$work/server.log" 2>&1 &
  pid=$!
  for _ in $(seq 1 50); do
    curl -s -o /dev/null "http://127.0.0.1:$port/healthz" && return
    sleep 0.1
  done
  echo "server did not start" >&2
  exit 1
}

# measure <label> <command...>
measure() {
  local label="$1" started
  shift
  started=$(python3 -c 'import time; print(time.time())')
  if ! "$@" >/dev/null 2>"$work/stderr"; then
    echo "FAILED: $label"
    tail -3 "$work/stderr"
  fi
  python3 - "$label" "$started" "$(ps -o time= -p "$pid")" "$(ps -o rss= -p "$pid")" <<'EOF'
import sys, time
label, started, cpu, rss = sys.argv[1], float(sys.argv[2]), sys.argv[3].strip(), int(sys.argv[4]) // 1024
print(f"{label:<44} {time.time() - started:7.2f} s   server cpu {cpu:>8}   rss {rss:>5} MiB")
EOF
}

stored() {
  find "$work/data/objects" -name '*.pack' -exec du -ck {} + | tail -1 | awk '{printf "  stored packs: %.0f MiB\n", $1 / 1024}'
}

mkdir -p "$work/data"
echo "$(git -C "$source_repo" rev-list --count "$branch") commits, $(git -C "$source_repo" rev-list --objects "$branch" | wc -l | tr -d ' ') objects on $branch"
start
measure "push (all history, one pack)" git -C "$source_repo" push -q "$url" "$branch:refs/heads/main"
stored

# Restart between steps that should start cold.
stop
start
cd "$work"
measure "full clone, before compaction (walk)" git clone -q --bare "$url" walked
measure "compact" curl -sf -X POST "$api/compact"
stop
start
measure "full clone, after compaction (bitmaps)" git clone -q --bare "$url" full
git -C full fsck --connectivity-only >/dev/null 2>&1 || echo "FAILED: the clone is not complete"
measure "clone --depth 1" git clone -q --bare --depth 1 "$url" shallow
measure "clone --filter=blob:none" git clone -q --bare --filter=blob:none "$url" blobless
measure "clone --filter=blob:limit=10k" git clone -q --bare --filter=blob:limit=10k "$url" limited
measure "clone --filter=tree:0" git clone -q --bare --filter=tree:0 "$url" treeless
git -C full update-ref refs/heads/main main~500
measure "fetch 500 commits" git -C full fetch -q "$url" main
measure "commits API, first page" curl -sf "$api/commits?limit=30"
measure "commits API, 50000 commits in" curl -sf "$api/commits?limit=30&skip=50000"
measure "compare API, 2000 commits apart" curl -sf "$api/compare?base=$(git -C full rev-parse main~2000)&head=main"
stored
