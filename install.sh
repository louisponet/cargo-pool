#!/bin/sh
# Install cargo-pool and set it up in one go:
#
#   curl -sSf https://raw.githubusercontent.com/louisponet/cargo-pool/main/install.sh | sh
#
# Arguments are passed on to `cargo-pool install`, e.g. `| sh -s -- --no-wrapper`.
set -eu

if [ "$(uname -s)" != "Linux" ]; then
    echo "cargo-pool: only Linux is supported for now" >&2
    exit 1
fi
if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo-pool: cargo not found; install Rust first (https://rustup.rs)" >&2
    exit 1
fi

cargo install --locked --git https://github.com/louisponet/cargo-pool

bin="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin/cargo-pool"
"$bin" install "$@"
