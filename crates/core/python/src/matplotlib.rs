pub(crate) use _c_internal_utils::module_def;

#[rustpython_vm::pymodule(name = "matplotlib._c_internal_utils")]
mod _c_internal_utils {
    use rustpython_vm::{FromArgs, PyObjectRef, VirtualMachine};

    #[derive(FromArgs)]
    struct Window {
        #[pyarg(any, name = "hwnd")]
        _hwnd: PyObjectRef,
    }

    #[pyfunction]
    const fn display_is_valid() -> bool {
        false
    }

    #[pyfunction]
    const fn xdisplay_is_valid() -> bool {
        false
    }

    #[pyfunction(name = "Win32_GetCurrentProcessExplicitAppUserModelID")]
    fn get_app_user_model_id(vm: &VirtualMachine) -> PyObjectRef {
        vm.ctx.none()
    }

    #[pyfunction(name = "Win32_SetCurrentProcessExplicitAppUserModelID")]
    fn set_app_user_model_id(_appid: PyObjectRef) {}

    #[pyfunction(name = "Win32_GetForegroundWindow")]
    fn get_foreground_window(vm: &VirtualMachine) -> PyObjectRef {
        vm.ctx.none()
    }

    #[pyfunction(name = "Win32_SetForegroundWindow")]
    fn set_foreground_window(_window: Window) {}

    #[pyfunction(name = "Win32_SetProcessDpiAwareness_max")]
    const fn set_process_dpi_awareness_max() {}
}
