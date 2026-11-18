# Development

- Keep code simple and changes small. Match the surrounding code and preserve unrelated work.
- No code comments, doc comments, README edits, tests, test-only dependencies or Cargo features (`local-cells` on `nanocode-code-mode` is the one exception).
- `bin/nanocode-sim` is the deterministic simulator and the one exception to "no tests": checks live there and are run by hand by seed (`bin/nanocode-sim/sweep.sh`), and a new invariant there comes with a mutation the simulator catches, as tempoxyz/zempo requires.
- No magic constants or arbitrary caps: a limit comes from a protocol, a measurement or caller policy. Surface invalid states; never hide them with silent truncation, clamping or fallbacks.
- Make invalid states unrepresentable: model a lifecycle as one enum whose variants carry only what that phase has.
- Lock-free: no Mutex, RwLock or Condvar. Use nanocode-sync (an owning task, Actor or Mailbox; Atom changed copy-on-write with side effects after the commit; Slot, Signal, atomics). Blocking I/O runs on an owning worker behind channels, never on Tokio workers.
- One workspace of reusable crates that each build for native and WASM with parity; bindings, apps, services and wasm-bindgen live elsewhere. Follow [the crate layout](crates/README.md): contracts in `core/`, backend-generic orchestration in `runtime/`, each provider's wire client, tools and driver in `backends/<provider>`, host adapters in `integrations/`; `nanocode` only re-exports.
- Preserve public APIs. Use authentic, version-pinned upstream Claude and Codex prompts and tool shapes, keeping SDK additions separate, and share Code Mode and computer use across both backends.
- Inference, subagents and durability live in the brain; Code Mode runs on the user's machine. Never silently replay dispatched work: report uncertain outcomes.
- Validate with focused builds (`cargo check --locked --lib -p <crate>`, again with `--target wasm32-unknown-unknown`) and back performance changes with reproducible before/after measurements.
- Check behavior with harnesses in ignored `output/`, such as `output/hand-wiring`: a crate with its own `[workspace]` and path dependencies on these crates, one binary per check or benchmark, run against local mocks, with builds compared by interleaved runs. Keep every artifact there; never commit them, credentials or personal data.
- Work on master in the main checkout, commit only your own changes and push each one; no pull requests. There is no CI: build, release and deploy by hand. Use subagents only when asked.
- Keep reference-build dependencies in the repository. Separate online acquisition from builds. Verify local references with OS network access denied.
- Keep `SHIPPY_REMOTE_BUILD=0` for local checks. Preserve compiler wrappers. Record external toolchains and system libraries explicitly.
- Use `cf --profile tempo` for Cloudflare operations, with `CLOUDFLARE_ACCOUNT_ID=0a39052b0a32ba8c8444345fe21b7595`. Use Wrangler only for a documented missing capability.
- This file holds principles only and rarely changes. DECISIONS.md is the growing log: add what each change establishes and why, with the commands, dates and measurements behind it.

## Local reference checks

The root reference checks use the five registry sources in `vendor/cargo`, with upstream notices retained. CLI archives require separately reviewed local inputs.

```sh
scripts/reference-cargo.sh metadata
scripts/reference-cargo.sh check nanocode-bytes
```

The reference runner applies Cargo source replacement only to its own invocation; nested CLI, Swift and Python builds keep their existing configuration.
