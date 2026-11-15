fn main() {
    let manifest = std::fs::read_to_string("../../Cargo.toml")
        .expect("nanocode-front builds inside the nanocode repository");
    let version = manifest
        .split_once("[workspace.package]")
        .and_then(|(_, package)| {
            package
                .lines()
                .find_map(|line| line.strip_prefix("version = "))
        })
        .map(|quoted| quoted.trim().trim_matches('"'))
        .expect("the workspace manifest sets [workspace.package] version");
    println!("cargo:rerun-if-changed=../../Cargo.toml");
    println!("cargo:rustc-env=NANOCODE_VERSION={version}");
    for arg in [
        "-nostartfiles",
        "-nostdlib",
        "-static",
        "-no-pie",
        "-Wl,--build-id=none",
        "-Wl,-z,norelro",
        "-Wl,-z,noexecstack",
    ] {
        println!("cargo:rustc-link-arg-bins={arg}");
    }
}
