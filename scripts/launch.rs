use std::collections::hash_map::DefaultHasher;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::hash::Hasher;
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

type Time = (i64, i64);

const SOURCE: &[u8] = include_bytes!("launch.rs");

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().collect();
    let arg = |index: usize| args.get(index).map(Path::new);
    let root = |index: usize| arg(index).and_then(|root| fs::canonicalize(root).ok());
    match (args.get(1).map(|command| command.as_bytes()), arg(2), arg(3), arg(4), arg(5)) {
        (Some(b"find"), Some(source), Some(_), Some(store), None) => match root(3) {
            Some(root) => find(source, &root, store),
            None => ExitCode::from(2),
        },
        (Some(b"built"), Some(_), Some(store), Some(stamp), None) => match root(2) {
            Some(root) => built(&root, store, stamp),
            None => ExitCode::from(2),
        },
        (Some(b"record"), Some(_), Some(store), Some(directory), Some(stamp)) => {
            let Some(root) = root(2) else {
                let _ = fs::remove_file(stamp);
                return ExitCode::FAILURE;
            };
            let root = root.as_path();
            let recorded = fs::metadata(stamp).ok().and_then(|stamp| {
                let before = (stamp.mtime(), stamp.mtime_nsec());
                let material = material(root, store, Some(before))?;
                record(&entry(store, &material), directory, &material).ok()
            });
            let _ = fs::remove_file(stamp);
            if recorded.is_some() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        _ => ExitCode::from(2),
    }
}

fn entry(store: &Path, material: &[u8]) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    hasher.write(material);
    store.join(format!("{:016x}", hasher.finish()))
}

fn find(source: &Path, root: &Path, store: &Path) -> ExitCode {
    if fs::read(source).ok().as_deref() != Some(SOURCE) {
        return ExitCode::from(3);
    }
    if let Some(material) = material(root, store, None)
        && found(&entry(store, &material), &material)
    {
        return ExitCode::SUCCESS;
    }
    let stamp = store.join(format!(".stamp.{}", std::process::id()));
    if fs::create_dir_all(store).and_then(|()| fs::write(&stamp, b"")).is_ok() {
        let _ = io::stdout().write_all(stamp.as_os_str().as_bytes());
    }
    ExitCode::FAILURE
}

