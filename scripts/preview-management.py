"""Read-only UI preview: latest catalog, independently observed installation status."""
import json
import socket
from pathlib import Path
from urllib.request import urlopen
from urllib.parse import unquote, urlsplit
from http.server import ThreadingHTTPServer, SimpleHTTPRequestHandler

ROOT = Path(__file__).resolve().parents[1] / "dist"
ASSETS = json.loads((ROOT.parent / "manifests/management-assets.json").read_text(encoding="utf-8"))
CATALOG = "http://127.0.0.1:48021"
STATUS = "http://127.0.0.1:8769"

def read_json(base, path):
    with urlopen(base + path, timeout=3) as response:
        return json.load(response)

def snapshot():
    latest = read_json(CATALOG, "/api/management")
    try:
        observed = read_json(STATUS, "/api/management")
    except OSError:
        observed = {}
    previous = {m["id"]: m for m in observed.get("models", [])}
    models = []
    for definition in latest["models"]:
        old = previous.get(definition["id"])
        if old:
            row = dict(old)
            for key in ("id", "engine", "engineLabel", "label", "loadPolicy", "supported", "supportStatus"):
                if key in definition:
                    row[key] = definition[key]
        else:
            row = dict(definition)
            row.update(availability="unknown", managementState="preview_unknown",
                       modelInstalled=None, runtimeReady=None, runtimeInstallable=False, missingRuntimes=[],
                       available=False, installable=False, loadable=False,
                       previewStatus="unverified", statusMessage="実サーバーでの導入状態は未確認です。")
        if definition.get("managementState") == "disabled_by_policy":
            row.update(managementState="disabled_by_policy", statusMessage=definition["statusMessage"],
                       installable=False, loadable=False, available=False, missingRuntimes=[])
        models.append(row)
    if len({m["id"] for m in models}) != len(models):
        raise ValueError("モデルIDが重複しています")
    latest.update(models=models, tools=observed.get("tools", {}), toolsStatus="confirmed" if observed.get("tools") else "unknown", loaded=observed.get("loaded", []),
                  busy=True, status={"running": [], "queued": {}},
                  operation={"state": "complete", "message": "UIプレビュー（表示専用・操作は無効）"},
                  network={"urls": [], "authMode": "local"}, restartRequired=False, restartBlocked=True)
    return latest

class Preview(SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(ROOT), **kwargs)
    def log_message(self, *args):
        pass
    def send_json(self, value, status=200):
        body=json.dumps(value, ensure_ascii=False).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_GET(self):
        try:
            if self.path == "/api/health":
                h=read_json(CATALOG, "/api/health")
                version=json.loads((ROOT.parent / "package.json").read_text(encoding="utf-8"))["version"]
                return self.send_json({"serverVersion":version, "authMode":"local", "preview":True, "capabilities":{}})
            if self.path == "/api/management":
                return self.send_json(snapshot())
            if self.path == "/api/updates":
                version=json.loads((ROOT.parent / "package.json").read_text(encoding="utf-8"))["version"]
                return self.send_json({"currentVersion":version,"state":"preview","installed":False,"credentialConfigured":False,"available":None,"message":"表示専用プレビューでは更新操作は無効です。"})
        except (OSError, ValueError, KeyError):
            return self.send_json({"error":"最新版のモデル一覧を取得できません。旧一覧への代替表示は行いません。"}, 503)
        if self.path.startswith("/api/"):
            return self.send_json({"error":"表示専用です"}, 405)
        path=unquote(urlsplit(self.path).path)
        asset="management.html" if path in ("/", "/index.html") else path.removeprefix("/")
        if asset not in ASSETS:
            return self.send_error(404)
        self.path="/"+asset
        super().do_GET()
    def do_POST(self):
        self.send_json({"error":"表示専用です。サーバー設定は変更しません。"}, 405)
    def end_headers(self):
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

class PreviewServer(ThreadingHTTPServer):
    allow_reuse_address = False
    def server_bind(self):
        if hasattr(socket, "SO_EXCLUSIVEADDRUSE"):
            self.socket.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
        super().server_bind()

if __name__ == "__main__":
    print("http://127.0.0.1:48010/", flush=True)
    PreviewServer(("127.0.0.1", 48010), Preview).serve_forever()
