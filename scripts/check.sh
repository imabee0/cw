#!/usr/bin/env bash
set -euo pipefail
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny check   # requires cargo-deny installed; local reproduction of §9's CI lint step (Opus-audit-caught, F21 — without this, deny.toml's license/advisory check has no local counterpart and its first-ever run would be in CI on the first push, which is exactly what §0's "test locally so we don't thrash CI/CD" rules out)
