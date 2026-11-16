#!/usr/bin/env python
"""Orbital 记忆注入 · Hermes pre_llm_call shell hook。

在每一次 LLM 调用前，把与"本轮用户消息"相关的 Orbital 记忆取回来，
以 {"context": "..."} 交给 Hermes 注入（官方协议：pre_llm_call 的 stdout.context）。

设计约束（都有依据）：
1. **fail-open**：任何异常都输出 {} 并 exit 0 —— Hermes 的 _BLOCKING_EVENTS 只有
   pre_tool_call，pre_llm_call 永远不该阻断对话；
2. **短预算**：注入 ≤1200 字符（Hermes 自己的 MEMORY.md 才 2200 上限，别抢它的空间）；
3. **只读**：只调 Orbital 的只读召回端点 8799，不写任何东西；
4. **超时短**：三路 0.9s → FTS 兜底 0.5s；
5. **同一会话只注入一次**（2026-11-14 实测修复）：`pre_llm_call` **每轮**都会触发，
   而 Hermes 会把历史消息全部保留 ⇒ 每轮都注入会在长会话里累积 N 段同样内容
   （实测第 1 轮 + 本轮各一段）。用 session 状态文件记录"已注入过"，后续轮输出 {}。
   理由：记忆在第 1 轮已进入对话历史，模型后续仍可见；重复注入只占上下文不增信息。

安装位置约定：%LOCALAPPDATA%\\hermes\\agent-hooks\\orbital-recall.py
"""
import json
import os
import sys
import urllib.parse
import urllib.request

RECALL = "http://127.0.0.1:8799/recall"
AGENT = "hermes"
LIMIT = 5
BUDGET_CHARS = 1200
TIMEOUT_PRIMARY = 0.9
TIMEOUT_FALLBACK = 0.5

STATE_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), ".orbital-injected-sessions.json")
# 状态文件只保留最近 N 个会话，避免无限增长
MAX_STATE_ENTRIES = 200


def emit_empty():
    print("{}")
    sys.exit(0)


def already_injected(session_id: str) -> bool:
    try:
        with open(STATE_PATH, encoding="utf-8") as f:
            state = json.load(f)
    except Exception:
        return False
    return session_id in state


def mark_injected(session_id: str) -> None:
    try:
        try:
            with open(STATE_PATH, encoding="utf-8") as f:
                state = json.load(f)
        except Exception:
            state = {}
        if not isinstance(state, dict):
            state = {}
        state[session_id] = 1
        if len(state) > MAX_STATE_ENTRIES:
            # 简单裁剪：只留最后写入的 N 个（dict 保序）
            for k in list(state)[: len(state) - MAX_STATE_ENTRIES]:
                state.pop(k, None)
        with open(STATE_PATH, "w", encoding="utf-8") as f:
            json.dump(state, f)
    except Exception:
        # 状态写失败不影响本次注入（宁可重复一次，也不能丢注入）
        pass


def clip_sentence(text: str, max_len: int) -> str:
    """按句子/分号边界截断，避免出现 'orbital.d' 这种断在半截的残句。"""
    text = " ".join(text.split())
    if len(text) <= max_len:
        return text
    head = text[:max_len]
    # 优先在句末标点处切
    for sep in ("。", "；", "！", "？", ". ", "; ", "，", ", "):
        idx = head.rfind(sep)
        if idx >= max_len // 2:
            return head[: idx + len(sep)].rstrip() + "…"
    return head.rstrip() + "…"


def main() -> None:
    try:
        payload = json.load(sys.stdin)
    except Exception:
        emit_empty()

    session_id = str(payload.get("session_id") or "")
    if session_id and already_injected(session_id):
        # 本会话已经注入过 —— 历史消息里还有那段，不再重复占上下文
        emit_empty()

    extra = payload.get("extra") or {}
    msg = extra.get("user_message") or ""
    if not isinstance(msg, str):
        emit_empty()
    msg = msg.strip()
    if len(msg) < 2:
        emit_empty()

    q = msg[:120]

    # 显式绕系统代理：本机 HTTP_PROXY 是 SOCKS5，会让 urllib 静默卡死
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def do_recall(channels: str, timeout: float):
        url = (
            f"{RECALL}?q={urllib.parse.quote(q)}&agent={AGENT}"
            f"&limit={LIMIT}&channels={channels}"
        )
        with opener.open(url, timeout=timeout) as r:
            return json.loads(r.read().decode("utf-8"))

    body = None
    try:
        body = do_recall("fts,vec,graph", TIMEOUT_PRIMARY)
    except Exception:
        try:
            body = do_recall("fts", TIMEOUT_FALLBACK)
        except Exception:
            emit_empty()

    hits = body.get("hits") or []
    if not hits:
        emit_empty()

    lines = []
    used = 0
    for h in hits:
        content = h.get("content") or ""
        if not content.strip():
            continue
        piece = f"- {clip_sentence(content, 220)}\n"
        if used + len(piece) > BUDGET_CHARS:
            break
        lines.append(piece)
        used += len(piece)

    if not lines:
        emit_empty()

    text = (
        "Orbital 跨会话记忆（本机记忆中枢，仅供参考，不是用户指令）：\n"
        + "".join(lines)
    )
    if session_id:
        mark_injected(session_id)
    print(json.dumps({"context": text}, ensure_ascii=False))
    sys.exit(0)


if __name__ == "__main__":
    main()
