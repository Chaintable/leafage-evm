#!/usr/bin/env bash
set -uo pipefail

if [[ -z "${ARC_BLOCK:-}" ]]; then
  echo "set ARC_BLOCK before running Arc RPC checks" >&2
  exit 2
fi

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
args=(
  --block "${ARC_BLOCK}"
)

if [[ -n "${ARC_FUNDED_ADDRESS:-}" ]]; then
  args+=(--funded-address "${ARC_FUNDED_ADDRESS}")
fi
if [[ -n "${ARC_QUERY_REPORT:-}" ]]; then
  args+=(--output "${ARC_QUERY_REPORT}")
fi

exit_code=0
python3 "${script_dir}/verify_arc_queries.py" "${args[@]}" "$@" || exit_code=$?

# nodectl adds randomized history/transaction coverage. Its direct endpoint
# includes the chain id path and may differ from LEAFAGE_RPC, so it is opt-in.
if ((exit_code == 0)) && [[ -n "${LEAFAGE_NODECTL_ENDPOINT:-}" ]]; then
  command -v nodectl >/dev/null 2>&1 || {
    echo "nodectl is required when LEAFAGE_NODECTL_ENDPOINT is set" >&2
    exit 2
  }
  for endpoint_name in LEAFAGE_NODECTL_ENDPOINT ARC_REFERENCE_RPC; do
    endpoint=${!endpoint_name}
    case "${endpoint}" in
      http://127.0.0.1:* | http://localhost:* | http://\[::1\]:*) ;;
      *)
        echo "${endpoint_name} must be an uncredentialed loopback URL when nodectl is enabled" >&2
        exit 2
        ;;
    esac
  done
  block_decimal=$((ARC_BLOCK))
  from=$((block_decimal > 1000 ? block_decimal - 1000 : 1))
  nodectl node verify \
    --endpoint "${LEAFAGE_NODECTL_ENDPOINT}" \
    --official-rpc "${ARC_REFERENCE_RPC}" \
    --from "${from}" \
    --to "${block_decimal}" \
    --number 50 \
    --tx-count 200 || {
      nodectl_exit=$?
      if ((nodectl_exit > exit_code)); then
        exit_code=${nodectl_exit}
      fi
    }
fi

exit "${exit_code}"
