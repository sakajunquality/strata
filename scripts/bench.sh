#!/usr/bin/env bash
# Rough scale check against a synthetic repository. Prints wall-clock time of
# common operations and the server's resident memory after each one. Clones
# skip the checkout unless stated, so the times are the server's and the
# transfer's, not the client writing files.
#
#   FILES=50000 COMMITS=100 ./scripts/bench.sh
#
# Uses the release binary: run `cargo build --release` first.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
bin="$repo_root/target/release/strata"
port="${PORT:-18081}"
files="${FILES:-50000}"
commits="${COMMITS:-100}"
block_cache_mb="${BLOCK_CACHE_MB:-64}"
object_cache_mb="${OBJECT_CACHE_MB:-64}"
url="http://127.0.0.1:$port/bench/mono.git"
work="$(mktemp -d)"
pid=""

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=bench GIT_AUTHOR_EMAIL=bench@example.com
export GIT_COMMITTER_NAME=bench GIT_COMMITTER_EMAIL=bench@example.com

stop() {
  [ -n "$pid" ] && kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null || true
  pid=""
}
trap 'stop; rm -rf "$work"' EXIT

start() {
  "$bin" --listen "127.0.0.1:$port" --data-dir "$work/data" --auto-create --scratch-dir "$work" \
    --block-cache-mb "$block_cache_mb" --object-cache-mb "$object_cache_mb" >>"$work/server.log" 2>&1 &
  pid=$!
  for _ in $(seq 1 50); do
    curl -s -o /dev/null "http://127.0.0.1:$port/" && return
    sleep 0.1
  done
  echo "server did not start" >&2
  exit 1
}

rss_mb() { echo $(($(ps -o rss= -p "$pid") / 1024)); }

# measure <label> <command...>: prints the label, seconds taken, and server memory afterwards.
measure() {
  local label="$1" started
  shift
  started=$(python3 -c 'import time; print(time.time())')
  "$@" >/dev/null 2>&1
  python3 - "$label" "$started" "$(rss_mb)" <<'EOF'
import sys, time
label, started, rss = sys.argv[1], float(sys.argv[2]), sys.argv[3]
print(f"{label:<44} {time.time() - started:7.2f} s   server RSS {rss:>5} MiB")
EOF
}

echo "generating $files files, $commits commits..."
git init -q -b main "$work/src"
cd "$work/src"
python3 - "$files" <<'EOF'
import os, random, sys
random.seed(1)
n = int(sys.argv[1])
words = [("%08x" % random.getrandbits(32)) for _ in range(5000)]
for i in range(n):
    d = f"pkg{i // 100 // 20:03d}/mod{i // 100:04d}"
    os.makedirs(d, exist_ok=True)
    with open(f"{d}/file{i:06d}.txt", "w") as f:
        f.write("\n".join(" ".join(random.choices(words, k=8)) for _ in range(12)) + "\n")
EOF
git add .
git commit -q -m "import"
python3 - "$files" "$commits" <<'EOF'
import random, subprocess, sys
random.seed(2)
n, commits = int(sys.argv[1]), int(sys.argv[2])
for c in range(commits):
    for i in random.sample(range(n), 20):
        with open(f"pkg{i // 100 // 20:03d}/mod{i // 100:04d}/file{i:06d}.txt", "a") as f:
            f.write(f"change {c}\n")
    subprocess.run(["git", "commit", "-q", "-am", f"change {c}"], check=True)
EOF
git remote add origin "$url"
# git may still be repacking in the background, so files can vanish under du.
echo "local .git size: $(du -sh .git 2>/dev/null | cut -f1)"

start
echo "caches: block ${block_cache_mb} MiB, object ${object_cache_mb} MiB; server RSS at start $(rss_mb) MiB"
echo
measure "push (whole history, one pack)" git push -q origin main

# Restart so that nothing is warm: every read below starts from object storage.
stop
start
cd "$work"
measure "full clone, before compaction (walk)" git clone -q --bare "$url" full-walk
measure "compact (commit-graph + bitmaps)" curl -sf -X POST "http://127.0.0.1:$port/api/repos/bench/mono/compact"
stop
start
measure "full clone, after compaction (bitmap, cold)" git clone -q --bare "$url" full
measure "clone --depth 1 --no-checkout" git clone -q --depth 1 --no-checkout "$url" shallow
measure "clone --depth 1 with checkout" git clone -q --depth 1 "$url" shallow-checkout
measure "clone --depth 1 --filter=blob:none --no-checkout" git clone -q --depth 1 --filter=blob:none --no-checkout "$url" blobless

# A burst of small pushes, as a busy repository would see. Compaction runs
# behind them and should keep the number of packs small.
cd "$work/src"
burst() {
  for i in $(seq 1 "$1"); do
    echo "burst change $i" >>"pkg000/mod0000/file00000$((i % 10)).txt"
    git commit -q -am "burst $i"
    git push -q origin main
  done
}
measure "30 pushes of 1 commit each" burst 30
sleep 2
packs_after_burst=$(find "$work/data/objects" -name '*.pack' | wc -l | tr -d ' ')

cd "$work"
measure "fetch in full clone (30 new commits)" git -C full fetch -q origin "+refs/heads/*:refs/heads/*"
measure "fetch --depth 1 in shallow clone" git -C shallow fetch -q --depth 1
measure "full clone, bitmap + recent pushes" git clone -q --bare "$url" full-again

[ "$(git -C full rev-parse HEAD)" = "$(git -C src rev-parse HEAD)" ]
[ "$(git -C full-again rev-parse HEAD)" = "$(git -C src rev-parse HEAD)" ]
git -C full fsck --connectivity-only >/dev/null 2>&1
git -C full-again fsck --connectivity-only >/dev/null 2>&1
echo
echo "objects in the repository: $(git -C full-again count-objects -v | awk '/^count:|^in-pack:/ {n += $2} END {print n}')"
echo "packs in the manifest after the burst: $(sqlite3 "$work/data/meta.sqlite" 'select count(*) from packs')"
echo "pack files in storage (including replaced ones awaiting deletion): $packs_after_burst"
echo "storage: $(du -sh "$work/data/objects" | cut -f1)"
