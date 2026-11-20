//! `complete`, `compopt` and `bind` of a non-interactive bash: completion specs are
//! kept and printed (`complete -p`), nothing ever completes, and `bind` warns that
//! line editing is off.

use crate::host::{OpenOptions, error_text};
use crate::state::{R, Vp};

use super::{out, s};

/// `-o` options in the order bash prints them.
const OPTS: [&str; 8] = [
    "bashdefault",
    "default",
    "dirnames",
    "filenames",
    "noquote",
    "nosort",
    "nospace",
    "plusdirs",
];

/// Actions in the order bash prints them; the first twelve have a short flag.
const ACTS: [(&str, u8); 24] = [
    ("alias", b'a'),
    ("builtin", b'b'),
    ("command", b'c'),
    ("directory", b'd'),
    ("export", b'e'),
    ("file", b'f'),
    ("group", b'g'),
    ("job", b'j'),
    ("keyword", b'k'),
    ("service", b's'),
    ("user", b'u'),
    ("variable", b'v'),
    ("arrayvar", 0),
    ("binding", 0),
    ("disabled", 0),
    ("enabled", 0),
    ("function", 0),
    ("helptopic", 0),
    ("hostname", 0),
    ("running", 0),
    ("setopt", 0),
    ("shopt", 0),
    ("signal", 0),
    ("stopped", 0),
];

/// The names bash keeps the `-D`, `-E` and `-I` specs under.
const DEFAULT: &[u8] = b"_DefaultCmD_";
const EMPTY: &[u8] = b"_EmptycmD_";
const INITIAL: &[u8] = b"_InitialWorD_";

const USAGE: &str = "complete: usage: complete [-abcdefgjksuv] [-pr] [-DEI] [-o option] [-A action] [-G globpat] [-W wordlist] [-F function] [-C command] [-X filterpat] [-P prefix] [-S suffix] [name ...]";
const COMPOPT_USAGE: &str = "compopt: usage: compopt [-o|+o option] [-DEI] [name ...]";

/// Flags of the arguments a spec keeps, in the order bash prints them.
const ARG_FLAGS: [u8; 7] = *b"GWPSXCF";

/// One `complete` specification.
#[derive(Clone, Default, PartialEq, Debug)]
pub(crate) struct Spec {
    opts: u8,
    acts: u32,
    args: [Option<Vec<u8>>; 7],
}

/// The specs by command name, oldest first.
#[derive(Clone, Default, Debug)]
pub(crate) struct Specs(Vec<(Vec<u8>, Spec)>);

impl Specs {
    fn set(&mut self, name: &[u8], spec: Spec) {
        match self.0.iter_mut().find(|(n, _)| n == name) {
            Some(e) => e.1 = spec,
            None => self.0.push((name.to_vec(), spec)),
        }
    }

    /// Bash's walk of its hash table: by bucket, newest first in each.
    fn walk(&self) -> Vec<&(Vec<u8>, Spec)> {
        let mut v: Vec<(usize, &(Vec<u8>, Spec))> = self.0.iter().enumerate().collect();
        v.sort_by_key(|(i, (n, _))| (bucket(n), std::cmp::Reverse(*i)));
        v.into_iter().map(|(_, e)| e).collect()
    }
}

/// Buckets of bash's completion table (`COMPLETE_HASH_BUCKETS`).
const BUCKETS: u32 = 512;

/// Bash's `hash_string` (FNV-1) bucket.
fn bucket(name: &[u8]) -> u32 {
    let mut h: u32 = 2_166_136_261;
    for &b in name {
        h = h.wrapping_mul(16_777_619);
        h ^= u32::from(b);
    }
    h & (BUCKETS - 1)
}

fn quote(s: &[u8]) -> Vec<u8> {
    let mut o = vec![b'\''];
    for &b in s {
        if b == b'\'' {
            o.extend_from_slice(b"'\\''");
        } else {
            o.push(b);
        }
    }
    o.push(b'\'');
    o
}

