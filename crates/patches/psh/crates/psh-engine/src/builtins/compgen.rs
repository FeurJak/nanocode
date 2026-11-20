use crate::expand::Fields;
use crate::state::{R, Vp};

use super::out;

pub(super) async fn compgen(vp: &mut Vp, args: &[Vec<u8>]) -> R {
    let mut function = None;
    let mut words: Option<Vec<u8>> = None;
    let mut variables = false;
    let mut prefix = Vec::new();
    let mut suffix = Vec::new();
    let mut i = 1;
    while let Some(a) = args.get(i) {
        if a == b"--" {
            i += 1;
            break;
        }
        if a.len() < 2 || a[0] != b'-' {
            break;
        }
        let mut j = 1;
        while j < a.len() {
            let c = a[j];
            j += 1;
            if c == b'v' {
                variables = true;
                continue;
            }
            if !matches!(c, b'A' | b'F' | b'P' | b'S' | b'W') {
                return crate::expand::unsupported(vp, &format!("compgen -{}", c as char)).await;
            }
            let value = if j < a.len() {
                let v = a[j..].to_vec();
                j = a.len();
                v
            } else {
                i += 1;
                match args.get(i) {
                    Some(v) => v.clone(),
                    None => {
                        vp.error(&format!(
                            "compgen: -{}: option requires an argument",
                            c as char
                        ))
                        .await;
                        return Ok(2);
                    }
                }
            };
            match c {
                b'F' => function = Some(value),
                b'W' => words = Some(value),
                b'P' => prefix = value,
                b'S' => suffix = value,
                _ if value == b"variable" => variables = true,
                _ => {
                    return crate::expand::unsupported(
                        vp,
                        &format!("compgen -A {}", String::from_utf8_lossy(&value)),
                    )
                    .await;
                }
            }
        }
        i += 1;
    }
    let word = args.get(i).cloned().unwrap_or_default();
    if let Some(name) = function {
        return complete(vp, &name, word).await;
    }
    let mut found: Vec<Vec<u8>> = Vec::new();
    if variables {
        let mut names: Vec<Vec<u8>> = vp
            .vars
            .iter()
            .filter(|(_, v)| v.value != crate::vars::Value::Unset)
            .map(|(n, _)| n.as_bytes().to_vec())
            .collect();
        names.sort();
        found.extend(names);
    }
    if let Some(list) = words {
        let mut f = Fields::new(vp, true);
        f.unquoted(&list);
        f.finish_word();
        found.extend(f.out.into_iter().map(|f| f.text));
    }
    let mut text = Vec::new();
    let mut any = false;
    for w in found.iter().filter(|w| w.starts_with(&word)) {
        any = true;
        text.extend_from_slice(&prefix);
        text.extend_from_slice(w);
        text.extend_from_slice(&suffix);
        text.push(b'\n');
    }
    if !any {
        return Ok(1);
    }
    out(vp, "compgen", &text).await
}

async fn complete(vp: &mut Vp, name: &[u8], word: Vec<u8>) -> R {
    vp.error("compgen: warning: -F option may not work as you expect")
        .await;
    let name = String::from_utf8_lossy(name).into_owned();
    let Some(f) = vp.funcs.get(&*name).cloned() else {
        vp.error(&format!("completion: function `{name}' not found"))
            .await;
        return Ok(1);
    };
    let argv = vec![name.into_bytes(), b"compgen".to_vec(), word, Vec::new()];
    crate::exec::call_function(vp, f, argv, Vec::new()).await?;
    let words = vp
        .var_ref("COMPREPLY")
        .map(|v| v.values())
        .unwrap_or_default();
    super::unset(
        vp,
        &[b"unset".to_vec(), b"-v".to_vec(), b"COMPREPLY".to_vec()],
    )
    .await?;
    if words.is_empty() {
        return Ok(1);
    }
    let mut text = Vec::new();
    for w in words {
        text.extend_from_slice(&w);
        text.push(b'\n');
    }
    out(vp, "compgen", &text).await
}
