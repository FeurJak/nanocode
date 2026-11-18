# psh decisions

## 2026-11-17: preserve negative-zero spelling in flat loops

Integer slots retain numeric values, not the original bytes. Previously, loading
`x=-0` into a slot made `echo "$x"` print `0` inside a compiled loop; assignments
and word-list iteration could lose the same spelling. GNU Bash 5.2.21 preserves
`-0` until an arithmetic write normalizes it.

Only lossless decimal spellings may enter integer slots. Negative zero uses the
existing interpreter path, including literal assignments, variable reloads, and
word-list loop variables. Arithmetic evaluation still accepts negative zero.

The regression failed before the change (`0\n` instead of `-0\n`). Nine text
preservation cases and three arithmetic controls pass with step budgets 1 and
1000, each on synchronous and delayed test hosts: 48 executions. Completion is
followed by Idle in every case. The nine preservation results were independently
checked against `/usr/bin/bash`, not psh's nested `bash` command.

The commands in AGENTS.md pass with Rust 1.100.0 on Linux x86_64: four engine unit
tests, two documentation tests, and native and wasm32 library checks. This is
focused regression coverage, not general Bash compatibility or a performance
claim. The standalone lockfile records nanocode-sys's existing local djs-libm
dependency; no package versions or release artifacts change.
