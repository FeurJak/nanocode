//! The base snapshot (ARCH G2, "Images and dumps"; D1321): the image of an idle VM with its
//! realm and prelude, made by djs-base (`scripts/base.sh`) when djs-core changes, checked
//! in under `base/` and embedded here. A new VM is a copy of it, with no compile at startup.
//!
//! What the base holds: the atom table, the realm (global object, intrinsics, builtin
//! functions), the compiled prelude and the interpreter state of an idle VM. Nothing per
//! VM: no handles, no host functions, no pending calls; embedders add those after
//! [`new_vm`].
//!
//! `Vm::new` (which compiles the prelude) stays for tests and for making this image.

#![no_std]

extern crate alloc;

use djs_core::heap::{HeapError, LoadError};
use djs_core::vm::{Vm, VmOptions};

include!("../base/base.rs");
// The base image loads only into the djs-core that made it. A change to djs-core's sources,
// djs-libm's or the prelude, or other djs-core features than djs-base's, fails here
// instead of at the first `new_vm` (D2411).
const _: () = assert!(
    BUILT_BY == djs_core::vm::BUILD_HASH,
    "base/base.img was made by another djs-core build: run crates/patches/djs/scripts/base.sh, or enable djs-base's djs-core features through djs-prelude's"
);

// `BASE_NAMES`: the base's builtins and their paths for dumps (`DumpOptions::base_names`).

/// The base image (`Vm::bytes` of a fresh, collected VM of this build).
#[cfg(feature = "raw")]
pub static BASE: &[u8] = include_bytes!("../base/base.img");

// The coded base as the wasm module's custom section `djs_base` (`BASE_SECTION`): not in
// linear memory, so not readable by the instance itself. A `pub` static without
// `#[used]`: with `#[used]` (or through `include_bytes!`) the bytes also stay in the data
// segment.

/// A new VM from [`BASE`]. `opts.prelude` is ignored: the base has the prelude.
/// Fails only if the memory limit is smaller than the base.
#[cfg(feature = "raw")]
pub fn new_vm(opts: VmOptions) -> Result<Vm, HeapError> {
    new_vm_reserved(opts, 0)
}

/// [`new_vm`] with the arena capacity of a heap of `live` bytes
/// ([`Vm::from_base_reserved`]).
#[cfg(feature = "raw")]
pub fn new_vm_reserved(opts: VmOptions, live: usize) -> Result<Vm, HeapError> {
    new_vm_from_reserved(BASE, opts, live)
}

/// A new VM from `base`, which is this build's base image ([`BASE`] or
/// [`unpack_base`]'s).
pub fn new_vm_from(base: &[u8], opts: VmOptions) -> Result<Vm, HeapError> {
    new_vm_from_reserved(base, opts, 0)
}

/// [`new_vm_from`] with the arena capacity of a heap of `live` bytes.
pub fn new_vm_from_reserved(base: &[u8], opts: VmOptions, live: usize) -> Result<Vm, HeapError> {
    match Vm::from_base_reserved(base, opts, live) {
        Ok(vm) => Ok(vm),
        Err(LoadError::TooLarge) => Err(HeapError::OutOfMemory),
        // Made for this build by djs-base and verified there: anything else is a build bug.
        Err(e) => panic!("the embedded base snapshot does not load: {e}"),
    }
}

/// A new VM whose arena is `base`, this build's base image in a buffer the caller owns
/// (D2060): no copy. With the arena capacity of a heap of `live` bytes.
pub fn new_vm_owned(
    base: alloc::vec::Vec<u8>,
    opts: VmOptions,
    live: usize,
) -> Result<Vm, HeapError> {
    match Vm::from_base_vec(base, opts, live) {
        Ok(vm) => Ok(vm),
        Err(LoadError::TooLarge) => Err(HeapError::OutOfMemory),
        Err(e) => panic!("the embedded base snapshot does not load: {e}"),
    }
}
