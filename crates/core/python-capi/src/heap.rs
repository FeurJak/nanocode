use std::{
    alloc::{Layout, alloc, alloc_zeroed, dealloc, realloc},
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::c_void,
    mem::size_of,
    ptr::null_mut,
};

use rustpython_vm::PyObject;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Plain,
    Proxy,
    Pending,
    Instance,
}

#[repr(C)]
pub struct Header {
    size: usize,
    pub kind: Cell<Kind>,
    pub queued: Cell<bool>,
    pub parked: Cell<bool>,
    pub link: Cell<*const PyObject>,
}

pub const ALIGN: usize = 16;
const HEADER: usize = size_of::<Header>().next_multiple_of(ALIGN);

fn layout(size: usize) -> Option<Layout> {
    Layout::from_size_align(HEADER.checked_add(size.max(1))?, ALIGN).ok()
}

pub fn allocate(size: usize, zeroed: bool) -> *mut c_void {
    let Some(layout) = layout(size) else {
        return null_mut();
    };
    let base = unsafe {
        if zeroed {
            alloc_zeroed(layout)
        } else {
            alloc(layout)
        }
    };
    if base.is_null() {
        return null_mut();
    }
    unsafe {
        base.cast::<Header>().write(Header {
            size,
            kind: Cell::new(Kind::Plain),
            queued: Cell::new(false),
            parked: Cell::new(false),
            link: Cell::new(std::ptr::null()),
        });
        base.add(HEADER).cast()
    }
}

#[cfg(target_family = "wasm")]
fn image() -> (usize, usize) {
    unsafe extern "C" {
        static __heap_base: u8;
    }
    (0, &raw const __heap_base as usize)
}

#[cfg(all(not(target_family = "wasm"), not(target_vendor = "apple")))]
fn image() -> (usize, usize) {
    unsafe extern "C" {
        static __executable_start: u8;
        static _end: u8;
    }
    (
        &raw const __executable_start as usize,
        &raw const _end as usize,
    )
}

#[cfg(target_vendor = "apple")]
fn image() -> (usize, usize) {
    unsafe extern "C" {
        static _mh_execute_header: u8;
        fn get_end() -> usize;
    }
    (&raw const _mh_execute_header as usize, unsafe { get_end() })
}

pub fn is_static<T>(pointer: *mut T) -> bool {
    let (start, end) = image();
    (start..end).contains(&(pointer as usize))
}

thread_local! {
    static STATIC_HEADERS: RefCell<HashMap<usize, &'static Header>> = RefCell::new(HashMap::new());
}

pub unsafe fn header<'a, T>(pointer: *mut T) -> &'a Header {
    if is_static(pointer) {
        return STATIC_HEADERS.with_borrow_mut(|headers| {
            *headers.entry(pointer as usize).or_insert_with(|| {
                Box::leak(Box::new(Header {
                    size: 0,
                    kind: Cell::new(Kind::Plain),
                    queued: Cell::new(false),
                    parked: Cell::new(false),
                    link: Cell::new(std::ptr::null()),
                }))
            })
        });
    }
    unsafe { &*pointer.cast::<u8>().sub(HEADER).cast::<Header>() }
}

pub unsafe fn release(pointer: *mut c_void) {
    if pointer.is_null() {
        return;
    }
    let base = unsafe { pointer.cast::<u8>().sub(HEADER) };
    let size = unsafe { (*base.cast::<Header>()).size };
    if let Some(layout) = layout(size) {
        unsafe { dealloc(base, layout) };
    }
}

pub unsafe fn resize(pointer: *mut c_void, size: usize) -> *mut c_void {
    if pointer.is_null() {
        return allocate(size, false);
    }
    let base = unsafe { pointer.cast::<u8>().sub(HEADER) };
    let old = unsafe { (*base.cast::<Header>()).size };
    let (Some(from), Some(to)) = (layout(old), layout(size)) else {
        return null_mut();
    };
    let moved = unsafe { realloc(base, from, to.size()) };
    if moved.is_null() {
        return null_mut();
    }
    unsafe {
        (*moved.cast::<Header>()).size = size;
        moved.add(HEADER).cast()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn PyMem_Malloc(size: usize) -> *mut c_void {
    allocate(size, false)
}

#[unsafe(no_mangle)]
pub extern "C" fn PyMem_Calloc(count: usize, size: usize) -> *mut c_void {
    count
        .checked_mul(size)
        .map_or(null_mut(), |size| allocate(size, true))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn PyMem_Realloc(pointer: *mut c_void, size: usize) -> *mut c_void {
    unsafe { resize(pointer, size) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn PyMem_Free(pointer: *mut c_void) {
    unsafe { release(pointer) }
}

#[unsafe(no_mangle)]
pub extern "C" fn PyMem_RawMalloc(size: usize) -> *mut c_void {
    allocate(size, false)
}

#[unsafe(no_mangle)]
pub extern "C" fn PyMem_RawCalloc(count: usize, size: usize) -> *mut c_void {
    PyMem_Calloc(count, size)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn PyMem_RawRealloc(pointer: *mut c_void, size: usize) -> *mut c_void {
    unsafe { resize(pointer, size) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn PyMem_RawFree(pointer: *mut c_void) {
    unsafe { release(pointer) }
}

#[unsafe(no_mangle)]
pub extern "C" fn PyObject_Malloc(size: usize) -> *mut c_void {
    allocate(size, false)
}

#[unsafe(no_mangle)]
pub extern "C" fn PyObject_Calloc(count: usize, size: usize) -> *mut c_void {
    PyMem_Calloc(count, size)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn PyObject_Realloc(pointer: *mut c_void, size: usize) -> *mut c_void {
    unsafe { resize(pointer, size) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn PyObject_Free(pointer: *mut c_void) {
    unsafe { release(pointer) }
}
