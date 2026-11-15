#!/usr/bin/python3 -I
"""fake-dist-api.py <state dir> <bucket dir> <cert> <key> [port]

The provision e2e's ONE fake server (Phase 8 WP-R1): an HTTPS server that
answers for the dist bucket and the backend API by Host header, so the
vendored bootstrap golden runs UNMODIFIED (its PROD hostnames are pointed at
127.0.0.1 through /etc/hosts, the cert is signed by a test CA the container
trusts). Stdlib only.

  dist hosts (any Host listed in <state>/dist-hosts): GET /<key> serves
      <bucket>/<key>; anything else is 404 (S3 masks it as 403; curl -f fails
      either way).
  api hosts (<state>/api-hosts):
      POST /v1/device-registrations/<code>/claim
          The claim ASSERTION point: every claim is appended to
          <state>/claims.log as one JSON line {code, body}. The reply follows
          <state>/claim-mode (default ok): ok | ok-nohw | 409 | 404 | 500.
          A code may be claimed once (a second claim of it is 404, as the
          backend's pending -> claimed flip).
      GET /ca.pem   the Amazon root CA stand-in (the claim's caUrl).
Every request goes to <state>/requests.log (method host path status).
"""
import http.server
import json
import os
import re
import ssl
import sys
import threading

STATE, BUCKET, CERT, KEY = sys.argv[1:5]
PORT = int(sys.argv[5]) if len(sys.argv) > 5 else 443
LOCK = threading.Lock()


def read(name, default=""):
    try:
        with open(os.path.join(STATE, name)) as f:
            return f.read().strip()
    except OSError:
        return default


def hosts(name):
    return set(read(name).split())


def append(name, line):
    with LOCK, open(os.path.join(STATE, name), "a") as f:
        f.write(line + "\n")


HARDWARE = {
    "schemaVersion": 2,
    "platform": "rpi-cm5",
    "video": [
        {"connector": "csi0", "sensor": "imx290"},
        {"connector": "csi1", "sensor": "imx290"},
    ],
}


class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_a):
        pass

    def send(self, status, body, ctype="application/json"):
        data = body.encode() if isinstance(body, str) else body
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
        append("requests.log", f"{self.command} {self.host()} {self.path} {status}")

    def host(self):
        return (self.headers.get("Host") or "").split(":")[0]

    def do_GET(self):
        h = self.host()
        if h in hosts("dist-hosts"):
            key = self.path.lstrip("/")
            p = os.path.join(BUCKET, key)
            if "/" in key or key.startswith(".") or not os.path.isfile(p):
                return self.send(404, '{"error":"NoSuchKey"}')
            with open(p, "rb") as f:
                return self.send(200, f.read(), "application/octet-stream")
        if h in hosts("api-hosts") and self.path == "/ca.pem":
            with open(os.path.join(STATE, "root-ca.pem"), "rb") as f:
                return self.send(200, f.read(), "application/x-pem-file")
        self.send(404, '{"message":"not found"}')

    def do_POST(self):
        h = self.host()
        m = re.fullmatch(r"/v1/device-registrations/([A-Za-z0-9_-]+)/claim", self.path)
        if h not in hosts("api-hosts") or not m:
            return self.send(404, '{"message":"not found"}')
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n).decode("utf-8", "replace")
        try:
            body = json.loads(raw)
        except ValueError:
            body = {"_unparsed": raw}
        code = m.group(1)
        append("claims.log", json.dumps({"code": code, "body": body}, sort_keys=True))
        mode = read("claim-mode", "ok")
        used = set(read("used-codes").split())
        if mode == "409":
            return self.send(409, '{"message":"A device with this serial is already registered in the workspace; delete it first to re-provision."}')
        if mode == "404" or code in used:
            return self.send(404, '{"message":"Registration code not found, already used, or expired"}')
        if mode == "500":
            return self.send(500, '{"message":"internal"}')
        append("used-codes", code)
        did = "dev-" + code
        api = h
        resp = {
            "deviceId": did,
            "thingName": did,
            "dataEndpoint": "e2e-ats.iot.us-east-1.amazonaws.com",
            "certPem": "-----BEGIN CERTIFICATE-----\nE2E-CERT-" + code + "\n-----END CERTIFICATE-----\n",
            "privateKey": "-----BEGIN RSA PRIVATE KEY-----\nE2E-KEY-" + code + "\n-----END RSA PRIVATE KEY-----\n",
            "caUrl": f"https://{api}/ca.pem",
            "name": body.get("hostname"),
            "region": "us-east-1",
            "credentialEndpoint": "e2e.credentials.iot.us-east-1.amazonaws.com",
            "roleAlias": "e2e-role-alias",
            "channels": [{"streamKey": "cam0", "channelName": did + "-cam0"}],
            "uploadBucket": "e2e-upload-bucket",
            "modelId": "model-e2e",
            "modelName": "Argus CM5 Rev1",
            "needsModel": False,
            "hardware": None if mode == "ok-nohw" else HARDWARE,
        }
        self.send(200, json.dumps(resp))


srv = http.server.ThreadingHTTPServer(("127.0.0.1", PORT), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(CERT, KEY)
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
with open(os.path.join(STATE, "server.ready"), "w") as f:
    f.write(str(os.getpid()) + "\n")
srv.serve_forever()
