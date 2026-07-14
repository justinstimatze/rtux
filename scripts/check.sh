#!/bin/bash
# check.sh — the single check suite, run by BOTH the local pre-commit hook
# (.githooks/pre-commit) and CI (.github/workflows/ci.yml), so local and CI
# strictness can't drift. Debug builds (fast) — the optimized build lives in the
# release workflow. Builds and tests always gate. clippy gates when clippy is
# installed (as on CI) and skips with a note otherwise; formatting is advisory
# (report-only) either way. So on any box with a full rustup toolchain — and on
# CI — the strictness is identical; a minimal toolchain still runs builds+tests.
# Flip fmt to a hard gate (add `|| fail=1`) once it's verified clean everywhere.
set -uo pipefail
cd "$(git rev-parse --show-toplevel)" || exit 1

fail=0
step() { echo; echo "== $* =="; }

step "fmt (advisory)"
if cargo fmt --version >/dev/null 2>&1; then
    cargo fmt --all --check \
        || echo "  formatting differs — run: cargo fmt --all  (advisory, not gating)"
else
    echo "  rustfmt not installed — skipping (rustup component add rustfmt)"
fi

step "clippy"
if cargo clippy --version >/dev/null 2>&1; then
    cargo clippy --all-targets --features hud || fail=1
else
    echo "  clippy not installed — skipping (rustup component add clippy)"
fi

step "build — daemon only (no GUI deps)"
cargo build || fail=1

step "build — hud"
cargo build --features hud || fail=1

step "test"
cargo test --features hud || fail=1

echo
if [[ $fail -ne 0 ]]; then
    echo "✗ checks FAILED"
    exit 1
fi
echo "✓ checks passed"
