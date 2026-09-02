#!/usr/bin/env bash
set -euo pipefail
# Every file in .github/workflows must be a real workflow — a snippet
# (cargo-dist github-build-setup) placed there is run as its own workflow
# and fails every push.
for f in .github/workflows/*.yml .github/workflows/*.yaml; do
  [ -f "$f" ] || continue
  if ! grep -qE '^on:' "$f"; then
    echo "error: $f is not a GitHub Actions workflow (missing top-level on:)" >&2
    exit 1
  fi
done
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny check   # requires cargo-deny installed; local reproduction of §9's CI lint step (Opus-audit-caught, F21 — without this, deny.toml's license/advisory check has no local counterpart and its first-ever run would be in CI on the first push, which is exactly what §0's "test locally so we don't thrash CI/CD" rules out)
