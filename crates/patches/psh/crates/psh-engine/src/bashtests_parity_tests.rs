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

#[test]
fn bash_cmds_is_an_empty_associative_array() {
    assert_eq!(out("declare -A\n"), "declare -A BASH_ALIASES=()\ndeclare -A BASH_CMDS=()\n");
}

#[test]
fn declare_f_names_show_readonly_functions() {
    assert_eq!(
        out("f() { :; }\ng() { :; }\nreadonly -f f\ndeclare -F\ndeclare -Fr\n"),
        "declare -fr f\ndeclare -f g\ndeclare -fr f\n"
    );
}

#[test]
fn assignment_before_export_of_that_name_persists() {
    assert_eq!(
        out("foo=\"\" export foo\ndeclare -p foo\nbar=1 export baz\necho \"[$bar]\"\n"),
        "declare -x foo=\"\"\n[]\n"
    );
}

#[test]
fn nested_time_runs_the_command() {
    assert_eq!(out("TIMEFORMAT=\ntime time echo hi\n"), "hi\n");
}

#[test]
fn backtick_escaped_backslash_before_newline_is_not_a_continuation() {
    assert_eq!(
        out("a=`printf '%s' 'x\\\\\ny'`\necho \"[$a]\"\nb=`echo p\\\nq`\necho \"[$b]\"\n"),
        "[x\\\ny]\n[pq]\n"
    );
}

#[test]
fn char_range_through_backquote_and_backslash() {
    assert_eq!(
        out("echo {Z..a}\necho {a..Z}\n"),
        "Z [  ] ^ _ ` a\na ` _ ^ ]  [ Z\n"
    );
}

#[test]
fn space_after_dollar_brace_that_is_no_command_list_parses() {
    assert_eq!(
        out("eval 'foo() { _; } >_[${ $() }] ;{ echo eval ok; }'\n"),
        "eval ok\n"
    );
}

#[test]
fn arith_store_to_an_empty_subscript_stores_nothing() {
    assert_eq!(
        out("echo $(( A[$key]++ ))\ndeclare -p A\necho $?\n"),
        "0\n1\n"
    );
}

#[test]
fn backquote_body_lines_count_from_its_command() {
    assert_eq!(
        out("true\necho `echo $LINENO\necho $LINENO`\n"),
        "3 4\n"
    );
}

#[test]
fn time_alone_prints_with_its_space() {
    assert_eq!(
        out("f() { time; echo; }\ndeclare -f f\n"),
        "f () \n{ \n    time ;\n    echo\n}\n"
    );
}

#[test]
fn length_and_bang_heads_with_operators() {
    assert_eq!(
        out("set -- a b\necho ${#-x} ${#/a/b} ${!-p} ${!:-q}\n(echo ${#+})\n(echo ${#%})\necho $?\n"),
        "2 2 p q\n1\n"
    );
}

#[test]
fn integer_assignment_arith_error_ends_the_shell() {
    assert_eq!(
        out("declare -i i\n(i=0#4; echo no)\necho $?\n(f() { i+=1/0; }; f; echo no)\necho $?\n"),
        "1\n1\n"
    );
    assert_eq!(run("declare -i i\ni=0#4\necho no\n").0, 1);
    assert_eq!(out("declare -i i\ni=0#4\necho no\n"), "");
}

#[test]
fn command_substitution_body_lines_count_from_its_command() {
    assert_eq!(
        out("x=$(echo $LINENO\n)\necho $x\nx=$(\n\necho $LINENO\n)\necho $x\necho a \\\n$(echo $LINENO)\n:\necho $(echo 'a\nb' $LINENO)\n"),
        "2\n7\na 9\na b 14\n"
    );
}

#[test]
fn set_k_takes_assignment_arguments_as_assignments() {
    assert_eq!(
        out("set -k\na=5 $E c=7 $E d+=8\necho $a $c $d\nf() { echo \"$x\" \"$@\"; }\nf 1 x=2 3 'y=4'\n"),
        "5 7 8\n2 1 3 y=4\n"
    );
}

#[test]
fn posix_single_quote_in_double_quoted_default_word_is_plain() {
    assert_eq!(
        out("echo \"${IFS+'a'}\"\nset -o posix\necho \"${IFS+'bar}\"\necho ${IFS+'}'z} \"${IFS+'}'z}\"\n"),
        "'a'\n'bar\n}z ''z}\n"
    );
}

