//! A path that names one of the shell's descriptors (\`/dev/stdin\`, \`/dev/fd/N\`,
//! \`/proc/self/fd/N\`) opens a regular file again from its start, as Linux opens it
//! for bash, whatever the shell read of it before; a pipe is shared instead.

use std::process::Command;

fn sh(name: &str, src: &str) -> String {
    let dir = std::env::temp_dir().join(format!("psh-bp-devfd-{}-{name}", std::process::id()));
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
fn a_redirection_from_dev_stdin_starts_the_file_over() {
    for path in ["/dev/stdin", "/dev/fd/0", "/proc/self/fd/0"] {
        let src = format!("{{ read a; read b < {path}; read c; echo \"$a/$b/$c\"; }} < f");
        assert_eq!(sh("redir", &src), "one/one/two\n", "{path}");
    }
    assert_eq!(
        sh("fd3", "exec 3< f; read -u3 a; read b < /dev/fd/3; echo \"$a/$b\""),
        "one/one\n"
    );
}

#[test]
fn a_utility_opening_dev_stdin_reads_the_whole_file() {
    assert_eq!(
        sh("cat", "{ read a; cat /dev/stdin; } < f"),
        "one\ntwo\nthree\nfour\n"
    );
    assert_eq!(sh("head", "{ read a; head -1 /dev/stdin; read b; echo \"$b\"; } < f"), "one\ntwo\n");
}

#[test]
fn a_pipe_is_shared() {
    assert_eq!(
        sh("pipe", "printf 'one\\ntwo\\n' | { read a; read b < /dev/stdin; echo \"$a/$b\"; }"),
        "one/two\n"
    );
}

#[test]
fn writing_to_dev_stdout_truncates_or_appends_the_file() {
    assert_eq!(sh("trunc", "{ echo aaaa; echo b > /dev/stdout; } > g; cat g"), "b\n");
    assert_eq!(
        sh("append", "{ echo aaaa; echo b >> /dev/stdout; } > g; cat g"),
        "aaaa\nb\n"
    );
}
