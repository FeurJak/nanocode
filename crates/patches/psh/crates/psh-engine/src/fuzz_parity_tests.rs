//! Regressions found by differential fuzzing against bash 5.2.

use super::run_on_test_host as run;

fn out(s: &str) -> String {
    String::from_utf8_lossy(&run(s).1).into_owned()
}

#[test]
fn a_brace_expanded_declaration_argument_splits_like_a_command_word() {
    // bash: the words brace expansion makes lose their assignment flag.
    let s = "arr=(x 'y z' w.txt); export E=\"\"\"${arr[@]}\"-{a,b}c\"-x\"; echo \"$E\"";
    assert_eq!(out(s), "x\n");
    assert_eq!(out("x='m -1'; declare v={a,b}$x 2>/dev/null; echo \"$v\""), "bm\n");
    assert_eq!(out("x='m -1'; declare v={a,b}\"$x\"; echo \"$v\""), "bm -1\n");
    // Without a brace expansion it stays an assignment.
    assert_eq!(out("a=(x 'y z'); export E=\"${a[@]}\"; echo \"$E\""), "x y z\n");
    assert_eq!(out("f() { local a={1..3}; echo $a; }; f"), "3\n");
}

fn err(s: &str) -> String {
    String::from_utf8_lossy(&run(s).2).into_owned()
}

#[test]
fn a_simple_command_takes_the_line_its_second_token_ends_on() {
    assert_eq!(out("echo 'a\nb' $LINENO"), "a\nb 2\n");
    assert_eq!(out("echo $LINENO 'a\nb'"), "1 a\nb\n");
    assert!(err("nosuch 'a\nb'").contains("line 2: nosuch"));
    assert!(err("nosuch \\\nb").contains("line 2: nosuch"));
    assert!(err("{ nosuch 'a\nb'; }").contains("line 2: nosuch"));
    assert!(err("x=1 nosuch 'a\nb'").contains("line 1: nosuch"));
    assert!(err("nosuch a 'b\nc'").contains("line 1: nosuch"));
    assert!(err("nosuch\necho").contains("line 1: nosuch"));
}

#[test]
fn an_external_commands_redirections_expand_in_its_child() {
    assert_eq!(out("n=0; cat >/dev/null <<E\n$((n++))\nE\necho \"$n\""), "0\n");
    assert_eq!(out("n=0; cat <<< $((n++)); echo \"$n\""), "0\n0\n");
    assert_eq!(out("cat >${x:=g} </dev/null; echo \"[$x]\""), "[]\n");
    assert_eq!(out("cat {fd}>/dev/null </dev/null; echo \"[$fd]\""), "[]\n");
    assert_eq!(out("n=0; command cat >/dev/null <<< $((n++)); echo \"$n\""), "0\n");
    assert_eq!(out("n=0; nosuch >/dev/null <<< $((n++)) 2>/dev/null; echo \"$n\""), "0\n");
    // Builtins and functions run in the shell, so theirs do.
    assert_eq!(out("n=0; true <<< $((n++)); echo \"$n\""), "1\n");
    assert_eq!(out("n=0; f() { :; }; f <<< $((n++)); echo \"$n\""), "1\n");
    assert_eq!(out("exec {fd}>/dev/null; echo \"[$fd]\""), "[10]\n");
}
