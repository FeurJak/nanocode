//! The `read` builtin on a regular file reads ahead a block and keeps what lies past
//! its line for its next read; whoever else uses the file's offset sees it where the
//! shell stopped, as bash leaves it.

use std::process::Command;

fn sh(name: &str, src: &str) -> String {
    let dir = std::env::temp_dir().join(format!("psh-bp-read-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("f"), "one\ntwo\nthree\nfour\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_psh"))
        .arg("-c")
        .arg(src)
        .current_dir(&dir)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_program_after_read_reads_on_from_the_next_line() {
    assert_eq!(
        sh("cat", "{ read a; echo \"<$a>\"; cat; } < f"),
        "<one>\ntwo\nthree\nfour\n"
    );
    assert_eq!(
        sh("head", "{ read a; read b; head -n1; echo \"$a$b\"; } < f"),
        "three\nonetwo\n"
    );
}

#[test]
fn reads_and_programs_take_turns() {
    let src = "exec 3< f; read -u3 a; sed -n 1p <&3; read -u3 b; echo \"$a $b\"; cat <&3";
    // sed reads the rest of the file and leaves the offset at its end; bash's sed
    // seeks back to the end of the line it printed (GNU sed does not).
    let out = sh("turns", src);
    assert!(out.starts_with("two\n"), "{out:?}");
}

#[test]
fn a_while_read_loop_with_a_program_inside_keeps_its_place() {
    let src = "while read -r l; do echo \"[$l]\"; read -r m <&0; /bin/echo \"($m)\"; done < f";
    assert_eq!(sh("loop", src), "[one]\n(two)\n[three]\n(four)\n");
}

#[test]
fn a_subshell_and_a_background_job_see_the_offset() {
    assert_eq!(
        sh(
            "sub",
            "{ read a; (read b; echo \"$b\"); read c; echo \"$a $c\"; } < f"
        ),
        "two\none three\n"
    );
    assert_eq!(
        sh("bg", "{ read a; cat & wait; echo \"$a\"; } < f"),
        "two\nthree\nfour\none\n"
    );
}

#[test]
fn the_offset_after_the_shell_ends_is_past_its_lines() {
    // The shell reads one line of the file the outer command shares with it.
    let psh = env!("CARGO_BIN_EXE_psh");
    let src = format!("{{ {psh} -c 'read a; echo \"$a\"'; cat; }} < f");
    assert_eq!(sh("exit", &src), "one\ntwo\nthree\nfour\n");
}

#[test]
fn a_seek_or_a_write_sees_the_offset() {
    assert_eq!(
        sh(
            "write",
            "exec 3<> f; read -u3 a; echo X >&3; exec 3<&-; cat f"
        ),
        "one\nX\no\nthree\nfour\n"
    );
}
