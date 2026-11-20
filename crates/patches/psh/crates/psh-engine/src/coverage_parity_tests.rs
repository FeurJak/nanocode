//! Regressions from the coverage-2 and coverage-3 bash parity suites (bash 5.2).

use super::run_on_test_host as run;

fn out(s: &str) -> String {
    String::from_utf8_lossy(&run(s).1).into_owned()
}

fn nctool_stub<'a>(
    _ctx: &'a mut crate::util::UtilCtx<'_>,
    _argv: &'a [Vec<u8>],
) -> crate::util::LocalBoxFuture<'a, i32> {
    Box::pin(async { 0 })
}

static SANDBOX_UTILS: [crate::util::UtilEntry; 1] = [crate::util::UtilEntry {
    names: &["nctool"],
    mode: crate::util::Mode::PathFallback,
    destructive: false,
    run: nctool_stub,
}];

/// Stdout of `source` in a sandbox shell (no program files) with a `nctool` utility.
fn sandbox_out(source: &str) -> String {
    use crate::{Script, Shell, Step};
    let host = std::rc::Rc::new(crate::testhost::TestHost::new());
    let mut cfg = host.config();
    cfg.env = vec![(b"PATH".to_vec(), b"/usr/local/bin:/usr/bin:/bin".to_vec())];
    cfg.utilities = crate::util::UtilPolicy {
        entries: &SANDBOX_UTILS,
        sandbox: true,
        ..crate::util::UtilPolicy::default()
    };
    let mut shell = Shell::new(cfg);
    shell.start(Script::command(source));
    while let Step::Yield | Step::Blocked = shell.step(1000) {}
    String::from_utf8_lossy(&host.stdout()).into_owned()
}

#[test]
fn sandbox_utilities_and_builtins_stand_in_for_programs_in_usr_bin() {
    assert_eq!(
        sandbox_out("type -a echo; type -P echo nctool; type nctool; command -V nctool"),
        "echo is a shell builtin\necho is /usr/bin/echo\n/usr/bin/echo\n/usr/bin/nctool\n\
         nctool is /usr/bin/nctool\nnctool is /usr/bin/nctool\n"
    );
    assert_eq!(sandbox_out("command -pv nctool"), "/usr/bin/nctool\n");
    assert_eq!(
        sandbox_out("hash nctool; echo $?; hash -t nctool; type nctool; hash -d nctool"),
        "0\n/usr/bin/nctool\nnctool is hashed (/usr/bin/nctool)\n"
    );
    assert_eq!(sandbox_out("PATH=/nowhere; type -P echo; echo $?"), "1\n");
}
