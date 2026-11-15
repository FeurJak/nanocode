import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import threading

root = Path(__file__).resolve().parents[1]
fixture_path = root / 'crates/backends/codex/upstream/request.json'
parser = argparse.ArgumentParser()
parser.add_argument('--nanocode', type=Path, default=root / 'target/debug/nanocode')
parser.add_argument('--codex', type=Path)
parser.add_argument('--keep', type=Path)
args = parser.parse_args()
fixture = json.loads(fixture_path.read_text())
prompt = fixture['prompt']
model = fixture['model']
uuid = re.compile(r'[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}')


class Mock(http.server.ThreadingHTTPServer):
    def __init__(self):
        super().__init__(('127.0.0.1', 0), Handler)
        self.requests = []


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def reply(self, status, kind, data):
        self.send_response(status)
        self.send_header('content-type', kind)
        self.send_header('content-length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self.reply(200, 'application/json', json.dumps({'object': 'list', 'data': [{'id': model, 'object': 'model'}]}).encode())

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('content-length') or 0))
        if self.headers.get('content-encoding') == 'zstd':
            body = subprocess.run(['zstd', '-dc'], input=body, capture_output=True, check=True).stdout
        self.server.requests.append({
            'method': self.command,
            'path': self.path,
            'headers': {name.lower(): value for name, value in self.headers.items() if name.lower() != 'authorization'},
            'body': json.loads(body),
        })
        message = {'type': 'message', 'id': 'msg_mock', 'role': 'assistant', 'status': 'completed',
                   'content': [{'type': 'output_text', 'text': 'done', 'annotations': []}]}
        response = {'id': 'resp_mock', 'object': 'response', 'status': 'completed', 'output': [message],
                    'usage': {'input_tokens': 1, 'output_tokens': 1, 'total_tokens': 2,
                              'input_tokens_details': {'cached_tokens': 0},
                              'output_tokens_details': {'reasoning_tokens': 0}}}
        events = [
            ('response.created', {'response': dict(response, status='in_progress', output=[])}),
            ('response.output_item.added', {'output_index': 0, 'item': dict(message, status='in_progress', content=[])}),
            ('response.output_text.delta', {'output_index': 0, 'content_index': 0, 'item_id': 'msg_mock', 'delta': 'done'}),
            ('response.output_item.done', {'output_index': 0, 'item': message}),
            ('response.completed', {'response': response}),
        ]
        data = ''.join(f'event: {kind}\ndata: {json.dumps(dict(event, type=kind))}\n\n' for kind, event in events)
        self.reply(200, 'text/event-stream', data.encode())


def capture(command, env, scratch):
    mock = Mock()
    threading.Thread(target=mock.serve_forever, daemon=True).start()
    base = f'http://127.0.0.1:{mock.server_address[1]}/v1'
    home = scratch / 'home'
    work = scratch / 'work'
    home.mkdir()
    work.mkdir()
    env = {key: value.replace('{base}', base).replace('{home}', str(home)) for key, value in env.items()}
    full = dict(PATH=os.environ['PATH'], HOME=str(home), TERM='dumb', **env)
    if 'CODEX_HOME' in env:
        (home / '.codex').mkdir()
        (home / '.codex/config.toml').write_text(
            'model_provider = "mock"\n[model_providers.mock]\nname = "mock"\n'
            f'base_url = "{base}"\nenv_key = "MOCK_KEY"\nwire_api = "responses"\n')
    run = subprocess.run(command, cwd=work, env=full, capture_output=True, text=True, timeout=120)
    mock.shutdown()
    if not mock.requests:
        raise SystemExit(f'{command[0]} sent no request (exit {run.returncode}):\n{run.stdout}\n{run.stderr}')
    return scrub(mock.requests[0], [(str(work), '<cwd>'), (str(home), '<home>')])


def scrub(value, paths):
    if isinstance(value, dict):
        return {key: scrub(item, paths) for key, item in value.items()}
    if isinstance(value, list):
        return [scrub(item, paths) for item in value]
    if isinstance(value, str):
        for path, name in paths:
            value = value.replace(path, name)
        return uuid.sub('<uuid>', value)
    return value


