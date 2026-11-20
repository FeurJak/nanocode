# Local checks

Run from the repository root. This keeps the standalone workspace's static-link
flags out of the native test harness. Preserve compiler wrappers.

```sh
export SHIPPY_REMOTE_BUILD=0 SHIPPY_EXEC=local CARGO_BUILD_JOBS=2
cargo test --locked --offline --manifest-path crates/patches/psh/Cargo.toml \
  -p psh-engine -p psh-syntax -p psh-regex
cargo check --locked --offline --manifest-path crates/patches/psh/Cargo.toml \
  -p psh-engine --lib
cargo check --locked --offline --manifest-path crates/patches/psh/Cargo.toml \
  -p psh-engine --lib --target wasm32-unknown-unknown
```

The Wasm check requires the `wasm32-unknown-unknown` Rust standard library, acquired
before checking. The focused crates require no registry downloads. These commands
check the vendored engine, not the released SDK or a provider deployment.
