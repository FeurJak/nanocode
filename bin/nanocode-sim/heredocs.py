#!/usr/bin/env python3
import argparse
import json
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
SOURCE = 'crates/backends/codex/src/builtin_tools/apply_patch/mod.rs'
p = argparse.ArgumentParser()
p.add_argument('--mutations', action='store_true')
p.add_argument('--source-ref')
a = p.parse_args()
(ROOT / 'output').mkdir(exist_ok=True)
work = Path(tempfile.mkdtemp(prefix='heredoc-regression-', dir=ROOT / 'output'))
text = subprocess.check_output(['git', 'show', f'{a.source_ref}:{SOURCE}'], cwd=ROOT, text=True) if a.source_ref else (ROOT / SOURCE).read_text()
start = text.index('pub(crate) fn heredoc(')
end = text.index('\n#[derive(Clone, Debug, Eq, PartialEq)]\npub enum PatchOperation', start)
core = text[start:end]
if '-> Option<' in core.splitlines()[0]:
    core = core.replace('fn heredoc(', 'fn original_heredoc(', 1)
    core += '\nfn heredoc(script: &str) -> Result<Option<(String, Option<String>, String)>, String> { Ok(original_heredoc(script)) }\n'
cases = [
    ('parameter_in_body', 'PATCH', 'value=${PROJECT_FILE}', True),
    ('command_substitution', 'PATCH', '$(printf value)', True),
    ('arithmetic_substitution', 'PATCH', '$((1+2))', True),
    ('backticks', 'PATCH', '`printf value`', True),
    ('escaped_dollar', 'PATCH', r'\$PROJECT_FILE', True),
    ('backslash_newline', 'PATCH', 'left\\\nright', True),
    ('escaped_backslash', 'PATCH', r'left\\right', True),
    ('single_quotes', "'PATCH'", r'${NAME} $(false) `false` \\', False),
    ('double_quotes', '"PATCH"', r'${NAME} $(false) `false` \\', False),
    ('unquoted_plain', 'PATCH', 'plain', False),
]
tests = ''
for name, delimiter, body, reject in cases:
    patch = f'*** Begin Patch\n*** Add File: sample.txt\n+{body}\n*** End Patch'
    script = f'apply_patch <<{delimiter}\n{patch}\nPATCH\n'
    expected = 'assert!(result.is_err(), "raw expansion accepted: {result:?}"); assert!(result.unwrap_err().contains("quote the delimiter"));' if reject else f'assert_eq!(result.unwrap(), Some(({json.dumps(patch)}.to_owned(), None, String::new())));'
    tests += f'#[test] fn {name}() {{ let result = heredoc({json.dumps(script)}); {expected} }}\n'
tests += r'''
#[test] fn parameter_in_path() { assert!(heredoc("apply_patch <<PATCH\n*** Begin Patch\n*** Add File: $PROJECT_FILE\n+value\n*** End Patch\nPATCH\n").is_err()); }
#[test] fn alias_cd_and_rest() { let script = "cd 'folder name' && applypatch <<'PATCH'\n*** Begin Patch\n*** Add File: a\n+$literal\n*** End Patch\nPATCH\nprintf done"; let (p,cd,rest) = heredoc(script).unwrap().unwrap(); assert!(p.contains("+$literal")); assert_eq!(cd.as_deref(),Some("folder name")); assert_eq!(rest,"printf done"); }
#[test] fn rejected_rest_never_returned() { assert!(heredoc("apply_patch <<PATCH\n+$variable\nPATCH\nprintf done").is_err()); }
#[test] fn ordinary_command() { assert!(heredoc("printf plain").unwrap().is_none()); }
#[test] fn unterminated() { assert!(heredoc("apply_patch <<PATCH\nmissing terminator").unwrap().is_none()); }
'''
def run(label, source):
    path = work / (label + '.rs')
    path.write_text(source + tests)
    binary = work / label
    subprocess.run(['rustc', '--edition=2024', '--test', str(path), '-o', str(binary)], check=True)
    r = subprocess.run([str(binary), '--nocapture'], text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    (work / (label + '.log')).write_text(r.stdout)
    print(r.stdout, end='')
    return r.returncode
result = run('current', core)
if result == 0 and a.mutations:
    old = "if !quoted && patch.contains(['$', '\\x60', '\\\\'])"
    for label, new in [('raw-unquoted', "if false && !quoted && patch.contains(['$', '\\x60', '\\\\'])"), ('reject-quoted', "if patch.contains(['$', '\\x60', '\\\\'])")]:
        assert core.count(old) == 1, label
        assert run(label, core.replace(old, new)) != 0, 'mutation survived: ' + label
        print('caught mutation:', label)
print('artifacts:', work)
raise SystemExit(result)
