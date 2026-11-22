#!/usr/bin/env python3
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys

root = Path(__file__).resolve().parent.parent


def run(*args):
    return subprocess.check_output(args, cwd=root, text=True).strip()


def require(condition, message):
    if not condition:
        raise SystemExit('release-cli-attest: ' + message)


def revision():
    require(not run('git', 'status', '--porcelain', '--untracked-files=no'), 'tracked source must be clean')
    return run('git', 'rev-parse', 'HEAD')


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def files(directory):
    result = {}
    for path in sorted(directory.rglob('*')):
        require(not path.is_symlink(), 'symlink in lane: ' + str(path))
        if path.is_file() and path.name != '.attestation.json':
            result[path.relative_to(directory).as_posix()] = digest(path)
    require(bool(result), 'empty build lane')
    return result


action, lane, directory, *extra = sys.argv[1:]
require(action in ('begin', 'finish', 'pack'), 'unknown action')
require(lane in ('darwin-aarch64', 'darwin-x86_64', 'linux-arm64', 'linux-amd64', 'windows-x86_64', 'windows-aarch64'), 'unknown lane')
directory = Path(directory)
path = directory / '.attestation.json'
if action == 'begin':
    require(not path.exists(), 'build receipt already exists')
    require(os.environ.get('NANOCODE_NO_CACHE') == '1', 'binary cache restore must be disabled')
    tools = {'rustc': run('rustc', '-vV'), 'cargo': run('cargo', '-V')}
    for tool in ('cc', 'clang', 'aarch64-linux-gnu-gcc', 'lld-link', 'xcodebuild'):
        if shutil.which(tool):
            result = subprocess.run([tool, '-version' if tool == 'xcodebuild' else '--version'], capture_output=True, text=True)
            if result.returncode == 0:
                tools[tool] = (result.stdout + result.stderr).strip()
    att = {'schema': 1, 'lane': lane, 'core_sha': revision(), 'tracked_clean': True,
           'started_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
           'host': {'system': platform.system(), 'release': platform.release(), 'machine': platform.machine()},
           'toolchains': tools, 'profile': os.environ.get('PROFILE', 'dist'),
           'binary_cache_restore': False, 'bolt': os.environ.get('NANOCODE_BOLT') == '1',
           'command': 'scripts/release-cli.sh lane ' + lane,
           'scope': 'unsigned local build receipt; not independently verified provenance'}
    with path.open('x') as stream:
        json.dump(att, stream, indent=2)
else:
    require(path.is_file() and not path.is_symlink(), 'missing build receipt; rebuild this lane')
    att = json.loads(path.read_text())
    require(att['core_sha'] == revision() and att['lane'] == lane and att['tracked_clean'] is True, 'source or lane changed since build began')
    current = files(directory)
    if action == 'finish':
        require('lane_files' not in att, 'build receipt already finished')
        att['lane_files'] = current
        att['binaries'] = {name: value for name, value in current.items() if not name.startswith('.')}
        require(bool(att['binaries']), 'no built binaries')
        att['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        path.write_text(json.dumps(att, indent=2) + '\n')
    else:
        require(att.get('lane_files') == current, 'lane files changed since build finished')
        dist = Path(extra[0])
        prefix = 'parts-' + att['core_sha'][:12] + '-' + lane
        att['manifests'] = {prefix + suffix: digest(dist / (prefix + suffix)) for suffix in ('.sums', '.inputs')}
        (dist / (prefix + '.attestation.json')).write_text(json.dumps(att, indent=2) + '\n')
