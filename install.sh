#!/usr/bin/env bash
# Build and install updatium from this repository.
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: ./install.sh [OPTIONS]

Build and install the updatium release binary.

Options:
  --prefix DIR  Install into DIR/bin (default: $PREFIX/bin or ~/.local/bin)
  --help        Show this help and exit
  --version     Show the package version and exit

Environment:
  PREFIX        Installation prefix. --prefix takes precedence.
  CARGO         Cargo executable to use (default: cargo)
  RUSTC         Rust compiler executable to check (default: rustc)

Examples:
  ./install.sh
  ./install.sh --prefix "$HOME/.local"
  PREFIX="$HOME/.local" ./install.sh
EOF
}

version() {
    awk -F '"' '$1 ~ /^version[[:space:]]*=/ { print $2; exit }' "$ROOT/Cargo.toml"
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
CARGO="${CARGO:-cargo}"
RUSTC="${RUSTC:-rustc}"

while (($# > 0)); do
    case "$1" in
        --prefix)
            if (($# < 2)); then
                echo "install.sh: --prefix requires a directory" >&2
                exit 2
            fi
            PREFIX=$2
            shift 2
            ;;
        --prefix=*)
            PREFIX=${1#*=}
            shift
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        --version|-V)
            version
            exit 0
            ;;
        *)
            echo "install.sh: unknown option: $1" >&2
            echo "Try './install.sh --help'." >&2
            exit 2
            ;;
    esac
done

if [[ -z "$PREFIX" ]]; then
    echo "install.sh: installation prefix must not be empty" >&2
    exit 2
fi

if ! command -v "$CARGO" >/dev/null 2>&1; then
    cat >&2 <<'EOF'
install.sh: cargo was not found.
Install Rust 1.90 or newer from https://rustup.rs/ and run this script again.
EOF
    exit 1
fi

if ! command -v "$RUSTC" >/dev/null 2>&1; then
    cat >&2 <<'EOF'
install.sh: rustc was not found.
Install Rust 1.90 or newer from https://rustup.rs/ and run this script again.
EOF
    exit 1
fi

RUST_VERSION=$("$RUSTC" --version | awk '{print $2}')
if [[ -z "$RUST_VERSION" ]] || [[ "$(printf '%s\n' 1.90.0 "$RUST_VERSION" | sort -V | head -n1)" != 1.90.0 ]]; then
    printf 'install.sh: Rust 1.90 or newer is required; found %s\n' "${RUST_VERSION:-unknown}" >&2
    exit 1
fi

if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1 && ! command -v clang >/dev/null 2>&1; then
    echo "install.sh: a C compiler (cc, gcc, or clang) is required" >&2
    exit 1
fi

if [[ "$(uname -s)" != Linux ]]; then
    echo "install.sh: updatium currently supports Linux only" >&2
    exit 1
fi

printf 'Building updatium %s...\n' "$(version)"
"$CARGO" build --release --manifest-path "$ROOT/Cargo.toml"

BIN_DIR="$PREFIX/bin"
mkdir -p "$BIN_DIR"
install -m 0755 "$ROOT/target/release/updatium" "$BIN_DIR/updatium"

printf 'Installed %s/bin/updatium\n' "$PREFIX"
if [[ ":${PATH}:" != *":$BIN_DIR:"* ]]; then
    printf 'Add %s to PATH if it is not already present.\n' "$BIN_DIR"
fi
