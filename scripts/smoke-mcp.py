# -*- coding: utf-8 -*-
"""MCP stdio 冒烟：起真实 orbital-mcp 进程，逐个调用 10 个工具。

判据：
  1. initialize 返回 serverInfo
  2. tools/list 恰好 10 个工具且名字齐全
  3. write → 拿 id；重复写 → duplicate=true
  4. get → 读回同一条
  5. recall → 命中
  6. update → importance 改掉
  7. link + related → 能连上
  8. fact_add → 结构化事实可写（重复同值回 duplicate）；recall 带 fact_entity_id 过滤生效\n 9. stats → 计数正确
  9. forget(soft) → 之后 get 报 MEMORY_NOT_FOUND
 10. 错误令牌 → AGENT_TOKEN_INVALID（不是崩溃）
"""
import json
import os
import subprocess
import sys
import tempfile
import shutil

EXE = sys.argv[1] if len(sys.argv) > 1 else r"crates\orbital-mcp\target\debug\orbital-mcp.exe"
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

def run_register(db, agent):
    env = dict(os.environ, ORBITAL_DB_PATH=db)
    r = subprocess.run([EXE, "--register", agent], env=env, capture_output=True, text=True, encoding="utf-8")
    if r.returncode != 0:
        raise SystemExit("register failed: %s" % r.stderr)
    return r.stdout.strip()

class Mcp:
    def __init__(self, db, token):
        env = dict(os.environ, ORBITAL_DB_PATH=db, ORBITAL_AGENT_TOKEN=token)
        self.p = subprocess.Popen([EXE], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True, encoding="utf-8", bufsize=1)
        self.n = 0

    def call(self, method, params=None):
        self.n += 1
        msg = {"jsonrpc": "2.0", "id": self.n, "method": method}
        if params is not None:
            msg["params"] = params
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()
        line = self.p.stdout.readline()
        if not line:
            raise SystemExit("no response (process died? stderr=%s)" % self.p.stderr.read()[:500])
        return json.loads(line)

    def tool(self, name, args=None):
        r = self.call("tools/call", {"name": name, "arguments": args or {}})
        res = r.get("result") or {}
        txt = (res.get("content") or [{}])[0].get("text", "{}")
        try:
            payload = json.loads(txt)
        except Exception:
            payload = txt
        return res.get("isError", False), payload

    def close(self):
        try:
            self.p.stdin.close()
        except Exception:
            pass
        self.p.wait(timeout=10)

