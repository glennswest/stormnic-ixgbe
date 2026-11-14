#!/bin/sh
# rustc wrapper (#12), set in .cargo/config.toml: cargo runs it as
# `rustc-remap.sh <rustc> <args>`. For a crate compile (CARGO_MANIFEST_DIR
# set), map the crate's source directory to `<name>-<version>`, so panic and
# unwrap locations in dependencies don't embed the CARGO_HOME path. Probes
# (`rustc -vV`, `--print`) run unchanged.
#
# Under `cargo clippy` the workspace wrapper comes first
# (`rustc-remap.sh clippy-driver <rustc> <args>`); clippy-driver wants the
# rustc path as its first argument, so the flag goes after it.
prog="$1"
shift
inner=""
if [ "$#" -gt 0 ] && [ "$(basename -- "$1")" = rustc ]; then
    inner="$1"
    shift
fi
if [ -n "${CARGO_MANIFEST_DIR:-}" ] && [ -n "${CARGO_PKG_NAME:-}" ]; then
    set -- "--remap-path-prefix=$CARGO_MANIFEST_DIR=$CARGO_PKG_NAME-$CARGO_PKG_VERSION" "$@"
fi
if [ -n "$inner" ]; then
    exec "$prog" "$inner" "$@"
fi
exec "$prog" "$@"
