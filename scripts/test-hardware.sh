#!/usr/bin/env bash
# Run only on the remote build box, through sc-build after pushing.
set -euo pipefail
mkdir -p target
for t in hardware rings snp decode; do
    rustc --edition=2021 --test "test/$t.rs" -o "target/$t-tests"
    "target/$t-tests"
done
