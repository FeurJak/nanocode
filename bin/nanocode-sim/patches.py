#!/usr/bin/env python3
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / 'crates/backends/codex/src/builtin_tools/apply_patch/mod.rs'
p = argparse.ArgumentParser()
p.add_argument('--mutations', action='store_true')
a = p.parse_args()
(ROOT / 'output').mkdir(exist_ok=True)
work = Path(tempfile.mkdtemp(prefix='patch-regression-', dir=ROOT / 'output'))
text = SOURCE.read_text()
marker = '#[derive(Clone, Debug, Eq, PartialEq)]\npub enum PatchOperation'
assert text.count(marker) == 1
prefix = '''#![allow(dead_code)]
use std::{collections::{HashMap, HashSet}, path::{Component, Path, PathBuf}, sync::atomic::{AtomicBool, Ordering}};
static LENIENT_MATCHING: AtomicBool = AtomicBool::new(false);
'''
for name in ['parser', 'seek_sequence', 'streaming_parser']:
    prefix += '#[path=' + json.dumps(str(SOURCE.parent / (name + '.rs'))) + '] mod ' + name + ';\n'
prefix += 'use parser::{Hunk, UpdateFileChunk, parse_patch};\n'
core = text[text.index(marker):]
tests = r'''
#[cfg(test)] mod checks {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CASE: AtomicUsize = AtomicUsize::new(0);
    fn edit(body: &str) -> String { format!("*** Begin Patch\n*** Update File: sample.txt\n@@\n{body}*** End Patch") }
    fn check(original: &str, patch: &str, expected: &str) {
        let files = HashMap::from([(PathBuf::from("sample.txt"), original.to_owned())]);
        let p = plan(patch, &files).unwrap();
        assert_eq!(p.operations(), &[PatchOperation::Write { path: PathBuf::from("sample.txt"), contents: expected.to_owned() }]);
        let dir = PathBuf::from(std::env::var_os("PATCH_CASES").unwrap()).join(CASE.fetch_add(1, Ordering::Relaxed).to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("sample.txt"), original).unwrap();
        apply(patch, &dir).unwrap();
        assert_eq!(std::fs::read(dir.join("sample.txt")).unwrap(), expected.as_bytes());
    }
    #[test] fn preserves_terminal_newlines() {
        for count in 0..=3 {
            let old = format!("head\nold\ntail{}", "\n".repeat(count));
            let new = old.replacen("old", "new", 1);
            check(&old, &edit("-old\n+new\n"), &new);
        }
    }
    #[test] fn last_line_without_newline() { check("old", &edit("-old\n+new\n*** End of File\n"), "new"); }
    #[test] fn preserves_unrelated_command_flags() {
        check("git clone -b https://old@example.invalid/repo", &edit("-git clone -b https://old@example.invalid/repo\n+git clone -b https://new@example.invalid/repo\n"), "git clone -b https://new@example.invalid/repo");
    }
    #[test] fn append_preserves_blank_lines() {
        for count in 0..=3 {
            let old = format!("head{}", "\n".repeat(count));
            let new = if count == 0 { "head\nnew".to_owned() } else { format!("{old}new\n") };
            check(&old, &edit("+new\n"), &new);
        }
    }
    #[test] fn deleting_every_line_is_empty() { for old in ["old", "old\n"] { check(old, &edit("-old\n"), ""); } }
    #[test] fn explicit_blank_line_edit() { check("head\nold\n\n", &edit("-old\n-\n+new\n"), "head\nnew\n"); }
    #[test] fn inserting_into_empty_file() { check("", &edit("+new\n"), "new"); }
    #[test] fn add_file_still_has_its_newline() {
        let p = plan("*** Begin Patch\n*** Add File: sample.txt\n+new\n*** End Patch", &HashMap::new()).unwrap();
        assert_eq!(p.operations(), &[PatchOperation::Write { path: PathBuf::from("sample.txt"), contents: "new\n".to_owned() }]);
    }
    #[test] fn replacement_is_one_write() {
        check("old", "*** Begin Patch\n*** Delete File: sample.txt\n*** Add File: sample.txt\n+new\n*** End Patch", "new\n");
    }
    #[test] fn replacement_alias_is_one_write() {
        check("old", "*** Begin Patch\n*** Delete File: ./sample.txt\n*** Add File: folder/../sample.txt\n+new\n*** End Patch", "new\n");
    }
    #[test] fn workspace_alias_replacement_is_one_write() {
        for reverse in [false, true] {
            let dir = PathBuf::from(std::env::var_os("PATCH_CASES").unwrap()).join(format!("workspace-{reverse}"));
            std::fs::create_dir_all(&dir).unwrap();
            let target = dir.join("sample.txt");
            std::fs::write(&target, "old").unwrap();
            #[cfg(unix)] {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let relative = Path::new("sample.txt");
            let (delete, add) = if reverse { (relative, target.as_path()) } else { (target.as_path(), relative) };
            let patch = format!("*** Begin Patch\n*** Delete File: {}\n*** Add File: {}\n+new\n*** End Patch", delete.display(), add.display());
            let hunks = parse(&patch).unwrap();
            validate_unique_targets(&hunks, |p| normalize_path(&resolve(&dir, p))).unwrap();
            let planned = plan_hunks(hunks, &HashMap::new(), false, |p| normalize_path(&resolve(&dir, p))).unwrap();
            assert_eq!(planned.operations().len(), 1);
            assert!(matches!(&planned.operations()[0], PatchOperation::Write { .. }));
            let summary = apply(&patch, &dir).unwrap();
            assert_eq!(summary, format!("Success. Updated the following files:\nM {}\n", add.display()));
            assert_eq!(std::fs::read(&target).unwrap(), b"new\n");
            #[cfg(unix)] {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o755);
            }
        }
    }
    #[test] fn malformed_later_hunk_never_changes_file() {
        let dir = PathBuf::from(std::env::var_os("PATCH_CASES").unwrap()).join("malformed"); std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.txt"); std::fs::write(&path, "old").unwrap();
        assert!(apply("*** Begin Patch\n*** Delete File: sample.txt\n*** Add File: sample.txt\n+new\n*** Update File: missing\n@@\n-old\n+new\n*** End Patch", &dir).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "old");
    }
    #[test] fn empty_update_is_byte_identity() {
        for old in ["", "head", "head\n", "head\n\n", "\n", "\n\n"] {
            assert_eq!(apply_chunks(old, &[], Path::new("sample.txt"), false).unwrap(), old);
        }
    }
    #[test] fn standalone_delete_still_deletes() {
        let p = plan("*** Begin Patch\n*** Delete File: sample.txt\n*** End Patch", &HashMap::new()).unwrap();
        assert_eq!(p.operations(), &[PatchOperation::Delete { path: PathBuf::from("sample.txt") }]);
    }
    #[test] fn replacement_summary_is_modified_once() {
        let p = plan("*** Begin Patch\n*** Delete File: sample.txt\n*** Add File: sample.txt\n+new\n*** End Patch", &HashMap::new()).unwrap();
        assert_eq!(p.summary(), "Success. Updated the following files:\nM sample.txt\n");
    }
    #[test] fn ambiguous_duplicates_stay_rejected() {
        let files = HashMap::from([(PathBuf::from("sample.txt"), "old".to_owned())]);
        for patch in [
            "*** Begin Patch\n*** Add File: sample.txt\n+a\n*** Add File: ./sample.txt\n+b\n*** End Patch",
            "*** Begin Patch\n*** Delete File: sample.txt\n*** Add File: sample.txt\n+a\n*** Add File: sample.txt\n+b\n*** End Patch",
            "*** Begin Patch\n*** Update File: sample.txt\n*** Move to: other.txt\n@@\n-old\n+new\n*** Add File: sample.txt\n+a\n*** End Patch"
        ] { assert!(plan(patch, &files).is_err(), "{patch}"); }
    }
}
'''
def run(label, body, select=None):
    path = work / (label + '.rs')
    path.write_text(prefix + body + tests)
    binary = work / label
    subprocess.run(['rustc', '--edition=2024', '--test', str(path), '-o', str(binary)], check=True)
    env = dict(os.environ, PATCH_CASES=str(work / (label + '-cases')))
    cmd = [str(binary), '--nocapture']
    if select: cmd += [select]
    r = subprocess.run(cmd, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    (work / (label + '.log')).write_text(r.stdout)
    print(r.stdout, end='')
    return r.returncode
if run('baseline', core) != 0:
    raise SystemExit('patch regression failed')
if a.mutations:
    changes = [
        ('normalize-eof', 'if trailing_newline {', 'if !original_lines.last().is_some_and(String::is_empty) {', 'preserves_terminal_newlines'),
        ('reject-replacement', 'if let Hunk::AddFile { .. } = hunk\n            && deleted.remove', 'if let Hunk::AddFile { .. } = hunk\n            && LENIENT_MATCHING.load(Ordering::Relaxed)\n            && deleted.remove', 'replacement_is_one_write'),
        ('skip-plan-validation', '    validate_unique_targets(&hunks, normalize_path)?;\n', '', 'ambiguous_duplicates_stay_rejected'),
        ('raw-path-identity', 'resolve_target(deleted) == resolve_target(&path)', 'deleted == &path', 'workspace_alias_replacement_is_one_write'),
        ('delete-before-write', '                    operations.retain(|operation| {\n                        !matches!(operation, PatchOperation::Delete { path: deleted } if resolve_target(deleted) == resolve_target(&path))\n                    });\n', '', 'replacement_is_one_write'),
    ]
    for label, old, new, select in changes:
        if old not in core:
            raise SystemExit('mutation site missing: ' + label)
        if run(label, core.replace(old, new), select) == 0:
            raise SystemExit('mutation survived: ' + label)
        print('caught mutation:', label)
print('artifacts:', work)
