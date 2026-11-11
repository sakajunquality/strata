#!/usr/bin/env bash
# Times the requests git makes against a repository with very many refs:
# 2,000 commits, 100,000 branches and 50,000 annotated tags by default. Shows
# the server's time for each request (best of five), without git's start-up.
#
#   cargo build --release && ./scripts/bench-refs.sh
#   STRATA_META=postgres://... ./scripts/bench-refs.sh    # with a shared metadata store
#
# The refs are pushed by speaking the push protocol directly: git's own HTTP
# push compares every ref it sends with every ref it has, and takes hours for
# this many.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
bin="${STRATA_BIN:-$repo_root/target/release/strata}"
port="${PORT:-18085}"
branches="${BRANCHES:-100000}"
tags="${TAGS:-50000}"
work="$(mktemp -d)"
url="http://127.0.0.1:$port/bench/refs.git"
api="http://127.0.0.1:$port/api/repos/bench/refs"
pid=""
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
trap '[ -n "$pid" ] && kill "$pid" 2>/dev/null; rm -rf "$work"' EXIT

echo "building a repository with $branches branches and $tags tags"
git init -q --bare -b main "$work/source.git"
python3 - "$branches" "$tags" <<'EOF' | git -C "$work/source.git" fast-import --quiet
import sys
branches, tags = int(sys.argv[1]), int(sys.argv[2])
out, who = sys.stdout, "A <a@example.com>"
for i in range(1, 2001):
    message, body = f"commit {i}\n", f"content {i}\n"
    out.write(f"commit refs/heads/main\nmark :{i}\nauthor {who} {1700000000 + i} +0000\ncommitter {who} {1700000000 + i} +0000\n")
    out.write(f"data {len(message)}\n{message}")
    if i > 1:
        out.write(f"from :{i - 1}\n")
    out.write(f"M 100644 inline dir{i % 50}/file{i % 400}.txt\ndata {len(body)}\n{body}\n")
for n in range(branches):
    out.write(f"reset refs/heads/agent/{n % 500}/task-{n}\nfrom :{n % 2000 + 1}\n\n")
for n in range(tags):
    message = f"release {n}\n"
    out.write(f"tag v{n}\nfrom :{n % 2000 + 1}\ntagger {who} 1700000000 +0000\ndata {len(message)}\n{message}\n")
EOF

"$bin" --listen "127.0.0.1:$port" --data-dir "$work/data" --auto-create --scratch-dir "$work" >"$work/server.log" 2>&1 &
pid=$!
for _ in $(seq 1 50); do
  curl -s -o /dev/null "http://127.0.0.1:$port/healthz" && break
  sleep 0.1
done

python3 - "$work/source.git" "$url" <<'EOF'
import subprocess, sys, time, urllib.request
repo, url = sys.argv[1], sys.argv[2]
listed = subprocess.run(["git", "-C", repo, "for-each-ref", "--format=%(objectname) %(refname)"], capture_output=True, text=True, check=True)
refs = [line.split(" ", 1) for line in listed.stdout.splitlines()]
pack = subprocess.run(["git", "-C", repo, "pack-objects", "--all", "--stdout", "-q"], input=b"", capture_output=True, check=True).stdout
started = time.time()
# The list of updates in one request is limited in size: send it in pieces, the pack with the first.
for at in range(0, len(refs), 50000):
    lines = [f"{'0' * 40} {oid} {name}".encode() for oid, name in refs[at:at + 50000]]
    lines[0] += b"\0report-status"
    body = b"".join(b"%04x" % (len(line) + 5) + line + b"\n" for line in lines) + b"0000" + (pack if at == 0 else b"")
    request = urllib.request.Request(url + "/git-receive-pack", data=body, headers={"content-type": "application/x-git-receive-pack-request"})
    answer = urllib.request.urlopen(request).read()
    assert b"unpack ok" in answer and b"ng " not in answer, answer[:300]
print(f"pushed {len(refs)} refs and {len(pack) >> 10} KiB of objects in {time.time() - started:.1f} s")
EOF

pkt() { printf '%04x%s' $((${#1} + 4)) "$1"; }
ls_refs() { # prefixes...: writes the request git sends to list refs
  {
    pkt $'command=ls-refs\n'
    pkt $'object-format=sha1\n'
    printf 0001
    pkt $'peel\n'
    pkt $'symrefs\n'
    for prefix in "$@"; do pkt "ref-prefix $prefix"$'\n'; done
    printf 0000
  } >"$work/request"
}
measure() { # label, curl arguments...
  local label="$1" best=999999999 micros size=0
  shift
  for _ in 1 2 3 4 5; do
    read -r micros size < <(curl -s -o /dev/null -w '%{time_total} %{size_download}\n' "$@" | awk '{printf "%d %d\n", $1 * 1000000, $2}')
    [ "$micros" -lt "$best" ] && best=$micros
  done
  printf '%-46s %8.1f ms %10d bytes\n' "$label" "$(echo "$best / 1000" | bc -l)" "$size"
}
v2=(-H 'git-protocol: version=2')
post=(-X POST -H 'content-type: application/x-git-upload-pack-request' --data-binary "@$work/request")

measure "fetch: capabilities" "${v2[@]}" "$url/info/refs?service=git-upload-pack"
ls_refs refs/heads/main refs/tags/refs/heads/main refs/heads/refs/heads/main
measure "fetch: list one branch" "${v2[@]}" "${post[@]}" "$url/git-upload-pack"
ls_refs refs/heads/agent/7/
measure "fetch: list the branches under a prefix" "${v2[@]}" "${post[@]}" "$url/git-upload-pack"
ls_refs refs/tags/
measure "fetch: list every tag" "${v2[@]}" "${post[@]}" "$url/git-upload-pack"
ls_refs HEAD refs/heads/ refs/tags/
measure "fetch: list everything, as a clone does" "${v2[@]}" "${post[@]}" "$url/git-upload-pack"
measure "push: the opening list of every ref" "$url/info/refs?service=git-receive-pack"
measure "api: newest commit of the default branch" "$api/commits?limit=1"
measure "api: refs under a prefix" "$api/refs?prefix=refs/heads/agent/7/"
measure "api: every ref" "$api/refs"
