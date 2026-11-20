//! Regressions from snippets of GNU bash 5.2's own test suite (the bash-tests parity suite).

use super::run_on_test_host as run;

fn out(s: &str) -> String {
    String::from_utf8_lossy(&run(s).1).into_owned()
}

#[test]
fn bracket_collating_symbols_and_equivalence_classes() {
    let script = r#"
for p in a:'[[.a.]]' -:'[[.hyphen.]-9]' p:'[[.a.]-[.z.]]' -:'[[.-.]]' ' ':'[[.space.]]' \
    4:'[[.-.]-9]' c:'[[.yyz.][.a.]-z]' c:'[[.yyz.][.a.]-[.z.]]' p:'[[.a.]-[.zz.]p]' \
    p:'[[.aa.]-[.z.]p]' c:'[[.yyz.]cde]' a:'[[.cb.]a-Za]' b:'[[=b=]]' q:'[[.yyz.]]' \
    q:'[[.a.]-[.zz.]]'; do
  s=${p%%:*} pat=${p#*:}
  case $s in $pat) echo "y $pat" ;; *) echo "n $pat" ;; esac
done
case $'\t' in [[.space.][.tab.][.newline.]]) echo tab ;; esac
case abc in [[:alpha:]][[=b=]][[:ascii:]]) echo abc ;; esac
"#;
    assert_eq!(
        out(script),
        "y [[.a.]]\ny [[.hyphen.]-9]\ny [[.a.]-[.z.]]\ny [[.-.]]\ny [[.space.]]\ny [[.-.]-9]\n\
         y [[.yyz.][.a.]-z]\ny [[.yyz.][.a.]-[.z.]]\ny [[.a.]-[.zz.]p]\ny [[.aa.]-[.z.]p]\n\
         y [[.yyz.]cde]\ny [[.cb.]a-Za]\ny [[=b=]]\nn [[.yyz.]]\nn [[.a.]-[.zz.]]\ntab\nabc\n"
    );
}

#[test]
fn bracket_invalid_class_name_matches_nothing() {
    assert_eq!(
        out("case a in [[:al:]]) echo bad ;; *) echo ok ;; esac\ncase ':' in [[:al:]]) echo bad ;; *) echo ok ;; esac\n[[ x == [[:nope:]x] ]] && echo x\n"),
        "ok\nok\nx\n"
    );
}

#[test]
fn pattern_substitution_on_an_empty_value() {
    assert_eq!(
        out("var=\necho \"${var/#/x}\" \"${var/*/x}\" \"${var//*/x}\" \"${var/%/y}\" \"[${var/a*/x}]\"\n"),
        "x x x y []\n"
    );
}

#[test]
fn file_comparisons_with_an_empty_name_are_false() {
    assert_eq!(
        out("[[ '' -ef . ]]; echo $?\n[[ . -ef '' ]]; echo $?\n[ '' -nt . ]; echo $?\n[ . -nt '' ]; echo $?\n[[ . -ef . ]]; echo $?\n"),
        "1\n1\n1\n0\n0\n"
    );
}
