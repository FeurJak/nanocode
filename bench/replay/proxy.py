import base64, datetime, hashlib, hmac, http.client, json, os, struct, subprocess, sys, threading, time, urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REGION = os.environ.get("PROXY_REGION") or os.environ["AWS_REGION"]
HOST = f"bedrock-runtime.{REGION}.amazonaws.com"
PROFILE = os.environ.get("PROXY_PROFILE") or os.environ.get("AWS_PROFILE")
LOG = os.environ.get("PROXY_LOG") or os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../output/replay/proxy.jsonl")
LOCK = threading.Lock()
CREDS = {}


def credentials():
    with LOCK:
        expires = datetime.datetime.fromisoformat(CREDS["Expiration"]).timestamp() if CREDS.get("Expiration") else (float("inf") if CREDS else 0)
        if expires < time.time() + 600:
            profile = ["--profile", PROFILE] if PROFILE else []
            raw = subprocess.check_output(["aws", "configure", "export-credentials", *profile, "--format", "process"])
            CREDS.update(json.loads(raw))
        return dict(CREDS)


def mac(key, text):
    return hmac.new(key, text.encode(), hashlib.sha256).digest()


def signed(path, body):
    c = credentials()
    now = datetime.datetime.now(datetime.timezone.utc)
    amz, date = now.strftime("%Y%m%dT%H%M%SZ"), now.strftime("%Y%m%d")
    payload = hashlib.sha256(body).hexdigest()
    headers = [("host", HOST), ("x-amz-content-sha256", payload), ("x-amz-date", amz)]
    if c.get("SessionToken"):
        headers.append(("x-amz-security-token", c["SessionToken"]))
    names = ";".join(name for name, _ in headers)
    canonical_path = urllib.parse.quote(path, safe="/-_.~")
    canonical = "\n".join(["POST", canonical_path, ""] + [f"{n}:{v}" for n, v in headers] + ["", names, payload])
    scope = f"{date}/{REGION}/bedrock/aws4_request"
    text = "\n".join(["AWS4-HMAC-SHA256", amz, scope, hashlib.sha256(canonical.encode()).hexdigest()])
    key = mac(mac(mac(mac(("AWS4" + c["SecretAccessKey"]).encode(), date), REGION), "bedrock"), "aws4_request")
    signature = hmac.new(key, text.encode(), hashlib.sha256).hexdigest()
    out = dict(headers)
    out["authorization"] = f"AWS4-HMAC-SHA256 Credential={c['AccessKeyId']}/{scope}, SignedHeaders={names}, Signature={signature}"
    out["content-type"] = "application/json"
    return out


def events(buffer):
    found = []
    while len(buffer) >= 12:
        total, header_bytes = struct.unpack(">II", buffer[:8])
        if len(buffer) < total:
            break
        frame, buffer = buffer[:total], buffer[total:]
        payload = frame[12 + header_bytes:total - 4]
        try:
            found.append(json.loads(base64.b64decode(json.loads(payload)["bytes"])))
        except (KeyError, ValueError):
            found.append({"type": "frame", "raw": payload[:200].decode(errors="replace")})
    return found, buffer


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_POST(self):
        start = time.time()
        body = self.rfile.read(int(self.headers.get("content-length", "0")))
        upstream = http.client.HTTPSConnection(HOST, timeout=600)
        upstream.request("POST", self.path, body=body, headers=signed(self.path, body))
        response = upstream.getresponse()
        self.send_response(response.status)
        for name, value in response.getheaders():
            if name.lower() not in ("transfer-encoding", "content-length", "connection"):
                self.send_header(name, value)
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()
        first, buffer, usage, raw = None, b"", {}, b""
        while chunk := response.read1(65536):
            first = first or time.time() - start
            self.wfile.write(b"%x\r\n" % len(chunk) + chunk + b"\r\n")
            self.wfile.flush()
            if response.status != 200 or "json" in (response.getheader("content-type") or ""):
                raw += chunk
                continue
            found, buffer = events(buffer + chunk)
            for event in found:
                if event.get("type") == "message_start":
                    usage.update(event["message"].get("usage", {}))
                if event.get("type") == "message_delta":
                    usage.update(event.get("usage", {}))
        self.wfile.write(b"0\r\n\r\n")
        if response.status == 200 and raw:
            usage = json.loads(raw).get("usage", {})
            raw = b""
        request = json.loads(body)
        record = {"at": round(start, 3), "path": self.path, "status": response.status, "first_byte_s": round(first or 0, 3),
                  "total_s": round(time.time() - start, 3), "usage": usage, "error": raw.decode(errors="replace")[:500],
                  "cache_control": request.get("cache_control"),
                  "system_sha256": hashlib.sha256(json.dumps(request.get("system"), sort_keys=True).encode()).hexdigest(),
                  "tools_sha256": hashlib.sha256(json.dumps(request.get("tools"), sort_keys=True).encode()).hexdigest(),
                  "tool_names": [tool.get("name") for tool in request.get("tools", [])],
                  "messages": len(request.get("messages", [])), "body": request}
        with LOCK, open(LOG, "a") as log:
            log.write(json.dumps(record) + "\n")


os.makedirs(os.path.dirname(os.path.abspath(LOG)), exist_ok=True)
ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 18601), Handler).serve_forever()
