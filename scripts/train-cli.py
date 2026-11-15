#!/usr/bin/env python3
import fcntl
import http.server
import json
import os
import pty
import re
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

REPLY = """## Plan

The **workspace** has a few files; here is what each one does:

1. @notes.txt@ holds the *notes*
2. @main.rs@ prints a greeting
   - nested item with a [link](https://example.com)

| file | lines |
| --- | --- |
| notes.txt | 2 |
| main.rs | 3 |

~~~rust
fn main() {
    println!("hello");
}
~~~

> Quoted text, then a final word: zebracorn
""".replace("@", chr(96))
SENTINEL = b"zebracorn"
SHELL_STEPS = re.compile(r"steps=(\d+)")
WINDOW = (40, 120)
MEMORY_AFTER_FIRST_FRAME = 1.0


def user_text(item):
    content = item.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(part.get("text", "") for part in content if isinstance(part, dict))
    return ""


def next_action(body):
    items = body.get("input") or []
    if isinstance(items, str):
        items = [{"role": "user", "content": items}]
    last_user = max((i for i, item in enumerate(items) if item.get("role") == "user"), default=-1)
    wanted = SHELL_STEPS.search(user_text(items[last_user])) if last_user >= 0 else None
    done = sum(1 for item in items[last_user + 1:] if str(item.get("type", "")).endswith("call_output"))
    offered = [*(body.get("tools") or []), *(tool for item in items if item.get("type") == "additional_tools" for tool in item.get("tools") or [])]
    while any(tool.get("type") == "namespace" for tool in offered):
        offered = [nested for tool in offered for nested in (tool.get("tools") or [] if tool.get("type") == "namespace" else [tool])]
    tools = {(tool.get("type"), tool.get("name")) for tool in offered}
    if wanted and done < int(wanted.group(1)):
        if ("function", "exec_command") in tools:
            return "function_call", "exec_command", json.dumps({"cmd": "ls"})
        if ("custom", "exec") in tools:
            return "custom_tool_call", "exec", "const r = await tools.exec_command({ cmd: 'ls' }); text(r.output ?? JSON.stringify(r))"
    return "message", None, REPLY if tools else "Workspace notes"


