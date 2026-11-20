use std::io::Read;

use nanocode::codex_tools::apply_patch;
use psh_engine::util::{BlockingOptions, LocalBoxFuture, Mode, SyncCtx, UtilCtx, UtilEntry};

pub const UTILITY: UtilEntry = UtilEntry {
    destructive: false,
    mode: Mode::PathFallback,
    names: &["apply_patch", "applypatch"],
    run: utility,
};

fn utility<'a>(ctx: &'a mut UtilCtx<'_>, argv: &'a [Vec<u8>]) -> LocalBoxFuture<'a, i32> {
    Box::pin(ctx.run_blocking(BlockingOptions { stdin: true }, argv.to_vec(), body))
}

fn body(ctx: &mut SyncCtx, argv: Vec<Vec<u8>>) -> i32 {
    let patch = match argv.as_slice() {
        [_] => {
            let mut patch = Vec::new();
            if let Err(error) = ctx.stdin().read_to_end(&mut patch) {
                ctx.error_raw(
                    format!("Error: Failed to read PATCH from stdin.\n{error}\n").as_bytes(),
                );
                return 1;
            }
            if patch.is_empty() {
                ctx.error_raw(b"Usage: apply_patch 'PATCH'\n       echo 'PATCH' | apply-patch\n");
                return 2;
            }
            patch
        }
        [_, patch] => patch.clone(),
        _ => {
            ctx.error_raw(b"Error: apply_patch accepts exactly one argument.\n");
            return 2;
        }
    };
    let Ok(patch) = String::from_utf8(patch) else {
        ctx.error_raw(b"Error: apply_patch requires a UTF-8 PATCH argument.\n");
        return 1;
    };
    let directory = match ctx.native_path(ctx.cwd()) {
        Some(Ok(directory)) => directory,
        Some(Err(error)) => {
            ctx.error(&error.to_string());
            return 1;
        }
        None => {
            ctx.error("needs the machine's own files");
            return 1;
        }
    };
    ctx.no_replay();
    match apply_patch::apply(&patch, &directory) {
        Ok(summary) => match ctx.write(summary.as_bytes()) {
            Ok(()) => 0,
            Err(_) => 1,
        },
        Err(error) => {
            ctx.error_raw(format!("{error}\n").as_bytes());
            1
        }
    }
}
