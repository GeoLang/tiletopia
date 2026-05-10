#!/bin/bash
# Cross-compile for ARM64
set -euo pipefail

TARGET=${1:-aarch64-unknown-linux-gnu}

echo "Building for target: $TARGET"
cargo build --release --target "$TARGET"
