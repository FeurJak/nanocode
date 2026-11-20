//! Regressions from the realworld bash parity suite: snippets cut from shell scripts of
//! public projects (completion files, prompt themes, test helpers), checked against
//! bash 5.2.

use super::run_on_test_host as run;

fn all(s: &str) -> (i32, String, String) {
    let (st, o, e) = run(s);
    (
        st,
        String::from_utf8_lossy(&o).into_owned(),
        String::from_utf8_lossy(&e).into_owned(),
    )
}

#[test]
fn complete_keeps_and_prints_specs() {
    let (st, o, e) = all(
        "complete -F _foo foo; echo rc=$?; complete -p foo; \
         complete -o default -o nospace -W \"a b'c\" bar; complete -p bar; \
         complete -d -A function -F _g -P pre g; complete -p g",
    );
    assert_eq!(
        o,
        "rc=0\ncomplete -F _foo foo\ncomplete -o default -o nospace -W 'a b'\\''c' bar\n\
         complete -d -A function -P 'pre' -F _g g\n"
    );
    assert_eq!((st, e.as_str()), (0, ""));
}

#[test]
fn complete_remove_and_missing_specs() {
    let (st, o, e) = all(
        "complete -F f a b; complete -r a; echo rc=$?; complete -p a; echo rc=$?; \
         complete -r nosuch; echo rc=$?; complete -p b",
    );
    assert_eq!(o, "rc=0\nrc=1\nrc=0\ncomplete -F f b\n");
    assert_eq!(st, 0);
    assert!(e.contains("complete: a: no completion specification"), "{e}");
}

#[test]
fn complete_default_empty_and_bad_options() {
    let (_, o, _) = all("complete -o bashdefault -D; complete -E -F e; complete -p -D -E");
    assert_eq!(o, "complete -o bashdefault -D\ncomplete -F e -E\n");
    let (_, o, e) = all("complete -z x; echo rc=$?; complete -o bogus x; echo rc=$?");
    assert_eq!(o, "rc=2\nrc=2\n");
    assert!(e.contains("complete: -z: invalid option\ncomplete: usage: complete [-abcdefgjksuv]"), "{e}");
    assert!(e.contains("complete: bogus: invalid option name"), "{e}");
}

#[test]
fn compopt_outside_completion_and_on_a_name() {
    let (_, o, e) = all(
        "compopt -o nospace; echo rc=$?; complete -o nospace -F f x; compopt +o nospace -o default x; \
         complete -p x; compopt -o default nosuch; echo rc=$?",
    );
    assert_eq!(o, "rc=1\ncomplete -o default -F f x\nrc=1\n");
    assert!(e.contains("compopt: not currently executing completion function"), "{e}");
    assert!(e.contains("compopt: nosuch: no completion specification"), "{e}");
}

#[test]
fn bind_without_line_editing_warns_and_succeeds() {
    let (st, o, e) = all("bind '\"\\e[A\": history-search-backward'; echo rc=$?; bind -x '\"\\C-x\": foo'; echo rc=$?");
    assert_eq!((st, o.as_str()), (0, "rc=0\nrc=0\n"));
    assert_eq!(e.matches("bind: warning: line editing not enabled").count(), 2, "{e}");
}

#[test]
fn indirect_expansion_of_an_unset_positional_is_empty() {
    let (st, o, e) = all("f(){ echo \"[${!1}]\"; }; f; echo rc=$?; set --; echo \"[${!1}]\"");
    assert_eq!((st, o.as_str(), e.as_str()), (0, "[]\nrc=0\n[]\n", ""));
    let (_, o, e) = all("set -u; f(){ echo \"[${!1}]\"; }; f; echo rc=$?");
    assert_eq!(o, "");
    assert!(e.contains("!1: unbound variable"), "{e}");
}

#[test]
fn export_f_takes_function_names_that_are_not_identifiers() {
    let (st, o, e) = all("a::b(){ echo hi; }; export -f a::b; echo rc=$?; export -nf a::b; echo rc=$?");
    assert_eq!((st, o.as_str(), e.as_str()), (0, "rc=0\nrc=0\n", ""));
}
