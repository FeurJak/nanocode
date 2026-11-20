//! Behaviour pinned by performance changes: each fast path gives what the slow one did.

use super::run_on_test_host as run;

fn out(s: &str) -> String {
    String::from_utf8_lossy(&run(s).1).into_owned()
}

#[test]
fn an_array_length_counts_every_kind_of_variable() {
    assert_eq!(
        out("a=(x '' z); a[9]=q; echo ${#a[@]} ${#a[*]} \"${#a[@]}\""),
        "4 4 4\n"
    );
    assert_eq!(out("declare -A m=([k]=1 [j]=2); echo ${#m[@]}"), "2\n");
    assert_eq!(
        out("s=abc; e=; echo ${#s[@]} ${#e[@]} ${#nosuch[@]}"),
        "1 1 0\n"
    );
    assert_eq!(
        out("declare -a d; declare -A h; echo ${#d[@]} ${#h[*]}"),
        "0 0\n"
    );
    assert_eq!(out("a=(1 2 3); declare -n r=a; echo ${#r[@]}"), "3\n");
    assert_eq!(out("set -u; a=(); echo ${#a[@]}"), "0\n");
    assert_eq!(out("f() { echo ${#FUNCNAME[@]}; }; g() { f; }; g"), "2\n");
    assert_eq!(
        out(
            "a=(); for i in {1..3000}; do a+=($i); done; n=0; for ((i=0; i<${#a[@]}; i++)); do ((n+=a[i])); done; echo $n"
        ),
        "4501500\n"
    );
}