#[test]
fn printf_v_overflowed_star_width_eats_the_next_byte() {
    let (_, o, e) = run(
        "x=5; printf -v v '% *.*o\t%*d' '-9223372036854775809' 9 '$x' '%' 20; declare -p v\n",
    );
    assert_eq!(String::from_utf8_lossy(&o), "declare -- v=\"20\"\n");
    let e = String::from_utf8_lossy(&e);
    assert!(e.contains("warning: 9: Numerical result out of range"), "{e}");
}

#[test]
fn xtrace_of_arith_for_clauses_keeps_trailing_blanks() {
    let (_, _, e) = run("set -x\nfor (( i=0 ; i<1 ; i++)) ; do :; done\nfor ((  ;  ; )) ; do break; done\n");
    assert_eq!(
        String::from_utf8_lossy(&e),
        "+ (( i=0  ))\n+ (( i<1  ))\n+ :\n+ (( i++ ))\n+ (( i<1  ))\n+ (( 1 ))\n+ (( 1 ))\n+ break\n"
    );
}

#[test]
fn assignments_in_front_of_declaration_builtins_scope_like_bash() {
    let script = r#"f5(){ z=y typeset z; echo "|$z|"; }; f5; z=42 f5
f2(){ export x; }; x=4 f2; echo "x=$x"
b=1 declare -r b=2; declare -p b
a=1 declare a=2; echo "${a-unset}"
f(){ local v=x; unset v; declare -p v; }; v=t f
"#;
    assert_eq!(
        out(script),
        "|y|\n|y|\nx=4\ndeclare -rx b=\"2\"\nunset\ndeclare -x v\n"
    );
}

#[test]
fn new_locals_keep_the_export_attribute_and_may_shadow_a_callers_readonly() {
    let script = r#"export E=abc
g(){ local E; declare -p E; }; g
aaa(){ local x=1; echo "aaa $x"; }; ddd(){ local -r x=0; aaa; }; ddd
shopt -s localvar_inherit; s=X; f(){ local -a s; declare -p s; }; f
"#;
    assert_eq!(out(script), "declare -x E\naaa 1\ndeclare -a s=([0]=\"X\")\n");
}

#[test]
fn ignoreeof_and_ignoreeof_variable_follow_each_other() {
    let script = "IGNOREEOF=0; shopt -o ignoreeof; f(){ local -; set +o ignoreeof; }; f\n\
                  echo $IGNOREEOF; set +o ignoreeof; echo ${IGNOREEOF-unset}\n";
    assert_eq!(out(script), "ignoreeof      \ton\n10\nunset\n");
}

#[test]
fn exit_trap_after_exit_in_a_function_sees_the_function() {
    let script = "trap 'echo \"trap:$FUNCNAME:$x:$1:${BASH_LINENO[*]}\"' EXIT\n\
                  f(){ local x=1; exit 3; }\ng(){ f a; }\ng b\n";
    assert_eq!(out(script), "trap:f:1:a:3 4\n");
}

#[test]
fn double_quoted_expansion_in_an_arithmetic_subscript_is_one_key() {
    let script = r#"declare -A assoc; key='x],b[$(echo uname >&2)'
(( assoc["$key"]++ )); (( "assoc[$key]++" )); declare -p assoc
typeset -A a; b="80's"; ((++a["$b"])); [[ $((++a["$b"])) ]]; echo ${a[$b]}
"#;
    let (_, o, e) = run(script);
    assert_eq!(
        String::from_utf8_lossy(&o),
        "declare -A assoc=([\"x],b[\\$(echo uname >&2)\"]=\"2\" )\n2\n"
    );
    assert_eq!(String::from_utf8_lossy(&e), "");
}

#[test]
fn keys_of_bash_cmds_and_bash_aliases_are_the_current_tables() {
    assert_eq!(
        out("hash -p /usr/sbin/foo foo; alias q=r; echo ${!BASH_CMDS[@]} ${!BASH_ALIASES[@]}\n"),
        "foo q\n"
    );
}

#[test]
fn a_subscript_arithmetic_error_in_a_script_abandons_only_its_line() {
    let script = r#"cat > s.sh <<'EOF'
a=( [1+]=x ); echo same
echo next
unset 'a[1+]'; echo same
echo ${a[1+]}
echo end
EOF
bash s.sh; echo "rc=$?"
a=( [1+]=x ); echo "-c ends here"
"#;
    let (st, o, _) = run(script);
    assert_eq!(String::from_utf8_lossy(&o), "next\nend\nrc=0\n");
    assert_eq!(st, 1);
}

#[test]
fn a_compound_indexed_key_drops_its_ctlesc_bytes() {
    assert_eq!(
        out("xy=3; b=( [$'x\\001y']=1 ); declare -p b\n"),
        "declare -a b=([3]=\"1\")\n"
    );
}

