#!/usr/bin/env bash
# Tries the Kubernetes example on a laptop, in a kind cluster.
#
#   ./kind.sh up      create the cluster, build and load the image, deploy
#   ./kind.sh test    drive the compute nodes with git, and take some away meanwhile
#   ./kind.sh down    delete the cluster
#
# Needs docker, kubectl and kind. Set KIND to the kind binary if it is not on PATH.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo_root="$(cd "$here/../.." && pwd)"
cluster="${KIND_CLUSTER:-strata}"
context="kind-$cluster"
kind() { command "${KIND:-kind}" "$@"; }
k() { kubectl --context "$context" -n strata "$@"; }
# The compute pods that are taking requests, by address.
pods() { k get endpointslices -l kubernetes.io/service-name=strata-pods -o jsonpath='{range .items[*].endpoints[?(@.conditions.ready==true)]}{.addresses[0]}{" "}{end}'; }
in_test_pod() { k exec -i strata-test -- env PODS="$(pods)" sh -s -- "$@" <"$here/test.sh"; }

case "${1:-}" in
up)
  kind get clusters | grep -qx "$cluster" || kind create cluster --name "$cluster" --config "$here/kind.yaml"
  docker build -t strata:local "$repo_root"
  kind load docker-image strata:local --name "$cluster"
  # Workers join a little after the cluster reports itself created; pods
  # scheduled before that would all land on the first one to be ready.
  kubectl --context "$context" wait --for=condition=Ready nodes --all --timeout 180s >/dev/null
  kubectl --context "$context" apply -k "$here/local"
  k rollout status statefulset/postgres --timeout 180s
  k rollout status deployment/object-store --timeout 300s
  # The bucket is made by a script the store runs once it is up.
  until k exec deploy/object-store -- awslocal s3api head-bucket --bucket strata >/dev/null 2>&1; do sleep 2; done
  k rollout status deployment/strata --timeout 180s
  k get pods -o wide
  ;;

test)
  k delete pod strata-test --ignore-not-found --wait >/dev/null
  k run strata-test --image=alpine/git:2.49.1 --restart=Never --command -- sleep 3600 >/dev/null
  k wait --for=condition=Ready pod/strata-test --timeout 120s >/dev/null
  k exec strata-test -- apk add --no-cache -q curl

  echo "== three nodes, one repository"
  in_test_pod basics

  echo "== pushes keep landing while a pod is deleted, the nodes are replaced and two more are added"
  in_test_pod load 60 &
  load=$!
  sleep 5
  victim="$(k get pods -l app=strata -o jsonpath='{.items[0].metadata.name}')"
  echo "- deleting $victim"
  k delete pod "$victim" --wait=false >/dev/null
  sleep 10
  echo "- restarting all of them, one at a time"
  k rollout restart deployment/strata >/dev/null
  k rollout status deployment/strata --timeout 300s >/dev/null
  echo "- scaling to five"
  k scale deployment/strata --replicas 5 >/dev/null
  k rollout status deployment/strata --timeout 180s >/dev/null
  wait "$load"

  echo "== everything is on every node, the new ones included"
  in_test_pod verify
  k get pods -l app=strata -o wide
  k scale deployment/strata --replicas 3 >/dev/null
  k delete pod strata-test --wait=false >/dev/null
  echo "passed"
  ;;

down)
  kind delete cluster --name "$cluster"
  ;;

*)
  sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'
  exit 1
  ;;
esac
