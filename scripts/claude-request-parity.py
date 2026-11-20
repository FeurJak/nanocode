import argparse
import gzip
import hashlib
import http.server
import json
import os
from pathlib import Path
import re
import shutil
import ssl
import subprocess
import tempfile
import threading

root = Path(__file__).resolve().parents[1]
fixture_path = root / 'crates/backends/claude/upstream/request.json'
manifest = json.loads((root / 'crates/backends/claude/prompts/upstream.json').read_text())
parser = argparse.ArgumentParser()
parser.add_argument('--nanocode', type=Path, default=root / 'target/debug/nanocode')
parser.add_argument('--claude', type=Path, help='capture the pinned Claude Code executable into the fixture again')
parser.add_argument('--keep', type=Path, help="write nanocode's scrubbed requests here")
parser.add_argument('--record', action='store_true', help='store the differences found now as the documented digests')
args = parser.parse_args()
fixture = json.loads(fixture_path.read_text())
prompt, tools = fixture['prompt'], fixture['tools']
uuid = re.compile(r'[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}')


def stream(body, blocks, stop, counter):
    message = {'id': f'msg_{counter:03d}', 'type': 'message', 'role': 'assistant', 'model': body['model'],
               'content': [], 'stop_reason': None, 'stop_sequence': None,
               'usage': {'input_tokens': 10, 'output_tokens': 1}}
    events = [('message_start', {'type': 'message_start', 'message': message})]
    for index, block in enumerate(blocks):
        if block['type'] == 'text':
            start, delta = {'type': 'text', 'text': ''}, {'type': 'text_delta', 'text': block['text']}
        else:
            start = {'type': 'tool_use', 'id': block['id'], 'name': block['name'], 'input': {}}
            delta = {'type': 'input_json_delta', 'partial_json': json.dumps(block['input'])}
        events += [('content_block_start', {'type': 'content_block_start', 'index': index, 'content_block': start}),
                   ('content_block_delta', {'type': 'content_block_delta', 'index': index, 'delta': delta}),
                   ('content_block_stop', {'type': 'content_block_stop', 'index': index})]
    events += [('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': stop, 'stop_sequence': None},
                                  'usage': {'output_tokens': 5}}),
               ('message_stop', {'type': 'message_stop'})]
    return ''.join(f'event: {name}\ndata: {json.dumps(value)}\n\n' for name, value in events).encode()


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def reply(self, data, kind):
        self.send_response(200)
        self.send_header('content-type', kind)
        self.send_header('content-length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == '/page':
            return self.reply(fixture['page'].encode(), 'text/html; charset=utf-8')
        self.reply(json.dumps({'data': [], 'has_more': False}).encode(), 'application/json')

    def do_POST(self):
        data = self.rfile.read(int(self.headers.get('content-length') or 0))
        if self.headers.get('content-encoding') == 'gzip':
            data = gzip.decompress(data)
        body = json.loads(data)
        if '/messages' not in self.path or 'count_tokens' in self.path:
            return self.reply(json.dumps({'input_tokens': 10}).encode(), 'application/json')
        if not body.get('tools'):
            self.server.side.append({'path': self.path, 'body': body})
            data = stream(body, [{'type': 'text', 'text': 'Page summary.'}], 'end_turn', 900 + len(self.server.side))
            return self.reply(data, 'text/event-stream')
        self.server.requests.append({
            'method': self.command,
            'path': self.path,
            'headers': {name.lower(): value for name, value in self.headers.items()
                        if name.lower() not in ('authorization', 'x-api-key')},
            'body': body,
        })
        step = sum(1 for message in body['messages'] if message['role'] == 'assistant'
                   and isinstance(message['content'], list)
                   for block in message['content'] if block.get('type') == 'tool_use')
        script = json.loads(json.dumps(self.server.script).replace('<cwd>', str(self.server.work)).replace('<page>', self.server.page))
        if step < len(script):
            name, value = script[step]
            blocks, stop = [{'type': 'tool_use', 'id': f'toolu_{step:02d}', 'name': name, 'input': value}], 'tool_use'
        else:
            blocks, stop = [{'type': 'text', 'text': 'Done.'}], 'end_turn'
        self.reply(stream(body, blocks, stop, len(self.server.requests)), 'text/event-stream')


def certificates(scratch):
    def openssl(*arguments):
        subprocess.run(['openssl', *arguments], cwd=scratch, check=True, capture_output=True)
    openssl('req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes', '-days', '2',
            '-subj', '/CN=parity CA', '-keyout', 'ca.key', '-out', 'ca.pem',
            '-addext', 'basicConstraints=critical,CA:TRUE', '-addext', 'keyUsage=critical,keyCertSign')
    openssl('req', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes', '-subj', '/CN=127.0.0.1',
            '-keyout', 'leaf.key', '-out', 'leaf.csr')
    (scratch / 'leaf.ext').write_text('subjectAltName=IP:127.0.0.1\nbasicConstraints=CA:FALSE\n'
                                      'extendedKeyUsage=serverAuth\n')
    openssl('x509', '-req', '-in', 'leaf.csr', '-CA', 'ca.pem', '-CAkey', 'ca.key', '-CAcreateserial', '-days', '2',
            '-extfile', 'leaf.ext', '-out', 'leaf.pem')
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(scratch / 'leaf.pem', scratch / 'leaf.key')
    return scratch / 'ca.pem', context


def capture(command, env, scratch, script):
    work, home = scratch / 'work', scratch / 'home'
    authority, context = certificates(scratch)
    pages = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    pages.socket = context.wrap_socket(pages.socket, server_side=True)
    threading.Thread(target=pages.serve_forever, daemon=True).start()
    page = f'https://127.0.0.1:{pages.server_address[1]}'
    (work / 'sub').mkdir(parents=True)
    home.mkdir()
    for name, text in fixture['files'].items():
        (work / name).write_text(text)
    git = ['git', '-c', 'user.name=parity', '-c', 'user.email=parity@example.com']
    subprocess.run(['git', 'init', '-q', '-b', 'master'], cwd=work, check=True)
    subprocess.run(['git', 'add', '-A'], cwd=work, check=True)
    subprocess.run(git + ['commit', '-qm', 'init'], cwd=work, check=True)
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    server.requests, server.side, server.work, server.page = [], [], work, page
    server.script = script
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = server.base = f'http://127.0.0.1:{server.server_address[1]}'
    env = {key: value.replace('{base}', base).replace('{home}', str(home)) for key, value in env.items()}
    full = dict(PATH=os.environ['PATH'], HOME=str(home), TERM='dumb', SHELL='/bin/bash', LANG='C.UTF-8',
                SSL_CERT_FILE=str(authority), NODE_EXTRA_CA_CERTS=str(authority),
                GIT_CONFIG_GLOBAL='/dev/null', GIT_CONFIG_NOSYSTEM='1', **env)
    run = subprocess.run(command, cwd=work, env=full, capture_output=True, text=True, timeout=300)
    server.shutdown()
    server.server_close()
    pages.shutdown()
    pages.server_close()
    if not server.requests:
        raise SystemExit(f'{command[0]} sent no request (exit {run.returncode}):\n{run.stdout}\n{run.stderr}')
    return scrub({'requests': server.requests, 'side': server.side}, work, home, base, page)


def scrub(value, *names):
    if isinstance(value, dict):
        return {key: scrub(item, *names) for key, item in value.items()}
    if isinstance(value, list):
        return [scrub(item, *names) for item in value]
    if isinstance(value, str):
        work, home, base, page = map(str, names)
        value = value.replace(work, '<cwd>').replace(home, '<home>').replace(base, '<base>').replace(page, '<page>')
        value = re.sub(r'\n - (Platform|Shell|OS Version): [^\n]*', lambda m: f'\n - {m.group(1)}: <host>', value)
        value = re.sub(r"(Today's date is (now )?)\d{4}-\d{2}-\d{2}", r'\1<date>', value)
        value = re.sub(r'(Recent commits:\n)[0-9a-f]{7,}', r'\1<commit>', value)
        return uuid.sub('<uuid>', value)
    return value


def digest(value):
    text = value if isinstance(value, str) else json.dumps(value, sort_keys=True, ensure_ascii=False)
    return hashlib.sha256(text.encode()).hexdigest()[:12]


def short(value):
    text = value if isinstance(value, str) else json.dumps(value, sort_keys=True, ensure_ascii=False)
    return text if len(text) <= 80 else f'{digest(text)} ({len(text)} chars)'


def without_cache(value):
    if isinstance(value, dict):
        if isinstance(value.get('content'), str) and value.get('role'):
            value = dict(value, content=blocks(value['content']))
        return {key: without_cache(item) for key, item in value.items() if key != 'cache_control'}
    if isinstance(value, list):
        return [without_cache(item) for item in value]
    return value


def blocks(content):
    return [{'type': 'text', 'text': content}] if isinstance(content, str) else content


def request_facts(body):
    out = {}
    for key, value in body.items():
        if key in ('messages', 'system') or key == 'tools' and value:
            continue
        if key == 'metadata':
            for name, item in value.items():
                if name == 'user_id' and isinstance(item, str) and item.startswith('{'):
                    for field, inner in json.loads(item).items():
                        out[f'metadata.user_id.{field}'] = 'present' if inner else json.dumps(inner)
                else:
                    out[f'metadata.{name}'] = 'present' if name == 'user_id' else short(item)
            continue
        out[f'param:{key}'] = short(value)
    system = list(blocks(body.get('system', [])))
    if system and system[0]['text'].startswith('x-anthropic-billing-header:'):
        header = re.sub(r'cch=[0-9a-f]+', 'cch=<hash>', system.pop(0)['text'])
        out['system:billing-header'] = re.sub(r'(cc_version=[0-9.]+)\.[0-9a-f]+', r'\1.<hash>', header)
    out['system'] = str(len(system))
    for at, block in enumerate(system):
        out[f'system[{at}]'] = short(block['text'])
    out['tools'] = ','.join(tool['name'] for tool in body.get('tools', []))
    for tool in body.get('tools', []):
        out[f"tool:{tool['name']}"] = digest(without_cache(tool))
    marks = [f'system[{at}]' for at, block in enumerate(system) if 'cache_control' in block]
    last = len(body['messages']) - 1
    for at, message in enumerate(body['messages']):
        for part, block in enumerate(blocks(message['content'])):
            if 'cache_control' in block:
                marks.append(f"message[{'last' if at == last else at - last}][{part}]")
    marks += [f'tool:{tool["name"]}' for tool in body.get('tools', []) if 'cache_control' in tool]
    if 'cache_control' in body:
        marks.append('automatic')
    out['cache markers'] = ' '.join(marks) or 'none'
    out.pop('param:cache_control', None)
    return out


def facts(capture):
    requests = capture['requests']
    out = {'requests': str(len(requests)), 'side requests': str(len(capture['side']))}
    for index, request in enumerate(capture['side']):
        body = dict(request['body'], messages=[])
        out.update({f'side{index}.{key}': value for key, value in request_facts(body).items()})
        for at, message in enumerate(request['body'].get('messages', [])):
            for part, block in enumerate(blocks(message['content'])):
                out[f"side{index}.m{at}.{message['role']}[{part}]"] = short(block.get('text', block))
    first = requests[0]
    out['request'] = f"{first['method']} {first['path']}"
    for name, value in first['headers'].items():
        if name in ('host', 'content-length', 'connection', 'accept-encoding', 'content-encoding'):
            continue
        if name == 'anthropic-beta':
            value = ','.join(sorted(value.split(',')))
        out[f'header:{name}'] = value
    base = request_facts(first['body'])
    out.update(base)
    final = without_cache(requests[-1]['body'])
    for index, request in enumerate(requests[1:], 1):
        for key, value in request_facts(request['body']).items():
            if base.get(key) != value:
                out[f'r{index}.{key}'] = value
        for key in base.keys() - request_facts(request['body']).keys():
            out[f'r{index}.{key}'] = '<absent>'
        messages = without_cache(request['body']['messages'])
        if messages != final['messages'][:len(messages)]:
            changed = next(at for at, message in enumerate(messages) if message != final['messages'][at])
            out[f'r{index}.history'] = f'message {changed} changes later'
    for at, message in enumerate(final['messages']):
        for part, block in enumerate(blocks(message['content'])):
            key = f"m{at}.{message['role']}[{part}].{block.get('type')}"
            if block.get('type') == 'text':
                out[key] = 'prompt' if block['text'] == prompt else short(block['text'])
            elif block.get('type') == 'tool_use':
                out[key] = f"{block['name']} {short(list(block['input'].items()))}"
            elif block.get('type') == 'tool_result':
                rest = {k: v for k, v in block.items() if k not in ('type', 'tool_use_id', 'content')}
                out[key] = short(block.get('content'))
                if rest:
                    out[key + '.fields'] = json.dumps(rest, sort_keys=True)
            else:
                out[key] = short(block)
    return out


def compare(model, scratch):
    script = fixture['script'] if fixture['models'][model]['steps'] else []
    if args.claude:
        (scratch / 'claude').mkdir()
        command = [str(args.claude.resolve()), '-p', prompt, '--model', model, '--no-session-persistence',
                   '--output-format', 'json', '--setting-sources', '', '--settings', '{"skipWebFetchPreflight": true}',
                   '--strict-mcp-config', '--mcp-config', '{"mcpServers":{}}', '--dangerously-skip-permissions',
                   '--tools', tools]
        fixture['capture'][model] = capture(command, {
            'ANTHROPIC_API_KEY': 'sk-mock', 'ANTHROPIC_BASE_URL': '{base}', 'CLAUDE_CONFIG_DIR': '{home}/.claude',
            'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1', 'CLAUDE_CODE_DISABLE_AUTO_MEMORY': '1',
            'DISABLE_AUTOUPDATER': '1'}, scratch / 'claude', script)
    (scratch / 'nanocode').mkdir()
    command = [str(args.nanocode.resolve()), 'exec', '--ephemeral', '--no-tempo-tools', '--harness', 'claude',
               '-m', model, prompt]
    return capture(command, {'NANOCODE_ANTHROPIC_API_KEY': 'sk-mock', 'NANOCODE_ANTHROPIC_BASE_URL': '{base}/v1',
                             'NANOCODE_HOME': '{home}/.nanocode'}, scratch / 'nanocode', script)


if args.claude and hashlib.sha256(args.claude.resolve().read_bytes()).hexdigest() != manifest['executable_sha256']:
    raise SystemExit('this Claude Code differs from the pinned official executable')
upstream, local, ours = {}, {}, {}
for model in fixture['models']:
    scratch = Path(tempfile.mkdtemp())
    try:
        ours[model] = compare(model, scratch)
    finally:
        shutil.rmtree(scratch)
    skip = tuple(fixture['models'][model].get('skip', [])) or ('\0',)
    for side, found in ((upstream, facts(fixture['capture'][model])), (local, facts(ours[model]))):
        side.update({f'{model}: {key}': value for key, value in found.items() if not key.startswith(skip)})
if args.keep:
    args.keep.write_text(json.dumps(ours, indent=1, ensure_ascii=False) + '\n')
found = {key: {'claude': upstream.get(key, '<absent>'), 'nanocode': local.get(key, '<absent>')}
         for key in upstream.keys() | local.keys() if upstream.get(key) != local.get(key)}
documented = fixture['differences']
if args.record:
    fixture['differences'] = {key: {'reason': documented.get(key, {}).get('reason', 'TODO'), **found[key]}
                              for key in sorted(found)}
if args.claude or args.record:
    fixture_path.write_text(json.dumps(fixture, indent=1, ensure_ascii=False) + '\n')
drift = 0
for key in sorted(found):
    expected = documented.get(key)
    if expected is None:
        mark = '+'
    elif {k: expected.get(k) for k in ('claude', 'nanocode')} != found[key]:
        mark = '~'
    else:
        mark = ' '
    drift += mark != ' '
    print(f"{mark} {key}: claude={found[key]['claude']!r} nanocode={found[key]['nanocode']!r}")
    if mark == '~':
        print(f"    documented: claude={expected.get('claude')!r} nanocode={expected.get('nanocode')!r}")
for key in sorted(documented.keys() - found.keys()):
    drift += 1
    print(f'- {key}: documented but no longer differs')
print(f'{len(found)} differences, {drift} undocumented, changed or stale')
if drift and not args.record:
    raise SystemExit(1)
