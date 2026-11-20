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
