# -*- coding: utf-8 -*-
"""T-M7-01 注入延迟 P95 实测（200 次，开/关缓存各一轮）。

复用 smoke-inject.py 的做法：假上游 + 真 core 召回端点 + 真 proxy。
只测注入那一段的耗时（proxy 写入 metrics jsonl 的 elapsed_ms），不看上游耗时。

用法：
  python inject-p95.py <recall-server.exe> <proxy.exe> <fixture.db>
"""
import http.client
import http.server
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time


class Upstream(http.server.BaseHTTPRequestHandler):
    """假上游：收到什么都回 200 JSON（只关心"proxy 有没有把请求转发出去"）。"""

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        self.rfile.read(n)
        resp = json.dumps({"ok": True}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(resp)))
        self.end_headers()
        self.wfile.write(resp)

    def log_message(self, *a):
        pass


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def wait_port(port, timeout=10.0):
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
    c.close()
    return r.status, {}, data


def start_upstream(port):
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", port), Upstream)
    th = threading.Thread(target=srv.serve_forever, daemon=True)
    th.start()
    return srv

ROUNDS = 200


def pct(nums, p):
    if not nums:
        return None
    s = sorted(nums)
    k = max(0, min(len(s) - 1, int(round((p / 100.0) * len(s) + 0.5)) - 1))
    return s[k]


def run_round(recall_exe, proxy_exe, fixture, cache_on: bool, tag: str):
    tmp = tempfile.mkdtemp(prefix=f"orbital-p95-{tag}-")
    up_port = free_port()
    up = start_upstream(up_port)
    proxy_port = free_port()
    metrics_dir = os.path.join(tmp, "OrbitalPro", "metrics")  # proxy 写到 %LOCALAPPDATA%\OrbitalPro\metrics
    os.makedirs(metrics_dir, exist_ok=True)

    # recall-server 的用法：<db路径> <bind地址>；端口从 stdout 的 RECALL_SERVER_READY 读
    env_core = os.environ.copy()
    env_core["ORBITAL_SECRETS"] = os.path.join(tmp, "no-secrets.json")
    core = subprocess.Popen(
        [recall_exe, fixture, "127.0.0.1:0"],
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
    if core_port is None:
        raise RuntimeError("core 只读端点未启动")

    env_proxy = os.environ.copy()
    env_proxy["ORBITAL_UPSTREAM"] = f"http://127.0.0.1:{up_port}"
    env_proxy["ORBITAL_INJECT"] = "1"
    env_proxy["ORBITAL_CORE_URL"] = f"http://127.0.0.1:{core_port}"
    env_proxy["ORBITAL_PROXY_PORT"] = str(proxy_port)
    env_proxy["ORBITAL_PROXY_USE_SYSTEM_PROXY"] = "0"
    # proxy 的 metrics 写到 %LOCALAPPDATA%\\metrics → 把 LOCALAPPDATA 指到临时目录
    env_proxy["LOCALAPPDATA"] = tmp
    if cache_on:
        env_proxy["ORBITAL_INJECT_CACHE"] = "1"
    proxy = subprocess.Popen(
        [proxy_exe],
        env=env_proxy,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    wait_port(proxy_port, 10)

    payload = {
        "model": "test",
        "messages": [
            {"role": "system", "content": "你是助手"},
            {"role": "user", "content": "Orbital 的记忆库路径在哪？"},
        ],
    }
    t_start = time.time()
    for _ in range(ROUNDS):
        post_json(proxy_port, "/v1/chat/completions", payload)
    wall = time.time() - t_start

    time.sleep(0.3)
    elapsed = []
    cache_hits = 0
    cache_misses = 0
    for name in os.listdir(metrics_dir):
        with open(os.path.join(metrics_dir, name), encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                r = json.loads(line)
                if r.get("elapsed_ms") is not None:
                    elapsed.append(r["elapsed_ms"])
                if r.get("cache") == "hit":
                    cache_hits += 1
                elif r.get("cache") == "miss":
                    cache_misses += 1

    proxy.terminate()
    core.terminate()
    up.shutdown()
    try:
        proxy.wait(timeout=5)
        core.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proxy.kill()
        core.kill()
    shutil.rmtree(tmp, ignore_errors=True)

    return {
        "rounds": ROUNDS,
        "cache_on": cache_on,
        "samples": len(elapsed),
        "cache_hit": cache_hits,
        "cache_miss": cache_misses,
        "wall_ms_per_req": round(wall / ROUNDS * 1000, 2),
        "p50": pct(elapsed, 50),
        "p95": pct(elapsed, 95),
        "max": max(elapsed) if elapsed else None,
    }


def main():
    if len(sys.argv) < 4:
        print(__doc__)
        return 2
    recall_exe, proxy_exe, fixture = sys.argv[1:4]
    results = []
    for cache_on, tag in [(False, "off"), (True, "on")]:
        r = run_round(recall_exe, proxy_exe, fixture, cache_on, tag)
        results.append(r)
        print(json.dumps(r, ensure_ascii=False))

    off, on = results[0], results[1]
    print()
    print("=== 对比 ===")
    ok = on["p95"] is not None and on["p95"] < 10
    print(f"关闭缓存 P95 = {off['p95']} ms（命中 0，miss {off['cache_miss']}）")
    print(f"开启缓存 P95 = {on['p95']} ms（hit {on['cache_hit']} / miss {on['cache_miss']}）")
    print("判据（命中时 P95 < 10ms）:", "PASS" if ok else "FAIL（如实记录）")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
