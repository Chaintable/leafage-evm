#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
CYAN='\033[0;36m'
NC='\033[0m'

info()  { echo -e "${GREEN}[INFO]${NC} $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*"; exit 1; }
step()  { echo -e "\n${CYAN}===> $*${NC}"; }

# ── Cleanup on failure ────────────────────────────────────────────────
TEMP_FILES=()

cleanup() {
    local exit_code=$?
    if [[ ${#TEMP_FILES[@]} -gt 0 ]]; then
        for f in "${TEMP_FILES[@]}"; do
            rm -f "$f" 2>/dev/null || true
        done
    fi
    if [[ $exit_code -ne 0 ]]; then
        echo
        echo -e "${RED}[ERROR] Deployment failed (exit code: ${exit_code}). Check the output above for details.${NC}"
    fi
}
trap cleanup EXIT

# ── Defaults ──────────────────────────────────────────────────────────
ETH_DATA_DIR="/eth"
LEAFAGE_DATA_DIR="/nodex-eth"
ARCHIVE_MODE=false
SKIP_SNAPSHOT=false

usage() {
    cat <<EOF
Usage: $0 [OPTIONS]

One-click deployment for leafage-evm Ethereum node.

Options:
  --eth-data-dir DIR        Ethereum data directory      (default: /eth)
  --leafage-data-dir DIR    Leafage data directory       (default: /nodex-eth)
  --archive                 Enable archive mode          (default: state mode)
  --skip-snapshot           Skip snapshot download       (use existing data)
  -h, --help                Show this help message

Interactive mode:
  Run without arguments to be prompted for each option.

Examples:
  $0
  $0 --eth-data-dir /data/eth --leafage-data-dir /data/leafage --archive
EOF
    exit 0
}

# ── Parse CLI arguments ───────────────────────────────────────────────
INTERACTIVE=true
while [[ $# -gt 0 ]]; do
    INTERACTIVE=false
    case "$1" in
        --eth-data-dir)      [[ $# -ge 2 ]] || error "--eth-data-dir requires a value"
                             ETH_DATA_DIR="$2";      shift 2 ;;
        --leafage-data-dir)  [[ $# -ge 2 ]] || error "--leafage-data-dir requires a value"
                             LEAFAGE_DATA_DIR="$2";  shift 2 ;;
        --archive)           ARCHIVE_MODE=true;       shift   ;;
        --skip-snapshot)     SKIP_SNAPSHOT=true;      shift   ;;
        -h|--help)           usage ;;
        *) error "Unknown option: $1. Use -h for help." ;;
    esac
done

# ── Interactive prompts ───────────────────────────────────────────────
if [[ "$INTERACTIVE" == true ]]; then
    echo -e "${CYAN}╔══════════════════════════════════════════╗${NC}"
    echo -e "${CYAN}║      leafage-evm One-Click Deploy       ║${NC}"
    echo -e "${CYAN}╚══════════════════════════════════════════╝${NC}"
    echo

    read -rp "Ethereum data directory [${ETH_DATA_DIR}]: " input
    ETH_DATA_DIR="${input:-$ETH_DATA_DIR}"

    read -rp "Leafage data directory [${LEAFAGE_DATA_DIR}]: " input
    LEAFAGE_DATA_DIR="${input:-$LEAFAGE_DATA_DIR}"

    echo
    echo "Leafage snapshot mode:"
    echo "  1) State mode   (~150GB, lightweight)"
    echo "  2) Archive mode (~450GB, full historical state)"
    read -rp "Select mode [1]: " mode_input
    case "${mode_input:-1}" in
        2) ARCHIVE_MODE=true  ;;
        *) ARCHIVE_MODE=false ;;
    esac

    echo
fi

# ── Show configuration ────────────────────────────────────────────────
if [[ "$ARCHIVE_MODE" == true ]]; then
    MODE_LABEL="archive"
else
    MODE_LABEL="state"
fi

echo -e "${GREEN}Configuration:${NC}"
echo "  ETH_DATA_DIR     = ${ETH_DATA_DIR}"
echo "  LEAFAGE_DATA_DIR = ${LEAFAGE_DATA_DIR}"
echo "  Mode             = ${MODE_LABEL}"
echo

if [[ "$INTERACTIVE" == true ]]; then
    read -rp "Proceed? [Y/n]: " confirm
    case "${confirm:-Y}" in
        [Yy]*) ;;
        *) echo "Aborted."; exit 0 ;;
    esac
fi

# ── Check prerequisites ──────────────────────────────────────────────
step "Checking prerequisites"

missing=()
command -v docker  >/dev/null 2>&1 || missing+=("docker")

if [[ "$SKIP_SNAPSHOT" != true ]]; then
    command -v aws   >/dev/null 2>&1 || missing+=("aws (AWS CLI)")
    command -v zstd  >/dev/null 2>&1 || command -v unzstd >/dev/null 2>&1 || missing+=("zstd")
fi

