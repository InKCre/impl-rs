#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

sdk_manifest=vendor/ext-reg/runtimes/core-rs/Cargo.toml
cargo fmt --manifest-path "$sdk_manifest" -- --check
cargo clippy --manifest-path "$sdk_manifest" --all-targets --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --manifest-path "$sdk_manifest" --no-deps --locked