def main():
    # 第二个参数可选：指定一个已存在的库来跑（用于在真实库副本上验证迁移/打点）。
    # 不传就建临时库，保持原来的行为。
    if len(sys.argv) > 2 and sys.argv[2]:
        db = sys.argv[2]
        tmp = os.path.dirname(db) or "."
        if not os.path.exists(db):
            print("FAIL 指定的库不存在: " + db)
            return 1
    else:
        tmp = tempfile.mkdtemp(prefix="orbital-smoke-")
        db = os.path.join(tmp, "t.db")
    fails = []

    def check(name, cond, detail=""):
        print(("PASS " if cond else "FAIL ") + name + ("" if cond else "  <- " + str(detail)))
        if not cond:
            fails.append(name)

    tok = run_register(db, "dsh")
    check("register 返回 64 位令牌", len(tok) == 64, tok[:8])

    m = Mcp(db, tok)
    r = m.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "smoke"}})
    check("initialize", (r.get("result") or {}).get("serverInfo", {}).get("name") == "orbital", r)

    r = m.call("tools/list")
    tools = [t["name"] for t in (r.get("result") or {}).get("tools", [])]
    want = ["memory_write","memory_recall","memory_get","memory_update","memory_forget","memory_link","memory_related","memory_stats","orbital_health","memory_fact_add"]
    check("tools/list 10 个且齐全", sorted(tools) == sorted(want), tools)

    # 统计基线：指定库模式（第二个参数）下，库里已经有真实数据，
    # 后面的 stats 断言必须用"基线 + 增量"而不是写死绝对值。
    # 空库时 base_memories == 0，期望值仍是 2（与原断言等价）。
    err_b, st_b = m.tool("memory_stats", {})
    base_memories = ((st_b or {}).get("totals", {}) or {}).get("memories", 0) if not err_b else 0

    err, w = m.tool("memory_write", {"content": "orbital proxy listens on port 8790", "scope": "private"})
    mid = w.get("id") if isinstance(w, dict) else None
    check("write 成功且带 write_mode/deletion_mode",
          (not err) and bool(mid) and w.get("write_mode") == "exact" and w.get("deletion_mode") == "soft", w)

    err, w2 = m.tool("memory_write", {"content": "orbital proxy listens on port 8790"})
    check("重复写 → duplicate=true 且同 id", (not err) and w2.get("duplicate") is True and w2.get("id") == mid, w2)

    err, g = m.tool("memory_get", {"id": mid})
    check("get 读回同一条", (not err) and g.get("content") == "orbital proxy listens on port 8790", g)

    err, hits = m.tool("memory_recall", {"query": "proxy", "limit": 5})
    check("recall 命中", (not err) and isinstance(hits, list) and len(hits) == 1, hits)

    err, u = m.tool("memory_update", {"id": mid, "importance": 5})
    err2, g2 = m.tool("memory_get", {"id": mid})
    check("update 生效", (not err) and g2.get("importance") == 5, g2)

    err, w3 = m.tool("memory_write", {"content": "second memory about ports"})
    mid2 = w3.get("id") if isinstance(w3, dict) else None
    err, l = m.tool("memory_link", {"source_id": mid, "target_id": mid2, "edge_type": "semantic", "weight": 0.9})
    check("link 成功", not err, l)
    err, rel = m.tool("memory_related", {"id": mid, "depth": 1})
    check("related 能连回", (not err) and isinstance(rel, list) and any(x.get("id") == mid2 for x in rel), rel)

    # T-M3-03：结构化事实（走写队列）+ 按实体过滤召回
    err, fa = m.tool("memory_fact_add", {
        "memory_id": mid, "entity_type": "project", "entity_id": "project:orbital-pro",
        "property": "port", "value": "8790", "confidence": 0.9,
    })
    check("fact_add 成功（非 duplicate）", (not err) and isinstance(fa, dict) and fa.get("duplicate") is False, fa)
    err, fa2 = m.tool("memory_fact_add", {
        "memory_id": mid, "entity_type": "project", "entity_id": "project:orbital-pro",
        "property": "port", "value": "8790", "confidence": 0.9,
    })
    check("fact_add 重复同值 → duplicate=true", (not err) and isinstance(fa2, dict) and fa2.get("duplicate") is True, fa2)
    err, fr = m.tool("memory_recall", {
        "query": "port", "limit": 10, "fact_entity_id": "project:orbital-pro", "fact_property": "port",
    })
    fr_hits = (fr or {}).get("hits", []) if isinstance(fr, dict) else []
    check("recall + fact_filter 只返回登记过该事实的记忆",
          (not err) and any(h.get("id") == mid for h in fr_hits)
          and all(h.get("id") != mid2 for h in fr_hits), fr)
    err, st = m.tool("memory_stats", {})
    check("stats 计数正确", (not err) and isinstance(st, dict)
          and st.get("totals", {}).get("memories") == base_memories + 2,
          "base=%s 实际=%s" % (base_memories, st))

    err, f = m.tool("memory_forget", {"id": mid2, "mode": "soft"})
    err3, g3 = m.tool("memory_get", {"id": mid2})
    check("soft 删后 get 报 MEMORY_NOT_FOUND",
          (not err) and err3 and isinstance(g3, dict) and g3.get("code") == "MEMORY_NOT_FOUND", g3)

    m.close()

    # 错误令牌
    bad = Mcp(db, "deadbeef" * 8)
    err, e = bad.tool("memory_stats", {})
    check("错误令牌 → AGENT_TOKEN_INVALID（不崩溃）",
          err and isinstance(e, dict) and e.get("code") == "AGENT_TOKEN_INVALID", e)
    bad.close()

    # 只在"自己建的临时库"时才清理；指定库模式（第二参数）下，
    # 那个库是调用方的（通常是真实库副本），**绝不能删**。
    # （踩过：把 tmp 指向副本目录后，这里把整个副本 rmtree 掉了）
    if len(sys.argv) <= 2 or not sys.argv[2]:
        shutil.rmtree(tmp, ignore_errors=True)
    print("")
    if fails:
        print("SMOKE: FAIL (%d)" % len(fails))
        return 1
    print("SMOKE: PASS")
    return 0

if __name__ == "__main__":
    sys.exit(main())
