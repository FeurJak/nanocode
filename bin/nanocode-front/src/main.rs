#![no_std]
#![no_main]

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("nanocode-front is the Linux x86_64 front; other platforms run nanocode itself");

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod front {
    use core::arch::{asm, global_asm};
    use core::ffi::c_void;
    use core::mem::MaybeUninit;

    const VERSION: &[u8] = concat!("nanocode ", env!("NANOCODE_VERSION"), "\n").as_bytes();
    const AGENT: &[u8] = b"nanocode-agent\0";
    const EXEC: &[u8] = b"nanocode-exec\0";
    const SELF: &[u8] = b"/proc/self/exe\0";
    const PATH_MAX: usize = 4096;
    const AT_FDCWD: isize = -100;
    const NOT_RUN: usize = 127;
    const ENAMETOOLONG: isize = 36;
    const ENOENT: isize = 2;

    mod nr {
        pub const WRITE: usize = 1;
        pub const EXECVE: usize = 59;
        pub const EXIT_GROUP: usize = 231;
        pub const READLINKAT: usize = 267;
    }

    global_asm!(".globl _start", "_start:", "mov rdi, rsp", "and rsp, -16", "call {entry}", "ud2", entry = sym entry);

    unsafe fn syscall(number: usize, a: usize, b: usize, c: usize, d: usize) -> isize {
        let result: isize;
        unsafe {
            asm!("syscall", inlateout("rax") number as isize => result, in("rdi") a, in("rsi") b, in("rdx") c, in("r10") d, lateout("rcx") _, lateout("r11") _, options(nostack));
        }
        result
    }

    fn write(fd: usize, bytes: &[u8]) -> isize {
        unsafe { syscall(nr::WRITE, fd, bytes.as_ptr() as usize, bytes.len(), 0) }
    }

    fn exit(code: usize) -> ! {
        unsafe {
            syscall(nr::EXIT_GROUP, code, 0, 0, 0);
            core::hint::unreachable_unchecked()
        }
    }

    unsafe fn equals(argument: *const u8, expected: &[u8]) -> bool {
        expected
            .iter()
            .enumerate()
            .all(|(index, byte)| unsafe { *argument.add(index) } == *byte)
    }

    fn fail(stage: &[u8], path: &[u8], error: isize) -> ! {
        let mut digits = [0u8; 20];
        let mut at = digits.len();
        let mut value = error.unsigned_abs();
        loop {
            at -= 1;
            digits[at] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        for part in [
            b"nanocode: cannot ".as_slice(),
            stage,
            b" ",
            path,
            b": errno ",
            &digits[at..],
            b"\n",
        ] {
            write(2, part);
        }
        exit(NOT_RUN)
    }

    unsafe extern "C" fn entry(stack: *const usize) -> ! {
        let argc = unsafe { *stack };
        let argv = unsafe { stack.add(1) } as *const *const u8;
        let envp = unsafe { argv.add(argc + 1) };
        if argc == 2 {
            let argument = unsafe { *argv.add(1) };
            if unsafe { equals(argument, b"--version\0") || equals(argument, b"-V\0") } {
                exit(usize::from(write(1, VERSION) < 0));
            }
        }
        let mut buffer = MaybeUninit::<[u8; PATH_MAX + AGENT.len()]>::uninit();
        let length = unsafe {
            syscall(
                nr::READLINKAT,
                AT_FDCWD as usize,
                SELF.as_ptr() as usize,
                buffer.as_mut_ptr() as usize,
                PATH_MAX,
            )
        };
        if length < 0 {
            fail(b"read", &SELF[..SELF.len() - 1], length);
        }
        let length = length as usize;
        if length == PATH_MAX {
            fail(b"read", &SELF[..SELF.len() - 1], -ENAMETOOLONG);
        }
        let start = buffer.as_mut_ptr() as *mut u8;
        let directory = unsafe { core::slice::from_raw_parts(start, length) }
            .iter()
            .rposition(|byte| *byte == b'/')
            .map_or(0, |slash| slash + 1);
        if argc >= 2 && unsafe { equals(*argv.add(1), b"exec\0") } {
            let (path, error) = unsafe { run(start, directory, EXEC, argv, envp) };
            if error != -ENOENT {
                fail(b"run", &path[..path.len() - 1], error)
            }
        }
        let (path, error) = unsafe { run(start, directory, AGENT, argv, envp) };
        fail(b"run", &path[..path.len() - 1], error)
    }

    unsafe fn run<'a>(
        start: *mut u8,
        directory: usize,
        name: &[u8],
        argv: *const *const u8,
        envp: *const *const u8,
    ) -> (&'a [u8], isize) {
        unsafe { start.add(directory).copy_from_nonoverlapping(name.as_ptr(), name.len()) };
        let path = unsafe { core::slice::from_raw_parts(start, directory + name.len()) };
        let error = unsafe {
            syscall(
                nr::EXECVE,
                path.as_ptr() as usize,
                argv as usize,
                envp as usize,
                0,
            )
        };
        (path, error)
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn memcpy(
        destination: *mut c_void,
        source: *const c_void,
        length: usize,
    ) -> *mut c_void {
        let (to, from) = (destination.cast::<u8>(), source.cast::<u8>());
        for index in 0..length {
            unsafe {
                to.add(index)
                    .write_volatile(from.add(index).read_volatile())
            };
        }
        destination
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn memset(destination: *mut c_void, byte: i32, length: usize) -> *mut c_void {
        let to = destination.cast::<u8>();
        for index in 0..length {
            unsafe { to.add(index).write_volatile(byte as u8) };
        }
        destination
    }

    #[unsafe(no_mangle)]
    const extern "C" fn rust_eh_personality() {}

    #[panic_handler]
    fn panic(_: &core::panic::PanicInfo) -> ! {
        exit(NOT_RUN)
    }
}
