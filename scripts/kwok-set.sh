#!/usr/bin/env bash
# Creates, reports on and deletes a set of local kwok clusters for multi-cluster measurements
# (issue #447; the method is in docs/spikes/kwok-set-measurements.md):
#
#   scripts/kwok-set.sh create [--members N] [--pods N]   (default 5 clusters, 10000 pods each)
#   scripts/kwok-set.sh report
#   scripts/kwok-set.sh delete
#
# Safety:
#   - Every kubeconfig write goes to one scratch file, FLINT_KWOK_KUBECONFIG (default
#     .dev/kwok-set/kubeconfig under the current directory). The path must contain "kwok-set".
#     ~/.kube/config and .dev/kubeconfig are refused, also through links, and the caller's
#     KUBECONFIG is ignored.
#   - Every kubectl call names that file and one of this script's contexts (kwok-flint-kwok-N).
#     Any other context is refused.
#   - `delete` removes only clusters named flint-kwok-1 to flint-kwok-5.
#   - Nothing is installed; kwokctl, kubectl and docker (or the binary runtime) must exist.
set -euo pipefail

prefix=flint-kwok-
max_members=5
members=5
pods=10000
scratch=

die() { echo "error: $*" >&2; exit 1; }

# Path of an existing or future file, with symlinks resolved.
real_path() {
  local dir base target
  dir=$(dirname -- "$1")
  base=$(basename -- "$1")
  [ -d "$dir" ] || { echo "$1"; return; }
  dir=$(cd "$dir" && pwd -P)
  if [ -L "$dir/$base" ]; then
    target=$(readlink "$dir/$base")
    case $target in
      /*) real_path "$target" ;;
      *) real_path "$dir/$target" ;;
    esac
  else
    echo "$dir/$base"
  fi
}

resolve_kubeconfig() {
  unset KUBECONFIG
  local raw=${FLINT_KWOK_KUBECONFIG:-$PWD/.dev/kwok-set/kubeconfig}
  local forbidden
  case $raw in /*) ;; *) raw=$PWD/$raw ;; esac
  case $raw in *kwok-set*) ;; *) die "the scratch kubeconfig path must contain kwok-set: $raw" ;; esac
  mkdir -p "$(dirname -- "$raw")"
  scratch=$(real_path "$raw")
  case $scratch in *kwok-set*) ;; *) die "the scratch kubeconfig resolves outside kwok-set: $scratch" ;; esac
  for forbidden in "$HOME/.kube/config" "$PWD/.dev/kubeconfig"; do
    if [ "$scratch" = "$(real_path "$forbidden")" ]; then die "refusing to use $forbidden"; fi
  done
  case $scratch in */.kube/config | */.dev/kubeconfig) die "refusing to use $scratch" ;; esac
  export KUBECONFIG=$scratch
}

need() { command -v "$1" > /dev/null 2>&1 || die "$1 is not installed (this script installs nothing)"; }

name_of() { echo "${prefix}$1"; }
context_of() { echo "kwok-${prefix}$1"; }

# Refuses any context this script did not create.
check_context() {
  case $1 in
    "kwok-${prefix}"[1-5]) ;;
    *) die "refusing context $1: not created by this script" ;;
  esac
}

# kc <member> kubectl-args...
kc() {
  local ctx
  ctx=$(context_of "$1")
  shift
  check_context "$ctx"
  kubectl --kubeconfig "$scratch" --context "$ctx" "$@"
}

owned_clusters() {
  kwokctl get clusters 2> /dev/null | grep -E "^${prefix}[1-5]$" || true
}

cmd_create() {
  local i
  need kwokctl
  need kubectl
  while [ $# -gt 0 ]; do
    case $1 in
      --members) members=${2:-}; shift 2 ;;
      --pods) pods=${2:-}; shift 2 ;;
      *) die "unknown option $1" ;;
    esac
  done
  case $members in '' | *[!0-9]*) die "--members needs a number" ;; esac
  case $pods in '' | *[!0-9]*) die "--pods needs a number" ;; esac
  if [ "$members" -lt 1 ] || [ "$members" -gt "$max_members" ]; then die "--members is 1 to $max_members"; fi
  for i in $(seq 1 "$members"); do
    echo "creating $(name_of "$i")"
    kwokctl create cluster --name "$(name_of "$i")" --kubeconfig "$scratch" --wait 120s
    check_context "$(context_of "$i")"
    # A node holds 110 pods by default.
    kwokctl scale node --name "$(name_of "$i")" --replicas "$(( (pods + 109) / 110 ))"
    kwokctl scale pod fake-pod --name "$(name_of "$i")" --replicas "$pods" --namespace default
  done
  echo "kubeconfig: $scratch"
  cmd_report
}

cmd_report() {
  local found=0 name i count
  need kwokctl
  need kubectl
  for name in $(owned_clusters); do
    found=1
    i=${name#"$prefix"}
    count=$(kc "$i" get pods --all-namespaces --no-headers 2> /dev/null | wc -l | tr -d ' ')
    echo "$name context=$(context_of "$i") pods=$count"
  done
  if [ "$found" = 0 ]; then echo "no ${prefix}N clusters"; fi
  if command -v docker > /dev/null 2>&1; then
    docker stats --no-stream --format '{{.Name}} cpu={{.CPUPerc}} mem={{.MemUsage}}' 2> /dev/null \
      | grep "kwok-${prefix}" || true
  fi
}

cmd_delete() {
  local name
  need kwokctl
  for name in $(owned_clusters); do
    echo "deleting $name"
    kwokctl delete cluster --name "$name" --kubeconfig "$scratch"
  done
}

usage() { sed -n '2,16p' "$0"; }

[ $# -ge 1 ] || { usage; exit 2; }
action=$1
shift
case $action in -h | --help) usage; exit 0 ;; esac
resolve_kubeconfig
case $action in
  create) cmd_create "$@" ;;
  report) cmd_report ;;
  delete) cmd_delete ;;
  *) die "unknown command $action (create, report or delete)" ;;
esac
