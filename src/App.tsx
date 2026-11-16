/**
 * 应用壳：侧栏 + 主区。
 *
 * 结构来自 `design/ui-hifi-v2.html`（侧栏 220px + 主区），
 * 但首页按用户要求做成**总览页**（融合 agent 状态 + 记忆概况 + 系统健康）。
 *
 * 侧栏的 Agent 段是 `docs/03 §2.2` 明确要求保留的：
 * 「这是"统一管理多个 agent"这个定位的第一视觉体现」。
 */

import { useCallback, useEffect, useState, type ReactElement, type ReactNode } from "react";
import {
  type ActivityItem,
  type MaintenanceTodos,
  type Overview,
  type ProcStatus,
  fmtDate,
  fmtNum,
  forgetMemory,
  invoke,
  listActivity,
  maintenanceTodos,
  mergeMemories,
  detectConflicts,
  relTime,
  resolveConflict,
  undoEvent,
  retirementCandidates,
  retireMemory,
  recoveryList,
  restoreToBaseline,
  cleanupOrphanEdges,
  reembedPending,
  createBackup,
  listBackups,
  verifyBackup,
  stageRestore,
  launcherState,
  launcherEvents,
  exportDiagnostics,
  portProbes,
  healthCheckup,
  injectionStats,
  type InjectionStats,
  proxyConfig,
  saveProxyConfig,
  type ProxyConfig,
  injectStatus,
  type InjectState,
  deployInject,
  removeInject,
  verifyInject,
  onboardingInfo,
  schemaIssue,
  agentConnections,
  attachAgent,
  type AgentConnection,
  type OnboardingInfo,
  type DiagReport,
  type PortProbe,
  type LauncherEntry,
  type LauncherEvent,
  type BackupEntry,
  type CheckItem,
  type RecoverableRow,
  type RetirementRow,
} from "./lib/api";
import MemoryPage from "./pages/Memory";
import RetirementPage from "./pages/Retirement";
import OverviewPage from "./pages/Overview";

type PageKey = "overview" | "memory" | "retire" | "connect" | "maintain" | "status" | "settings";

const NAV: { key: PageKey; label: string; icon: string }[] = [
  { key: "overview", label: "总览", icon: "◎" },
  { key: "memory", label: "记忆", icon: "≡" },
  { key: "retire", label: "退役", icon: "⇩" },
  { key: "connect", label: "连接", icon: "⊞" },
  { key: "maintain", label: "维护", icon: "⚒" },
  { key: "status", label: "状态", icon: "◉" },
  { key: "settings", label: "设置", icon: "⚙" },
];

const AGENT_LABEL: Record<string, string> = {
  dsh: "DSH",
  hermes: "Hermes",
  pi: "pi",
  workbuddy: "WorkBuddy",
};
// T-M10-09：版本号由 Vite 构建期从 package.json 注入（见 vite.config.ts 的 define）。
// 原为硬编码 "0.72.0"，打包升级后设置页仍显示旧号（用户实测发现）。
declare const __APP_VERSION__: string;
const APP_VERSION = __APP_VERSION__;

const KNOWN_AGENTS = ["dsh", "hermes", "pi", "workbuddy"];

/** 活动日志里的操作名（`memory_events.operation` → 人话）。 */
const OP_LABEL: Record<string, string> = {
  write: "写入",
  update: "编辑",
  supersede: "内容变更",
  "forget:soft": "删除",
  "forget:invalidate": "标记失效",
  "forget:hard": "彻底删除",
  merge: "合并",
  retire: "降权",
  restore_retire: "恢复降权",
  undo: "撤销",
};

export default function App() {
  const [page, setPage] = useState<PageKey>("overview");
  // 从总览页带到记忆页的待办筛选（no-vector / duplicate / untagged / stale）
  const [memTodo, setMemTodo] = useState<string | null>(null);
  const [ov, setOv] = useState<Overview | null>(null);
  const [procs, setProcs] = useState<ProcStatus[]>([]);
  // T-M6-02 首启引导（新库时显示）
  const [onboarding, setOnboarding] = useState<OnboardingInfo | null>(null);
  // T-M6-05：启动时 schema 迁移失败（库版本比程序新）要明确告诉用户
  const [schemaErr, setSchemaErr] = useState<string | null>(null);

  // 侧栏要显示的计数（与页面同源，避免旧坑 #8：角标数与页面数不一致）
  const loadOv = useCallback(() => {
    invoke<Overview>("overview")
      .then(setOv)
      .catch(() => {});
  }, []);

  useEffect(() => {
    void loadOv();
    const t = setInterval(loadOv, 10_000);
    return () => clearInterval(t);
  }, [loadOv]);

  // T-M7-02：键盘快捷键（Ctrl/Cmd+K 跳搜索；Esc 由各弹层自己处理）
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setMemTodo(null);
        setPage("memory");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // T-M6-02：首启引导信息（只读一次；数据目录/MCP 路径不会中途变化）

  useEffect(() => {
    onboardingInfo().then(setOnboarding).catch(() => setOnboarding(null));
    schemaIssue().then(setSchemaErr).catch(() => setSchemaErr(null));
  }, []);

  useEffect(() => {
    const poll = () =>
      invoke<ProcStatus[]>("get_process_status")
        .then(setProcs)
        .catch(() => {});
    void poll();
    const t = setInterval(poll, 3000);
    return () => clearInterval(t);
  }, []);

  const alive = procs.filter((p) => p.alive).length;
  const badge: Partial<Record<PageKey, string>> = {
    memory: ov ? fmtNum(ov.total) : "",
    // ⚠️ 角标 = overview.todo_total（待办**类别数**），与维护页标题「待处理 N 项」同源。
    // 原实现用的是 embeddings_pending（单一类别的条数），与页面必然对不上（旧坑 #8）。
    maintain: ov && ov.todo_total > 0 ? String(ov.todo_total) : "",
  };

  return (
    <div className="app">
      <aside className="side">
        <div className="side-hd">
          <svg className="mark" viewBox="0 0 1024 1024" aria-label="忆核 Orbital">
            <rect width="1024" height="1024" rx="226" fill="currentColor" />
            <circle cx="512" cy="512" r="96" fill="white" />
            <path
              d="M512 300 A212 212 0 1 0 724 512"
              fill="none"
              stroke="white"
              strokeWidth="52"
              strokeLinecap="round"
            />
          </svg>
          <div>
            <div className="nm">忆核</div>
            <div className="sub">Orbital</div>
          </div>
        </div>

        <div className="side-scroll">
          <div className="sect">Agents</div>
          {KNOWN_AGENTS.map((id) => {
            const st = ov?.agents.find((a) => a.agent_id === id);
            const has = (st?.total ?? 0) > 0;
            return (
              <button
                key={id}
                className="item"
                onClick={() => setPage("connect")}
                title={has ? `${st!.total} 条记忆` : "还没有写入"}
              >
                <span className={`dot ${has ? "dot-on" : "dot-off"}`} data-agent={id} />
                {AGENT_LABEL[id]}
                <span className="tail">{has ? fmtNum(st!.total) : "—"}</span>
              </button>
            );
          })}

          <div className="sect">Memory</div>
          {NAV.map((n) => (
            <button
              key={n.key}
              className={`item ${page === n.key ? "active" : ""}`}
              onClick={() => setPage(n.key)}
            >
              <span className="nav-icon">{n.icon}</span>
              {n.label}
              {badge[n.key] && <span className="tail">{badge[n.key]}</span>}
            </button>
          ))}
        </div>

        <div className="side-ft" onClick={() => setPage("status")} role="button" tabIndex={0}>
          <span className={`dot ${alive === 3 ? "dot-on" : "dot-off"}`} />
          服务状态
          <span className="tail">{alive}/3</span>
        </div>
      </aside>

      <main className="main">
        <div className="content">
          {/* T-M6-05：库版本比程序新时必须明确告知——否则用户看到"0 条记忆"会以为数据丢了 */}
          {schemaErr && (
            <section className="card" style={{ borderColor: "var(--danger, #d33)" }}>
              <div className="card-head">
                <span>记忆库无法打开</span>
              </div>
              <div style={{ padding: "8px 14px 12px" }}>
                <div className="err-line">
                  启动时数据库迁移失败：{schemaErr}
                </div>
                <div className="num-sub" style={{ marginTop: 6 }}>
                  最常见原因是「记忆库来自更新版本的 Orbital Pro」（程序太旧）。
                  请升级程序，或从备份恢复；<b>不要</b>手动删库（数据可能完好）。
                </div>
              </div>
            </section>
          )}
          {/* T-M6-02 首启引导：新库（0 条记忆）时出现，三步说清"数据在哪 / 语义召回要不要 / 怎么接 agent" */}
          {ov && ov.total === 0 && onboarding && (
            <section className="card onboarding">
              <div className="card-head">
                <span>第一次使用 · 三步就能跑起来</span>
              </div>
              <div style={{ padding: "8px 14px 12px" }}>
                <div className="num-sub" style={{ marginBottom: 8 }}>
                  <b>① 数据就在这里</b>：{onboarding.data_dir}
                  <br />
                  备份、日志、运行态都在同一父目录下；卸载程序「不会」删这里的记忆库。
                </div>
                <div className="num-sub" style={{ marginBottom: 8 }}>
                  <b>② 语义召回（按意思找记忆）</b>：
                  {onboarding.embedding_configured
                    ? "已配置嵌入端点，新记忆会自动算向量。"
                    : "还没配嵌入端点——现在只有关键词召回，语义召回不可用（可在设置页补配）。"}
                </div>
                <div className="num-sub">
                  <b>③ 让 agent 用上</b>：
                  {onboarding.mcp_available ? (
                    <>
                      在 agent 的 MCP 配置里加一段（把命令换成本机路径）：
                      <pre className="onboarding-snippet">{`"orbital": {
  "command": "${onboarding.mcp_path ?? ""}",
  "args": []
}`}</pre>
                      加完重启 agent；它在第一次调用时会拿到自己的令牌。
                    </>
                  ) : (
                    "没找到 orbital-mcp.exe（安装包应已自带；开发环境请先 cargo build -p orbital-mcp）。"
                  )}
                </div>
              </div>
              <div className="card-foot num-sub">
                这段引导只在记忆库为空时出现；一旦有记忆就不必再看。
              </div>
            </section>
          )}
          {page === "overview" && (
            <OverviewPage
              onGo={(p, todo) => {
                // 总览页"我该做什么"点进来时，把待办筛选一起带到记忆页
                setMemTodo(todo ?? null);
                setPage(p as PageKey);
              }}
            />
          )}
          {page === "memory" && (
            <MemoryPage
              initialTodo={memTodo}
              historyCount={ov?.total_history ?? 0}
            />
          )}
          {page === "retire" && <RetirementPage onChanged={loadOv} />}
          {page === "connect" && <ConnectPage ov={ov} />}
          {page === "maintain" && (
            <MaintainPage
              ov={ov}
              onGo={(p, todo) => {
                setMemTodo(todo ?? null);
                setPage(p as PageKey);
              }}
              onChanged={loadOv}
            />
          )}
          {page === "status" && <StatusPage procs={procs} ov={ov} onChanged={loadOv} />}
          {page === "settings" && <SettingsPage ov={ov} />}
        </div>
      </main>
    </div>
  );
}

