import http.server, json, os, pty, select, signal, statistics, struct, subprocess, sys, tempfile, threading, time, fcntl, termios

ROOT = os.getcwd()
BIN = os.path.join(os.environ.get("CARGO_TARGET_DIR", os.path.join(ROOT, "target")), "release")
RUNS = int(os.environ.get("NANOCODE_UPHILL_RUNS", "9"))
REPLY = "zqxj-reply"


class Model(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        self.reply("application/json", json.dumps({"data": []}).encode())

    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length") or 0))
        events = [
            {"type": "message_start", "message": {"id": "msg_uphill", "type": "message", "role": "assistant", "model": "claude-sonnet-5-5", "content": [], "stop_reason": None, "stop_sequence": None, "usage": {"input_tokens": 1, "output_tokens": 1}}},
            {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
            {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": REPLY}},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 1}},
            {"type": "message_stop"},
        ]
        self.reply("text/event-stream", "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events).encode())

    def reply(self, kind, body):
        self.send_response(200)
        self.send_header("content-type", kind)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def build():
    subprocess.run(["cargo", "build", "--locked", "--release", "-p", "nanocode-cli", "--bin", "nanocode", "--bin", "nanocode-exec"], check=True, stdout=sys.stderr)


def environment(port):
    home = tempfile.mkdtemp(prefix="nanocode-uphill-")
    return home, dict(
        {k: v for k, v in os.environ.items() if not k.startswith(("NANOCODE_", "ANTHROPIC_", "OPENAI_"))},
        HOME=home,
        NANOCODE_HOME=home,
        NANOCODE_ANTHROPIC_BASE_URL=f"http://127.0.0.1:{port}/v1",
        NANOCODE_ANTHROPIC_API_KEY="uphill",
        NANOCODE_MODEL="claude-sonnet-5-5",
        TERM="xterm-256color",
    )


def exec_once(port):
    home, env = environment(port)
    start = time.perf_counter_ns()
    child = subprocess.Popen([os.path.join(BIN, "nanocode-exec"), "exec", "say hi"], cwd=home, env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    seen = b""
    while REPLY.encode() not in seen:
        chunk = os.read(child.stdout.fileno(), 65536)
        if not chunk:
            raise SystemExit(f"nanocode-exec exited before its reply: {seen!r}")
        seen += chunk
    first = time.perf_counter_ns() - start
    while os.read(child.stdout.fileno(), 65536):
        pass
    child.stdout.close()
    _, status, usage = os.wait4(child.pid, 0)
    total = time.perf_counter_ns() - start
    child.returncode = os.waitstatus_to_exitcode(status)
    if child.returncode != 0:
        raise SystemExit(f"nanocode-exec exited {child.returncode}")
    return first, total, usage.ru_maxrss * 1024


def read_until(fd, marker, deadline, seen):
    while marker not in seen:
        left = deadline - time.monotonic()
        if left <= 0:
            raise SystemExit(f"nanocode TUI never drew {marker!r}: {seen[-400:]!r}")
        ready, _, _ = select.select([fd], [], [], left)
        if ready:
            seen += os.read(fd, 65536)
    return seen


def rss(pid):
    with open(f"/proc/{pid}/status") as status:
        for line in status:
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
    raise SystemExit("no VmRSS")


def tui_once(port):
    home, env = environment(port)
    start = time.perf_counter_ns()
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(home)
        path = os.path.join(BIN, "nanocode")
        os.execve(path, [path], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    deadline = time.monotonic() + 30
    seen = read_until(fd, b"\x1b[?2026l", deadline, b"")
    frame = time.perf_counter_ns() - start
    seen = read_until(fd, b"new session", deadline, seen)
    seen = read_until(fd, b"\x1b[?2026l", deadline, seen[seen.index(b"new session"):])
    ready = time.perf_counter_ns() - start
    idle = rss(pid)
    os.write(fd, b"hi\r")
    sent = time.perf_counter_ns()
    read_until(fd, REPLY.encode(), deadline, b"")
    token = time.perf_counter_ns() - sent
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
    os.close(fd)
    return frame, ready, token, idle


def version_once():
    start = time.perf_counter_ns()
    subprocess.run([os.path.join(BIN, "nanocode-exec"), "--version"], check=True, stdout=subprocess.DEVNULL)
    return time.perf_counter_ns() - start


def median(values):
    return statistics.median(values)


def main():
    build()
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Model)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    port = server.server_address[1]
    exec_once(port)
    tui_once(port)
    version = [version_once() for _ in range(RUNS)]
    execs = [exec_once(port) for _ in range(RUNS)]
    tuis = [tui_once(port) for _ in range(RUNS)]
    server.shutdown()
    lines = [
        f"startup {median(version) / 1e3:.1f}us",
        f"tui-first-frame {median(t[0] for t in tuis) / 1e3:.1f}us",
        f"tui-ready {median(t[1] for t in tuis) / 1e3:.1f}us",
        f"tui-ttft {median(t[2] for t in tuis) / 1e3:.1f}us",
        f"tui-rss {int(median(t[3] for t in tuis))}B",
        f"exec-ttft {median(e[0] for e in execs) / 1e3:.1f}us",
        f"exec-turn {median(e[1] for e in execs) / 1e3:.1f}us",
        f"exec-peak-rss {int(median(e[2] for e in execs))}B",
        f"cli-size nanocode {os.path.getsize(os.path.join(BIN, 'nanocode'))}B",
        f"cli-size nanocode-exec {os.path.getsize(os.path.join(BIN, 'nanocode-exec'))}B",
    ]
    with open(os.environ["UPHILL_METRICS"], "a") as metrics:
        metrics.write("".join(line + "\n" for line in lines))
    print("\n".join(lines), file=sys.stderr)


main()
