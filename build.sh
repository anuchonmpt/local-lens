#!/bin/sh
# Build with an existing Cargo, or install Rust inside this app folder.
set -eu
cd "$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"

if command -v cargo >/dev/null 2>&1; then
    exec cargo build "$@"
fi

# GUI applications may not inherit the shell's Cargo PATH. Respect custom
# installations too, and put rustc beside Cargo on PATH before building.
for cargo_bin in "${CARGO_HOME:-$HOME/.cargo}/bin" "$PWD/.toolchain/cargo/bin" \
    /opt/homebrew/opt/rust/bin /usr/local/opt/rust/bin; do
    if [ -x "$cargo_bin/cargo" ]; then
        export PATH="$cargo_bin:$PATH"
        if [ "$cargo_bin" = "$PWD/.toolchain/cargo/bin" ]; then
            export CARGO_HOME="$PWD/.toolchain/cargo"
            export RUSTUP_HOME="${RUSTUP_HOME:-$PWD/.toolchain/rustup}"
        fi
        exec cargo build "$@"
    fi
done

export CARGO_HOME="${CARGO_HOME:-$PWD/.toolchain/cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$PWD/.toolchain/rustup}"
export PATH="$CARGO_HOME/bin:$PATH"
if [ ! -x "$CARGO_HOME/bin/cargo" ]; then
    mkdir -p .toolchain
    installer="$(mktemp "$PWD/.toolchain/rustup-init.XXXXXX")"
    trap 'rm -f "$installer"' EXIT HUP INT TERM
    if ! curl --fail --silent --show-error --location --connect-timeout 10 \
        --max-time 60 https://sh.rustup.rs -o "$installer"; then
        printf '%s\n' 'Rust setup failed: Cargo is missing and the Rust download server is unreachable. Restore network access and run ./build.sh again.' >&2
        exit 1
    fi
    sh "$installer" -y --no-modify-path --profile minimal --default-toolchain stable
    rm -f "$installer"
    trap - EXIT HUP INT TERM
fi
exec cargo build "$@"
