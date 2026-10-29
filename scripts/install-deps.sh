#!/usr/bin/env bash
# Build tools for cookie-web on Ubuntu/Debian: apt packages + Rust (rustup).
#
#   ./scripts/install-deps.sh
#
# Installs build-essential, pkg-config, libssl-dev (plus curl, git, ca-certificates), then Rust
# for the user running the script (not root, even under sudo). Safe to run again: it updates Rust.
set -euo pipefail

if [ -t 1 ]; then B=$'\e[1m' G=$'\e[32m' R=$'\e[31m' N=$'\e[0m'; else B='' G='' R='' N=''; fi
step() { echo; echo "${B}==> $*${N}"; }
ok() { echo "  ${G}✓${N} $*"; }
die() { echo "  ${R}✗ $*${N}" >&2; exit 1; }

case "${1:-}" in -h | --help) sed -n '2,7p' "$0"; exit 0 ;; esac
command -v apt-get >/dev/null || die "this script is for Ubuntu/Debian (apt-get not found)"
if [ "$(id -u)" -eq 0 ]; then SUDO=""; else SUDO="sudo"; sudo -v || die "sudo is required"; fi

# Rust goes to the real user's home even under sudo, so ~/.cargo isn't owned by root.
RUST_USER="${SUDO_USER:-$(id -un)}"
as_user() {
	if [ "$(id -un)" = "$RUST_USER" ]; then "$@"; else sudo -u "$RUST_USER" -H "$@"; fi
}

step "Installing build packages (apt)"
$SUDO env DEBIAN_FRONTEND=noninteractive apt-get update -qq
$SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
	build-essential pkg-config libssl-dev curl git ca-certificates >/dev/null
ok "build-essential $(dpkg-query -W -f='${Version}' build-essential) · pkg-config · libssl-dev · curl · git"

step "Installing Rust for $RUST_USER"
# shellcheck disable=SC2016 # $HOME expands in the user's shell
RUSTUP="$(as_user bash -c 'command -v rustup || echo "$HOME/.cargo/bin/rustup"')"
if as_user test -x "$RUSTUP"; then
	as_user "$RUSTUP" self update >/dev/null 2>&1 || true
	as_user "$RUSTUP" update stable --no-self-update >/dev/null
	ok "updated the stable toolchain"
else
	as_user bash -c 'curl --proto "=https" --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable' >/dev/null
	ok "installed rustup + stable"
fi
# shellcheck disable=SC2016
CARGO="$(as_user bash -c 'echo "$HOME/.cargo/bin/cargo"')"
ok "$(as_user "$CARGO" --version)"
echo "  (new shells pick up cargo automatically; in this one run: source \"\$HOME/.cargo/env\")"
