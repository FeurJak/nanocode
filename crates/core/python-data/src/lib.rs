#![no_std]

extern crate alloc;

use alloc::vec::Vec;

include!(concat!(env!("OUT_DIR"), "/site_data.rs"));

#[cfg(target_family = "wasm")]
mod blob {
    use super::{DATA_LEN, Vec};
    use alloc::vec;

    #[unsafe(link_section = "nanocode.python.data")]
    #[allow(dead_code)]
    static DATA: [u8; DATA_LEN] = *include_bytes!(concat!(env!("OUT_DIR"), "/python-data.bin"));

    #[link(wasm_import_module = "python")]
    unsafe extern "C" {
        fn host_data(offset: u32, bytes: *mut u8, len: u32) -> u32;
    }

    pub fn read(offset: u32, len: u32) -> Option<Vec<u8>> {
        let mut bytes = vec![0; len as usize];
        (unsafe { host_data(offset, bytes.as_mut_ptr(), len) } == 0).then_some(bytes)
    }
}

#[cfg(not(target_family = "wasm"))]
mod blob {
    use super::Vec;

    static DATA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/python-data.bin"));

    pub fn read(offset: u32, len: u32) -> Option<Vec<u8>> {
        let start = offset as usize;
        DATA.get(start..start + len as usize).map(<[u8]>::to_vec)
    }
}

pub fn read(offset: u32, len: u32) -> Option<Vec<u8>> {
    blob::read(offset, len)
}

fn module(name: &str) -> Option<&'static (&'static str, bool, u32, u32)> {
    MODULES
        .binary_search_by(|entry| entry.0.cmp(name))
        .ok()
        .map(|index| &MODULES[index])
}

pub fn find_module(name: &str) -> Option<bool> {
    module(name).map(|entry| entry.1)
}

pub fn load_module(name: &str) -> Option<Vec<u8>> {
    let entry = module(name)?;
    blob::read(entry.2, entry.3)
}
