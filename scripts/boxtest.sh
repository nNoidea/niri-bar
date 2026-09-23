#!/usr/bin/env bash
set -euo pipefail

CONTAINER_NAME="niri-bar-ci-test"
IMAGE="ubuntu:24.04"

# Check if distrobox is installed
if ! command -v distrobox >/dev/null 2>&1; then
    echo "Error: distrobox is not installed on the host system." >&2
    exit 1
fi

cleanup() {
    echo "==> Cleaning up container '${CONTAINER_NAME}'..."
    distrobox rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true
    echo "==> Container and its files deleted."
}
trap cleanup EXIT INT TERM

# Ensure any previous container with this name is removed
distrobox rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true

echo "==> Creating temporary distrobox container '${CONTAINER_NAME}' (${IMAGE})..."
distrobox create \
    --name "${CONTAINER_NAME}" \
    --image "${IMAGE}" \
    --additional-packages "libgtk-3-dev libgtk-layer-shell-dev pkg-config build-essential curl git" \
    --pull \
    --yes

echo "==> Running test suite inside '${CONTAINER_NAME}'..."
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

distrobox enter "${CONTAINER_NAME}" -- bash -c "
    set -euo pipefail
    export RUSTUP_HOME=/usr/local/rustup
    export CARGO_HOME=/usr/local/cargo
    export PATH=\"/usr/local/cargo/bin:\$PATH\"

    if ! command -v cargo >/dev/null 2>&1; then
        echo '==> Installing Rust toolchain inside container...'
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sudo -E sh -s -- -y --default-toolchain stable --profile minimal --component rustfmt,clippy
        sudo chown -R \"\$(id -u):\$(id -g)\" /usr/local/cargo /usr/local/rustup
    fi

    cd \"${REPO_DIR}\"
    echo '==> Running make test inside clean container...'
    make test
"