fn print(name: &[u8], spec: &Spec) -> Vec<u8> {
    let mut o = b"complete ".to_vec();
    for (i, n) in OPTS.iter().enumerate() {
        if spec.opts & (1 << i) != 0 {
            o.extend_from_slice(format!("-o {n} ").as_bytes());
        }
    }
    for (i, (n, c)) in ACTS.iter().enumerate() {
        if spec.acts & (1 << i) != 0 {
            if *c != 0 {
                o.extend_from_slice(&[b'-', *c, b' ']);
            } else {
                o.extend_from_slice(format!("-A {n} ").as_bytes());
            }
        }
    }
    for (i, a) in spec.args.iter().enumerate() {
        if let Some(a) = a {
            o.extend_from_slice(&[b'-', ARG_FLAGS[i], b' ']);
            if ARG_FLAGS[i] == b'F' {
                o.extend_from_slice(a);
            } else {
                o.extend_from_slice(&quote(a));
            }
            o.push(b' ');
        }
    }
    match name {
        DEFAULT => o.extend_from_slice(b"-D"),
        EMPTY => o.extend_from_slice(b"-E"),
        INITIAL => o.extend_from_slice(b"-I"),
        _ => o.extend_from_slice(name),
    }
    o.push(b'\n');
    o
}

struct Parsed {
    spec: Spec,
    /// The `-o` options `+o` takes away (`compopt`).
    off: u8,
    print: bool,
    remove: bool,
    names: Vec<Vec<u8>>,
    /// Any option besides `-p` and `-r`.
    given: bool,
}

/// The options of `complete` (or of `compopt`); an error is the messages for status 2.
fn parse(args: &[Vec<u8>], compopt: bool) -> Result<Parsed, Vec<String>> {
    let name = if compopt { "compopt" } else { "complete" };
    let usage = if compopt { COMPOPT_USAGE } else { USAGE };
    let mut p = Parsed {
        spec: Spec::default(),
        off: 0,
        print: false,
        remove: false,
        names: Vec::new(),
        given: false,
    };
    let mut i = 1;
    let mut special: Vec<&[u8]> = Vec::new();
    while let Some(a) = args.get(i) {
        if a == b"--" {
            i += 1;
            break;
        }
        let plus = compopt && a.first() == Some(&b'+');
        if a.len() < 2 || !(a[0] == b'-' || plus) {
            break;
        }
        let lead = if plus { '+' } else { '-' };
        let mut j = 1;
        while j < a.len() {
            let c = a[j];
            j += 1;
            let takes = if compopt {
                c == b'o'
            } else {
                matches!(c, b'o' | b'A' | b'G' | b'W' | b'F' | b'C' | b'X' | b'P' | b'S')
            };
            let value = if !takes {
                Vec::new()
            } else if j < a.len() {
                let v = a[j..].to_vec();
                j = a.len();
                v
            } else {
                i += 1;
                match args.get(i) {
                    Some(v) => v.clone(),
                    None => {
                        return Err(vec![
                            format!("{name}: -{}: option requires an argument", c as char),
                            usage.to_string(),
                        ]);
                    }
                }
            };
            match c {
                b'D' => special.push(DEFAULT),
                b'E' => special.push(EMPTY),
                b'I' => special.push(INITIAL),
                b'o' => {
                    let Some(k) = OPTS.iter().position(|o| o.as_bytes() == &value[..]) else {
                        return Err(vec![format!(
                            "{name}: {}: invalid option name",
                            String::from_utf8_lossy(&value)
                        )]);
                    };
                    if plus {
                        p.off |= 1 << k;
                    } else {
                        p.spec.opts |= 1 << k;
                    }
                    p.given = true;
                }
                _ if compopt => {
                    return Err(vec![
                        format!("{name}: {lead}{}: invalid option", c as char),
                        usage.to_string(),
                    ]);
                }
                b'p' => p.print = true,
                b'r' => p.remove = true,
                b'A' => {
                    let Some(k) = ACTS.iter().position(|(n, _)| n.as_bytes() == &value[..]) else {
                        return Err(vec![format!(
                            "{name}: {}: invalid action name",
                            String::from_utf8_lossy(&value)
                        )]);
                    };
                    p.spec.acts |= 1 << k;
                    p.given = true;
                }
                _ if takes => {
                    let k = ARG_FLAGS.iter().position(|&f| f == c).unwrap_or(0);
                    p.spec.args[k] = Some(value);
                    p.given = true;
                }
                _ => match ACTS.iter().position(|(_, f)| *f == c) {
                    Some(k) => {
                        p.spec.acts |= 1 << k;
                        p.given = true;
                    }
                    None => {
                        return Err(vec![
                            format!("{name}: {lead}{}: invalid option", c as char),
                            usage.to_string(),
                        ]);
                    }
                },
            }
        }
        i += 1;
    }
    p.names = special.into_iter().map(<[u8]>::to_vec).collect();
    if p.names.is_empty() {
        p.names = args[i.min(args.len())..].to_vec();
    }
    Ok(p)
}

