/**
 * 总览页（首页）—— 一屏回答三个问题：
 *   ① 我有多少记忆（分层 + 今日新增）
 *   ② 谁在给我写（**条形图**：长度=数量，颜色=身份）
 *   ③ 需要我做什么（进程健康 + 待办）
 *
 * 设计原则：
 * - **每个数字都是入口**（红线 3：不是看板）。点「记忆总数」去记忆页，点 agent 去连接页。
 * - **能用图就不用文字**：数量差异用条形长度表达，一眼能看出"谁写得多"。
 * - 只用 token（红线 4）。
 * - 数据全来自 `overview` 一次调用；失败**如实报错**，不显示假数据。
 */

import { useEffect, useState } from "react";
import {
  type AgentStat,
  type Overview,
  type ProcStatus,
  fmtNum,
  invoke,
  relTime,
} from "../lib/api";

const KNOWN_AGENTS = ["dsh", "hermes", "pi", "workbuddy"] as const;
const AGENT_LABEL: Record<string, string> = {
  dsh: "DSH",
  hermes: "Hermes",
  pi: "pi",
  workbuddy: "WorkBuddy",
};

function StatCard(props: {
  label: string;
  value: string;
  sub?: string;
  tone?: "normal" | "ok" | "warn" | "danger";
  /** 0..1 的微进度（例如"今日新增 / 总量"），不给就不画 */
  progress?: number;
  onClick?: () => void;
}) {
  const { label, value, sub, tone = "normal", progress, onClick } = props;
  return (
    <button
      type="button"
      className={`stat-card stat-${tone} ${onClick ? "stat-clickable" : ""}`}
      onClick={onClick}
      disabled={!onClick}
    >
      <div className="stat-value">{value}</div>
      <div className="stat-label">{label}</div>
      {progress !== undefined && (
        <div className="stat-progress">
          <i style={{ width: `${Math.min(100, Math.max(0, progress * 100))}%` }} />
        </div>
      )}
      {sub && <div className="stat-sub">{sub}</div>}
    </button>
  );
}

/** 一个 agent 的条形行：条长 = 该 agent 记忆数 / 最大者。 */
function AgentBar(props: {
  id: string;
  label: string;
  stat?: AgentStat;
  max: number;
  total: number;
  onClick: () => void;
}) {
  const { id, label, stat, max, total, onClick } = props;
  const n = stat?.total ?? 0;
  const pct = total > 0 ? (n / Math.max(1, max)) * 100 : 0;
  const today = stat?.today ?? 0;
  // 今日新增占该 agent 的比例（叠在条里用更深的色）
  const todayPct = n > 0 ? (today / n) * 100 : 0;

  return (
    <button type="button" className="abar-row" data-agent={id} onClick={onClick}>
      <span className={`dot ${n > 0 ? "dot-on" : "dot-off"}`} data-agent={id} />
      <span className="abar-name">{label}</span>

      <span className="abar-track">
        <span className="abar-fill" data-agent={id} style={{ width: `${pct}%` }}>
          {today > 0 && (
            <span className="abar-today" data-agent={id} style={{ width: `${todayPct}%` }} />
          )}
        </span>
      </span>

      <span className="abar-num">{n > 0 ? fmtNum(n) : "—"}</span>
      <span className="abar-meta num-sub">{n > 0 ? relTime(stat?.last_at ?? null) : "未写入"}</span>
    </button>
  );
}

