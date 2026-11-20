//! Regressions from the coverage-2 and coverage-3 bash parity suites (bash 5.2).

use super::run_on_test_host as run;

fn out(s: &str) -> String {
    String::from_utf8_lossy(&run(s).1).into_owned()
}

#[test]
fn prompt_transform_decodes_octal_and_expands_like_a_prompt() {
    assert_eq!(out(r"x='a\101b'; echo ${x@P}"), "aAb\n");
    assert_eq!(out(r"y='$((1+2)) $HOME'; HOME=/h; echo ${y@P}"), "3 /h\n");
    assert_eq!(out(r"a=('\101' '$((2*2))'); echo ${a[@]@P}"), "A 4\n");
}

#[test]
fn seeded_random_follows_bash_sequence() {
    assert_eq!(out("RANDOM=42; echo $RANDOM $RANDOM $RANDOM"), "17772 26794 1435\n");
    assert_eq!(out("RANDOM=0; a=$RANDOM; RANDOM=0; [ $a = $RANDOM ] && echo same"), "same\n");
}

#[test]
fn unicode_escapes_stay_escaped_outside_utf8_locales() {
    // bash decodes `$'…'` as it parses a line, so the locale is set a line before.
    assert_eq!(out("LC_ALL=C\necho $'\\u0043\\u00e9\\U0001F600'"), "C\\u00E9\\U0001F600\n");
    assert_eq!(out("LC_ALL=C.UTF-8\necho $'\\u00e9'"), "\u{e9}\n");
    assert_eq!(out(r"LC_ALL=C; x='\u00e9'; echo ${x@E}"), "\\u00E9\n");
}

#[test]
fn quoted_prefix_names_with_no_match_give_no_word() {
    assert_eq!(out(r#"set -- "${!zzq_@}"; echo $#; set -- "${!zzq_*}"; echo $#"#), "0\n1\n");
    assert_eq!(out(r#"zq_a=1 zq_b=2; set -- "${!zq_@}"; echo $# $2"#), "2 zq_b\n");
}

#[test]
fn printf_empty_time_format_is_the_locale_time() {
    assert_eq!(out("TZ=UTC printf '%()T\\n' 3661"), "01:01:01\n");
}
