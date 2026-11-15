use std::process::ExitCode;

const USAGE: &str = "\
usage: djs-base [--check]

Makes djs-prelude's base snapshot with this djs-core and writes
crates/djs-prelude/base/base.img and base.rs. Run it after changing djs-core,
djs-libm or builtins/gen/: djs-prelude does not compile with a base another
djs-core build made.

  --check     do not write; exit 1 if the checked-in base is out of date
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let check = match args.as_slice() {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let base = djs_base::make();
    let dir = djs_base::dir();
    let stale = djs_base::stale(&dir, &base);
    if check {
        if stale.is_empty() {
            return ExitCode::SUCCESS;
        }
        eprintln!(
            "out of date in {}: {} (run scripts/base.sh)",
            dir.display(),
            stale.join(", ")
        );
        return ExitCode::FAILURE;
    }
    if let Err(e) = djs_base::write(&dir, &base) {
        eprintln!("cannot write {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }
    println!("{} bytes, {}", base.image.len(), dir.display());
    ExitCode::SUCCESS
}
