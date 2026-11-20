//! `complete`, `compopt` and `bind` of a non-interactive bash: completion specs are
//! kept and printed (`complete -p`), nothing ever completes, and `bind` warns that
//! line editing is off.

use crate::state::{R, Vp};

use super::out;

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

/// `bind` that defines key bindings warns and succeeds, as in a shell without line
/// editing; one that lists or reads bindings is not supported yet.
pub(super) async fn bind(vp: &mut Vp, args: &[Vec<u8>]) -> R {
    let mut i = 1;
    while let Some(a) = args.get(i) {
        if a == b"--" || a.len() < 2 || a[0] != b'-' {
            break;
        }
        if a[1..].iter().any(|c| !matches!(c, b'x' | b'm')) {
            return crate::expand::unsupported(vp, "bind").await;
        }
        if a.ends_with(b"m") {
            i += 1;
        }
        i += 1;
    }
    vp.error("bind: warning: line editing not enabled").await;
    Ok(0)
}
