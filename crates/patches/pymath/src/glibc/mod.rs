//! Ports of glibc's libm routines that CPython 3.12 on Linux returns bit for
//! bit, used natively and on wasm alike. The native binary can't take them
//! from the system libm: nanocode-sys exports djs_libm's fdlibm versions under
//! the C names (atan2, log, log2, log10, sin, cos, ...) for the JavaScript
//! engine, which shadow glibc's.

mod atan2;
mod atan2_tbl;
mod log;
mod log_tbl;

pub(crate) use atan2::atan2;
pub(crate) use log::{log, log2, log10};
