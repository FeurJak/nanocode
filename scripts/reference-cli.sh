#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
case "${1:-}" in verify|stage) ;; *) echo 'usage: scripts/reference-cli.sh verify|stage' >&2; exit 2 ;; esac
[ "$(uname -s)-$(uname -m)" = Linux-x86_64 ] || { echo 'reference qualification requires Linux x86_64' >&2; exit 1; }
mkdir -p "$root/output"
home=$(mktemp -d "$root/output/reference-home-XXXXXX")
trap 'rm -rf "$home"' EXIT HUP INT TERM
unshare --user --map-root-user --net -- env -i HOME="$home" PATH=/usr/bin:/bin SHIPPY_REMOTE_BUILD=0 NANOCODE_REFERENCE_NETWORK=denied python3 "$root/scripts/reference-cli.py" "$1"
