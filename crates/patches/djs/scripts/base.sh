#!/usr/bin/env bash
# Remake djs-prelude's base snapshot (crates/djs-prelude/base/) with djs-base.
#   scripts/base.sh            write base.img and base.rs (after changing djs-core, djs-libm
#                              or builtins/gen/, which djs-prelude refuses to compile without)
#   scripts/base.sh --check    exit 1 if they are out of date (nanocode-sim --tidy checks too)
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo run --quiet --release -p djs-base -- "$@"