export default function OverviewPage(props: {
  /** 切页；第二个参数是"打开记忆页时预设的待办筛选" */
  onGo: (page: string, todo?: string) => void;
}) {
  const { onGo } = props;
  const [ov, setOv] = useState<Overview | null>(null);
  const [procs, setProcs] = useState<ProcStatus[]>([]);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    const load = () =>
      invoke<Overview>("overview")
        .then((d) => {
          setOv(d);
          setErr(null);
        })
        .catch((e) => setErr(String(e)));
    void load();
    const t = setInterval(load, 10_000);
    return () => clearInterval(t);
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
  const pending = ov?.embeddings_pending ?? 0;
  const activeAgents = ov
    ? KNOWN_AGENTS.filter((a) => ov.agents.some((x) => x.agent_id === a && x.total > 0)).length
    : 0;

  const agentTotals = KNOWN_AGENTS.map(
    (id) => ov?.agents.find((a) => a.agent_id === id)?.total ?? 0,
  );
  const maxAgent = Math.max(1, ...agentTotals);
  const sumAgents = agentTotals.reduce((a, b) => a + b, 0);

  const l1 = ov?.counts.l1 ?? 0;
  const l0 = ov?.counts.l0 ?? 0;
  const lSum = Math.max(1, l0 + l1);

  return (
    <>
      <header className="page-head">
        <div>
          <h1>总览</h1>
          <p className="desc">
            {ov
              ? `${fmtNum(ov.total)} 条记忆 · ${activeAgents}/4 个 agent 在用 · 今日 +${ov.today}`
              : "读取中…"}
          </p>
        </div>
        <button className="btn" onClick={() => onGo("memory")}>
          看全部记忆
        </button>
      </header>

      {err && (
        <div className="card card-danger err-line">
          <strong>读取失败</strong>：{err}
          <div className="num-sub">（不是"暂无数据"——数据源确实没读成功）</div>
        </div>
      )}

      {/* ① 四个指标（带微进步条） */}
      <div className="stats">
        <StatCard
          label="记忆总数"
          value={ov ? fmtNum(ov.total) : "—"}
          sub={ov ? (ov.today > 0 ? `今日 +${ov.today}` : "今日无新增") : undefined}
          tone="ok"
          progress={ov && ov.total > 0 ? Math.min(1, ov.today / ov.total) : 0}
          onClick={() => onGo("memory")}
        />
        <StatCard
          label="在用 agent"
          value={ov ? `${activeAgents}/4` : "—"}
          sub={ov && sumAgents > 0 ? `共写入 ${fmtNum(sumAgents)} 条` : "还没有 agent 写入"}
          progress={activeAgents / 4}
          onClick={() => onGo("connect")}
        />
        <StatCard
          label="进程健康"
          value={`${alive}/3`}
          sub={alive === 3 ? "GUI + core + proxy" : "有进程不可达"}
          tone={alive === 3 ? "ok" : "danger"}
          progress={alive / 3}
          onClick={() => onGo("status")}
        />
        <StatCard
          label="待处理"
          value={ov ? String(pending) : "—"}
          sub={pending > 0 ? "缺向量的记忆" : "没有待办"}
          tone={pending > 0 ? "warn" : "normal"}
          onClick={() => onGo("maintain")}
        />
      </div>

      <div className="two-col">
        {/* ② 谁在给我写 —— 条形图（长度=数量，颜色=身份） */}
        <section className="card">
          <div className="card-head">
            <span>谁在给我写</span>
            <button className="btn btn-sm" onClick={() => onGo("connect")}>
              管理接入
            </button>
          </div>

          <div className="abar-list">
            {KNOWN_AGENTS.map((id) => (
              <AgentBar
                key={id}
                id={id}
                label={AGENT_LABEL[id]}
                stat={ov?.agents.find((a) => a.agent_id === id)}
                max={maxAgent}
                total={sumAgents}
                onClick={() => onGo("connect")}
              />
            ))}
          </div>

          <div className="abar-legend num-sub">
            <span>
              <i className="lg-strong" /> 今日新增
            </span>
            <span>
              <i className="lg-soft" /> 历史
            </span>
            <span className="abar-note">
              条长按最大者归一 · 合计 {fmtNum(sumAgents)} 条
            </span>
          </div>

          <div className="card-foot num-sub">
            依赖身份门控（ADR-005）：每个 agent 只能读自己私有的，共享的才互相可见。
          </div>
        </section>

        {/* ③ 记忆分层 */}
        <section className="card">
          <div className="card-head">
            <span>记忆分层</span>
            <span className="num-sub">ADR-031：按表统计</span>
          </div>

          <div className="layer-row" data-layer="l0">
            <div className="layer-name">
              <b>L0</b>
              <span className="num-sub">原始对话</span>
            </div>
            <div className="layer-bar">
              <i style={{ width: `${(l0 / lSum) * 100}%` }} className="bar-l0" />
            </div>
            <div className="layer-val">
              {fmtNum(l0)}
              {l0 === 0 && <span className="num-sub">M3 采集</span>}
            </div>
          </div>

          <div className="layer-row" data-layer="l1">
            <div className="layer-name">
              <b>L1</b>
              <span className="num-sub">原子记忆</span>
            </div>
            <div className="layer-bar">
              <i style={{ width: `${(l1 / lSum) * 100}%` }} className="bar-l1" />
            </div>
            <div className="layer-val">{fmtNum(l1)}</div>
          </div>

          <div className="layer-row muted-row" data-layer="l2">
            <div className="layer-name">
              <b>L2</b>
              <span className="num-sub">场景块</span>
            </div>
            <div className="layer-bar" />
            <div className="layer-val num-sub">M3 引入</div>
          </div>

          <div className="layer-row muted-row" data-layer="l3">
            <div className="layer-name">
              <b>L3</b>
              <span className="num-sub">用户画像</span>
            </div>
            <div className="layer-bar" />
            <div className="layer-val num-sub">建议不做</div>
          </div>

          <div className="card-foot num-sub">
            向量 {ov ? ov.embeddings_ok : "—"} 条已算 · {pending} 条待算 ·{" "}
            {ov ? (ov.db_version >= 2 ? `schema v${ov.db_version}` : `schema v${ov.db_version}（下次写入自动迁到 v2）`) : "schema —"}
          </div>
        </section>
      </div>

      {/* 近 7 天写入趋势（ADR-033：单色实心柱，长度编码数值，无渐变/阴影） */}
      <section className="card">
        <div className="card-head">
          <span>近 7 天写入</span>
          <span className="num-sub">
            合计 {fmtNum((ov?.trend ?? []).reduce((a, b) => a + b.n, 0))} 条
          </span>
        </div>
        <div className="trend">
          {(ov?.trend ?? []).map((d) => {
            const maxN = Math.max(1, ...(ov?.trend ?? []).map((x) => x.n));
            return (
              <div className="trend-col" key={d.day} title={`${d.day}：${d.n} 条`}>
                <span className="trend-n num">{d.n > 0 ? d.n : ""}</span>
                <span className="trend-track">
                  <i className="trend-fill" style={{ height: `${(d.n / maxN) * 100}%` }} />
                </span>
                <span className="trend-day num-sub">{d.day.slice(5)}</span>
              </div>
            );
          })}
          {!ov && <div className="num-sub">读取中…</div>}
        </div>
      </section>

      {/* ③ 我该做什么（第三层：每个数字都是入口，不是看板） */}
      <section className="card">
        <div className="card-head">
          <span>我该做什么</span>
          <span className="num-sub">点一行 → 记忆页按同一条件筛好</span>
        </div>
        <div className="todo-list">
          {[
            {
              key: "no-vector",
              label: "缺向量",
              note: "语义召回用不上",
              n: ov?.embeddings_pending ?? 0,
              unavailable: false,
              // 影响功能才警示（ADR-033 规则 3：正常状态保持灰）
              warn: true,
            },
            {
              key: "pending-extract",
              label: "待抽取",
              note:
                ov?.pending_extract == null
                  ? "conversations 表还没迁移出来（v2 后才有）"
                  : "对话已存、还没提炼成记忆",
              n: ov?.pending_extract ?? 0,
              unavailable: ov?.pending_extract == null,
              warn: true,
            },
            {
              key: "duplicate",
              label: "重复内容",
              note: "同 agent 同内容多条",
              n: ov?.duplicates ?? 0,
              unavailable: false,
              warn: false,
            },
            {
              key: "untagged",
              label: "未打标签",
              note: "按标签筛选会漏掉",
              n: ov?.untagged ?? 0,
              unavailable: false,
              warn: false,
            },
            {
              key: "stale",
              label: "90 天未访问",
              note: "遗忘候选",
              n: ov?.stale ?? 0,
              unavailable: false,
              warn: false,
            },
          ].map((t) => (
            <button
              key={t.key}
              type="button"
              className="todo-row"
              disabled={t.unavailable || t.n === 0}
              onClick={() => onGo("memory", t.key)}
            >
              <span className="todo-label">{t.label}</span>
              <span className="todo-note num-sub">{t.note}</span>
              <span className={`todo-n num ${t.warn && t.n > 0 ? "todo-non" : ""}`}>
                {t.unavailable ? "—" : fmtNum(t.n)}
              </span>
            </button>
          ))}
        </div>
      </section>

      {/* ④ 最近写入（时间轴样式） */}
      <section className="card">
        <div className="card-head">
          <span>最近写入</span>
          <button className="btn btn-sm" onClick={() => onGo("memory")}>
            全部 →
          </button>
        </div>
        {ov && ov.recent.length === 0 ? (
          <div className="empty">
            <b>还没有记忆</b>
            agent 通过 MCP 写入后会出现在这里。
          </div>
        ) : (
          <ul className="timeline">
            {ov?.recent.map((m) => (
              <li key={m.id} className="tl-row" data-agent={m.agent_id} onClick={() => onGo("memory")}>
                <span className="tl-dot" data-agent={m.agent_id} />
                <span className="tl-agent" data-agent={m.agent_id}>
                  {AGENT_LABEL[m.agent_id] ?? m.agent_id}
                </span>
                <span className="tl-text">{m.summary}</span>
                <span className="num-sub tl-time">{relTime(m.created_at)}</span>
              </li>
            ))}
          </ul>
        )}
      </section>
    </>
  );
}
