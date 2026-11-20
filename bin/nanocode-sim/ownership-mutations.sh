#!/usr/bin/env bash
set -euo pipefail
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
mkdir -p "$root/output"
work=$(mktemp -d "$root/output/ownership-mutations.XXXXXX")
git clone --quiet --shared "$root" "$work/core"
for path in crates/runtime/durability/src/session.rs bin/nanocode-sim/src/whole.rs bin/nanocode-sim/src/whole/ownership.rs; do
  mkdir -p "$work/core/$(dirname "$path")"
  cp "$root/$path" "$work/core/$path"
done
cd "$work/core"
export SHIPPY_REMOTE_BUILD=0
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-1}
cargo run --offline --locked -p nanocode-sim -- --scenario runtime --store memory --deferred-start --calm --seed 17 --twice > "$work/baseline.log" 2>&1
printf 'baseline_exit=0\n' > "$work/exits.txt"
python3 - <<'PY'
from pathlib import Path
p = Path('crates/runtime/durability/src/session.rs')
s = p.read_text()
old = '*self = Self::Owned(owner.clone());'
assert s.count(old) == 1
p.write_text(s.replace(old, '*self = Self::Deferred(owner.clone());'))
PY
set +e
cargo run --offline --locked -p nanocode-sim -- --scenario runtime --store memory --deferred-start --calm --seed 17 --twice > "$work/mutant.log" 2>&1
status=$?
printf 'mutant_exit=%s\n' "$status" >> "$work/exits.txt"
set -e
if [ "$status" -eq 0 ]; then
  printf 'FAIL: ownership mutation escaped: %s\n' "$work"
  exit 1
fi
if ! grep -q 'panicked at' "$work/mutant.log" || ! grep -q 'initial settlement obligation survived successful admission' "$work/mutant.log"; then
  printf 'FAIL: mutation did not reach the ownership invariant: %s\n' "$work"
  exit 1
fi
printf 'PASS: production ownership mutation caught; evidence: %s\n' "$work"
