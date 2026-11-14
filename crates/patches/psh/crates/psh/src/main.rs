//! The `psh` program: `psh::run_as` with the process's arguments (`lib.rs`).

fn main() -> std::process::ExitCode {
    let mut args = std::env::args_os();
    let arg0 = args.next().unwrap_or_default();
    psh::run_as(arg0, args.collect())
}
