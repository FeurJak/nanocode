//! Makes the base snapshot djs-prelude embeds (ARCH "Images and dumps", D1321): a VM with
//! the prelude installed, collected, as a heap image. The GC is deterministic (D1090), so
//! the same djs-core gives the same bytes; the image is verified here once, so loading it
//! can skip verification. The image and its constants are checked in under
//! djs-prelude/base/ so that djs-prelude needs no build script: a build script would link
//! djs-core, and everything above djs-prelude would wait for djs-core's code generation
//! instead of its metadata.

use std::path::{Path, PathBuf};

use djs_core::vm::{Vm, VmOptions};

pub const IMAGE: &str = "base.img";
pub const CONSTANTS: &str = "base.rs";

/// The base image and the Rust constants djs-prelude includes with it.
pub struct Base {
    pub image: Vec<u8>,
    pub constants: String,
}

pub fn make() -> Base {
    let mut vm = Vm::new(VmOptions::default()).expect("out of memory making the base");
    // Without growth slack (0043), canonical (0101), like cells together (D2251).
    vm.prepare_base_image()
        .expect("out of memory making the base image");
    let image = vm.bytes().to_vec();
    // Full verification (cell structure, references, build hash) when it is made.
    if let Err(e) = Vm::from_bytes(image.clone(), VmOptions::default()) {
        panic!("the base image does not load: {e}");
    }
    let base_end = djs_core::heap::Heap::image_base_end(&image);
    // BUILT_BY: the build hash of the djs-core that made the image, which djs-prelude
    // compares with its own djs-core's.
    let constants = format!(
        "// Made by crates/patches/djs/scripts/base.sh with base.img: do not edit.\n\n/// The base image's builtin prefix (`Roots::BASE_END`).\npub const BASE_END: u32 = {base_end};\nconst BUILT_BY: u32 = {:#010x};\n",
        djs_core::vm::BUILD_HASH
    );
    Base { image, constants }
}

/// djs-prelude/base/, where the base is checked in.
pub fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../djs-prelude/base")
}

/// The files under `dir` that differ from `base`.
pub fn stale(dir: &Path, base: &Base) -> Vec<&'static str> {
    let mut out = Vec::new();
    if std::fs::read(dir.join(IMAGE)).ok().as_deref() != Some(&base.image[..]) {
        out.push(IMAGE);
    }
    if std::fs::read(dir.join(CONSTANTS)).ok().as_deref() != Some(base.constants.as_bytes()) {
        out.push(CONSTANTS);
    }
    out
}

pub fn write(dir: &Path, base: &Base) -> std::io::Result<()> {
    std::fs::write(dir.join(IMAGE), &base.image)?;
    std::fs::write(dir.join(CONSTANTS), &base.constants)
}
