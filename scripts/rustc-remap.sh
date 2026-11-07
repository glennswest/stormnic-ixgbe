#!/bin/sh
# rustc wrapper (#12), set in .cargo/config.toml: cargo runs it as
# `rustc-remap.sh <rustc> <args>`. For a crate compile (CARGO_MANIFEST_DIR
# set), map the crate's source directory to `<name>-<version>`, so panic and
# unwrap locations in dependencies don't embed the CARGO_HOME path. Probes
# (`rustc -vV`, `--print`) run unchanged.
rustc="$1"
shift
if [ -n "${CARGO_MANIFEST_DIR:-}" ] && [ -n "${CARGO_PKG_NAME:-}" ]; then
    exec "$rustc" "--remap-path-prefix=$CARGO_MANIFEST_DIR=$CARGO_PKG_NAME-$CARGO_PKG_VERSION" "$@"
fi
exec "$rustc" "$@"