/// The error, then the usage line bash prints without its prefix.
async fn errors(vp: &Vp, lines: Vec<String>) -> R {
    for (i, l) in lines.iter().enumerate() {
        if i == 0 {
            vp.error(l).await;
        } else {
            vp.error_raw(format!("{l}\n").as_bytes()).await;
        }
    }
    Ok(2)
}

pub(super) async fn complete(vp: &mut Vp, args: &[Vec<u8>]) -> R {
    let p = match parse(args, false) {
        Ok(p) => p,
        Err(e) => return errors(vp, e).await,
    };
    if p.remove {
        let specs = std::rc::Rc::make_mut(&mut vp.completions);
        if p.names.is_empty() {
            specs.0.clear();
        } else {
            specs.0.retain(|(n, _)| !p.names.contains(n));
        }
        return Ok(0);
    }
    if p.print || (!p.given && p.names.is_empty()) {
        let mut text = Vec::new();
        let mut status = 0;
        if p.names.is_empty() {
            for (n, s) in vp.completions.walk() {
                text.extend(print(n, s));
            }
        } else {
            for n in &p.names {
                let found = vp
                    .completions
                    .0
                    .iter()
                    .find(|(m, _)| m == n)
                    .map(|(m, s)| print(m, s));
                match found {
                    Some(t) => text.extend(t),
                    None => {
                        if !text.is_empty() {
                            out(vp, "complete", &std::mem::take(&mut text)).await?;
                        }
                        vp.error(&format!(
                            "complete: {}: no completion specification",
                            String::from_utf8_lossy(n)
                        ))
                        .await;
                        status = 1;
                    }
                }
            }
        }
        if !text.is_empty() && out(vp, "complete", &text).await? != 0 {
            return Ok(1);
        }
        return Ok(status);
    }
    let specs = std::rc::Rc::make_mut(&mut vp.completions);
    for n in &p.names {
        specs.set(n, p.spec.clone());
    }
    Ok(0)
}

pub(super) async fn compopt(vp: &mut Vp, args: &[Vec<u8>]) -> R {
    let p = match parse(args, true) {
        Ok(p) => p,
        Err(e) => return errors(vp, e).await,
    };
    if p.names.is_empty() {
        vp.error("compopt: not currently executing completion function")
            .await;
        return Ok(1);
    }
    let mut status = 0;
    for n in &p.names {
        let specs = std::rc::Rc::make_mut(&mut vp.completions);
        let missing = match specs.0.iter_mut().find(|(m, _)| m == n) {
            Some((_, s)) => {
                s.opts = (s.opts | p.spec.opts) & !p.off;
                false
            }
            None => true,
        };
        if missing {
            vp.error(&format!(
                "compopt: {}: no completion specification",
                String::from_utf8_lossy(n)
            ))
            .await;
            status = 1;
        }
    }
    Ok(status)
}

