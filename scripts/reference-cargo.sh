#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
case "${1:-}" in metadata) set -- metadata --locked --offline --format-version=1 ;; check) [ $# = 2 ] || exit 2; set -- check --locked --offline -p "$2" ;; *) echo 'usage: scripts/reference-cargo.sh metadata | check <package>' >&2; exit 2 ;; esac
rust=$(dirname "$(rustup which --toolchain 1.97.1 cargo)")
mkdir -p "$root/output"
home=$(mktemp -d "$root/output/reference-cargo-XXXXXX")
trap 'rm -rf "$home"' EXIT HUP INT TERM
mkdir -p "$home/cargo"
cd "$root"
vendor=$(printf '%s' "$root/vendor/cargo" | sed 's/\\/\\\\/g; s/"/\\"/g')
set -- cargo --config 'source.crates-io.replace-with="reference-vendored-sources"' --config "source.reference-vendored-sources.directory=\"$vendor\"" "$@"
[ "${SHIPPY_REMOTE+x}" != x ] || set -- "SHIPPY_REMOTE=$SHIPPY_REMOTE" "$@"
if [ "${RUSTC_WRAPPER+x}" = x ]; then
  [ -n "$RUSTC_WRAPPER" ] || { echo 'RUSTC_WRAPPER must not disable the configured wrapper' >&2; exit 2; }
  set -- "RUSTC_WRAPPER=$RUSTC_WRAPPER" "$@"
fi
if [ "${RUSTC_WORKSPACE_WRAPPER+x}" = x ]; then
  [ -n "$RUSTC_WORKSPACE_WRAPPER" ] || { echo 'RUSTC_WORKSPACE_WRAPPER must not be empty' >&2; exit 2; }
  set -- "RUSTC_WORKSPACE_WRAPPER=$RUSTC_WORKSPACE_WRAPPER" "$@"
fi
if [ "${CARGO_BUILD_RUSTC_WRAPPER+x}" = x ]; then
  [ -n "$CARGO_BUILD_RUSTC_WRAPPER" ] || { echo 'CARGO_BUILD_RUSTC_WRAPPER must not disable the configured wrapper' >&2; exit 2; }
  set -- "CARGO_BUILD_RUSTC_WRAPPER=$CARGO_BUILD_RUSTC_WRAPPER" "$@"
fi
if [ "${CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER+x}" = x ]; then
  [ -n "$CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER" ] || { echo 'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER must not be empty' >&2; exit 2; }
  set -- "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER=$CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER" "$@"
fi
unshare --user --map-root-user --net -- env -i HOME="$home" CARGO_HOME="$home/cargo" PATH="$rust:/usr/bin:/bin" CARGO_NET_OFFLINE=true NANOCODE_BUILDCACHE_URL=off SHIPPY_REMOTE_BUILD=0 "$@"
