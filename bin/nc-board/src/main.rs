#[cfg(not(target_family = "wasm"))]
mod cli;
#[cfg(not(target_family = "wasm"))]
mod dir;

#[cfg(not(target_family = "wasm"))]
fn main() -> std::process::ExitCode {
    cli::main()
}

#[cfg(target_family = "wasm")]
fn main() {}
