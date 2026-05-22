#!/usr/bin/env bash
#
# Build VXN1, bundle it as VXN1.clap, and install it to the user CLAP directory.
#
# This delegates to `cargo xtask bundle`, which knows how to assemble a proper
# macOS .clap *bundle* (Contents/MacOS/VXN1 + Info.plist) — a plain rename of the
# .dylib is not a valid plugin on macOS. On Linux/Windows the .clap is just the
# shared library renamed, which xtask also handles.
#
# Usage:
#   ./deploy.sh            # release build, bundle, and install
#   ./deploy.sh --debug    # debug build instead of release
#
# Install destinations (per OS, chosen by xtask):
#   macOS    ~/Library/Audio/Plug-Ins/CLAP/VXN1.clap
#   Linux    ~/.clap/VXN1.clap
#   Windows  %LOCALAPPDATA%\Programs\Common\CLAP\VXN1.clap

set -euo pipefail

# Run from the repository root (the directory containing this script).
cd "$(dirname "$0")"

PROFILE="--release"
if [[ "${1:-}" == "--debug" ]]; then
    PROFILE=""
fi

echo "==> Building and installing VXN1.clap..."
cargo xtask bundle ${PROFILE} --install

echo "==> Done."
