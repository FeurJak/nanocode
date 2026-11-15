use djs_libm as libm;

#[unsafe(no_mangle)]
extern "C" fn atan2(y: f64, x: f64) -> f64 {
    libm::atan2(y, x)
}

#[unsafe(no_mangle)]
extern "C" fn cos(x: f64) -> f64 {
    libm::cos(x)
}

#[unsafe(no_mangle)]
extern "C" fn exp(x: f64) -> f64 {
    libm::exp(x)
}

#[unsafe(no_mangle)]
extern "C" fn ldexp(x: f64, n: i32) -> f64 {
    libm::ldexp(x, n)
}

#[unsafe(no_mangle)]
extern "C" fn log(x: f64) -> f64 {
    libm::log(x)
}

#[unsafe(no_mangle)]
extern "C" fn log10(x: f64) -> f64 {
    libm::log10(x)
}

#[unsafe(no_mangle)]
extern "C" fn log2(x: f64) -> f64 {
    libm::log2(x)
}

#[unsafe(no_mangle)]
extern "C" fn logf(x: f32) -> f32 {
    libm::log(f64::from(x)) as f32
}

#[unsafe(no_mangle)]
extern "C" fn pow(x: f64, y: f64) -> f64 {
    libm::pow(x, y)
}

#[unsafe(no_mangle)]
extern "C" fn sin(x: f64) -> f64 {
    libm::sin(x)
}

#[unsafe(no_mangle)]
extern "C" fn sinf(x: f32) -> f32 {
    libm::sin(f64::from(x)) as f32
}

#[unsafe(no_mangle)]
unsafe extern "C" fn sincos(x: f64, sin: *mut f64, cos: *mut f64) {
    unsafe {
        sin.write(libm::sin(x));
        cos.write(libm::cos(x));
    }
}