/// The functions `bind -l` lists, as readline 8.2 names them.
const FUNCTIONS: &[&str] = &[
    "abort", "accept-line", "alias-expand-line", "arrow-key-prefix", "backward-byte",
    "backward-char", "backward-delete-char", "backward-kill-line", "backward-kill-word",
    "backward-word", "beginning-of-history", "beginning-of-line", "bracketed-paste-begin",
    "call-last-kbd-macro", "capitalize-word", "character-search",
    "character-search-backward", "clear-display", "clear-screen", "complete",
    "complete-command", "complete-filename", "complete-hostname", "complete-into-braces",
    "complete-username", "complete-variable", "copy-backward-word", "copy-forward-word",
    "copy-region-as-kill", "dabbrev-expand", "delete-char", "delete-char-or-list",
    "delete-horizontal-space", "digit-argument", "display-shell-version",
    "do-lowercase-version", "downcase-word", "dump-functions", "dump-macros",
    "dump-variables", "dynamic-complete-history", "edit-and-execute-command",
    "emacs-editing-mode", "end-kbd-macro", "end-of-history", "end-of-line",
    "exchange-point-and-mark", "fetch-history", "forward-backward-delete-char",
    "forward-byte", "forward-char", "forward-search-history", "forward-word",
    "glob-complete-word", "glob-expand-word", "glob-list-expansions",
    "history-and-alias-expand-line", "history-expand-line", "history-search-backward",
    "history-search-forward", "history-substring-search-backward",
    "history-substring-search-forward", "insert-comment", "insert-completions",
    "insert-last-argument", "kill-line", "kill-region", "kill-whole-line", "kill-word",
    "magic-space", "menu-complete", "menu-complete-backward", "next-history",
    "next-screen-line", "non-incremental-forward-search-history",
    "non-incremental-forward-search-history-again",
    "non-incremental-reverse-search-history",
    "non-incremental-reverse-search-history-again", "old-menu-complete",
    "operate-and-get-next", "overwrite-mode", "possible-command-completions",
    "possible-completions", "possible-filename-completions",
    "possible-hostname-completions", "possible-username-completions",
    "possible-variable-completions", "previous-history", "previous-screen-line",
    "print-last-kbd-macro", "quoted-insert", "redraw-current-line", "re-read-init-file",
    "reverse-search-history", "revert-line", "self-insert", "set-mark",
    "shell-backward-kill-word", "shell-backward-word", "shell-expand-line",
    "shell-forward-word", "shell-kill-word", "shell-transpose-words", "skip-csi-sequence",
    "spell-correct-word", "start-kbd-macro", "tab-insert", "tilde-expand",
    "transpose-chars", "transpose-words", "tty-status", "undo", "universal-argument",
    "unix-filename-rubout", "unix-line-discard", "unix-word-rubout", "upcase-word",
    "vi-append-eol", "vi-append-mode", "vi-arg-digit", "vi-back-to-indent",
    "vi-backward-bigword", "vi-backward-word", "vi-bword", "vi-bWord", "vi-change-case",
    "vi-change-char", "vi-change-to", "vi-char-search", "vi-column", "vi-complete",
    "vi-delete", "vi-delete-to", "vi-edit-and-execute-command", "vi-editing-mode",
    "vi-end-bigword", "vi-end-word", "vi-eof-maybe", "vi-eword", "vi-eWord",
    "vi-fetch-history", "vi-first-print", "vi-forward-bigword", "vi-forward-word",
    "vi-fword", "vi-fWord", "vi-goto-mark", "vi-insert-beg", "vi-insertion-mode",
    "vi-match", "vi-movement-mode", "vi-next-word", "vi-overstrike", "vi-overstrike-delete",
    "vi-prev-word", "vi-put", "vi-redo", "vi-replace", "vi-rubout", "vi-search",
    "vi-search-again", "vi-set-mark", "vi-subst", "vi-tilde-expand", "vi-undo",
    "vi-unix-word-rubout", "vi-yank-arg", "vi-yank-pop", "vi-yank-to", "yank",
    "yank-last-arg", "yank-nth-arg", "yank-pop",
];

