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
#   - Cluster names carry a run id derived from the scratch kubeconfig path
#     (flint-kwok-<id>-N), so another worktree or kubeconfig never sees or deletes them.
#   - Every kubectl call names that file and one of this run's contexts (kwok-flint-kwok-<id>-N).
#     Any other context is refused.
#   - `delete` removes only clusters named flint-kwok-<id>-1 to -5 of this run.
#   - Nothing is installed; kwokctl, kubectl and docker (or the binary runtime) must exist.
set -euo pipefail

prefix=
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
    if [ "$scratch" = "$(real_path "$forbidden")" ] || [ "$scratch" -ef "$forbidden" ]; then
      die "refusing to use $forbidden"
    fi
  done
  case $scratch in */.kube/config | */.dev/kubeconfig) die "refusing to use $scratch" ;; esac
  export KUBECONFIG=$scratch
  # Run id: the checksum of the resolved scratch path.
  prefix=flint-kwok-$(printf '%s' "$scratch" | cksum | cut -d' ' -f1)-
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

# Prints this run's cluster names. A failed listing fails the script; only a successful listing
# with no match is empty.
owned_clusters() {
  local listing
  listing=$(kwokctl get clusters) || die "kwokctl get clusters failed"
  printf '%s\n' "$listing" | grep -E "^${prefix}[1-5]$" || true
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
  # Decimal, so a leading zero is not read as octal. Validated before anything is created.
  members=$((10#$members))
  pods=$((10#$pods))
  if [ "$members" -lt 1 ] || [ "$members" -gt "$max_members" ]; then die "--members is 1 to $max_members"; fi
  for i in $(seq 1 "$members"); do
    echo "creating $(name_of "$i")"
    # A node holds 110 pods by default.
    {
      kwokctl create cluster --name "$(name_of "$i")" --kubeconfig "$scratch" --wait 120s &&
        check_context "$(context_of "$i")" &&
        kwokctl scale node --name "$(name_of "$i")" --replicas "$(( (pods + 109) / 110 ))" &&
        kwokctl scale pod fake-pod --name "$(name_of "$i")" --replicas "$pods" --namespace default
    } || die "member $(name_of "$i") failed to create; run '$0 delete' to clean up"
  done
  echo "kubeconfig: $scratch"
  cmd_report
}

cmd_report() {
  local found=0 failed=0 name i count names
  need kwokctl
  need kubectl
  names=$(owned_clusters)
  for name in $names; do
    found=1
    i=${name#"$prefix"}
    # Only the fixture pods made by `kwokctl scale pod fake-pod`.
    if count=$(kc "$i" get pods --namespace default --selector kwok.x-k8s.io/kwokctl-scale=fake-pod \
      --no-headers | wc -l | tr -d ' '); then
      echo "$name context=$(context_of "$i") pods=$count"
    else
      echo "error: member $name: could not list pods" >&2
      failed=1
    fi
  done
  if [ "$found" = 0 ]; then echo "no ${prefix}N clusters"; fi
  if command -v docker > /dev/null 2>&1; then
    docker stats --no-stream --format '{{.Name}} cpu={{.CPUPerc}} mem={{.MemUsage}}' 2> /dev/null \
      | grep "kwok-${prefix}" || true
  fi
  return "$failed"
}

cmd_delete() {
  local name names failed=0
  need kwokctl
  names=$(owned_clusters)
  for name in $names; do
    echo "deleting $name"
    kwokctl delete cluster --name "$name" --kubeconfig "$scratch" || {
      echo "error: member $name: delete failed" >&2
      failed=1
    }
  done
  return "$failed"
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