def digest(value):
    text = value if isinstance(value, str) else json.dumps(value, sort_keys=True)
    return hashlib.sha256(text.encode()).hexdigest()[:12]


def tag(text):
    first = text.lstrip().split('\n', 1)[0]
    match = re.match(r'<([a-z_ ]+)>', first)
    return match.group(1) if match else first[:40]


def facts(request):
    out = {'request': f"{request['method']} {request['path']}"}
    for name, value in request['headers'].items():
        if name in ('host', 'content-length'):
            continue
        if name == 'x-codex-turn-metadata':
            value = ' '.join(sorted(json.loads(value)))
        out[f'header:{name}'] = value
    body = request['body']
    for key, value in body.items():
        if key in ('input', 'tools'):
            continue
        if key == 'client_metadata':
            for name, item in json.loads(value.get('x-codex-turn-metadata') or '{}').items():
                if name in ('installation_id', 'session_id', 'thread_id', 'turn_id', 'root_turn_id',
                            'window_id', 'context_window_id', 'turn_started_at_unix_ms'):
                    item = 'present'
                elif isinstance(item, (dict, list)):
                    item = digest(item)
                out[f'turn-metadata:{name}'] = json.dumps(item, sort_keys=True)
            value = sorted(value)
        if key == 'prompt_cache_key':
            value = 'present'
        out[f'param:{key}'] = json.dumps(value, sort_keys=True)
    out['tier'] = body.get('service_tier') or 'none'
    tools = list(body.get('tools') or [])
    for item in body.get('input', []):
        if item.get('type') == 'additional_tools':
            tools += item['tools']
    for tool in tools:
        if tool.get('type') == 'namespace':
            out[f"namespace:{tool['name']}"] = tool.get('description', '')
            for child in tool['tools']:
                out[f"tool:{tool['name']}.{child['name']}"] = digest(child)
        else:
            out[f"tool:{tool.get('name') or tool.get('type')}"] = digest(tool)
    for item in body.get('input', []):
        if item.get('type') == 'additional_tools':
            out['input:additional_tools'] = item['role']
            continue
        for part in item.get('content', []):
            text = part.get('text', '')
            name = 'prompt' if text == prompt else tag(text)
            out[f"input:{item.get('role')}:{name}"] = digest(text)
    return out


scratch = Path(tempfile.mkdtemp())
try:
    if args.codex:
        (scratch / 'codex').mkdir()
        command = [str(args.codex.resolve()), 'exec', '--json', '--dangerously-bypass-approvals-and-sandbox',
                   '--skip-git-repo-check', '-m', model, '-c', 'model_reasoning_effort=high', prompt]
        fixture['request'] = capture(command, {'CODEX_HOME': '{home}/.codex', 'MOCK_KEY': 'sk-mock'}, scratch / 'codex')
        fixture_path.write_text(json.dumps(fixture, indent=1, sort_keys=True) + '\n')
    (scratch / 'nanocode').mkdir()
    command = [str(args.nanocode.resolve()), 'exec', '--ephemeral', '--codex-orchestration', 'code-mode',
               '-m', model, '-t', 'high', prompt]
    ours = capture(command, {'NANOCODE_OPENAI_BASE_URL': '{base}', 'OPENAI_API_KEY': 'sk-mock'}, scratch / 'nanocode')
finally:
    shutil.rmtree(scratch)
if args.keep:
    args.keep.write_text(json.dumps(ours, indent=1, sort_keys=True) + '\n')
upstream = facts(fixture['request'])
local = facts(ours)
found = {key for key in upstream.keys() | local.keys() if upstream.get(key) != local.get(key)}
documented = fixture['differences']
for key in sorted(found):
    mark = ' ' if key in documented else '+'
    print(f'{mark} {key}: codex={upstream.get(key)!r} nanocode={local.get(key)!r}')
for key in sorted(documented.keys() - found):
    print(f'- {key}: documented but no longer differs')
print(f'{len(found)} differences, {len(found - documented.keys())} undocumented, {len(documented.keys() - found)} stale')
if found != documented.keys():
    raise SystemExit(1)