/// Readline's variables and their values in a non-interactive bash 5.2; `*` is
/// `convert-meta`, off in a UTF-8 locale.
const VARIABLES: &[(&str, &str)] = &[
    ("bind-tty-special-chars", "on"),
    ("blink-matching-paren", "off"),
    ("byte-oriented", "off"),
    ("colored-completion-prefix", "off"),
    ("colored-stats", "off"),
    ("completion-ignore-case", "off"),
    ("completion-map-case", "off"),
    ("convert-meta", "*"),
    ("disable-completion", "off"),
    ("echo-control-characters", "on"),
    ("enable-active-region", "off"),
    ("enable-bracketed-paste", "off"),
    ("enable-keypad", "off"),
    ("enable-meta-key", "on"),
    ("expand-tilde", "off"),
    ("history-preserve-point", "off"),
    ("horizontal-scroll-mode", "off"),
    ("input-meta", "on"),
    ("mark-directories", "on"),
    ("mark-modified-lines", "off"),
    ("mark-symlinked-directories", "off"),
    ("match-hidden-files", "on"),
    ("menu-complete-display-prefix", "off"),
    ("meta-flag", "on"),
    ("output-meta", "on"),
    ("page-completions", "on"),
    ("prefer-visible-bell", "on"),
    ("print-completions-horizontally", "off"),
    ("revert-all-at-newline", "off"),
    ("show-all-if-ambiguous", "off"),
    ("show-all-if-unmodified", "off"),
    ("show-mode-in-prompt", "off"),
    ("skip-completed-text", "off"),
    ("visible-stats", "off"),
    ("bell-style", "audible"),
    ("comment-begin", "#"),
    ("completion-display-width", "-1"),
    ("completion-prefix-display-length", "0"),
    ("completion-query-items", "100"),
    ("editing-mode", "emacs"),
    ("emacs-mode-string", "@"),
    ("history-size", "0"),
    ("keymap", "emacs"),
    ("keyseq-timeout", "500"),
    ("vi-cmd-mode-string", "(cmd)"),
    ("vi-ins-mode-string", "(ins)"),
];

/// `bind` of a shell without line editing: it warns, then lists readline's functions
/// (`-l`) and variables (`-v`, `-V`), lists no macros or shell commands (`-s`,
/// `-S`, `-X`) and accepts bindings it never uses; listing key bindings (`-p`,
/// `-P`, `-q`, `-u`) is not supported yet.
pub(super) async fn bind(vp: &mut Vp, args: &[Vec<u8>]) -> R {
    let mut flags = Vec::new();
    let mut file = None;
    let mut i = 1;
    while let Some(a) = args.get(i) {
        if a == b"--" {
            break;
        }
        if a.len() < 2 || a[0] != b'-' {
            break;
        }
        for (j, &c) in a.iter().enumerate().skip(1) {
            match c {
                b'l' | b'v' | b'V' | b's' | b'S' | b'X' | b'x' => flags.push(c),
                b'm' | b'r' | b'f' => {
                    let value = match a.len() > j + 1 {
                        true => a[j + 1..].to_vec(),
                        false => {
                            i += 1;
                            match args.get(i) {
                                Some(v) => v.clone(),
                                None => return crate::expand::unsupported(vp, "bind").await,
                            }
                        }
                    };
                    if c == b'f' {
                        file = Some(value);
                    }
                    break;
                }
                _ => return crate::expand::unsupported(vp, "bind").await,
            }
        }
        i += 1;
    }
    vp.error("bind: warning: line editing not enabled").await;
    let mut buf = Vec::new();
    if flags.contains(&b'l') {
        for f in FUNCTIONS {
            buf.extend_from_slice(f.as_bytes());
            buf.push(b'\n');
        }
    }
    let utf8 = vp.utf8();
    for (flag, readable) in [(b'V', true), (b'v', false)] {
        if !flags.contains(&flag) {
            continue;
        }
        for (name, value) in VARIABLES {
            let value = match *value {
                "*" if utf8 => "off",
                "*" => "on",
                v => v,
            };
            let line = match readable {
                true => format!("{name} is set to `{value}'\n"),
                false => format!("set {name} {value}\n"),
            };
            buf.extend_from_slice(line.as_bytes());
        }
    }
    if !buf.is_empty() {
        let status = out(vp, "bind", &buf).await?;
        if status != 0 {
            return Ok(status);
        }
    }
    if let Some(f) = file {
        let path = vp.abs_path(&f);
        if let Err(e) = vp.host().open(&path, OpenOptions::read()).await {
            let why = error_text(&e);
            vp.error(&format!("bind: {}: cannot read: {why}", s(&f))).await;
            return Ok(1);
        }
    }
    Ok(0)
}
