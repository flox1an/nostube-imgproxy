#!/bin/bash
# Build script for rust-imgproxy
# Ensures meson/ninja are in PATH for AVIF support

# Add common locations for meson/ninja to PATH
# Adjust these paths based on where you installed meson/ninja
export PATH="$HOME/Library/Python/3.9/bin:$PATH"  # macOS pip --user
export PATH="$HOME/.local/bin:$PATH"              # Linux pip --user
export PATH="/usr/local/bin:$PATH"                # System-wide install

# Check if meson is available
if ! command -v meson &> /dev/null; then
    echo "Error: meson not found in PATH"
    echo "Install with: pip3 install --user meson ninja"
    exit 1
fi

# Run cargo with all arguments passed through
if [ "$1" = "run" ]; then
    cargo run "$@"
else
    cargo build "$@"
fi

