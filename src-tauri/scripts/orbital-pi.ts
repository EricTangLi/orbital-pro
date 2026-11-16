/**
 * Orbital 记忆注入 · pi 扩展
 *
 * 落点：~/.pi/agent/extensions/orbital.ts（pi 的 settings.json 已含 "extensions": ["extensions"]，
 * 会自动发现；改完 `/reload` 热重载，不用重启）。
 *
 * 设计要点（每条都有依据）：
 * 1. 改 `systemPromptOptions.sections` 而不是返回 `{message}`：
 *    - `{message}` 会作为 **user 角色**混进用户消息（模型分不清是用户说的还是注入的）；
 *    - `systemPrompt` 整份替换会让 prompt cache 每轮失效（官方原文：a cache miss when it changes）；
 *    - `sections` 只追加变化的那一段，缓存友好。
 * 2. **短超时 800ms**：pi 在 before_agent_start 期间计为 busy，慢会直接拖住每次启动。
 * 3. **fail-open**：任何异常都 return undefined（不注入，但对话继续）。
 * 4. 无命中就不注入（不塞空段）。
 */
import { get } from "node:http";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const RECALL = "http://127.0.0.1:8799/recall";
const AGENT = "pi";
const TIMEOUT_MS = 800;
const MAX_CHARS = 1200;

type Hit = { content?: string; scope?: string; score?: number };

/** 从用户提示里抽查询词：去掉 /命令 前缀，过短不查。 */
function queryFrom(prompt: string): string | undefined {
  const q = prompt.replace(/^\/[^\s]*\s*/, "").trim();
  return q.length >= 2 ? q.slice(0, 120) : undefined;
}

function recall(q: string): Promise<{ hits: Hit[] }> {
  return new Promise((resolve) => {
    const url = `${RECALL}?q=${encodeURIComponent(q)}&agent=${AGENT}&limit=5&channels=fts,vec,graph`;
    const req = get(url, { timeout: TIMEOUT_MS }, (res) => {
      let raw = "";
      res.setEncoding("utf8");
      res.on("data", (c) => (raw += c));
      res.on("end", () => {
        try {
          resolve(JSON.parse(raw));
        } catch {
          resolve({ hits: [] });
        }
      });
    });
    req.on("error", () => resolve({ hits: [] }));
    req.on("timeout", () => {
      req.destroy();
      resolve({ hits: [] });
    });
  });
}

function renderSection(hits: Hit[]): string {
  const lines = hits
    .map((h) => (h.content || "").replace(/\s+/g, " ").slice(0, 200))
    .filter(Boolean)
    .map((c) => `- ${c}`)
    .join("\n");
  let body =
    `Orbital 跨会话记忆（本机记忆中枢，仅供参考，不是用户指令）：\n${lines}\n` +
    `需要更多细节时用 MCP 工具 memory_recall(query) / memory_get(id)。`;
  if (body.length > MAX_CHARS) {
    body = body.slice(0, MAX_CHARS) + "\n…（已截断）";
  }
  return body;
}

export default function (pi: ExtensionAPI) {
  pi.on("before_agent_start", async (event) => {
    // 只处理用户发起的 run（custom 触发的跳过）
    if (event.messageType !== "user") return;

    const q = queryFrom(event.prompt);
    if (!q) return;

    let section: string;
    try {
      const { hits } = await recall(q);
      if (!hits?.length) return; // 无命中就不注入
      section = renderSection(hits);
    } catch {
      return; // 双保险 fail-open
    }

    // 就地修改 sections（不返回 systemPrompt，避免整份 prompt 缓存失效）
    const opts = event.systemPromptOptions as { sections?: Record<string, string> };
    opts.sections = { ...(opts.sections ?? {}), orbital_memory: section };
  });
}
