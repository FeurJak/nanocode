//! Utilities matching GNU on cases found by comparing psh with bash.

use std::process::Command;

fn sh(src: &str) -> (String, i32) {
    let dir = std::env::temp_dir().join(format!(
        "psh-bp-utils-{}-{:x}",
        std::process::id(),
        src.len() * 31 + src.bytes().map(usize::from).sum::<usize>()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_psh"))
        .arg("-c")
        .arg(src)
        .current_dir(&dir)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn xargs_runs_the_items_before_an_unmatched_quote() {
    assert_eq!(
        sh("printf '%s\\n' x x \"q'q\" | xargs echo; echo $?"),
        ("x x\n1\n".into(), 0)
    );
}

#[test]
fn basename_takes_a_dash_operand_as_the_suffix() {
    assert_eq!(sh("basename end -x; basename x -s"), ("end\nx\n".into(), 0));
}

#[test]
fn comm_checks_order_once_a_line_is_unpairable() {
    let src = "printf '%s\\n' 3 1 2 10 b a b > f1; printf '%s\\n' b a b c | comm -12 - f1 2>&1";
    assert_eq!(
        sh(src),
        (
            "comm: file 2 is not in sorted order\nb\ncomm: file 1 is not in sorted order\na\nb\ncomm: input is not in sorted order\n".into(),
            1
        )
    );
    let src = "printf '%s\\n' 3 1 2 10 b a b > f1; sed s/a/X/ f1 | comm -12 - f1 2>&1";
    assert_eq!(sh(src), ("3\n1\n2\n10\nb\nb\n".into(), 0));
}

#[test]
fn join_keeps_a_trailing_empty_field_and_empty_lines() {
    let src = "printf 'k1 v1\\nk2 v2\\n' > f2; printf 'k1 \\n' | join - f2";
    assert_eq!(sh(src), ("k1  v1\n".into(), 0));
    let src = "printf 'k1 v1\\nk2 v2\\nb y\\n' > f2; echo | join - f2 2>/dev/null";
    assert_eq!(sh(src), (String::new(), 1));
}


#[test]
fn numfmt_pads_and_converts_one_field() {
    assert_eq!(sh("numfmt --padding=8 42; numfmt --padding=-4 7"), ("      42\n7   \n".into(), 0));
    let src = "printf 'a 2048\\nb 10\\n' | numfmt --field=2 --to=iec; echo 123456 | numfmt --grouping";
    assert_eq!(sh(src), ("a 2.0K\nb 10\n123456\n".into(), 0));
}

#[test]
fn wc_writes_each_count_before_the_next_inputs_error() {
    assert_eq!(
        sh("printf '1\\n2\\n' > f; wc -l f nope f 2>&1; echo $?"),
        (
            "2 f\nwc: nope: No such file or directory\n2 f\n4 total\n1\n".into(),
            0
        )
    );
}

#[test]
fn xargs_reports_a_missing_command_in_its_own_words() {
    assert_eq!(
        sh("echo a | xargs nosuchcmd 2>&1; echo $?"),
        ("xargs: nosuchcmd: No such file or directory\n127\n".into(), 0)
    );
}

#[test]
fn cksum_prints_the_posix_crc_and_digests_like_gnu() {
    let src = "printf 'hello\\n' > a; : > e; cksum a e; printf 'hello\\n' | cksum; cksum nope a 2>/dev/null; echo $?";
    assert_eq!(sh(src), ("3015617425 6 a\n4294967295 0 e\n3015617425 6\n3015617425 6 a\n1\n".into(), 0));
    let src = "printf 'hello\\n' > a; cksum -a md5 a; cksum -a sha256 --untagged a; cksum -a sha1 --base64 a; cksum -a bsd a; cksum -a sysv a";
    assert_eq!(
        sh(src),
        (
            "MD5 (a) = b1946ac92492d2347c6235b4d2611184\n5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03  a\nSHA1 (a) = 9XLTlvrpIGYocU+yzgD3LpTyJY8=\n36979     1 a\n542 1 a\n"
                .into(),
            0
        )
    );
}

#[test]
fn sort_rejects_two_ways_to_compare_one_key() {
    let src = "printf '2\\n1\\n' > f; sort -n -g f 2>&1; echo $?; sort -k1,1n -k1,1Vh f 2>&1; echo $?; sort -n -r -b f; echo $?";
    assert_eq!(
        sh(src),
        ("sort: options '-gn' are incompatible\n2\nsort: options '-hV' are incompatible\n2\n2\n1\n0\n".into(), 0)
    );
}

#[test]
fn wc_pads_redirected_stdin_to_its_size() {
    let src = "printf 'a b\\nc\\n' > f; wc < f; wc -l -c < f; printf 'x\\n' | wc -l -c";
    assert_eq!(sh(src), ("2 3 6\n2 6\n      1       2\n".into(), 0));
}

#[test]
fn expand_joins_every_tab_list_and_wants_them_ascending() {
    let src = "printf 'a\\tb\\tc\\td\\n' > f; expand -t2,6 -t8 f; expand -t4 -t2 f 2>&1; echo $?; unexpand -t2,6 -t4 f 2>&1; echo $?";
    assert_eq!(
        sh(src),
        ("a b   c d\nexpand: tab sizes must be ascending\n1\nunexpand: tab sizes must be ascending\n1\n".into(), 0)
    );
}

#[test]
fn tail_of_nothing_prints_no_headers() {
    let src = "echo x > a; echo y > b; tail -n 0 a b; tail -c 0 -v a; tail -n +2 a b; echo $?";
    assert_eq!(sh(src), ("==> a <==\n\n==> b <==\n0\n".into(), 0));
}

#[test]
fn grep_quiet_wins_over_count_and_file_lists() {
    let src = "echo x > a; grep -q -L y a; echo $?; grep -q -c y a; echo $?; grep -q -l x a; echo $?";
    assert_eq!(sh(src), ("1\n1\n0\n".into(), 0));
}

#[test]
fn cut_empty_output_delimiter_is_nul() {
    let src = "echo 'a b c' | cut -d' ' -f1,3 --output-delimiter='' | od -An -c";
    assert_eq!(sh(src), ("   a  \\0   c  \\n\n".into(), 0));
}
