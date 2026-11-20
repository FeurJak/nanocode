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

#[test]
fn an_expansion_error_in_an_external_commands_redirection_fails_only_it() {
    assert_eq!(out("cat <<< ${zz?boom} 2>/dev/null; echo \"rc=$?\""), "rc=127\n");
    assert_eq!(out("cat <<< $((1/0)) 2>/dev/null; echo \"rc=$?\""), "rc=1\n");
    assert_eq!(out("cat <<E 2>/dev/null\n${zz?boom}\nE\necho \"rc=$?\""), "rc=127\n");
    assert_eq!(out("cat <<E 2>/dev/null || echo \"or $?\"\n$((1/0))\nE\necho \"after $?\""), "or 1\nafter 0\n");
    // A builtin's, a function's and a compound's are the shell's own, so the line is
    // abandoned.
    assert_eq!(out("true <<E 2>/dev/null || echo or\n$((1/0))\nE\necho \"after $?\""), "after 1\n");
    assert_eq!(out("f() { cat; }; f <<E 2>/dev/null; echo same\n$((1/0))\nE\necho \"after $?\""), "after 1\n");
    assert_eq!(out("{ cat; } <<E 2>/dev/null; echo same\n$((1/0))\nE\necho \"after $?\""), "after 1\n");
    assert_eq!(out("true <<< ${zz?boom} 2>/dev/null; echo \"rc=$?\""), "");
}

#[test]
fn a_command_word_starting_with_a_percent_resumes_a_job() {
    let (_, o, e) = run("%y; echo \"rc=$?\"; %y() { echo fn; }; %y");
    assert_eq!(String::from_utf8_lossy(&o), "rc=1\n");
    assert!(String::from_utf8_lossy(&e).contains("fg: no job control"));
    assert!(String::from_utf8_lossy(&run("%1 &\nwait").2).contains("bg: no job control"));
}

#[test]
fn printf_time_of_an_empty_or_unrepresentable_argument_is_the_epoch() {
    // Only a missing argument is now; an empty one is 0 and a time past what
    // localtime can represent falls back to 0 (out of range ones only warn).
    let (s, o, e) = run(
        "export TZ=UTC; printf '%(%Y)T|' '' 99999999999999999999 67768036191676800 0x10; \
         printf '%(%S)T\\n' 0x10",
    );
    assert_eq!(String::from_utf8_lossy(&o), "1970|1970|1970|1970|16\n");
    assert_eq!(s, 0);
    let e = String::from_utf8_lossy(&e);
    assert!(e.contains("warning: 99999999999999999999: Numerical result out of range"));
}

#[test]
fn printf_floats_are_long_doubles() {
    // bash converts with strtold, so values past f64's range keep their digits, and
    // nan and inf print in lowercase (uppercase for F E G A).
    let (_, o, e) = run(
        "printf '%e|%g|%.20f|%a\\n' 1e400 1e-4950 0.1 0.1; \
         printf '%f|%F|%E|%g|%+G|%5.1f|%-5e|%A\\n' nan -nan inf -inf nan inf -inf nan; \
         printf -v v '%.0f' 1e400; echo ${#v}; printf '%.2f\\n' -n",
    );
    assert_eq!(
        String::from_utf8_lossy(&o),
        "1.000000e+400|1.09356e-4950|0.10000000000000000000|0xc.ccccccccccccccdp-7\n\
         nan|-NAN|INF|-inf|+NAN|  inf|-inf |NAN\n401\n0.00\n"
    );
    let e = String::from_utf8_lossy(&e);
    assert!(e.contains("warning: 1e-4950: Numerical result out of range"));
    assert!(e.contains("-n: invalid number"));
}

#[test]
fn printf_floats_take_what_strtod_converts() {
    let (_, o, e) = run("printf '%.2f\\n' 2228e9x 12e3abc 1.5x 0x1Ap1 1e e3 -n");
    assert_eq!(
        String::from_utf8_lossy(&o),
        "2228000000000.00\n12000.00\n1.50\n52.00\n1.00\n0.00\n0.00\n"
    );
    let e = String::from_utf8_lossy(&e);
    assert!(e.contains("2228e9x: invalid number") && e.contains("1e: invalid number"));
    assert!(!e.contains("0x1Ap1"));
}

#[test]
fn a_forked_simple_command_that_fails_to_expand_runs_the_inherited_exit_trap() {
    // bash's child for a pipeline stage or job holds the trap until it runs the
    // command, so the trap's output goes down the pipe.
    let s = "trap 'echo bye' EXIT; echo $((1 % 0)) 2>/dev/null | { read -r q; echo \"got $q\"; }";
    assert_eq!(out(s), "got bye\nbye\n");
    assert_eq!(out("trap 'echo bye' EXIT; echo a | x=$((1/0)) 2>/dev/null | cat; echo after"), "bye\nafter\nbye\n");
    assert_eq!(out("trap 'echo bye' EXIT; echo $((1/0)) 2>/dev/null & wait; echo after"), "bye\nafter\nbye\n");
    // Not for a subshell, a group, a command substitution, a function's body, a failing
    // builtin or a prefix assignment to a readonly variable.
    assert_eq!(out("trap 'echo bye' EXIT; ( echo $((1/0)) ) 2>/dev/null | cat; echo after"), "after\nbye\n");
    assert_eq!(out("trap 'echo bye' EXIT; { echo $((1/0)); } 2>/dev/null | cat; echo after"), "after\nbye\n");
    assert_eq!(out("trap 'echo bye' EXIT; x=$(echo $((1/0))) 2>/dev/null; echo after"), "after\nbye\n");
    assert_eq!(out("trap 'echo bye' EXIT; f(){ echo $((1/0)); }; f 2>/dev/null | cat; echo after"), "after\nbye\n");
    assert_eq!(out("trap 'echo bye' EXIT; cd /nope 2>/dev/null | cat; echo after"), "after\nbye\n");
    assert_eq!(out("trap 'echo bye' EXIT; readonly r=1; r=2 echo hi 2>/dev/null | cat; echo after"), "hi\nafter\nbye\n");
    assert_eq!(out("trap 'echo bye' EXIT; ( echo $((1/0)) 2>/dev/null | cat ); echo after"), "after\nbye\n");
    assert_eq!(out("trap 'echo bye' EXIT; (trap 'echo in' EXIT; echo $((1/0)) 2>/dev/null | cat); echo after"), "in\nin\nafter\nbye\n");
}

