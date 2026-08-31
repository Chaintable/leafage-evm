#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
mode=${1:-all}
if (($# > 0)); then
  shift
fi

run_unit() {
  PYTHONDONTWRITEBYTECODE=1 python3 -B -W error::ResourceWarning \
    -m unittest discover -s "${script_dir}/tests" -p 'test_*.py' -v
}

case "${mode}" in
  unit)
    run_unit
    ;;
  rpc)
    "${script_dir}/test-arc-queries.sh" "$@"
    ;;
  all)
    run_unit
    "${script_dir}/test-arc-queries.sh" "$@"
    ;;
  *)
    echo "usage: $0 {unit|rpc|all} [RPC test arguments...]" >&2
    exit 2
    ;;
esac