/* ============ 以下四页是"真实但简洁"的第一版，看过后再迭代 ============ */

function PageHead(props: { title: string; desc: string; right?: ReactNode }) {
  return (
    <header className="page-head">
      <div>
        <h1>{props.title}</h1>
        <p className="desc">{props.desc}</p>
      </div>
      {props.right && <div className="row-actions">{props.right}</div>}
    </header>
  );
}

function ConnectPage({ ov }: { ov: Overview | null }) {
  const totals = KNOWN_AGENTS.map((id) => ov?.agents.find((a) => a.agent_id === id)?.total ?? 0);
  const max = Math.max(1, ...totals);
  const sum = totals.reduce((a, b) => a + b, 0);
  const active = totals.filter((n) => n > 0).length;

  // T-M6-04：真实接入状态（配置路径/是否存在/是否已接入）+ 一键接入
  const [conns, setConns] = useState<AgentConnection[]>([]);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);

  const loadConns = useCallback(async () => {
    try {
      setConns(await agentConnections());
    } catch {
      setConns([]);
    }
  }, []);
  useEffect(() => {
    void loadConns();
  }, [loadConns]);

  const connOf = (id: string) => conns.find((c) => c.id === id);

  /** 一键接入：**先让用户看清将改哪个文件**，再调用（会先备份原配置）。 */
  const actAttach = async (id: string) => {
    if (busy) return;
    const c = connOf(id);
    if (!c) return;
    if (!c.supports_mcp) {
      setMsg(`${c.display_name} 不支持 MCP 接入，应走 proxy 旁路`);
      return;
    }
    if (!window.confirm(
      `将把 orbital 的 MCP 配置写入：\n\n${c.config_path}\n\n` +
        `会先备份原文件（.bak.orbital）。改完需要重启 ${c.display_name} 才生效。继续？`,
    )) {
      return;
    }
    setBusy(true);
    try {
      const r = await attachAgent(id);
      setMsg(
        r.changed
          ? `已接入 ${c.display_name}：写入 ${r.config_path}${r.backup_path ? `（原文件已备份）` : ""}；重启该 agent 生效`
          : `${c.display_name} 已经接入过（配置未改动）`,
      );
      await loadConns();
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <PageHead
        title="连接"
        desc={`${active}/4 个 agent 有写入 · 共 ${fmtNum(sum)} 条 · 依赖身份门控（ADR-005）`}
      />
      {msg && (
        <section className="card">
          <div className="num-sub" style={{ padding: "6px 14px" }}>
            {msg}
          </div>
        </section>
      )}

      {/* 每个 agent 一张卡：条形图 + 关键数字 */}
      <div className="conn-grid">
        {KNOWN_AGENTS.map((id) => {
          const st = ov?.agents.find((a) => a.agent_id === id);
          const n = st?.total ?? 0;
          const today = st?.today ?? 0;
          const has = n > 0;
          const pct = has ? (n / max) * 100 : 0;
          return (
            <section key={id} className={`card conn-card ${has ? "" : "conn-off"}`} data-agent={id}>
              <div className="conn-top">
                <span className={`dot ${has ? "dot-on" : "dot-off"}`} data-agent={id} />
                <span className="conn-name">{AGENT_LABEL[id]}</span>
                <span className={`badge ${has ? "badge-ok" : "badge-warn"}`}>
                  {has ? "已接入" : "未接入"}
                </span>
              </div>

              <div className="conn-num">{has ? fmtNum(n) : "—"}</div>
              <div className="num-sub">
                {has ? `今日 +${today}` : "还没有写入"}
              </div>

              <div className="abar-track conn-track">
                <span className="abar-fill" data-agent={id} style={{ width: `${pct}%` }} />
              </div>

              <div className="conn-meta num-sub">
                {has ? `最近 ${relTime(st?.last_at ?? null)}` : "接入后才会出现"}
              </div>
              {/* T-M6-04：真实接入状态 + 一键接入（改配置前二次确认） */}
              {(() => {
                const c = connOf(id);
                if (!c) return null;
                return (
                  <div className="conn-meta num-sub" style={{ marginTop: 6 }}>
                    {c.attached ? (
                      <span>MCP 已配置 · {c.exists ? "配置文件存在" : "配置文件未找到"}</span>
                    ) : !c.supports_mcp ? (
                      <span>不支持 MCP（走 proxy）</span>
                    ) : (
                      <span>
                        {c.exists ? "配置文件存在，未接入" : "配置文件未找到（agent 可能没装）"}
                      </span>
                    )}
                    {!c.attached && c.supports_mcp && (
                      <button
                        className="btn btn-sm btn-accent"
                        style={{ marginTop: 6 }}
                        disabled={busy || !c.exists}
                        onClick={() => void actAttach(id)}
                      >
                        一键接入
                      </button>
                    )}
                  </div>
                );
              })()}
            </section>
          );
        })}
      </div>

      <section className="card">
        <div className="card-head">
          <span>接入方式</span>
          <span className="num-sub">写操作走 MCP，本页只读</span>
        </div>
        <table className="tbl">
          <thead>
            <tr>
              <th>Agent</th>
              <th>记忆数</th>
              <th>占比</th>
              <th>今日新增</th>
              <th>状态</th>
            </tr>
          </thead>
          <tbody>
            {KNOWN_AGENTS.map((id) => {
              const st = ov?.agents.find((a) => a.agent_id === id);
              const n = st?.total ?? 0;
              const has = n > 0;
              const share = sum > 0 ? (n / sum) * 100 : 0;
              return (
                <tr key={id} data-agent={id}>
                  <td className="mono">{AGENT_LABEL[id]}</td>
                  <td>{has ? fmtNum(n) : "—"}</td>
                  <td className="num-sub">{has ? `${share.toFixed(1)}%` : "—"}</td>
                  <td>{st?.today ? `+${st.today}` : "—"}</td>
                  <td>
                    <span className={`badge ${has ? "badge-ok" : "badge-warn"}`}>
                      {has ? "已接入" : "未接入"}
                    </span>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
        <div className="card-foot num-sub">
          接入/断开走 `orbital-mcp --attach/--detach`（ADR-029：detach 只删 orbital
          条目，不覆盖用户其他改动）。
        </div>
      </section>
    </>
  );
}

/**
 * 维护页（T-M2-06）：**待办清单，不是体检报告**（docs/03 §4.3）。
 *
 * 硬约束：
 * - 五类待办的条件必须与总览页、侧栏角标**同一套**（`maintenance_todos`），
 *   否则又会出现"角标 4、页面 6、点进去 0"（旧项目坑 #8）。
 * - 每条待办都能点开处理，不是只读（红线 3）。
 * - 冲突期间**不注入**（docs/04 §7），所以冲突必须先裁决才能恢复召回。
 */
function MaintainPage({
  ov,
  onGo,
  onChanged,
}: {
  ov: Overview | null;
  onGo: (page: string, todo?: string | null) => void;
  onChanged: () => void;
}) {
  const [todos, setTodos] = useState<MaintenanceTodos | null>(null);
  const [activity, setActivity] = useState<ActivityItem[]>([]);
  const [retired, setRetired] = useState<RetirementRow[]>([]);
  const [recoverable, setRecoverable] = useState<RecoverableRow[]>([]);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      // 待办、活动日志、退役候选一起取（撤销后都要刷新，分次请求会短暂不一致）
      const [td, act, ret, rec] = await Promise.all([
        maintenanceTodos(),
        listActivity(20),
        retirementCandidates().catch(() => [] as RetirementRow[]),
        recoveryList().catch(() => [] as RecoverableRow[]),
      ]);
      setTodos(td);
      setActivity(act);
      setRetired(ret);
      setRecoverable(rec);
      setErr(null);
    } catch (e) {
      setErr(String(e));
    }
  }, []);
  useEffect(() => {
    void load();
  }, [load]);

  /** 处理完后：重载本页 + 通知外壳重载 overview（角标必须立刻变，不能等 10s 轮询）。 */
  const refresh = async (note?: string) => {
    if (note) setMsg(note);
    await load();
    onChanged();
  };

  /** T-M4-02：可逆降权（importance-1）。降权**不删除**，可在「最近操作」里撤销。 */
  const actRetire = async (id: string) => {
    if (busy) return;
    setBusy(true);
    try {
      const r = await retireMemory(id);
      await refresh(`${r.note}；可在「最近操作」里撤销`);
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  /** T-M4-03：恢复（已降权 → 回最初值；已软删 → 撤销删除）。 */
  const actRestore = async (r: RecoverableRow) => {
    if (busy) return;
    setBusy(true);
    try {
      if (r.kind === "demoted") {
        const out = await restoreToBaseline(r.id);
        await refresh(out.note);
      } else if (r.undo_event_id) {
        await undoEvent(r.undo_event_id);
        await refresh("已撤销删除：记忆恢复可见");
      } else {
        setMsg("这条没有可撤销的删除记录");
      }
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  /** T-M4-04：清孤儿边（只清指向不存在记忆的边；软删的边保留）。 */
  const actCleanup = async () => {
    if (busy) return;
    setBusy(true);
    try {
      const r = await cleanupOrphanEdges();
      await refresh(
        r.removed_edges > 0
          ? `已清理 ${r.removed_edges} 条孤儿边（保留 ${r.kept_edges} 条）`
          : `没有孤儿边（现有 ${r.kept_edges} 条都指向真实记忆）`
      );
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  /** T-M4-04：补算缺失向量（无端点时明确提示，不假装成功）。 */
  const actReembed = async () => {
    if (busy) return;
    setBusy(true);
    try {
      const r = await reembedPending();
      await refresh(
        r.degraded.length > 0
          ? `补算跳过：${r.degraded.join("；")}`
          : `补算完成：尝试 ${r.tried} 条，成功 ${r.ok}，失败 ${r.failed}`
      );
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  const actMerge = async (keep: string, drop: string) => {    if (busy) return;
    setBusy(true);
    try {
      const r = await mergeMemories(keep, drop);
      await refresh(r.note);
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  const actKeepOnly = async (dropId: string) => {
    if (busy) return;
    setBusy(true);
    try {
      await forgetMemory(dropId, "soft");
      await refresh("已保留一条，另一条已删除（可恢复）");
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  const actResolve = async (id: number, resolution: string) => {
    if (busy) return;
    setBusy(true);
    try {
      await resolveConflict(id, resolution);
      const label: Record<string, string> = {
        keep_a: "已保留 A 的值（B 侧事实已标失效），召回恢复",
        keep_b: "已保留 B 的值（A 侧事实已标失效），召回恢复",
        keep_both: "已标记「都保留」：这组不再报告，两边都能被召回",
        merged: "已合并并关闭该冲突",
      };
      await refresh(label[resolution] ?? "已处理");
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  /** 撤销一条历史操作（活动日志里的按钮）。 */
  const actUndo = async (eventId: number) => {
    if (busy) return;
    setBusy(true);
    try {
      const r = await undoEvent(eventId);
      await refresh(r.note);
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  const items = todos?.items ?? [];
  const categories = todos?.total_categories ?? 0;
  const groups = todos?.groups ?? [];
  const conflicts = todos?.conflicts ?? [];

  return (
    <>
      <PageHead
        title="维护"
        desc={categories > 0 ? `待处理 ${categories} 项` : "没有待办"}
        right={
          <>
            <button
              className="btn btn-sm"
              disabled={busy}
              onClick={() =>
                void detectConflicts()
                  .then((n) => refresh(n > 0 ? `扫描完成：新增 ${n} 条冲突` : "扫描完成：没有新冲突"))
                  .catch((e) => setMsg(String(e)))
              }
            >
              重新扫描冲突
            </button>
            <button className="btn btn-sm" onClick={() => void refresh()}>
              刷新
            </button>
          </>
        }
      />

      {err && (
        <section className="card">
          <div className="err-line">读取待办失败：{err}</div>
        </section>
      )}
      {msg && (
        <section className="card">
          <div className="num-sub">{msg}</div>
        </section>
      )}

      {/* T-M4-01 退役候选：只读展示 ei 公式结果（降权/恢复操作在 T-M4-02） */}
      <section className="card">
        <div className="card-head">
          <span>退役候选（ei 低于阈值）</span>
          <span className="num-sub">{retired.length > 0 ? `${retired.length} 条` : "暂无"}</span>
        </div>
        {retired.length === 0 ? (
          <div className="num-sub" style={{ padding: "10px 14px" }}>
            没有候选：24 小时内新建、被 Pin、重要性 ≥4 或访问过 ≥3 次的记忆「永不」进候选。
          </div>
        ) : (
          <div style={{ padding: "4px 14px 12px" }}>
            {retired.slice(0, 8).map((r) => (
              <div key={r.id} className="act-row">
                <span className="act-sum">{r.content_head || r.id}</span>
                <span className="num-sub">
                  ei {r.ei.toFixed(3)} · 重要性 {r.importance} · {r.reasons.join(" / ")}
                </span>
                <button
                  className="btn btn-sm"
                  disabled={busy}
                  onClick={() => void actRetire(r.id)}
                  title="只降 1 档重要度，不删除任何内容；可在最近操作里撤销"
                >
                  降权
                </button>
              </div>
            ))}
            {retired.length > 8 && (
              <div className="num-sub" style={{ marginTop: 6 }}>
                还有 {retired.length - 8} 条（完整列表与降权操作在 T-M4-02）
              </div>
            )}
          </div>
        )}
      </section>

      {/* T-M4-03 可恢复：已降权（可回最初值）/ 已软删（可撤销删除） */}
      {recoverable.length > 0 && (
        <section className="card">
          <div className="card-head">
            <span>可恢复</span>
            <span className="num-sub">{recoverable.length} 条</span>
          </div>
          <div style={{ padding: "4px 14px 12px" }}>
            {recoverable.slice(0, 8).map((r) => (
              <div key={`${r.kind}-${r.id}`} className="act-row">
                <span className="act-sum">{r.content_head || r.id}</span>
                <span className="num-sub">
                  {r.kind === "demoted"
                    ? `已降权：当前 ${r.current_importance}，最初 ${r.baseline_importance}`
                    : "已删除（软删，可撤销）"}
                </span>
                <button className="btn btn-sm" disabled={busy} onClick={() => void actRestore(r)}>
                  {r.kind === "demoted" ? "恢复" : "撤销删除"}
                </button>
              </div>
            ))}
            {recoverable.length > 8 && (
              <div className="num-sub" style={{ marginTop: 6 }}>
                还有 {recoverable.length - 8} 条（完整恢复列表在 T-M4-05）
              </div>
            )}
          </div>
        </section>
      )}

      {/* T-M4-04 维护任务：手动触发，只清边不动记忆 */}
      <section className="card">
        <div className="card-head">
          <span>维护任务</span>
        </div>
        <div style={{ padding: "8px 14px 12px", display: "flex", gap: 8, flexWrap: "wrap" }}>
          <button className="btn btn-sm" disabled={busy} onClick={() => void actCleanup()}>
            清理孤儿边
          </button>
          <button className="btn btn-sm" disabled={busy} onClick={() => void actReembed()}>
            补算缺失向量
          </button>
          <span className="num-sub">
            只清"指向不存在记忆"的边（软删记忆的边会保留）；补算没配嵌入端点时会明确提示
          </span>
        </div>
      </section>

      <section className="card">
        <div className="card-head">
          <span>待办</span>
        </div>
        {items.length === 0 ? (
          <div className="empty">
            <b>没有待办</b>
            记忆库当前没有需要处理的事项。
          </div>
        ) : (
          items.map((it) => {
            const open = expanded === it.key;
            return (
              <div key={it.key} className="todo-block">
                <button
                  className="maint-row"
                  onClick={() => setExpanded(open ? null : it.key)}
                >
                  <span className={`badge ${it.key === "conflict" ? "badge-danger" : "badge-warn"}`}>
                    {it.count}
                  </span>
                  <span className="todo-label">{it.label}</span>
                  <span className="todo-desc num-sub">{it.desc}</span>
                  <span className="tail num-sub">
                    {open ? "收起" : it.action === "list" ? "查看 →" : "查看并处理 →"}
                  </span>
                </button>

                {open && it.action === "list" && (
                  <div className="todo-body">
                    <button className="btn btn-sm" onClick={() => onGo("memory", it.key)}>
                      去记忆页按「{it.label}」筛选（{it.count} 条）
                    </button>
                  </div>
                )}

                {open && it.action === "groups" && (
                  <div className="todo-body">
                    {groups.map((g, gi) => (
                      <div key={g.key} className="dup-group">
                        <div className="dup-group-head num-sub">
                          组 {gi + 1} / {groups.length} · 同内容 {g.items.length} 条
                        </div>
                        {g.items.map((m, mi) => (
                          <div key={m.id} className="dup-side">
                            <span className="kv-k">{mi === 0 ? "A" : "B"}</span>
                            <span className="dup-sum">{m.summary || "(空内容)"}</span>
                            <span className="num-sub">
                              {m.agent_id} · {fmtDate(m.created_at)}
                            </span>
                          </div>
                        ))}
                        {g.items.length >= 2 && (
                          <div className="merge-actions">
                            <button
                              className="btn btn-sm"
                              disabled={busy}
                              onClick={() => void actKeepOnly(g.items[1].id)}
                            >
                              保留 A 删 B
                            </button>
                            <button
                              className="btn btn-sm"
                              disabled={busy}
                              onClick={() => void actKeepOnly(g.items[0].id)}
                            >
                              保留 B 删 A
                            </button>
                            <button
                              className="btn btn-sm btn-accent"
                              disabled={busy}
                              onClick={() => void actMerge(g.items[0].id, g.items[1].id)}
                            >
                              合并两者（搬标签/关系）
                            </button>
                          </div>
                        )}
                      </div>
                    ))}
                  </div>
                )}

                {open && it.action === "conflicts" && (
                  <div className="todo-body">
                    {conflicts.map((c, ci) => (
                      <div key={c.id} className="dup-group">
                        <div className="dup-group-head num-sub">
                          冲突 {ci + 1} / {conflicts.length} · {c.entity_type}:{c.entity_id} ·{" "}
                          {c.property}
                        </div>
                        <div className="dup-side">
                          <span className="kv-k">A</span>
                          <span className="dup-sum">
                            <b className="mono">{c.value_a}</b> — {c.summary_a || c.memory_a}
                          </span>
                          <span className="num-sub">{c.agent_a}</span>
                        </div>
                        <div className="dup-side">
                          <span className="kv-k">B</span>
                          <span className="dup-sum">
                            <b className="mono">{c.value_b}</b> — {c.summary_b || c.memory_b}
                          </span>
                          <span className="num-sub">{c.agent_b}</span>
                        </div>
                        <div className="merge-actions">
                          <button
                            className="btn btn-sm"
                            disabled={busy}
                            onClick={() => void actResolve(c.id, "keep_a")}
                          >
                            留 A
                          </button>
                          <button
                            className="btn btn-sm"
                            disabled={busy}
                            onClick={() => void actResolve(c.id, "keep_b")}
                          >
                            留 B
                          </button>
                          <button
                            className="btn btn-sm"
                            disabled={busy}
                            onClick={() => void actResolve(c.id, "keep_both")}
                          >
                            都保留
                          </button>
                          <button
                            className="btn btn-sm btn-accent"
                            disabled={busy}
                            onClick={() => void actMerge(c.memory_a, c.memory_b)}
                          >
                            合并两者
                          </button>
                        </div>
                      </div>
                    ))}
                  </div>
                )}
              </div>
            );
          })
        )}
        <div className="card-foot num-sub">
          五类待办与总览页、侧栏角标用同一个判定条件（`maintenance_todos`）；处理后计数实时减少。
          {ov?.conflicts === null &&
            " 注意：冲突表尚未迁移（数据库还是旧版本），冲突一类暂不可用。"}
        </div>
      </section>

      {/* T-M2-07 活动日志：每条可撤销的操作都能回滚到执行前；撤销本身也留痕 */}
      <section className="card">
        <div className="card-head">
          <span>最近操作</span>
        </div>
        {activity.length === 0 ? (
          <div className="empty">
            <b>还没有操作记录</b>
          </div>
        ) : (
          activity.map((a) => (
            <div key={a.event_id} className="act-row">
              <span className="badge">{OP_LABEL[a.operation] ?? a.operation}</span>
              <span className="act-sum">{a.summary || a.memory_id || "—"}</span>
              <span className="num-sub">
                {a.agent_id || "—"} · {fmtDate(a.created_at, "datetime")}
              </span>
              {a.undone ? (
                <span className="num-sub">已撤销</span>
              ) : a.undoable ? (
                <button className="btn btn-sm" disabled={busy} onClick={() => void actUndo(a.event_id)}>
                  撤销
                </button>
              ) : a.operation === "forget:hard" ? (
                <span className="num-sub">不可撤销</span>
              ) : (
                <span />
              )}
            </div>
          ))
        )}
        <div className="card-foot num-sub">
          点「撤销」会把该操作回滚到执行前；撤销本身也写进审计链（`undo_of` 指向原事件）。
          彻底删除**不可撤销**（数据已物理删除，只有 journal 快照）。
        </div>
      </section>
    </>
  );
}

function StatusPage({ procs, ov, onChanged }: { procs: ProcStatus[]; ov: Overview | null; onChanged: () => void }) {
  // T-M5-03：崩溃自愈的可观测性——重启次数与最近退出码
  const [launcher, setLauncher] = useState<LauncherEntry[]>([]);
  const [events, setEvents] = useState<LauncherEvent[]>([]);
  // T-M5-05：备份状态 / 端口一览
  const [backups, setBackups] = useState<BackupEntry[]>([]);
  const [ports, setPorts] = useState<PortProbe[]>([]);
  // T-M9-01：一键体检（每项可点开看判据与明细；空库相关项标 waived 而不是假绿）
  const [checks, setChecks] = useState<CheckItem[]>([]);
  const [openCheck, setOpenCheck] = useState<string | null>(null);
  // T-M10-06（J6）：注入统计——**不跟 3 秒轮询**（那是进程状态的节奏），
  // 只在切窗口时读一次；读的是 metrics JSONL，不碰库。
  const [inj, setInj] = useState<InjectionStats | null>(null);
  const [injDays, setInjDays] = useState(7);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    injectionStats(injDays)
      .then((s) => {
        if (alive) setInj(s);
      })
      .catch(() => {
        if (alive) setInj(null);
      });
    return () => {
      alive = false;
    };
  }, [injDays]);

  const load = useCallback(async () => {
    try {
      const [st, ev, bk, pr, ck] = await Promise.all([
        launcherState(),
        launcherEvents(5),
        listBackups(),
        portProbes(),
        healthCheckup(),
      ]);
      setLauncher(st);
      setEvents(ev);
      setBackups(bk);
      setPorts(pr);
      setChecks(ck);
    } catch {
      // 读不到运行态不算致命（应用可能刚启动）
      setLauncher([]);
    }
  }, []);

  useEffect(() => {
    void load();
    const t = setInterval(() => void load(), 3000);
    return () => clearInterval(t);
  }, [load]);

  /** 备份超期判据：最近一份 > 7 天（或从未备份）就该提醒。 */
  const backupStale = (): boolean => {
    if (backups.length === 0) return true;
    const age = Date.now() / 1000 - backups[0].modified;
    return age > 7 * 86400;
  };

  const actBackupNow = async () => {
    if (busy) return;
    setBusy(true);
    try {
      const r = await createBackup();
      setMsg(r.matches ? `备份完成且校验一致（${r.memories} 条记忆）` : "备份已写入但校验不一致");
      await load();
      onChanged();
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  const actReembed = async () => {
    if (busy) return;
    setBusy(true);
    try {
      const r = await reembedPending();
      setMsg(
        r.degraded.length > 0
          ? `补算跳过：${r.degraded.join("；")}`
          : `补算完成：尝试 ${r.tried}，成功 ${r.ok}，失败 ${r.failed}`,
      );
      await load();
      // 补算会改向量状态：必须刷新 overview，否则「已算/待算」停在旧值，用户以为没生效
      onChanged();
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  const restartsOf = (role: string) => launcher.find((l) => l.role === role)?.restarts ?? 0;

  return (
    <>
      <PageHead
        title="状态"
        desc={`${procs.filter((p) => p.alive).length}/3 进程健康 · 每 3 秒刷新`}
      />
      {/* T-M10-06（J6）：注入统计——回答"开这个值不值"。
          数据源是 proxy 的 metrics JSONL（只读）；四种空态分别说明，不给 0% 假绿。 */}
      <section className="card">
        <div className="card-head">
          <span>注入统计</span>
          <span className="num-sub" style={{ display: "flex", gap: 6 }}>
            {[
              { d: 1, label: "今日" },
              { d: 7, label: "7 天" },
              { d: 30, label: "30 天" },
            ].map((o) => (
              <button
                key={o.d}
                className={`btn btn-sm ${injDays === o.d ? "btn-accent" : ""}`}
                onClick={() => setInjDays(o.d)}
              >
                {o.label}
              </button>
            ))}
          </span>
        </div>
        <div style={{ padding: "4px 14px 12px" }}>
          {inj === null ? (
            <p className="num-sub">读取中…</p>
          ) : inj.empty ? (
            <p className="num-sub" style={{ lineHeight: 1.7 }}>
              {inj.reason === "no-dir" &&
                "还没有注入统计：proxy 从没写过 metrics。注入默认关闭，需在 agent 侧设 ORBITAL_INJECT=1（并让请求走 proxy）。"}
              {inj.reason === "no-file" &&
                `metrics 目录存在但还没有任何文件（${inj.dir}）——还没有请求经过 proxy。`}
              {inj.reason === "no-rows" &&
                `窗口内有文件但没有记录（${inj.dir}）——proxy 起来了但没有把请求转到它。`}
              {inj.reason === "all-off" &&
                `窗口内有 ${inj.total_rows} 次请求，但全部未注入（status=off）——检查 ORBITAL_INJECT=1 是否生效、以及 agent 是否真的走了 proxy。`}
              {inj.reason === "all-skipped" &&
                `窗口内有 ${inj.total_rows} 次请求，但一次都没注入成功（${inj.skipped_rows} 次被跳过/超时）——常见原因：core 的召回端点没起，或 300ms 超时被触发。`}
            </p>
          ) : (
            <>
              <div style={{ display: "flex", gap: 28, flexWrap: "wrap", alignItems: "baseline" }}>
                <div>
                  <b style={{ fontSize: 21 }}>{fmtNum(inj.injected_rows)}</b>
                  <span className="num-sub"> 次注入（共 {fmtNum(inj.total_rows)} 次请求）</span>
                </div>
                <div>
                  <b style={{ fontSize: 21 }}>{fmtNum(inj.hits_total)}</b>
                  <span className="num-sub"> 条记忆</span>
                </div>
                <div>
                  <b style={{ fontSize: 21 }}>{fmtNum(inj.tokens_total)}</b>
                  <span className="num-sub"> token</span>
                </div>
              </div>
              <p className="num-sub" style={{ marginTop: 8 }}>
                缓存命中{" "}
                {inj.total_rows > 0 ? Math.round((inj.cache_hit_rows / inj.total_rows) * 100) : 0}% · 超时{" "}
                {fmtNum(inj.timeout_rows)} 次 · 未注入（关/无命中）{fmtNum(inj.off_rows)} 次 · P50 {inj.p50_ms}ms /
                P95 {inj.p95_ms}ms
                {inj.bad_lines > 0 && ` · 坏数据 ${fmtNum(inj.bad_lines)} 行`}
              </p>
              {/* 趋势：复用总览页的 trend-col（不引新图表库，docs/10 只特批 recharts+Cytoscape） */}
              <div style={{ display: "flex", gap: 4, alignItems: "flex-end", height: 64, marginTop: 10 }}>
                {(() => {
                  const max = Math.max(1, ...inj.daily.map((d) => d.hits));
                  return inj.daily.map((d) => (
                    <div
                      key={d.day}
                      title={`${d.day}：${d.hits} 条 / ${d.tokens} token`}
                      style={{ flex: 1, display: "flex", flexDirection: "column", alignItems: "center", gap: 2 }}
                    >
                      <div
                        style={{
                          width: "70%",
                          height: Math.max(2, Math.round((d.hits / max) * 44)),
                          background: "var(--accent)",
                          borderRadius: 2,
                          opacity: 0.75,
                        }}
                      />
                      <span className="num-sub" style={{ fontSize: 10 }}>
                        {d.day.slice(5)}
                      </span>
                    </div>
                  ));
                })()}
              </div>
              <p className="num-sub" style={{ marginTop: 6 }}>
                原始文件：{inj.dir} · 当前不区分 agent（JSONL 无该字段）
              </p>
            </>
          )}
        </div>
      </section>
      {/* T-M9-01：一键体检——每项可点开看判据与明细；空库相关项标 waived，不给假绿 */}
      <section className="card">
        <div className="card-head">
          <span>一键体检</span>
          <span className="num-sub">
            通过 {checks.filter((c) => c.status === "pass").length} · 失败{" "}
            {checks.filter((c) => c.status === "fail").length} · 免检{" "}
            {checks.filter((c) => c.status === "waived").length}
            {checks.length === 0 ? "（正在体检…）" : ` / 共 ${checks.length} 项`}
          </span>
        </div>
        <div style={{ padding: "4px 14px 12px" }}>
          {checks.map((c) => (
            <div key={c.id}>
              <button
                className="act-row"
                style={{ width: "100%", textAlign: "left", background: "none", border: "none", cursor: "pointer" }}
                onClick={() => setOpenCheck(openCheck === c.id ? null : c.id)}
                aria-expanded={openCheck === c.id}
              >
                <span
                  className={`badge ${c.status === "pass" ? "badge-ok" : c.status === "fail" ? "badge-warn" : "badge-mute"}`}
                >
                  {c.status === "pass" ? "通过" : c.status === "fail" ? "失败" : "免检"}
                </span>
                <span className="act-sum">{c.title}</span>
                <span className="num-sub">{c.detail}</span>
                <span className="num-sub">{openCheck === c.id ? "▾" : "▸"}</span>
              </button>
              {openCheck === c.id && (
                <div className="num-sub" style={{ padding: "2px 8px 8px 8px", borderLeft: "2px solid var(--border)" }}>
                  <div>
                    <b>判据</b>：{c.criterion}
                  </div>
                  <div>
                    <b>明细</b>：{c.detail}
                  </div>
                  {c.status === "waived" && (
                    <div>
                      <b>为什么免检</b>：没有样本可判——让它"顺利通过"就是假绿（例如空库的"重复组≤5"当然成立）
                    </div>
                  )}
                </div>
              )}
            </div>
          ))}
        </div>
      </section>
      {msg && (
        <section className="card">
          <div className="num-sub" style={{ padding: "6px 14px" }}>
            {msg}
          </div>
        </section>
      )}
      <div className="stats">
        {procs.map((p) => (
          <div key={p.role} className={`stat-card ${p.alive ? "stat-ok" : "stat-danger"}`}>
            <div className="stat-value">{p.alive ? "运行" : "不可达"}</div>
            <div className="stat-label">
              {p.role === "gui" ? "GUI 界面" : p.role === "core" ? "Core 记忆核心" : "Proxy 注入代理"}
            </div>
            <div className="stat-sub mono">
              pid {p.pid ?? "—"} · 端口 {p.port ?? "—"}
            </div>
            {p.role !== "gui" && (
              <div className="stat-sub num-sub">
                崩溃自愈重启 {restartsOf(p.role)} 次（上限 3 次）
              </div>
            )}
            {!p.alive && p.error && <div className="stat-sub err-line">{p.error}</div>}
          </div>
        ))}
      </div>

      {/* T-M5-05 备份状态 */}
      <section className="card">
        <div className="card-head">
          <span>备份状态</span>
          <button className="btn btn-sm" disabled={busy} onClick={() => void actBackupNow()}>
            立即备份
          </button>
        </div>
        <div style={{ padding: "6px 14px 12px" }}>
          {backups.length === 0 ? (
            <div className="err-line">从未备份 —— 建议立即备份一次</div>
          ) : (
            <div className="num-sub">
              最近一份：<b>{fmtDate(backups[0].modified, "datetime")}</b>（
              {(backups[0].size_bytes / 1024).toFixed(0)} KB）· 共 {backups.length} 份
              {backupStale() && (
                <span className="err-line" style={{ marginLeft: 8 }}>
                  已超过 7 天未备份，建议立即备份
                </span>
              )}
            </div>
          )}
        </div>
      </section>

      {/* T-M5-05 向量健康：覆盖 M3 遗留的「有端点补算成功路径」 */}
      <section className="card">
        <div className="card-head">
          <span>向量健康</span>
          <button className="btn btn-sm" disabled={busy} onClick={() => void actReembed()}>
            补算缺失向量
          </button>
        </div>
        <div style={{ padding: "6px 14px 12px" }}>
          <div className="num-sub">
            已算 <b>{fmtNum(ov?.embeddings_ok ?? 0)}</b> · 待算{" "}
            <b>{fmtNum(ov?.embeddings_pending ?? 0)}</b>（共 {fmtNum(ov?.total ?? 0)} 条）
            {ov && ov.embeddings_pending > 0 && (
              <span style={{ marginLeft: 8 }}>语义召回对这几条暂不生效（关键词召回不受影响）</span>
            )}
          </div>
        </div>
      </section>

      {/* T-M5-05 端口一览：对照 M5-03 确立的端口表 */}
      <section className="card">
        <div className="card-head">
          <span>端口一览</span>
          <span className="num-sub">端口表：core 8791 · proxy 8790 · recall 8799</span>
        </div>
        <div style={{ padding: "4px 14px 12px" }}>
          {ports.map((p) => (
            <div key={p.port} className="act-row">
              <span className={`badge ${p.ok ? "" : "err-line"}`}>{p.ok ? "正常" : "异常"}</span>
              <span className="mono" style={{ width: 60 }}>
                {p.port}
              </span>
              <span className="act-sum">应由 {p.expected} 提供</span>
              <span className="num-sub">
                实际 {p.served_by ?? "无"} · {p.note}
              </span>
            </div>
          ))}
        </div>
        <div className="card-foot num-sub">
          端口表在 <code>HANDOFF.md</code>；core 的 health 与召回端点「必须」分开（M5-03 曾因同端口
          导致 core 起不来）。
        </div>
      </section>

      {events.length > 0 && (
        <section className="card">
          <div className="card-head">
            <span>最近的重启记录</span>
          </div>
          <div style={{ padding: "4px 14px 12px" }}>
            {events
              .slice()
              .reverse()
              .map((e, i) => (
                <div key={`${e.at}-${i}`} className="act-row">
                  <span className="badge">{e.event === "gave_up" ? "已放弃" : "已重启"}</span>
                  <span className="act-sum">{e.role}</span>
                  <span className="num-sub">
                    退出码 {e.exit_code ?? "—"} · 第 {e.restarts} 次 ·{" "}
                    {fmtDate(e.at, "datetime")}
                  </span>
                </div>
              ))}
          </div>
        </section>
      )}
    </>
  );
}

function SettingsPage({ ov }: { ov: Overview | null }) {
  const [backups, setBackups] = useState<BackupEntry[]>([]);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);
  // T-M5-02：恢复是破坏性操作，必须"选备份 → 输入「恢复」→ 确认"
  const [restoreTarget, setRestoreTarget] = useState<BackupEntry | null>(null);
  const [restoreWord, setRestoreWord] = useState("");
  // T-M5-04：诊断包导出结果
  const [diag, setDiag] = useState<DiagReport | null>(null);
  // T-M8-01：设置页 6 组 + 页内搜索
  const [search, setSearch] = useState("");
  const [onb, setOnb] = useState<OnboardingInfo | null>(null);
  const [conns, setConns] = useState<AgentConnection[]>([]);
  useEffect(() => {
    onboardingInfo().then(setOnb).catch(() => setOnb(null));
    agentConnections().then(setConns).catch(() => setConns([]));
  }, []);

  // T-M10-11（ADR-051）：上游与注入配置
  const [proxyCfg, setProxyCfg] = useState<ProxyConfig | null>(null);
  const [upstream, setUpstream] = useState("");
  const [inject, setInject] = useState(false);
  const [proxyMsg, setProxyMsg] = useState<string | null>(null);
  // T-M10-26：四家 agent 的注入适配器部署状态（只读检测）
  const [injectStates, setInjectStates] = useState<InjectState[]>([]);
  useEffect(() => {
    injectStatus().then(setInjectStates).catch(() => setInjectStates([]));
  }, []);
  useEffect(() => {
    proxyConfig()
      .then((c) => {
        setProxyCfg(c);
        setUpstream(c.upstream);
        setInject(c.inject);
      })
      .catch(() => setProxyCfg(null));
  }, []);
  /** 保存配置 → 后端写 proxy.conf.json → kill proxy 子进程（watchdog 自动拉起新进程读新配置）。 */
  const actSaveProxy = async () => {
    if (busy) return;
    setBusy(true);
    try {
      const r = await saveProxyConfig(upstream, inject);
      setProxyMsg(
        r.saved
          ? `已保存${r.restarted ? "，proxy 正在重启（约 1 秒后生效）" : "（未找到运行中的 proxy，下次启动生效）"}`
          : "保存失败",
      );
      setProxyCfg(await proxyConfig());
    } catch (e) {
      setProxyMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  // T-M10-27：一键部署 / 移除 / 自检（本版只支持 WorkBuddy）
  const [onboardMsg, setOnboardMsg] = useState<string | null>(null);
  // T-M10-29：反馈用 **toast**（fixed 定位，不依赖滚动位置——自检按钮曾被滚到视口外，
  // 页面内反馈块看不到；toast 悬浮在底部，任何滚动位置都可见）。4 秒自动消失。
  useEffect(() => {
    if (!onboardMsg) return;
    const t = setTimeout(() => setOnboardMsg(null), 4000);
    return () => clearTimeout(t);
  }, [onboardMsg]);
  const refreshInject = async () => {
    try {
      setInjectStates(await injectStatus());
    } catch {
      setInjectStates([]);
    }
  };
  const actDeploy = async (id: string) => {
    if (busy) return;
    setBusy(true);
    try {
      const r = await deployInject(id);
      setOnboardMsg(`已部署 ${r.agent}：区块 ${r.block_chars} 字符 · 备份 ${r.backup}`);
      await refreshInject();
    } catch (e) {
      setOnboardMsg(String(e));
    } finally {
      setBusy(false);
    }
  };
  const actRemove = async (id: string) => {
    if (busy) return;
    setBusy(true);
    try {
      const r = await removeInject(id);
      setOnboardMsg(
        r.removed
          ? `已移除 ${id}${r.backup ? `（备份 ${r.backup}）` : "（可一键恢复）"}`
          : `未改动：${r.note ?? ""}`,
      );
      await refreshInject();
    } catch (e) {
      setOnboardMsg(String(e));
    } finally {
      setBusy(false);
    }
  };
  const actVerifyInject = async (id: string) => {
    if (busy) return;
    setBusy(true);
    try {
      const r = await verifyInject(id);
      setOnboardMsg(`自检 ${r.agent}：召回 ${r.recall_count} 条 — ${r.note}`);
    } catch (e) {
      setOnboardMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  const loadBackups = useCallback(async () => {
    try {
      setBackups(await listBackups());
    } catch {
      // 目录还不存在 = 还没有备份，不是错误
      setBackups([]);
    }
  }, []);
  useEffect(() => {
    void loadBackups();
  }, [loadBackups]);

  /** T-M5-01：立即备份（在线备份 API）+ 自动校验。 */
  const actBackup = async () => {
    if (busy) return;
    setBusy(true);
    try {
      const r = await createBackup();
      setMsg(
        r.matches
          ? `备份完成且校验一致（${r.memories} 条记忆 · schema v${r.schema_version}）`
          : `备份已写入，但校验不一致：${r.path}`,
      );
      await loadBackups();
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  const actVerify = async (path: string) => {
    if (busy) return;
    setBusy(true);
    try {
      const r = await verifyBackup(path);
      setMsg(
        r.matches
          ? `校验通过：integrity ok、schema v${r.schema_version}、${r.memories} 条记忆与当前库一致`
          : `校验不一致：备份 ${r.memories} 条 / 当前 ${r.current_memories} 条`,
      );
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  /** T-M5-02：恢复（第一步只"排队"，重启后才真正替换；当前库会自动保底备份）。 */
  const actRestore = async () => {
    if (!restoreTarget || busy || restoreWord !== "恢复") return;
    setBusy(true);
    try {
      const r = await stageRestore(restoreTarget.path);
      setMsg(
        `已排队恢复（备份 ${r.memories} 条 · schema v${r.schema_version}）。` +
          `请完全退出并重新打开 Orbital Pro；当前库会先自动备份为 pre-restore-*.db`,
      );
      setRestoreTarget(null);
      setRestoreWord("");
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  // T-M7-02：恢复确认弹层支持 Esc 关闭（键盘可达）
  useEffect(() => {
    if (!restoreTarget) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setRestoreTarget(null);
        setRestoreWord("");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [restoreTarget]);

  /** T-M5-04：导出诊断包（日志 + launcher + db-info + env，已脱敏）。 */
  const actDiag = async () => {
    if (busy) return;
    setBusy(true);
    try {
      const r = await exportDiagnostics();
      setDiag(r);
      setMsg(`诊断包已导出：${r.files.length} 个文件（脱敏 ${r.redacted_lines} 行）`);
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  // T-M8-01：设置页 6 组（docs/05 M6 6.3）+ 页内搜索。
  // 搜索命中标题或内容里任一关键词就显示该组；空搜索显示全部。
  const hit = (s: string) => {
    const q = search.trim();
    if (!q) return true;
    return s.includes(q);
  };
  const groups: { key: string; title: string; body: ReactElement }[] = [
    {
      key: "general",
      title: "通用",
      body: (
        <dl className="kv">
          <div className="kv-item">
            <dt className="kv-k">主题</dt>
            <dd className="kv-v">跟随系统（浅色 / 深色自动切换）</dd>
          </div>
          <div className="kv-item">
            <dt className="kv-k">快捷键</dt>
            <dd className="kv-v">
              <code>Ctrl+K</code> 跳到搜索 · <code>Esc</code> 关闭弹层
            </dd>
          </div>
          <div className="kv-item">
            <dt className="kv-k">键盘可达</dt>
            <dd className="kv-v">Tab 逐项移动，焦点环可见（M7-02 已真机验证）</dd>
          </div>
        </dl>
      ),
    },
    {
      key: "agents",
      title: "Agent 接入",
      body: (
        <div style={{ padding: "4px 14px 12px" }}>
          {conns.length === 0 ? (
            <div className="num-sub">正在探测本机 agent…</div>
          ) : (
            conns.map((c) => (
              <div key={c.id} className="act-row">
                <span className="act-sum">{c.display_name}</span>
                <span className="num-sub mono" style={{ maxWidth: 420, overflow: "hidden" }}>
                  {c.config_path}
                </span>
                <span className={`badge ${c.attached ? "badge-ok" : "badge-warn"}`}>
                  {c.attached ? "已接入" : c.exists ? "未接入" : "未找到配置"}
                </span>
              </div>
            ))
          )}
          <div className="num-sub" style={{ marginTop: 6 }}>
            接入/断开在「连接」页操作（会先备份原配置）。
          </div>
        </div>
      ),
    },
    {
      key: "upstream",
      title: "上游与注入",
      body: (
        <div style={{ padding: "4px 14px 12px" }}>
          <div className="num-sub" style={{ marginBottom: 10, lineHeight: 1.7 }}>
            注入 = 每次对话自动把相关记忆塞进请求（agent 无感）。默认关闭；
            <b>只注入 scope=shared 的记忆</b>（private 不会被别的 agent 看到）。
          </div>
          <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" }}>
            <span className="num-sub" style={{ width: 84 }}>
              上游 API
            </span>
            <input
              className="input"
              style={{ width: 460 }}
              placeholder="http://127.0.0.1:11434 或 https://api.example.com"
              value={upstream}
              onChange={(e) => setUpstream(e.target.value)}
            />
          </div>
          <div style={{ display: "flex", gap: 8, alignItems: "center", marginTop: 10 }}>
            <span className="num-sub" style={{ width: 84 }}>
              自动注入
            </span>
            <label style={{ display: "flex", gap: 6, alignItems: "center", cursor: "pointer" }}>
              <input type="checkbox" checked={inject} onChange={(e) => setInject(e.target.checked)} />
              <span>把相关记忆注入每次对话（只注入 shared）</span>
            </label>
          </div>
          <div style={{ display: "flex", gap: 10, marginTop: 12, alignItems: "center", flexWrap: "wrap" }}>
            <button className="btn btn-sm btn-accent" onClick={() => void actSaveProxy()} disabled={busy}>
              保存并重启 proxy
            </button>
            <span className="num-sub">
              当前生效来源：{proxyCfg?.exists ? "proxy.conf.json（界面可改）" : "环境变量 / 默认（还没保存过）"}
            </span>
          </div>
          {proxyMsg && (
            <div className="num-sub" style={{ marginTop: 8 }}>
              {proxyMsg}
            </div>
          )}
          <div className="num-sub" style={{ marginTop: 8, wordBreak: "break-all" }}>
            配置文件：{proxyCfg?.path ?? "—"}
          </div>
          <div className="num-sub" style={{ marginTop: 6 }}>
            提示：保存后 proxy 会重启（约 1 秒）；「状态」页的「注入统计」能看到注入次数与 token。
          </div>
        </div>
      ),
    },
    {
      key: "inject-agents",
      title: "记忆注入接入",
      body: (
        <div style={{ padding: "4px 14px 12px" }}>
          <div className="num-sub" style={{ marginBottom: 10, lineHeight: 1.7 }}>
            让各个 agent 在对话时<b>自动带上</b> Orbital 的记忆。每家机制不同（文件 / hook / 插件 / 扩展），
            这里只显示<b>是否已部署</b>；部署方式见任务卡 T-M10-22/23。
          </div>
          {injectStates.length === 0 ? (
            <div className="num-sub">正在检测…</div>
          ) : (
            injectStates.map((s) => (
              <div key={s.id} className="act-row">
                <span className="act-sum">{s.name}</span>
                <span className="num-sub mono" style={{ maxWidth: 240, overflow: "hidden" }}>
                  {s.kind}
                </span>
                <span className={`badge ${s.deployed ? "badge-ok" : "badge-warn"}`}>
                  {s.deployed ? "已部署" : "未部署"}
                </span>
                <span className="num-sub" style={{ flex: 1 }}>
                  {s.detail}
                  {s.probe_source && (
                    <span style={{ opacity: 0.6 }}> · 位置来源：{s.probe_source}</span>
                  )}
                </span>
                {(s.id === "workbuddy" || s.id === "hermes" || s.id === "pi" || s.id === "dsh") && (
                  <>
                    {!s.deployed && (
                      <button
                        className="btn btn-sm btn-accent"
                        onClick={() => void actDeploy(s.id)}
                        disabled={busy}
                      >
                        一键部署
                      </button>
                    )}
                    {s.deployed && (
                      <button
                        className="btn btn-sm"
                        onClick={() => void actRemove(s.id)}
                        disabled={busy}
                      >
                        移除
                      </button>
                    )}
                  </>
                )}
                <button className="btn btn-sm" onClick={() => void actVerifyInject(s.id)} disabled={busy}>
                  自检
                </button>
              </div>
            ))
          )}
          <div className="num-sub" style={{ marginTop: 8 }}>
            提示：改完适配器要重启对应 agent 才生效（DSH 重启 / pi 重启或 /reload / Hermes 下一轮自动）。
          </div>
        </div>
      ),
    },
    {
      key: "memory",
      title: "记忆与存储",
      body: (
        <>
          <dl className="kv">
            <div className="kv-item">
              <dt className="kv-k">schema 版本</dt>
              <dd className="kv-v mono">v{ov?.db_version ?? "?"}</dd>
            </div>
            <div className="kv-item">
              <dt className="kv-k">记忆总数</dt>
              <dd className="kv-v mono">
                {fmtNum(ov?.total ?? 0)}
                {(ov?.total_history ?? 0) > 0 && (
                  <span className="num-sub" style={{ marginLeft: 6 }}>
                    （含历史版本 {fmtNum(ov?.total_history ?? 0)} 条）
                  </span>
                )}
              </dd>
            </div>
            <div className="kv-item">
              <dt className="kv-k">数据目录</dt>
              <dd className="kv-v mono" style={{ wordBreak: "break-all" }}>
                {onb?.data_dir ?? "—"}
              </dd>
            </div>
          </dl>
          <div className="card-foot num-sub">
            记忆库是单个 SQLite 文件；卸载程序「不会」删除这里的数据。
          </div>
        </>
      ),
    },
    {
      key: "retrieval",
      title: "模型与检索",
      body: (
        <>
          <dl className="kv">
            <div className="kv-item">
              <dt className="kv-k">向量已算</dt>
              <dd className="kv-v mono">{fmtNum(ov?.embeddings_ok ?? 0)}</dd>
            </div>
            <div className="kv-item">
              <dt className="kv-k">向量待算</dt>
              <dd className="kv-v mono">{fmtNum(ov?.embeddings_pending ?? 0)}</dd>
            </div>
            <div className="kv-item">
              <dt className="kv-k">嵌入端点</dt>
              <dd className="kv-v">
                {onb?.embedding_configured ? "已配置（新记忆自动算向量）" : "未配置（仅关键词召回）"}
              </dd>
            </div>
            <div className="kv-item">
              <dt className="kv-k">召回条数上限</dt>
              <dd className="kv-v mono">200（超出会被截断，防一次读爆内存）</dd>
            </div>
          </dl>
          <div className="card-foot num-sub">
            检索 = 关键词 + 向量 + 图扩展三路融合（RRF）；补算入口在「状态」页。
          </div>
        </>
      ),
    },
    {
      key: "backup",
      title: "数据与备份",
      body: (
        <>
          {msg && (
            <div className="num-sub" style={{ padding: "6px 14px" }}>
              {msg}
            </div>
          )}
          {backups.length === 0 ? (
            <div className="num-sub" style={{ padding: "10px 14px" }}>
              还没有备份。点右上「立即备份」创建第一份（备份完会自动校验）。
            </div>
          ) : (
            <div style={{ padding: "4px 14px 12px" }}>
              {backups.slice(0, 10).map((b) => (
                <div key={b.name} className="act-row">
                  <span className="act-sum">{b.name}</span>
                  <span className="num-sub">
                    {(b.size_bytes / 1024).toFixed(0)} KB · {fmtDate(b.modified, "datetime")}
                  </span>
                  <button className="btn btn-sm" disabled={busy} onClick={() => void actVerify(b.path)}>
                    校验
                  </button>
                  <button
                    className="btn btn-sm"
                    disabled={busy}
                    onClick={() => {
                      setRestoreTarget(b);
                      setRestoreWord("");
                    }}
                  >
                    恢复
                  </button>
                </div>
              ))}
            </div>
          )}
          {restoreTarget && (
            <div className="card" style={{ margin: "8px 14px 12px", borderColor: "var(--warn)" }}>
              <div className="card-head">
                <span>确认从备份恢复</span>
              </div>
              <div style={{ padding: "8px 14px 12px" }}>
                <div className="num-sub">
                  将用 <b>{restoreTarget.name}</b> 覆盖当前库（{fmtNum(ov?.total ?? 0)} 条记忆）。
                  当前库会先自动备份为 <code>pre-restore-*.db</code>；恢复在「重启后」生效。
                </div>
                <div style={{ display: "flex", gap: 8, marginTop: 8, alignItems: "center" }}>
                  <input
                    className="input"
                    placeholder="输入「恢复」确认"
                    value={restoreWord}
                    onChange={(e) => setRestoreWord(e.target.value)}
                    style={{ width: 220 }}
                  />
                  <button
                    className="btn btn-sm btn-accent"
                    disabled={busy || restoreWord !== "恢复"}
                    onClick={() => void actRestore()}
                  >
                    确认恢复
                  </button>
                  <button
                    className="btn btn-sm"
                    disabled={busy}
                    onClick={() => {
                      setRestoreTarget(null);
                      setRestoreWord("");
                    }}
                  >
                    取消
                  </button>
                </div>
              </div>
            </div>
          )}
          <div style={{ padding: "4px 14px 12px", borderTop: "1px solid var(--border)" }}>
            <div className="num-sub" style={{ marginBottom: 6 }}>
              日志与诊断：日志按天分文件（gui/core/proxy 各一份，保留 7 天）
            </div>
            <button className="btn btn-sm" disabled={busy} onClick={() => void actDiag()}>
              导出诊断包
            </button>
            {diag && (
              <div style={{ marginTop: 6 }}>
                <div className="num-sub">
                  已导出 {diag.files.length} 个文件、脱敏 {diag.redacted_lines} 行
                </div>
                <div className="mono" style={{ wordBreak: "break-all" }}>
                  {diag.dir}
                </div>
                <div className="num-sub">{diag.files.join(" · ")}</div>
              </div>
            )}
          </div>
          <div className="card-foot num-sub">
            用 SQLite「在线备份 API」（不是文件拷贝——WAL 下拷贝会漏数据）；诊断包不含记忆正文与凭据。
          </div>
        </>
      ),
    },
    {
      key: "about",
      title: "关于",
      body: (
        <>
          <dl className="kv">
            <div className="kv-item">
              <dt className="kv-k">版本</dt>
              <dd className="kv-v mono">{APP_VERSION}</dd>
            </div>
            <div className="kv-item">
              <dt className="kv-k">端口表</dt>
              <dd className="kv-v mono">core 8791 · proxy 8790 · recall 8799</dd>
            </div>
            <div className="kv-item">
              <dt className="kv-k">MCP 二进制</dt>
              <dd className="kv-v mono" style={{ wordBreak: "break-all" }}>
                {onb?.mcp_path ?? "未找到（安装包应自带）"}
              </dd>
            </div>
          </dl>
          <div className="card-foot num-sub">
            三进程模型（ADR-004）：同一 exe 以 --role core/proxy 拉起；卸载不会删除记忆库。
          </div>
        </>
      ),
    },
  ];

  const visible = groups.filter((g) => hit(g.title) || hit(g.key) || hit(g.title));

  return (
    <>
      {/* T-M10-29：操作反馈 toast —— fixed 定位、任何滚动位置可见、4s 自动消失 */}
      {onboardMsg && (
        <div
          role="status"
          onClick={() => setOnboardMsg(null)}
          style={{
            position: "fixed",
            bottom: 28,
            left: "50%",
            transform: "translateX(-50%)",
            background: "var(--fg, #111)",
            color: "var(--bg, #fff)",
            padding: "10px 18px",
            borderRadius: 8,
            boxShadow: "0 6px 24px rgba(0,0,0,.28)",
            zIndex: 9999,
            maxWidth: "70vw",
            fontSize: 13,
            lineHeight: 1.5,
            cursor: "pointer",
          }}
        >
          {onboardMsg}
        </div>
      )}
      <PageHead
        title="设置"
        desc="6 组：通用 / Agent 接入 / 记忆与存储 / 模型与检索 / 数据与备份 / 关于"
        right={
          <input
            className="input"
            placeholder="页内搜索（如 备份 / 端口 / 向量）"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            style={{ width: 260 }}
          />
        }
      />
      {search.trim() && (
        <div className="num-sub" style={{ marginBottom: 8 }}>
          匹配 {visible.length} / {groups.length} 组
        </div>
      )}
      {groups.map((g) => (
        <section
          key={g.key}
          className="card"
          style={visible.some((v) => v.key === g.key) ? undefined : { display: "none" }}
        >
          <div className="card-head">
            <span>{g.title}</span>
            {g.key === "backup" && (
              <button className="btn btn-sm btn-accent" disabled={busy} onClick={() => void actBackup()}>
                立即备份
              </button>
            )}
          </div>
          {g.body}
        </section>
      ))}
    </>
  );
}
