#!/usr/bin/env python3
import gzip
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parent.parent
vendor = root / 'vendor/reference-cli'
reference = json.loads((vendor / 'reference.json').read_text())
for entry in reference['files']:
    path = vendor / entry['name']
    if path.is_symlink() or not path.is_file() or path.stat().st_size != entry['bytes'] or hashlib.sha256(path.read_bytes()).hexdigest() != entry['sha256']:
        raise SystemExit('reference mismatch: ' + entry['name'])
spec = importlib.util.spec_from_file_location('reference_guard', root / 'scripts/reference-cli-guard.py')
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)
report = guard.verify(vendor, attestations=False)
if not report['ok']:
    raise SystemExit(json.dumps(report, indent=2))
if len(sys.argv) != 2 or sys.argv[1] not in ('verify', 'stage'):
    raise SystemExit('usage: scripts/reference-cli.sh verify|stage')
if os.environ.get('NANOCODE_REFERENCE_NETWORK') != 'denied':
    raise SystemExit('use scripts/reference-cli.sh for the OS network boundary')
try:
    with socket.create_connection(('1.1.1.1', 443), timeout=1):
        raise SystemExit('network boundary is not active')
except OSError:
    pass
out = root / 'output'
out.mkdir(exist_ok=True)
work = Path(tempfile.mkdtemp(prefix='reference-cli-', dir=out))
if sys.argv[1] == 'stage':
    staged = work / 'dist'
    staged.mkdir()
    for entry in reference['files']:
        shutil.copyfile(vendor / entry['name'], staged / entry['name'])
    report['staged'] = str(staged)
archive = vendor / ('nanocode-x86_64-unknown-linux-gnu.' + reference['core'][:12] + '.gz')
program = work / 'nanocode'
program.write_bytes(gzip.decompress(archive.read_bytes()))
program.chmod(0o755)
result = subprocess.run([str(program), '--version'], capture_output=True, text=True, check=True, timeout=15)
if result.stdout != 'nanocode 0.7.0\n' or result.stderr:
    raise SystemExit('unexpected CLI version output')
report.update({'core': reference['core'], 'network': 'Linux isolated user+network namespace', 'linux_amd64_version': result.stdout.strip(), 'runtime_scope': 'Linux x86_64 --version only; other five platforms headers/hashes only', 'build_scope': 'prebuilt reference materialization; no Rust compiler executed', 'python_toolchain': sys.version.split()[0]})
(work / 'receipt.json').write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps(report, indent=2))