class Responses(http.server.BaseHTTPRequestHandler):
    # HTTP/1.1 with chunked bodies on a kept-alive connection, as provider APIs and the benchmark's server answer:
    # an HTTP/1.0 close-delimited reply leaves the chunked and keep-alive paths untrained and in .text.cold.
    protocol_version = "HTTP/1.1"
    count = 0

    def log_message(self, *args):
        pass

    def do_GET(self):
        self.send_error(404)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
        if not self.path.endswith("/responses"):
            self.send_error(404)
            return
        Responses.count += 1
        rid, msg = "resp_%d" % Responses.count, "msg_%d" % Responses.count
        usage = {"input_tokens": 10, "input_tokens_details": {"cached_tokens": 0}, "output_tokens": 1, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 11}
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("transfer-encoding", "chunked")
        self.send_header("connection", "keep-alive")
        self.end_headers()

        def send(event):
            frame = ("event: %s\ndata: %s\n\n" % (event["type"], json.dumps(event))).encode()
            self.wfile.write(b"%x\r\n%s\r\n" % (len(frame), frame))
            self.wfile.flush()

        send({"type": "response.created", "response": {"id": rid, "status": "in_progress", "output": []}})
        kind, name, payload = next_action(body)
        if kind == "message":
            send({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": msg, "role": "assistant", "status": "in_progress", "content": []}})
            send({"type": "response.content_part.added", "item_id": msg, "output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}})
            for seq, word in enumerate(re.findall(r"\S*\s*", payload)):
                if word:
                    send({"type": "response.output_text.delta", "item_id": msg, "output_index": 0, "content_index": 0, "delta": word, "sequence_number": seq})
            part = {"type": "output_text", "text": payload, "annotations": []}
            item = {"type": "message", "id": msg, "role": "assistant", "status": "completed", "content": [part]}
            send({"type": "response.output_text.done", "item_id": msg, "output_index": 0, "content_index": 0, "text": payload})
            send({"type": "response.content_part.done", "item_id": msg, "output_index": 0, "content_index": 0, "part": part})
        else:
            field, delta = ("arguments", "response.function_call_arguments.delta") if kind == "function_call" else ("input", "response.custom_tool_call_input.delta")
            item = {"type": kind, "id": "call_%d" % Responses.count, "call_id": "call_%d" % Responses.count, "name": name, field: payload, "status": "completed"}
            send({"type": "response.output_item.added", "output_index": 0, "item": {**item, field: "", "status": "in_progress"}})
            send({"type": delta, "item_id": item["id"], "output_index": 0, "delta": payload})
        send({"type": "response.output_item.done", "output_index": 0, "item": item})
        send({"type": "response.completed", "response": {"id": rid, "status": "completed", "output": [item], "usage": usage}})
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()


def flavor(binary):
    version = subprocess.run([binary, "--version"], capture_output=True, text=True, check=True).stdout
    return "lite" if version.startswith("nc-lite") else "nanocode"


def interactive(command, env, cwd, prompt, keys, leave):
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.execvpe(command[0], command, env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", WINDOW[0], WINDOW[1], 0, 0))
    seen = bytearray()

    def pump(timeout):
        ready, _, _ = select.select([fd], [], [], timeout)
        if not ready:
            return True
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            return False
        seen.extend(chunk)
        return bool(chunk)

    def settle(deadline):
        end = time.monotonic() + deadline
        last = (len(seen), time.monotonic())
        while time.monotonic() < end and pump(0.1):
            if len(seen) != last[0]:
                last = (len(seen), time.monotonic())
            elif seen and time.monotonic() - last[1] > 0.5:
                return

    def wait_for(needle, start, deadline):
        end = time.monotonic() + deadline
        while needle not in seen[start:] and time.monotonic() < end and pump(0.1):
            pass

    settle(10)
    if prompt is None:
        end = time.monotonic() + MEMORY_AFTER_FIRST_FRAME
        while time.monotonic() < end and pump(0.1):
            pass
    else:
        for ch in prompt:
            os.write(fd, ch.encode())
        start = len(seen)
        os.write(fd, b"\r")
        wait_for(SENTINEL, start, 60)
        settle(2)
    for key in keys:
        os.write(fd, key)
        settle(1)
    for key in leave:
        os.write(fd, key)
        pump(0.1)
    end = time.monotonic() + 5
    while time.monotonic() < end:
        done, _ = os.waitpid(pid, os.WNOHANG)
        if done:
            os.close(fd)
            return
        pump(0.05)
    os.kill(pid, signal.SIGTERM)
    os.waitpid(pid, 0)
    os.close(fd)
    sys.exit("%s did not exit on its quit keys" % command[0])


def offline():
    # Train with only loopback reachable, as the benchmark runs: the Tempo MCP servers a session connects to at
    # startup then fail at once on every host, so the profile no longer depends on whether the build host reaches
    # them, and the paths a session runs when they fail are trained instead of left in .text.cold. An unprivileged
    # user and network namespace that keeps our uid; where the kernel refuses one, train online as before.
    uid, gid = os.getuid(), os.getgid()
    try:
        os.unshare(os.CLONE_NEWUSER | os.CLONE_NEWNET)
    except (AttributeError, OSError) as error:
        print("training online: no network namespace (%s)" % error, file=sys.stderr)
        return
    for name, value in (("setgroups", "deny"), ("uid_map", "%d %d 1" % (uid, uid)), ("gid_map", "%d %d 1" % (gid, gid))):
        with open("/proc/self/" + name, "w") as f:
            f.write(value)
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        request = struct.pack("16sH14x", b"lo", 0)
        flags = struct.unpack("16sH14x", fcntl.ioctl(s, 0x8913, request))[1]  # SIOCGIFFLAGS
        fcntl.ioctl(s, 0x8914, struct.pack("16sH14x", b"lo", flags | 1))  # SIOCSIFFLAGS, IFF_UP


def main():
    binary = os.path.abspath(sys.argv[1])
    if sys.platform.startswith("linux"):
        offline()
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Responses)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = "http://127.0.0.1:%d/v1" % server.server_address[1]
    scratch = tempfile.mkdtemp()
    work = os.path.join(scratch, "work")
    os.makedirs(work)
    with open(os.path.join(work, "notes.txt"), "w") as notes:
        notes.write("first\nsecond\n")
    with open(os.path.join(work, "main.rs"), "w") as main_rs:
        main_rs.write('fn main() {\n    println!("hello");\n}\n')
    env = {key: value for key, value in os.environ.items() if key in ("PATH", "LANG", "LLVM_PROFILE_FILE")}
    env.update({"HOME": scratch, "TERM": "xterm-256color", "AI_GATEWAY_API_KEY": "training", "NANOCODE_TELEMETRY": "off"})
    if flavor(binary) == "lite":
        home = os.path.join(scratch, ".nc-lite")
        session = ["--home", home, "--base-url", base, "-m", "luna"]
        leave = [b"\x04"]
    else:
        home = os.path.join(scratch, ".nanocode")
        session = ["--home", home, "--gateway", "vercel", "--gateway-base-url", base, "-m", "luna"]
        leave = [b"\x03", b"\x03"]
    if sys.argv[2:] == ["startup"]:
        interactive([binary, *session], env, work, None, [], leave)
        interactive([binary, *session, "--ephemeral"], env, work, None, [], leave)
        server.shutdown()
        shutil.rmtree(scratch)
        print("trained %s to its first frame" % binary)
        return
    if sys.argv[2:] == ["oneshot"]:
        subprocess.run([binary, "exec", *session, "--ephemeral", "Say hello steps=0"], env=env, cwd=work, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, check=True)
        server.shutdown()
        shutil.rmtree(scratch)
        print("trained %s with a one-shot prompt" % binary)
        return
    if sys.argv[2:] in (["tools"], ["codemode"]):
        # The bench's ten steps, three times: whether the store groups a step's writes depends on timing. codemode runs
        # them as Code Mode cells (exec), the opt-in orchestration, which the bench's nc tool lane still sends; nc-lite has none.
        orchestration = ["--codex-orchestration", "code-mode"] if sys.argv[2] == "codemode" and flavor(binary) != "lite" else []
        for _ in range(3):
            subprocess.run([binary, "exec", *session, *orchestration, "--ephemeral", "List the files steps=10"], env=env, cwd=work, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, check=True)
        server.shutdown()
        shutil.rmtree(scratch)
        print("trained %s with a %s turn" % (binary, "Code Mode tool" if orchestration else "tool"))
        return
    runs = [
        [binary, "--version"],
        [binary, "--help"],
        [binary, "exec", *session, "--ephemeral", "Say hello steps=0"],
        [binary, "exec", *session, "List the files steps=3"],
        [binary, "exec", *session, "--continue", "And again steps=1"],
        [binary, "sessions", "--home", home],
    ]
    for run in runs:
        subprocess.run(run, env=env, cwd=work, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, check=True)
    keys = [b"draft words", b"\x17", b"\x01", b"\x05", b"\x15", b"\x1b[A", b"\x1b[B", b"\x15"]
    interactive([binary, *session], env, work, "List the files steps=2", keys, leave)
    interactive([binary, *session, "--continue"], env, work, "Summarize steps=0", keys, leave)
    server.shutdown()
    shutil.rmtree(scratch)
    print("trained %s with %d model requests" % (binary, Responses.count))


main()