if [[ ${#missing[@]} -gt 0 ]]; then
    error "Missing required tools: ${missing[*]}\n  Please install them before running this script."
fi

# Check docker compose (v2 plugin or standalone)
if docker compose version >/dev/null 2>&1; then
    COMPOSE_CMD="docker compose"
elif command -v docker-compose >/dev/null 2>&1; then
    COMPOSE_CMD="docker-compose"
else
    error "Missing required tool: docker compose\n  Please install Docker Compose v2."
fi

info "All prerequisites met (compose: ${COMPOSE_CMD})"

# ── Create directories ────────────────────────────────────────────────
step "Creating data directories"

if ! mkdir -p "${ETH_DATA_DIR}/geth" "${ETH_DATA_DIR}/lighthouse" "${LEAFAGE_DATA_DIR}"; then
    error "Failed to create data directories. Check permissions (you may need sudo)."
fi
info "Created ${ETH_DATA_DIR}/{geth,lighthouse} and ${LEAFAGE_DATA_DIR}"

# ── Generate JWT secret ──────────────────────────────────────────────
JWT_PATH="${ETH_DATA_DIR}/geth/jwtsecret"
if [[ -f "${JWT_PATH}" ]]; then
    info "JWT secret already exists at ${JWT_PATH}, skipping"
else
    command -v openssl >/dev/null 2>&1 || error "openssl is required to generate JWT secret but was not found.\n  Install openssl or provide an existing jwtsecret at ${JWT_PATH}"
    if ! openssl rand -hex 32 > "${JWT_PATH}"; then
        error "Failed to generate JWT secret."
    fi
    info "Generated JWT secret at ${JWT_PATH}"
fi

# ── Download & extract snapshots ─────────────────────────────────────
if [[ "$SKIP_SNAPSHOT" == true ]]; then
    step "Skipping snapshot download (--skip-snapshot)"
else
    step "Downloading snapshots"

    GETH_SNAPSHOT="s3://blockchain-snapshot-backup/eth/geth-24646705.tar.zstd"
    if [[ "$ARCHIVE_MODE" == true ]]; then
        LEAFAGE_SNAPSHOT="s3://blockchain-snapshot-backup/eth/leafage-archive-24646705.tar.zstd"
    else
        LEAFAGE_SNAPSHOT="s3://blockchain-snapshot-backup/eth/leafage-24647777.tar.zstd"
    fi

    TMPDIR="${TMPDIR:-/tmp}"

    # Download geth snapshot
    GETH_FILE="${TMPDIR}/geth-snapshot.tar.zstd"
    TEMP_FILES+=("${GETH_FILE}")
    info "Downloading geth snapshot..."
    info "  ${GETH_SNAPSHOT}"
    if ! aws s3 cp "${GETH_SNAPSHOT}" "${GETH_FILE}"; then
        error "Failed to download geth snapshot. Check your AWS credentials and S3 access permissions.\n  Run 'aws sts get-caller-identity' to verify your AWS identity."
    fi

    info "Extracting geth snapshot to ${ETH_DATA_DIR}/ ..."
    if ! tar --use-compress-program=unzstd -xf "${GETH_FILE}" -C "${ETH_DATA_DIR}/"; then
        error "Failed to extract geth snapshot. Check disk space and file integrity.\n  Required: ~850GB free in ${ETH_DATA_DIR}/"
    fi
    rm -f "${GETH_FILE}"
    info "Geth snapshot extracted"

    # Download leafage snapshot
    LEAFAGE_FILE="${TMPDIR}/leafage-snapshot.tar.zstd"
    TEMP_FILES+=("${LEAFAGE_FILE}")
    info "Downloading leafage snapshot (${MODE_LABEL})..."
    info "  ${LEAFAGE_SNAPSHOT}"
    if ! aws s3 cp "${LEAFAGE_SNAPSHOT}" "${LEAFAGE_FILE}"; then
        error "Failed to download leafage snapshot. Check your AWS credentials and S3 access permissions.\n  Run 'aws sts get-caller-identity' to verify your AWS identity."
    fi

    info "Extracting leafage snapshot to ${LEAFAGE_DATA_DIR}/ ..."
    if ! tar --use-compress-program=unzstd -xf "${LEAFAGE_FILE}" -C "${LEAFAGE_DATA_DIR}/"; then
        error "Failed to extract leafage snapshot. Check disk space and file integrity.\n  Required: ~$( [[ "$ARCHIVE_MODE" == true ]] && echo "450GB" || echo "150GB" ) free in ${LEAFAGE_DATA_DIR}/"
    fi
    rm -f "${LEAFAGE_FILE}"
    info "Leafage snapshot extracted"
fi

# ── Write .env for docker compose ────────────────────────────────────
step "Generating docker compose configuration"

cat > "${SCRIPT_DIR}/.env" <<EOF
ETH_DATA_DIR=${ETH_DATA_DIR}
LEAFAGE_DATA_DIR=${LEAFAGE_DATA_DIR}
EOF
info "Written ${SCRIPT_DIR}/.env"

# ── Generate compose override for archive mode ───────────────────────
OVERRIDE_FILE="${SCRIPT_DIR}/docker-compose.override.yml"

if [[ "$ARCHIVE_MODE" == true ]]; then
    cat > "${OVERRIDE_FILE}" <<'EOF'
services:
  leafage-evm-x-eth:
    command:
      - standalone
      - --db-path=/nodex-eth
      - --listen-addr=0.0.0.0:8659
      - --chain-cfg=1
      - --rpc-addr=http://geth:8545
      - --archive
EOF
    info "Archive mode enabled via ${OVERRIDE_FILE}"
else
    rm -f "${OVERRIDE_FILE}"
    info "State mode (no compose override needed)"
fi

# ── Start services ───────────────────────────────────────────────────
step "Starting services"

cd "${SCRIPT_DIR}"
if ! ${COMPOSE_CMD} up -d; then
    error "Failed to start services. Check docker daemon status and image availability.\n  Run '${COMPOSE_CMD} logs' for details."
fi

info "Services started"

# ── Verify ───────────────────────────────────────────────────────────
step "Verifying services"

echo
${COMPOSE_CMD} ps
echo
info "Deployment complete!"
echo
echo "  Geth RPC:    http://localhost:8666"
echo "  Leafage RPC: http://localhost:8659"
echo
echo "  View logs:   cd ${SCRIPT_DIR} && ${COMPOSE_CMD} logs -f"
echo
