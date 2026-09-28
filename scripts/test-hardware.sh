#!/usr/bin/env bash
# Run only on the remote build box, through sc-build after pushing.
set -euo pipefail
mkdir -p target
rustc --edition=2021 --test test/hardware.rs -o target/hardware-tests
target/hardware-tests
