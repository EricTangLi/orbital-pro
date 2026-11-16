# -*- coding: utf-8 -*-
"""T-M3-06 注入冒烟：真起 core 只读端点 + proxy + 假上游，验证注入**真的发生**。

判据：
  1. proxy /health 显示 inject=on
  2. POST /v1/chat/completions → 响应头 X-Orbital-Inject 含 on(（经过假上游）
  3. 假上游**实际收到的 body** 里出现"长期记忆"段（这是"注入真的发生"的硬证据，不是只看响应头）
  4. 统计文件出现 record（status=on）
  5. core 不可达时 → X-Orbital-Inject: skipped(...)，上游仍收到原请求（不阻塞）

用法：python scripts/smoke-inject.py <core_exe> <proxy_exe> <fixture_db>
"""
import http.client
import http.server
import json
import os
import subprocess
import sys
import tempfile
import threading
import time

CORE = sys.argv[1] if len(sys.argv) > 1 else r"target\debug\examples\recall-server.exe"
PROXY = sys.argv[2] if len(sys.argv) > 2 else r"target\debug\orbital-proxy.exe"
DB = sys.argv[3] if len(sys.argv) > 3 else os.path.join(tempfile.gettempdir(), "orbital-t306.db")

PROXY_PORT = 8790
results = []


def check(name, ok, detail=""):
    results.append((name, ok))
    print(("PASS " if ok else "FAIL ") + name + (("  -> " + str(detail)) if not ok else ""))


class Upstream(http.server.BaseHTTPRequestHandler):
    received = []

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(n).decode("utf-8", "replace")
        Upstream.received.append(body)
        resp = json.dumps({"ok": True}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(resp)))
        self.end_headers()
        self.wfile.write(resp)

    def log_message(self, *a):
        pass


def wait_port(port, timeout=8.0):
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            c = http.client.HTTPConnection("127.0.0.1", port, timeout=1)
            c.request("GET", "/health")
            c.getresponse().read()
            c.close()
            return True
        except Exception:
            time.sleep(0.15)
    return False


def post_json(port, path, payload):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    body = json.dumps(payload).encode()
    c.request("POST", path, body=body, headers={"Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    hdrs = dict(r.getheaders())
    c.close()
    return r.status, hdrs, data


def main():
    tmp = tempfile.mkdtemp(prefix="orbital-inject-")
    metrics_dir = os.path.join(tmp, "metrics")
    os.makedirs(metrics_dir, exist_ok=True)

    # ADR-042：proxy 只注入 shared 记忆 → 把 fixture 里含 Hermes 的条目标成 shared
    import sqlite3

    con = sqlite3.connect(DB)
    n_shared = con.execute(
        "UPDATE memories SET scope='shared' WHERE content LIKE '%Hermes%' AND deleted_at IS NULL"
    ).rowcount
    con.commit()
    con.close()
    check("fixture 已标记 shared 记忆", n_shared > 0, n_shared)

    # 1) 假上游
    up = http.server.HTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=up.serve_forever, daemon=True).start()
    up_port = up.server_address[1]

    # 2) core 只读端点（example）
    env_core = dict(os.environ)
    env_core["ORBITAL_SECRETS"] = os.path.join(tmp, "no-secrets.json")
    core = subprocess.Popen(
        [CORE, DB, "127.0.0.1:0"],
        env=env_core,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
    )
    core_port = None
    deadline = time.time() + 15
    while time.time() < deadline:
        line = core.stdout.readline()
        if not line:
            break
        if "RECALL_SERVER_READY port=" in line:
            core_port = int(line.strip().split("=")[1])
            break
    check("core 只读端点已启动", core_port is not None, core.stderr.read() if core_port is None else "")

    # 3) proxy（指向假上游）
    env_proxy = dict(os.environ)
    env_proxy["ORBITAL_UPSTREAM"] = f"http://127.0.0.1:{up_port}"
    env_proxy["ORBITAL_INJECT"] = "1"
    env_proxy["ORBITAL_CORE_URL"] = f"http://127.0.0.1:{core_port}"
    env_proxy["ORBITAL_PROXY_USE_SYSTEM_PROXY"] = "0"
    env_proxy["LOCALAPPDATA"] = tmp  # 统计落到临时目录
    proxy = subprocess.Popen(
        [PROXY],
        env=env_proxy,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
    )
    ok_health = wait_port(PROXY_PORT)
    check("proxy 已启动", ok_health)
    if ok_health:
        try:
            status, hdrs, data = post_json(
                PROXY_PORT,
                "/v1/chat/completions",
                {"model": "x", "messages": [{"role": "user", "content": "Hermes 审计 排障"}]},
            )
        except Exception as e:
            print("POST 失败:", e)
            try:
                proxy.terminate()
                out, err = proxy.communicate(timeout=5)
                print("PROXY STDOUT:", (out or "")[-1500:])
                print("PROXY STDERR:", (err or "")[-3000:])
            except Exception:
                pass
            check("POST /v1/chat/completions 未崩溃", False, str(e))
            raise SystemExit(1)
        inj = hdrs.get("x-orbital-inject", "")
        check("响应头标注入成功", inj.startswith("on("), inj)
        check("上游确实收到请求", len(Upstream.received) >= 1)
        got = Upstream.received[-1] if Upstream.received else ""
        check("上游 body 含注入段（硬证据）", "长期记忆" in got, got[:300])

        # 统计文件
        real_metrics = os.path.join(tmp, "OrbitalPro", "metrics")
        files = [f for f in os.listdir(real_metrics) if f.startswith("injection-")] if os.path.isdir(real_metrics) else []
        check("统计文件已生成", len(files) >= 1, files)
        if files:
            lines = open(os.path.join(real_metrics, files[0]), encoding="utf-8").read().strip().splitlines()
            rec = json.loads(lines[-1]) if lines else {}
            check("统计记录 status=on 且 hits>0", rec.get("status") == "on" and rec.get("hits", 0) > 0, rec)

    # 4) core 不可达 → 跳过注入、原请求照发（不阻塞）
    Upstream.received.clear()
    env_proxy2 = dict(env_proxy)
    env_proxy2["ORBITAL_CORE_URL"] = "http://127.0.0.1:1"  # 必然连不上
    # 先停第一个 proxy（端口固定 8790），再起第二实例
    proxy.terminate()
    try:
        proxy.wait(timeout=5)
    except Exception:
        proxy.kill()
    time.sleep(0.8)
    proxy2 = subprocess.Popen(
        [PROXY],
        env=env_proxy2,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
    )
    time.sleep(1.2)
    try:
        if proxy2.poll() is None and wait_port(PROXY_PORT, 5):
            status, hdrs, data = post_json(
                PROXY_PORT,
                "/v1/chat/completions",
                {"model": "x", "messages": [{"role": "user", "content": "Hermes"}]},
            )
            inj = hdrs.get("x-orbital-inject", "")
            check("core 不可达 → skipped（不阻塞）", inj.startswith("skipped("), inj)
            check("core 不可达时上游仍收到原请求", len(Upstream.received) >= 1)
            check("跳过后 body 不含注入段", "长期记忆" not in (Upstream.received[-1] if Upstream.received else ""))
        else:
            check("core 不可达场景（proxy 未起来）", False, "proxy2 未监听")
    finally:
        for p in (proxy2, core):
            try:
                p.terminate()
                p.wait(timeout=5)
            except Exception:
                p.kill()

    print("")
    passed = sum(1 for _, ok in results if ok)
    print("INJECT SMOKE: %d/%d PASS" % (passed, len(results)))
    sys.exit(0 if passed == len(results) else 1)


if __name__ == "__main__":
    main()