fn built(root: &Path, store: &Path, stamp: &Path) -> ExitCode {
    let mut output = Vec::new();
    let binary = io::stdin().read_to_end(&mut output).ok().and_then(|_| {
        let mut binary = None;
        let mut finished = false;
        for line in output.split(|byte| *byte == b'\n') {
            finished |= line.starts_with(b"{\"reason\":\"build-finished\",\"success\":true");
            if line.starts_with(b"{\"reason\":\"compiler-artifact\"")
                && let Some(path) = executable(line)
                && path.ends_with(b"/nanocode")
            {
                binary = Some(path);
            }
        }
        binary.filter(|_| finished).map(|binary| PathBuf::from(OsString::from_vec(binary)))
    });
    let Some(binary) = binary.filter(|binary| runnable(binary) && runnable(&binary.with_file_name("nanocode-exec"))) else {
        let _ = fs::remove_file(stamp);
        return ExitCode::from(101);
    };
    let recording = env::current_exe().and_then(|helper| {
        Command::new(helper)
            .arg("record")
            .args([root, store, binary.parent().unwrap_or(Path::new("/")), stamp])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    });
    if recording.is_err() {
        let _ = fs::remove_file(stamp);
    }
    if io::stdout().write_all(binary.as_os_str().as_bytes()).is_ok() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

fn executable(line: &[u8]) -> Option<Vec<u8>> {
    let key = b"\"executable\":\"";
    let start = line.windows(key.len()).position(|window| window == key)? + key.len();
    let mut path = Vec::new();
    let mut bytes = line.get(start..)?.iter();
    while let Some(byte) = bytes.next() {
        match byte {
            b'"' => return Some(path),
            b'\\' => match bytes.next()? {
                escaped @ (b'"' | b'\\' | b'/') => path.push(*escaped),
                _ => return None,
            },
            byte => path.push(*byte),
        }
    }
    None
}

fn found(entry: &Path, material: &[u8]) -> bool {
    let binary = entry.join("nanocode");
    runnable(&entry.join("nanocode-exec"))
        && runnable(&binary)
        && fs::read(entry.join("key")).is_ok_and(|key| key == material)
        && io::stdout().write_all(binary.as_os_str().as_bytes()).is_ok()
}

fn runnable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn record(entry: &Path, built: &Path, material: &[u8]) -> io::Result<()> {
    fs::create_dir_all(entry)?;
    match fs::remove_file(entry.join("key")) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    for name in ["nanocode-exec", "nanocode", "key"] {
        let staging = entry.join(format!(".{name}.{}", std::process::id()));
        let written = if name == "key" { fs::write(&staging, material) } else { fs::hard_link(built.join(name), &staging).or_else(|_| fs::copy(built.join(name), &staging).map(drop)) };
        if let Err(error) = written.and_then(|()| fs::rename(&staging, entry.join(name))) {
            let _ = fs::remove_file(&staging);
            return Err(error);
        }
    }
    Ok(())
}

fn material(root: &Path, store: &Path, before: Option<Time>) -> Option<Vec<u8>> {
    let mut material = Vec::new();
    tracked(root, before, &mut material)?;
    toolchain(root, store, before, &mut material)?;
    material.extend_from_slice(format!("\n{}\n{}\nrelease\n", env::consts::OS, env::consts::ARCH).as_bytes());
    let home = env::var_os("HOME").map(PathBuf::from)?;
    let cargo_home = env::var_os("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".cargo"));
    let mut files: Vec<(&str, PathBuf)> = root
        .ancestors()
        .skip(1)
        .flat_map(|directory| {
            [".cargo/config", ".cargo/config.toml", "rust-toolchain", "rust-toolchain.toml"].map(|name| (name, directory.join(name)))
        })
        .collect();
    files.extend(["config", "config.toml"].map(|name| (name, cargo_home.join(name))));
    for (name, file) in files {
        if let Ok(text) = fs::read(&file) {
            older(&file, before)?;
            material.extend_from_slice(name.as_bytes());
            material.push(0);
            material.extend_from_slice(&text);
            material.push(0);
        }
    }
    let mut variables: Vec<(OsString, OsString)> = env::vars_os().filter(|(name, _)| relevant(name.as_bytes())).collect();
    variables.sort();
    for (name, value) in variables {
        material.extend_from_slice(name.as_bytes());
        material.push(b'=');
        material.extend_from_slice(value.as_bytes());
        material.push(0);
    }
    Some(material)
}

fn relevant(name: &[u8]) -> bool {
    let included = [
        "CARGO_", "RUST", "CC", "CXX", "AR", "LD", "CFLAGS", "CXXFLAGS", "CPPFLAGS", "PKG_CONFIG", "SDKROOT", "NANOCODE_",
        "VALGRIND", "TARGET", "HOST", "PATH", "LIBRARY_PATH", "CPATH",
    ];
    let excluded = [
        "CARGO_TARGET_DIR", "CARGO_HOME", "CARGO_TERM_", "CARGO_HTTP_", "CARGO_NET_", "CARGO_BUILD_JOBS", "CARGO_LOG",
        "NANOCODE_COMMAND_ID", "NANOCODE_BUILDCACHE_",
    ];
    (included.iter().any(|prefix| name.starts_with(prefix.as_bytes())) || name.ends_with(b"_DEPLOYMENT_TARGET"))
        && !excluded.iter().any(|prefix| name.starts_with(prefix.as_bytes()))
}

fn times(path: &Path) -> Option<[Time; 4]> {
    let link = fs::symlink_metadata(path).ok()?;
    let target = fs::metadata(path).unwrap_or_else(|_| link.clone());
    Some([
        (link.mtime(), link.mtime_nsec()),
        (link.ctime(), link.ctime_nsec()),
        (target.mtime(), target.mtime_nsec()),
        (target.ctime(), target.ctime_nsec()),
    ])
}

fn older(path: &Path, before: Option<Time>) -> Option<()> {
    match before {
        Some(before) => times(path)?.iter().all(|time| *time < before).then_some(()),
        None => Some(()),
    }
}

fn git_dir(root: &Path) -> Option<(PathBuf, PathBuf)> {
    let dot = root.join(".git");
    if fs::symlink_metadata(&dot).ok()?.is_dir() {
        return Some((dot.clone(), dot));
    }
    let text = fs::read(&dot).ok()?;
    let git = root.join(OsStr::from_bytes(text.strip_prefix(b"gitdir: ")?.trim_ascii_end()));
    let common = match fs::read(git.join("commondir")) {
        Ok(common) => git.join(OsStr::from_bytes(common.trim_ascii_end())),
        Err(_) => git.clone(),
    };
    Some((git, common))
}

fn tracked(root: &Path, before: Option<Time>, material: &mut Vec<u8>) -> Option<()> {
    let (git, common) = git_dir(root)?;
    let config = fs::read(common.join("config")).ok()?.to_ascii_lowercase();
    if config.windows(12).any(|window| window == b"objectformat") {
        return None;
    }
    let path = git.join("index");
    let metadata = fs::metadata(&path).ok()?;
    let data = fs::read(&path).ok()?;
    let again = fs::metadata(&path).ok()?;
    let stamp = (metadata.mtime(), metadata.mtime_nsec(), metadata.ino(), metadata.len());
    if stamp != (again.mtime(), again.mtime_nsec(), again.ino(), again.len()) || data.get(..4)? != b"DIRC" {
        return None;
    }
    let word = |at: usize| Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?));
    let half = |at: usize| Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?));
    let version = word(4)?;
    if version != 2 && version != 3 {
        return None;
    }
    let mut at = 12;
    for _ in 0..word(8)? {
        let flags = half(at + 60)?;
        if flags & 0xb000 != 0 || (flags & 0x4000 != 0 && (version != 3 || half(at + 62)? != 0)) {
            return None;
        }
        let start = if flags & 0x4000 != 0 { 64 } else { 62 };
        let length = match usize::from(flags & 0xfff) {
            0xfff => data.get(at + start..)?.iter().position(|byte| *byte == 0)?,
            length => length,
        };
        let name = data.get(at + start..at + start + length)?;
        let mode = word(at + 24)?;
        let file = root.join(OsStr::from_bytes(name));
        let current = fs::symlink_metadata(&file).ok()?;
        let kind = if current.file_type().is_symlink() {
            0o120000
        } else if current.file_type().is_file() && current.mode() & 0o100 != 0 {
            0o100755
        } else if current.file_type().is_file() {
            0o100644
        } else {
            return None;
        };
        let precise = word(at + 4)? != 0 || word(at + 12)? != 0;
        let fraction = |nanos: i64| if precise { nanos as u32 } else { 0 };
        let recorded = [word(at)?, word(at + 4)?, word(at + 8)?, word(at + 12)?, word(at + 20)?, word(at + 28)?, word(at + 32)?, word(at + 36)?];
        let seen = [
            current.ctime() as u32,
            fraction(current.ctime_nsec()),
            current.mtime() as u32,
            fraction(current.mtime_nsec()),
            current.ino() as u32,
            current.uid(),
            current.gid(),
            current.size() as u32,
        ];
        let written = (metadata.mtime() as u32, fraction(metadata.mtime_nsec()));
        let oid = data.get(at + 40..at + 60)?;
        if kind != mode || recorded != seen || ((recorded[2], recorded[3]) >= written && !same(&file, kind, oid)) {
            return None;
        }
        if let Some(before) = before
            && ((current.mtime(), current.mtime_nsec()) >= before || (current.ctime(), current.ctime_nsec()) >= before)
        {
            return None;
        }
        material.extend_from_slice(&mode.to_be_bytes());
        material.extend_from_slice(oid);
        material.extend_from_slice(name);
        material.push(0);
        at += (start + length + 8) & !7;
    }
    while data.len() >= at + 28 {
        if data.get(at..at + 4)? == b"link" {
            return None;
        }
        at += 8 + usize::try_from(word(at + 4)?).ok()?;
    }
    Some(())
}

