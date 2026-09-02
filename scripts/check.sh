#!/usr/bin/env bash
set -euo pipefail
n=$(find .github/workflows -maxdepth 1 \( -name '*.yml' -o -name '*.yaml' \) | wc -l)
if [ "$n" -ne 1 ]; then
  echo "error: expected exactly one GitHub Actions workflow, found $n" >&2
  find .github/workflows -maxdepth 1 \( -name '*.yml' -o -name '*.yaml' \) -print >&2
  exit 1
fi
if ! grep -qE '^on:' .github/workflows/ci.yml; then
  echo "error: .github/workflows/ci.yml is missing top-level on:" >&2
  exit 1
fi
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny check   # requires cargo-deny installed; local reproduction of §9's CI lint step (Opus-audit-caught, F21 — without this, deny.toml's license/advisory check has no local counterpart and its first-ever run would be in CI on the first push, which is exactly what §0's "test locally so we don't thrash CI/CD" rules out)
