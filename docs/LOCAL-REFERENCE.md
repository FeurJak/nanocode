# Local release references

## Reference inputs and qualification

The root Cargo graph includes its five registry dependencies in `vendor/cargo`, with their upstream notices. The reference runner selects these sources only for its own Cargo invocation and disables build-cache prefetch with `NANOCODE_BUILDCACHE_URL=off`. It preserves compiler wrappers and leaves nested workspaces unchanged.
CLI archives are not included: the prebuilt reference path needs separately reviewed local inputs. These checks do not establish an offline CLI, Swift or Python source build.
The original frozen results below describe core 6ed92be87b3a2f704a7af857a2237b3fccafbb27. The same lockfile at runtime revision 7d4cbc88a4e284cdc716728090a7bd291803ecc4 passed fresh-home Cargo metadata and a focused `nanocode-bytes` check with Cargo offline and cache prefetch disabled. That later host did not support an OS network namespace; it is not a second OS-isolation qualification.

Two local paths are separate: materialize the pinned CLI binaries, or resolve and check the Rust source graph.
Neither path publishes a release, changes branch settings or removes workflows.

## Prebuilt CLI

On Linux x86_64, run:

```sh
scripts/reference-cli.sh stage
```

The command enters an isolated Linux user and network namespace with a cleared environment and a fresh home.
It verifies all six platforms' archive hashes, input manifests, executable headers, gzip/xz equivalence and Python content hashes.
It copies the sixteen checked archives and metadata to a new `output/reference-cli-*/dist` directory.
It also executes only the Linux x86_64 CLI's `--version` mode. No inference request runs.
Other platforms receive header and hash checks only. These checks do not prove native execution on those platforms.

`vendor/reference-cli/reference.json` pins the exact public release bytes for core 6ed92be87b3a2f704a7af857a2237b3fccafbb27.
The public assets have no attached build attestations. A checksum or executable header is not a provenance attestation.
This path materializes prebuilt Rust outputs. It does not rebuild six native targets from source.

Linux, glibc, Python 3.12, util-linux and coreutils are external prerequisites, not vendored toolchains.
Retain the repository licenses and the license files in its third-party crate sources when distributing the reference.

## Cargo sources

```sh
scripts/reference-cargo.sh metadata > output/reference-metadata.json
scripts/reference-cargo.sh check nanocode-bytes
```

Install Rust 1.97.1 before these offline commands. They require the installed toolchain and never install it.
Both commands clear the environment, isolate networking, create an empty Cargo home and keep `SHIPPY_REMOTE_BUILD=0`.
They preserve only configured `SHIPPY_REMOTE`, `RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER`, `CARGO_BUILD_RUSTC_WRAPPER` and `CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER` alongside the minimal toolchain environment. Explicitly empty wrapper overrides are rejected. The repository's compiler wrapper remains configured. Organization API credentials are not forwarded.

The root workspace has 87 members. Complete metadata resolution includes 134 packages without a target-platform filter.
In that frozen qualification, 129 used path sources and five used the separately retained registry mirror in `vendor/cargo`.
There are no git-source packages in this resolved root graph. The five registry crates retain their licenses and Cargo checksum manifests.
Only `scripts/reference-cargo.sh` opts into this five-crate source replacement for its own invocation. Never put it in global `.cargo/config.toml`.

The metadata command proves dependency resolution, not compilation. The focused check proves only the named package.
Neither proves every Cargo feature, an independent nested workspace, six target builds or platform linking.
Full native builds still require their Rust targets, C/C++ toolchains and SDKs. Those toolchains are not vendored here.
Apple SDKs require an authorized Apple environment. Windows MSVC builds require the corresponding Windows SDK and toolchain.
The current CLI builder Dockerfile downloads its image and apt packages. It is not an offline reference build.
Likewise, `release-cli.sh` and `release-train.sh` remain online release/deploy paths, not offline entrypoints.

To refresh the registry sources after a reviewed lockfile change, run `cargo +1.97.1 vendor --locked vendor/cargo` during acquisition.
Review its configuration output rather than overwriting the existing compiler-wrapper configuration.
Then repeat the metadata command with an empty Cargo home and networking denied.

## Workflow removal gate

Core and nanocode-ts already contain no workflow files at the verified source epochs. No deletion was needed for those repositories.
Swift's workflow removal has a separate qualified, reviewed local replacement. Its evidence does not qualify a new core release.
Do not confuse a successful prebuilt reference with a completed source-build or platform qualification matrix.
Required-status settings must also be inspected before any approved removal. Do not bypass branch protection.
