#!/usr/bin/env python3
import argparse
import gzip
import hashlib
import json
import lzma
from pathlib import Path
import re
import struct
import sys
import zipfile

CORE = '6ed92be87b3a2f704a7af857a2237b3fccafbb27'
LANES = {'darwin-aarch64': ('aarch64-apple-darwin', 'macho', 0x100000c), 'darwin-x86_64': ('x86_64-apple-darwin', 'macho', 0x1000007), 'linux-arm64': ('aarch64-unknown-linux-gnu', 'elf', 183), 'linux-amd64': ('x86_64-unknown-linux-gnu', 'elf', 62), 'windows-x86_64': ('x86_64-pc-windows-msvc', 'pe', 0x8664), 'windows-aarch64': ('aarch64-pc-windows-msvc', 'pe', 0xaa64)}
HEX = re.compile(r'[0-9a-fA-F]{64}\Z')


def require(condition, message):
    if not condition:
        raise ValueError(message)


def safe(name):
    require(bool(name) and name not in ('.', '..') and not any(c in name for c in '/\\:\x00') and not any(c.isspace() for c in name), 'unsafe asset/member name: ' + repr(name))
    return name


def sha(data):
    return hashlib.sha256(data).hexdigest()


def filehash(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def architecture(data, lane, label):
    _, kind, expected = LANES[lane]
    if kind == 'elf':
        require(len(data) >= 64 and data[:4] == b'\x7fELF' and data[4] == 2 and data[5] in (1, 2) and data[6] == 1, label + ': invalid ELF64 header')
        actual = struct.unpack_from('<H' if data[5] == 1 else '>H', data, 18)[0]
    elif kind == 'macho':
        require(len(data) >= 32 and data[:4] in (b'\xcf\xfa\xed\xfe', b'\xfe\xed\xfa\xcf'), label + ': invalid thin Mach-O64 header')
        actual = struct.unpack_from('<I' if data[:4] == b'\xcf\xfa\xed\xfe' else '>I', data, 4)[0]
    else:
        require(len(data) >= 64 and data[:2] == b'MZ', label + ': invalid DOS header')
        offset = struct.unpack_from('<I', data, 60)[0]
        require(offset >= 64 and len(data) >= offset + 24 and data[offset:offset + 4] == b'PE\0\0', label + ': invalid PE header')
        actual = struct.unpack_from('<H', data, offset + 4)[0]
    require(actual == expected, label + ': wrong architecture for ' + lane + ': ' + str(actual))


def verify(root, attestations=False, core=CORE):
    require(re.fullmatch(r'[0-9a-f]{40}', core) is not None, 'core must be a full lowercase commit SHA')
    short = core[:12]
    require(root.is_dir(), 'staging directory not found')
    base = root / 'dist' if (root / 'dist').is_dir() else root
    errors, lanes, checksums, keys = [], {}, {}, {}
    checked = set()
    for lane, (triple, _, _) in LANES.items():
        try:
            prefix = 'parts-' + short + '-' + lane
            sums_path, inputs_path = base / (prefix + '.sums'), base / (prefix + '.inputs')
            require(sums_path.is_file() and inputs_path.is_file(), lane + ': missing sums/inputs')
            require(not sums_path.is_symlink() and not inputs_path.is_symlink(), lane + ': symlink manifest rejected')
            assets_dir = base / lane if (base / lane).is_dir() else base
            sums = {}
            for number, line in enumerate(sums_path.read_text().splitlines(), 1):
                fields = line.split()
                require(len(fields) == 2 and HEX.fullmatch(fields[0]), lane + ': invalid sums line ' + str(number))
                digest, asset = fields[0].lower(), safe(fields[1])
                require(asset.endswith(('.gz', '.xz', '.zip')), 'unsupported archive: ' + asset)
                require(asset not in sums, 'duplicate sums entry: ' + asset)
                require(asset not in checksums or checksums[asset] == digest, 'conflicting duplicate asset checksum: ' + asset)
                sums[asset] = digest
                checksums[asset] = digest
            require(bool(sums), lane + ': empty sums')
            inputs = []
            for number, line in enumerate(inputs_path.read_text().splitlines(), 1):
                fields = line.split()
                require(len(fields) in (2, 3) and HEX.fullmatch(fields[0]), lane + ': invalid input key/line ' + str(number))
                key, asset = fields[0].lower(), safe(fields[1])
                member = safe(fields[2]) if len(fields) == 3 else None
                require(asset in sums, 'input asset absent from lane sums: ' + asset)
                require((asset.endswith('.zip')) == (member is not None), 'input member required only for ZIP: ' + asset)
                identity = (asset, member)
                require(key not in keys or keys[key] == identity, 'conflicting duplicate input key: ' + key)
                keys[key] = identity
                require(identity not in inputs, 'duplicate input reference: ' + str(identity))
                inputs.append(identity)
            require(bool(inputs), lane + ': empty inputs')
            names = set(sums)
            if lane.startswith('windows-'):
                expected = 'nanocode-' + triple + '.' + short + '.zip'
                require(names == {expected}, lane + ': unexpected/missing Windows assets')
                require(set(inputs) == {(expected, 'nanocode.exe'), (expected, 'nanocode-python.exe')}, lane + ': missing/unexpected Windows input members')
            else:
                stem = 'nanocode-' + triple + '.' + short
                required = {stem + '.gz', stem + '.xz'}
                python = [a for a in names if re.fullmatch(r'nanocode-python-[0-9a-f]{64}\.xz', a)]
                require(len(python) == 1, lane + ': requires one content-addressed Python asset')
                required.add(python[0])
                if lane.startswith('darwin-'):
                    required.add('nanocode-cua-driver-' + triple + '.' + short + '.gz')
                optional = {'nanocode-exec-' + triple + '.' + short + ext for ext in ('.gz', '.xz')} | {'nanocode-front-' + triple + '.' + short + '.gz'} if lane.startswith('linux-') else set()
                require(required <= names and names <= required | optional, lane + ': missing/unexpected assets or revision')
                required_inputs = {(stem + '.xz', None), (python[0], None)}
                if lane.startswith('darwin-'):
                    required_inputs.add(('nanocode-cua-driver-' + triple + '.' + short + '.gz', None))
                require(set(inputs) == required_inputs, lane + ': missing/unexpected input associations')
            contents = {}
            binary_hashes = set()
            for asset, digest in sums.items():
                path = assets_dir / asset
                require(path.is_file() and not path.is_symlink() and path.resolve().is_relative_to(base.resolve()), 'missing, symlink or escaped artifact: ' + str(path))
                require(filehash(path) == digest, 'archive checksum mismatch: ' + asset)
                if asset.endswith('.zip'):
                    with zipfile.ZipFile(path) as archive:
                        members = archive.infolist()
                        require(len(members) == 2 and {m.filename for m in members} == {'nanocode.exe', 'nanocode-python.exe'}, 'ZIP members must be exactly nanocode.exe,nanocode-python.exe')
                        for member in members:
                            require(not member.is_dir() and (member.external_attr >> 16) & 0xf000 != 0xa000, 'ZIP symlink/directory rejected')
                            data = archive.read(member)
                            architecture(data, lane, asset + ':' + member.filename)
                            binary_hashes.add(sha(data))
                else:
                    opener = lzma.open if asset.endswith('.xz') else gzip.open
                    with opener(path, 'rb') as stream:
                        data = stream.read()
                    architecture(data, lane, asset)
                    digest_plain = sha(data)
                    binary_hashes.add(digest_plain)
                    contents[asset.rsplit('.', 1)[0]] = contents.get(asset.rsplit('.', 1)[0], digest_plain)
                    require(contents[asset.rsplit('.', 1)[0]] == digest_plain, 'gzip/xz CLI contents differ: ' + asset)
                    if asset.startswith('nanocode-python-'):
                        require(digest_plain == asset[len('nanocode-python-'):-3], 'Python content hash mismatch: ' + asset)
                checked.add(str(path))
            att_candidates = [base / (prefix + '.json'), base / (prefix + '.attestation.json'), base / ('attestation-' + lane + '.json'), base / lane / 'attestation.json', base / (lane + '.json'), base / (lane + '.attestation.json'), base / 'attestations' / (lane + '.json'), root / 'attestations' / (lane + '.json')]
            att_candidates = list(dict.fromkeys(att_candidates))
            found = [p for p in att_candidates if p.is_file()]
            require(not attestations or bool(found), lane + ': attestation required')
            for path in found:
                require(not path.is_symlink(), lane + ': symlink attestation rejected')
                att = json.loads(path.read_text())
                require(att.get('core_sha') == core and att.get('tracked_clean') is True, lane + ': attestation revision/cleanliness mismatch')
                require(att.get('schema') == 1 and att.get('lane') == lane, lane + ': attestation schema/lane mismatch')
                require(att.get('binary_cache_restore') is False and bool(att.get('toolchains', {}).get('rustc')) and bool(att.get('toolchains', {}).get('cargo')), lane + ': build/toolchain receipt required')
                require(att.get('manifests') == {p.name: filehash(p) for p in (sums_path, inputs_path)}, lane + ': attestation manifest mismatch')
                binaries = att.get('binaries', {})
                require(isinstance(binaries, dict) and bool(binaries) and all(isinstance(h, str) and HEX.fullmatch(h) for h in binaries.values()), lane + ': invalid attested binaries')
                require(set(binaries.values()) == binary_hashes, lane + ': attestation binary hash mismatch')
            lanes[lane] = {'ok': True, 'archives': len(sums), 'inputs': len(inputs), 'attestations': [str(p) for p in found]}
        except Exception as e:
            errors.append(lane + ': ' + str(e))
            lanes[lane] = {'ok': False, 'error': str(e)}
    for directory in {base} | {base / lane for lane in LANES if (base / lane).is_dir()}:
        for path in directory.iterdir():
            if path.name.endswith(('.gz', '.xz', '.zip')) and str(path) not in checked:
                errors.append('unverified archive: ' + str(path))
            if path.name.startswith('parts-') and path.name.endswith(('.sums', '.inputs')):
                allowed = {'parts-' + short + '-' + lane + suffix for lane in LANES for suffix in ('.sums', '.inputs')}
                if path.name not in allowed:
                    errors.append('unexpected part/revision: ' + path.name)
    return {'ok': not errors, 'core_sha': core, 'short_sha': short, 'validation_scope': 'archive integrity, manifest consistency, thin executable header architecture; not execution or provenance', 'lanes': lanes, 'errors': errors, 'verified_archive_paths': len(checked)}


def main():
    parser = argparse.ArgumentParser(description='Read-only six-lane Nanocode CLI artifact guard; thin 64-bit executable architecture checks, not executable validity or provenance.')
    parser.add_argument('staging', type=Path)
    parser.add_argument('--require-attestations', action='store_true')
    parser.add_argument('--core', default=CORE, help='full source commit SHA; defaults to the pinned reference revision')
    parser.add_argument('--report', type=Path)
    args = parser.parse_args()
    try:
        report = verify(args.staging, args.require_attestations, args.core)
    except Exception as e:
        report = {'ok': False, 'core_sha': args.core, 'errors': [str(e)]}
    output = json.dumps(report, indent=2, sort_keys=True) + '\n'
    if args.report:
        try:
            require(not args.report.exists(), 'report destination already exists; refusing overwrite')
            require(not args.report.resolve().is_relative_to(args.staging.resolve()), 'report must be outside staging')
            with args.report.open('x') as stream:
                stream.write(output)
        except Exception as e:
            report['ok'] = False
            report.setdefault('errors', []).append('report write failed: ' + str(e))
            output = json.dumps(report, indent=2, sort_keys=True) + '\n'
    print(output, end='')
    return 0 if report['ok'] else 1


if __name__ == '__main__':
    sys.exit(main())
