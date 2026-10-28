#!/usr/bin/env bash
# Updates EmpireBT's Rust dependencies, then builds, lints, and tests
# everything, so you know at once whether the update broke anything.
#
#   scripts/update-dependencies.sh          updates within the versions
#                                           Cargo.toml allows (Cargo.lock only)
#   scripts/update-dependencies.sh --major  also moves to new major versions
#                                           (edits Cargo.toml; these can need
#                                           code changes)
#
# Review the result with `git diff`, then commit it. To undo:
#   git checkout -- Cargo.lock Cargo.toml crates/*/Cargo.toml interop/Cargo.toml

set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
    sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
}

major=false
case "${1:-}" in
    "") ;;
    --major) major=true ;;
    -h | --help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
esac

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

trap 'printf "\n\033[31mStopped: the update broke something (see above).\033[0m\nTo undo: git checkout -- Cargo.lock Cargo.toml crates/*/Cargo.toml interop/Cargo.toml\n" >&2' ERR

if $major; then
    if ! cargo upgrade --version >/dev/null 2>&1; then
        echo "--major needs cargo-edit: cargo install cargo-edit" >&2
        exit 1
    fi
    step "Moving to new major versions (Cargo.toml)"
    cargo upgrade --incompatible
fi

step "Updating Cargo.lock"
cargo update

step "Building"
cargo build --workspace --all-targets --locked

step "Linting"
cargo clippy --workspace --all-targets --locked

step "Testing"
cargo test --workspace --locked

step "Done: everything builds and passes"
git --no-pager diff --stat -- Cargo.lock Cargo.toml crates/*/Cargo.toml interop/Cargo.toml
echo
echo "Review with git diff, then commit."
