use std::{env, fmt::Write as _, fs, path::Path};

use nanocode_compression::deflate::{
    Deflate, DeflateConfig, DeflateFlush, DeflateStatus, Strategy,
};

const DATA: &[&str] = &["matplotlib/mpl-data"];

fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .expect("data directory")
        .map(|entry| entry.expect("entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_none_or(|extension| extension != "py") {
            out.push(path);
        }
    }
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let config = DeflateConfig {
        level: 9,
        window_bits: -15,
        mem_level: 9,
        strategy: Strategy::Default,
    };
    let mut deflate = Deflate::new(config).expect("deflate");
    let mut out = vec![0; data.len() + data.len() / 8 + 1024];
    let status = deflate
        .deflate(data, &mut out, DeflateFlush::Finish)
        .expect("deflate");
    assert_eq!(status, DeflateStatus::StreamEnd);
    out.truncate(usize::try_from(deflate.total_out()).expect("size"));
    out
}

fn main() {
    println!("cargo:rerun-if-changed=site");
    let site = Path::new(env!("CARGO_MANIFEST_DIR")).join("site");
    let out = Path::new(&env::var("OUT_DIR").expect("OUT_DIR")).to_path_buf();
    let mut files = Vec::new();
    for dir in DATA {
        walk(&site.join(dir), &mut files);
    }
    let mut table = String::from("pub static FILES: &[(&str, u32, &[u8])] = &[\n");
    for (index, path) in files.iter().enumerate() {
        let data = fs::read(path).expect("data file");
        let packed = out.join(format!("site-data-{index}.z"));
        fs::write(&packed, deflate(&data)).expect("write");
        let name = path
            .strip_prefix(&site)
            .expect("under site")
            .to_str()
            .expect("utf-8 path");
        writeln!(
            table,
            "    ({name:?}, {}, include_bytes!({:?})),",
            data.len(),
            packed.to_str().expect("utf-8 path")
        )
        .expect("format");
    }
    table.push_str("];\n");
    fs::write(out.join("site_data.rs"), table).expect("write");
}
