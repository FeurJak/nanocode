//! Fingerprints the fold's source for checkpoints: a board reopens from a checkpoint only when
//! the build that wrote it folded with the same source, so any change here falls back to a full
//! replay once instead of trusting a fold an older build produced.

use std::{env, fs, path::Path};

fn collect(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
    let entries = fs::read_dir(dir).expect("board-fold's src directory reads");
    for entry in entries {
        let path = entry.expect("a src entry reads").path();
        if path.is_dir() {
            collect(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

/// The sources the fold's behaviour rests on beyond its own: the state machines its stages run
/// on and the schema compiler its blocks check with. A build outside the repository, without
/// them beside it, hashes its own source alone.
const BESIDE: [&str; 2] = ["../machine/src", "../schema/src"];

fn main() {
    let root = env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let root = Path::new(&root);
    let mut files = Vec::new();
    for dir in std::iter::once("src").chain(BESIDE) {
        let dir = root.join(dir);
        if dir.is_dir() {
            println!("cargo:rerun-if-changed={}", dir.display());
            collect(&dir, &mut files);
        }
    }
    files.sort();
    // FNV-1a over each file's path and its bytes without carriage returns, so a
    // checkout's line endings don't change it.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for file in files {
        let name = file
            .strip_prefix(root)
            .expect("a file under the crate or beside it")
            .to_string_lossy()
            .replace('\\', "/");
        let text = fs::read(&file).expect("a src file reads");
        let bytes = name
            .bytes()
            .chain([0])
            .chain(text.into_iter().filter(|byte| *byte != b'\r'))
            .chain([0]);
        for byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    println!("cargo:rustc-env=NANOCODE_BOARD_FOLD_SOURCE={hash:016x}");
}