fn same(file: &Path, kind: u32, oid: &[u8]) -> bool {
    let content = if kind == 0o120000 { fs::read_link(file).map(|target| target.into_os_string().into_vec()) } else { fs::read(file) };
    content.is_ok_and(|content| {
        let mut blob = format!("blob {}\0", content.len()).into_bytes();
        blob.extend_from_slice(&content);
        sha1(&blob) == oid
    })
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut state: [u32; 5] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in message.chunks_exact(64) {
        let mut words = [0u32; 80];
        for (index, word) in block.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..80 {
            words[index] = (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (index, word) in words.iter().enumerate() {
            let (f, k) = match index / 20 {
                0 => ((b & c) | (!b & d), 0x5a827999),
                1 => (b ^ c ^ d, 0x6ed9eba1),
                2 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let next = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*word);
            (e, d, c, b, a) = (d, c, b.rotate_left(30), a, next);
        }
        for (value, add) in state.iter_mut().zip([a, b, c, d, e]) {
            *value = value.wrapping_add(add);
        }
    }
    let mut digest = [0u8; 20];
    for (chunk, value) in digest.chunks_exact_mut(4).zip(state) {
        chunk.copy_from_slice(&value.to_be_bytes());
    }
    digest
}

fn toolchain(root: &Path, store: &Path, before: Option<Time>, material: &mut Vec<u8>) -> Option<()> {
    if env::var_os("RUSTC").is_some() {
        return None;
    }
    let probe = env::split_paths(&env::var_os("PATH")?).map(|directory| directory.join("rustc")).find(|path| runnable(path))?;
    let home = match env::var_os("RUSTUP_HOME") {
        Some(home) => PathBuf::from(home),
        None => PathBuf::from(env::var_os("HOME")?).join(".rustup"),
    };
    let mut inputs = vec![probe.clone(), probe.parent()?.join("../lib"), home.join("settings.toml"), home.join("toolchains")];
    for toolchain in fs::read_dir(home.join("toolchains")).into_iter().flatten().flatten() {
        inputs.extend([toolchain.path().join("bin/rustc"), toolchain.path().join("lib")]);
    }
    inputs.sort();
    let mut name = DefaultHasher::new();
    name.write(home.as_os_str().as_bytes());
    let mut newest = (i64::MIN, i64::MIN);
    for input in &inputs {
        if let Some(times) = times(input) {
            name.write(input.as_os_str().as_bytes());
            newest = times.into_iter().fold(newest, Time::max);
        }
    }
    for directory in root.ancestors() {
        for file in ["rust-toolchain", "rust-toolchain.toml"] {
            if let Ok(text) = fs::read(directory.join(file)) {
                name.write(&text);
            }
        }
    }
    if before.is_some_and(|before| newest >= before) {
        return None;
    }
    let memo = store.join(format!("rustc-{:016x}", name.finish()));
    let fresh = fs::metadata(&memo).is_ok_and(|memo| (memo.mtime(), memo.mtime_nsec()) > newest && memo.len() > 0);
    let version = match fresh.then(|| fs::read(&memo).ok()).flatten() {
        Some(version) => version,
        None => {
            let output = Command::new(&probe).arg("-vV").current_dir(root).output().ok()?;
            if !output.status.success() || output.stdout.is_empty() {
                return None;
            }
            let staging = store.join(format!(".rustc.{}", std::process::id()));
            if fs::create_dir_all(store).and_then(|()| fs::write(&staging, &output.stdout)).and_then(|()| fs::rename(&staging, &memo)).is_err() {
                let _ = fs::remove_file(&staging);
            }
            output.stdout
        }
    };
    material.extend_from_slice(&version);
    Some(())
}
