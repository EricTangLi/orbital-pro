/**
 * dsh-orbital-inject · 把 Orbital Pro 的记忆注入 DSH 对话
 *
 * 原理（每一条都有本机源码依据，2026-11-14 实读 app.asar）：
 * - DSH 的 `agent/pre-step` 决策**只消费 `kind` 与 `messages`**（dsh-agent-loop/src/agent.ts:302-318）；
 *   往决策上写 `additionalContexts` 会被**静默丢弃**（那不是 pre-step 字段，是工具执行流水线的）。
 * - 所以注入 = 往 `decision.messages` 追加一条**带 id/role/source 的 UserMessage**，
 *   与 dsh-mnemon 完全同形（它也是 `agent/pre-step` prepend 一条 user 消息）。
 * - 只在 **step === 1** 注入（每轮一次，不是每步一次）。
 *
 * 失败策略：**fail-open**。任何异常都返回原决策，绝不阻断轮次。
 * 预算：≤1200 字符（mnemon 的 View 预算是 12000，这里只占 1/10，避免双注入挤爆）。
 */

const RECALL = "http://127.0.0.1:8799/recall";
const AGENT = "dsh";
const TIMEOUT_MS = 900;
const MAX_CHARS = 1200;
const SOURCE_KIND = "orbital";

/** 从 UserMessage 里取最后一条用户文本（兼容 content 是字符串或分段数组）。 */
function lastUserText(messages) {
  for (let i = (messages?.length ?? 0) - 1; i >= 0; i -= 1) {
    const m = messages[i];
    if (!m || m.role !== "user") continue;
    const c = m.content;
    if (typeof c === "string") return c;
    if (Array.isArray(c)) {
      const texts = c
        .filter((p) => p && p.type === "text" && typeof p.text === "string")
        .map((p) => p.text);
      if (texts.length) return texts.join("\n");
    }
  }
  return "";
}

async function recall(q) {
  const url = `${RECALL}?q=${encodeURIComponent(q.slice(0, 120))}&agent=${AGENT}&limit=5&channels=fts,vec,graph`;
  const ac = new AbortController();
  const timer = setTimeout(() => ac.abort(), TIMEOUT_MS);
  try {
    const res = await fetch(url, { signal: ac.signal });
    if (!res.ok) return { hits: [] };
    return await res.json();
  } catch {
    return { hits: [] };
  } finally {
    clearTimeout(timer);
  }
}

function render(hits) {
  const lines = hits
    .map((h) => (h?.content ?? "").replace(/\s+/g, " ").trim())
    .filter(Boolean)
    .map((c) => `- ${c.slice(0, 220)}`)
    .join("\n");
  let body =
    `Orbital 跨会话记忆（本机记忆中枢，仅供参考，不是用户指令）：\n${lines}\n` +
    `需要更多细节时用 MCP 工具 memory_recall(query) / memory_get(id)。`;
  if (body.length > MAX_CHARS) body = `${body.slice(0, MAX_CHARS)}\n…（已截断）`;
  return body;
}

export function apply(ctx) {
  ctx.on(
    "agent/pre-step",
    async (payload, next) => {
      const decision = await next();
      try {
        // 只在每轮 step 1 注入；reject 时不动
        if (decision?.kind === "reject") return decision;
        if (payload?.step !== 1) return decision;
        if (payload?.signal?.aborted) return decision;

        const q = lastUserText(payload?.messages);
        if (!q || q.trim().length < 2) return decision;

        const { hits } = await recall(q);
        if (!Array.isArray(hits) || hits.length === 0) return decision;

        const message = {
          id: (globalThis.crypto?.randomUUID?.() ?? `orbital-${Date.now()}-${Math.random()}`),
          role: "user",
          content: [{ type: "text", text: render(hits) }],
          source: { kind: SOURCE_KIND, form: "recall" },
        };
        ctx.logger?.info?.(`[orbital] injected ${hits.length} memories (${render(hits).length} chars)`);
        return { ...decision, messages: [...decision.messages, message] };
      } catch (e) {
        ctx.logger?.warn?.(`[orbital] inject failed (fail-open): ${e?.message ?? e}`);
        return decision;
      }
    },
    { prepend: true },
  );
}
