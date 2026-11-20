fn main() {
    let linux = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "linux");
    let gnu = std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|env| env == "gnu");
    if linux && gnu {
        println!("cargo:rustc-link-arg-bins=-no-pie");
    }
}
