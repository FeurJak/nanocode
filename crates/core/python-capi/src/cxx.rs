use std::{
    ffi::{c_char, c_int},
    ptr::null_mut,
    slice,
};

use rustpython_vm::{AsObject, builtins::PyType};

use crate::{
    abi::{Object, Type},
    bridge::{arg, borrowed, guard, object, text},
};

#[unsafe(no_mangle)]
pub extern "C" fn PyWeakref_NewRef(target: *mut Object, callback: *mut Object) -> *mut Object {
    object(|vm| {
        let weakref = vm.ctx.types.weakref_type.as_object();
        let target = arg(vm, target)?;
        if callback.is_null() || callback == crate::bridge::none() {
            weakref.call((target,), vm)
        } else {
            weakref.call((target, arg(vm, callback)?), vm)
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn _PyType_Lookup(ty: *mut Type, name: *mut Object) -> *mut Object {
    guard(null_mut(), |vm| {
        let class = arg(vm, ty.cast())?;
        let Some(class) = class.downcast_ref::<PyType>() else {
            return Ok(null_mut());
        };
        let name = arg(vm, name)?;
        let Some(name) = name.downcast_ref::<rustpython_vm::builtins::PyStr>() else {
            return Ok(null_mut());
        };
        Ok(class
            .get_attr(vm.ctx.intern_str(name.as_wtf8()))
            .map_or(null_mut(), |value| borrowed(vm, &value)))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn PyUnicode_DecodeUTF32(
    data: *const c_char,
    length: isize,
    errors: *const c_char,
    byteorder: *mut c_int,
) -> *mut Object {
    object(|vm| {
        let order = if byteorder.is_null() {
            0
        } else {
            unsafe { *byteorder }
        };
        let encoding = match order {
            ..0 => "utf-32-le",
            0 => "utf-32",
            _ => "utf-32-be",
        };
        let bytes =
            unsafe { slice::from_raw_parts(data.cast::<u8>(), length.unsigned_abs()) }.to_vec();
        vm.call_method(
            vm.ctx.new_bytes(bytes).as_object(),
            "decode",
            (encoding, text(errors).unwrap_or("strict")),
        )
    })
}

