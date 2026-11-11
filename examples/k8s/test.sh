#!/bin/sh
# Runs inside the cluster, in a pod with git: drives the compute nodes
# through the Service (any pod) and one by one (each pod by its address).
#
#   test.sh basics          every node sees what any node wrote
#   test.sh load <seconds>  pushes through the Service for that long, retrying what fails
#   test.sh verify          those pushes are all there, on every node
#
# PODS holds the addresses of the compute pods.
set -eu

token=local-admin-token
# A repository of its own for each run, so that the test can be run again.
[ "$1" != basics ] || { rm -rf /work && mkdir -p /work && echo "demo/run-$(date +%s)" >/work/repo; }
repo="$(cat /work/repo)"
service="http://git:$token@strata.strata.svc/$repo.git"
pod() { echo "http://git:$token@$1:8080/$repo.git"; }
api() { curl -sf -H "Authorization: Bearer $token" "$@"; }

export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com
export GIT_TERMINAL_PROMPT=0
git config --global init.defaultBranch main
git config --global advice.detachedHead false

case "$1" in
basics)
  cd /work
  api -X PUT "http://strata.strata.svc/api/repos/$repo" >/dev/null
  git init -q src && cd src
  seq 1 500 >numbers.txt && git add . && git commit -q -m "first"
  git push -q "$service" main

  echo "- written through the Service, read from each pod"
  for ip in $PODS; do
    [ "$(git ls-remote "$(pod "$ip")" refs/heads/main | cut -f1)" = "$(git rev-parse HEAD)" ]
  done

  echo "- written through each pod in turn, read from the next one"
  set -- $PODS
  for round in 1 2 3; do
    for ip in $PODS; do
      echo "round $round via $ip" >>numbers.txt && git commit -q -am "round $round via $ip"
      git push -q "$(pod "$ip")" main
      shift; [ $# -gt 0 ] || set -- $PODS
      [ "$(git ls-remote "$(pod "$1")" refs/heads/main | cut -f1)" = "$(git rev-parse HEAD)" ]
    done
  done

  echo "- two pushes to one branch at once, through two pods: one lands"
  set -- $PODS
  git clone -q "$service" /work/a && git clone -q "$service" /work/b
  (cd /work/a && echo a >a.txt && git add . && git commit -q -m "from a")
  (cd /work/b && echo b >b.txt && git add . && git commit -q -m "from b")
  (cd /work/a && if git push -q "$(pod "$1")" main >/work/a.log 2>&1; then echo 0; else echo 1; fi >/work/a.status) &
  (cd /work/b && if git push -q "$(pod "$2")" main >/work/b.log 2>&1; then echo 0; else echo 1; fi >/work/b.status) &
  wait
  landed=0
  for side in a b; do [ "$(cat /work/$side.status)" = 0 ] && landed=$((landed + 1)); done
  [ "$landed" = 1 ] || { echo "expected one push to land, got $landed"; cat /work/a.log /work/b.log; exit 1; }
  # The other one fetches, rebases and gets in.
  for side in a b; do
    [ "$(cat /work/$side.status)" = 0 ] || (cd /work/$side && git pull -q --rebase "$service" main && git push -q "$service" main)
  done

  echo "- a 12 MiB file pushed through one pod, cloned whole from every other"
  cd /work/src && git pull -q "$service" main
  head -c 12582912 /dev/urandom >big.bin && git add . && git commit -q -m "big"
  set -- $PODS
  git push -q "$(pod "$1")" main
  for ip in $PODS; do
    rm -rf /work/clone && git clone -q "$(pod "$ip")" /work/clone
    cmp /work/src/big.bin /work/clone/big.bin
    git -C /work/clone fsck --strict >/dev/null 2>&1
  done
  echo "basics passed"
  ;;

load)
  # Pushes one after another through the Service while pods come and go
  # underneath. A push cut off by a pod going away is tried again.
  cd /work/src && git pull -q "$service" main
  failures=0
  n=0
  until=$(($(date +%s) + $2))
  while [ "$(date +%s)" -lt "$until" ]; do
    n=$((n + 1))
    echo "load $n" >>load.txt && git add . && git commit -q -m "load $n"
    until git push -q "$service" main 2>/work/push.err; do
      failures=$((failures + 1))
      [ "$failures" -lt 50 ] || { echo "giving up:"; cat /work/push.err; exit 1; }
      sleep 1
    done
    # And read it back through whichever pod answers.
    until remote="$(git ls-remote "$service" refs/heads/main 2>/dev/null)"; do
      failures=$((failures + 1))
      sleep 1
    done
    [ "${remote%%[[:space:]]*}" = "$(git rev-parse HEAD)" ] || { echo "push $n is not there: $remote"; exit 1; }
  done
  echo "load finished: $n pushes in $2 seconds, $failures requests tried again"
  ;;

verify)
  cd /work/src
  for ip in $PODS; do
    rm -rf /work/clone && git clone -q "$(pod "$ip")" /work/clone
    [ "$(git -C /work/clone rev-parse HEAD)" = "$(git rev-parse HEAD)" ]
    cmp load.txt /work/clone/load.txt
    git -C /work/clone fsck --strict >/dev/null 2>&1
  done
  api "http://strata.strata.svc/api/repos/$repo"
  echo "verified on $(echo $PODS | wc -w) pods"
  ;;
esac