#[test]
fn a_scalar_converted_to_an_associative_array_keeps_bash_s_table_size() {
    assert_eq!(
        out("foo=help; declare -A foo=([zero]=zero [one]=one); declare -p foo\n"),
        "declare -A foo=([zero]=\"zero\" [one]=\"one\" )\n"
    );
}

#[test]
fn a_readonly_compound_local_fails_twice() {
    let (_, _, e) = run("readonly q=1; f(){ local q=(a); }; f\n");
    let e = String::from_utf8_lossy(&e);
    assert_eq!(e.lines().count(), 2, "{e}");
    assert!(e.ends_with("local: q: readonly variable\n"), "{e}");
}

#[test]
fn a_syntax_error_inside_a_command_substitution_reads_as_bash_5_2() {
    let (st, _, e) = run(": $(case x in x) done esac)\n");
    assert_eq!(
        String::from_utf8_lossy(&e),
        "psh: -c: line 1: syntax error near unexpected token `done'\n\
         psh: -c: line 1: `: $(case x in x) done esac)'\n"
    );
    assert_eq!(st, 127);
}

#[test]
fn end_of_input_diagnostics_read_as_bash_5_2() {
    let (_, o, e) = run("eval 'echo x\\'\neval 'X() { (a)>\\'\nif true; then\n");
    assert_eq!(String::from_utf8_lossy(&o), "x\\\n");
    assert_eq!(
        String::from_utf8_lossy(&e),
        "psh: eval: line 4: syntax error: unexpected end of file\n\
         psh: -c: line 4: syntax error: unexpected end of file\n"
    );
}

#[test]
fn a_command_s_line_counts_continuations_its_second_word_ends_with() {
    let (_, _, e) = run("nosuch a\\\n  foo\n");
    assert_eq!(String::from_utf8_lossy(&e), "psh: line 2: nosuch: command not found\n");
}

#[test]
fn entering_a_traced_function_runs_the_debug_trap_at_its_definition_line() {
    let script = "set -T\ntrap 'echo \"d $LINENO\"' DEBUG\nf() {\n  :\n}\nf\ntrap - DEBUG\n";
    assert_eq!(out(script), "d 6\nd 3\nd 4\nd 7\n");
}

#[test]
fn extdebug_turns_functrace_and_errtrace_on_and_off() {
    assert_eq!(
        out("shopt -s extdebug; echo \"$-\"; set +T; echo \"$-\"; shopt -u extdebug; echo \"$-\"\n"),
        "hBETc\nhBEc\nhBc\n"
    );
}

#[test]
fn the_return_trap_after_source_runs_without_the_hidden_debug_trap() {
    let script = "echo : > s.sub\ntrap 'echo debug' DEBUG\ntrap 'echo ret' RETURN\n. ./s.sub\ntrap - DEBUG\n";
    assert_eq!(out(script), "debug\ndebug\nret\ndebug\n");
}

#[test]
fn a_strip_tabs_heredoc_delimiter_may_start_with_a_tab() {
    let script = "cat <<-'\tEND'\n\thello\n\tEND\ncat <<-END\n\tx\n\t\tEND\necho done\n";
    assert_eq!(out(script), "hello\nx\ndone\n");
}

#[test]
fn alias_text_is_read_as_input_across_its_end() {
    let script = r#"shopt -s expand_aliases
alias foo="echo 'Error:"
foo bar'
alias c=#
c a
c b
alias a='printf "<%s>\n" \'
a|cat
"#;
    assert_eq!(out(script), "Error: bar\n<|cat>\n");
}

#[test]
fn quoted_chars_in_a_regex_bracket_expression_stand_bare() {
    let script = r#"[[ '\' =~ ["."] ]] && echo bad1
[[ . =~ ["."] ]] && echo ok1
[[ '\' =~ [\.] ]] && echo bad2
[[ '\' =~ [^]"."] ]] && echo ok2
[[ ']' =~ [']'] ]] && echo ok3
[[ x =~ [\. ]]; echo "rc=$?"
"#;
    assert_eq!(out(script), "ok1\nok2\nok3\nrc=2\n");
}

#[test]
fn a_substring_arithmetic_error_names_the_parameter() {
    let (_, _, e) = run("x=abc; a=(p q); r=x\n: ${x:%}\n: ${a[@]:1:%}\n: ${!r:1/0}\n");
    assert_eq!(
        String::from_utf8_lossy(&e),
        "psh: line 2: x: %: syntax error: operand expected (error token is \"%\")\n\
         psh: line 3: a[@]: %: syntax error: operand expected (error token is \"%\")\n\
         psh: line 4: !r: 1/0: division by 0 (error token is \"0\")\n"
    );
}
