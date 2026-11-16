/**
 * 托盘面板（右键托盘图标弹出的紧凑视图，ADR-016 自绘面板）。
 *
 * 为什么单独一个视图：`index.html?panel=1` 以前没被消费，右键托盘实际上是
 * 把一个完整应用塞进 320×420 的窗口（布局被裁切）。这里按 `docs/05 M6 6.4`
 * 「一项一 Agent」做成紧凑面板。
 */
import { useEffect, useState } from "react";
import {
  type AgentConnection,
  type Overview,
  agentConnections,
  invoke,
  onboardingInfo,
} from "./lib/api";

const KNOWN: { id: string; label: string }[] = [
  { id: "dsh", label: "DSH" },
  { id: "hermes", label: "Hermes" },
  { id: "pi", label: "pi" },
  { id: "workbuddy", label: "WorkBuddy" },
];

export default function PanelView() {
  const [ov, setOv] = useState<Overview | null>(null);
  const [conns, setConns] = useState<AgentConnection[]>([]);

  useEffect(() => {
    const load = () => {
      invoke<Overview>("overview").then(setOv).catch(() => {});
      agentConnections().then(setConns).catch(() => {});
    };
    void load();
    const t = setInterval(load, 5000);
    return () => clearInterval(t);
  }, []);

  const openMain = () => {
    onboardingInfo().catch(() => {});
    invoke("show_main_window").catch(() => {});
    // T-M10-38：打开主窗后**显式收起面板**（不能只靠失焦——主窗抢焦点不总是即时）
    invoke("hide_panel").catch(() => {});
  };

  // T-M10-38：Esc 关闭面板（键盘可达）
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") invoke("hide_panel").catch(() => {});
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const total = ov?.total ?? 0;
  const alive = ov ? 3 : 0;

  return (
    <div className="panel">
      <header className="panel-head">
        <span className="panel-title">Orbital</span>
        <span className="panel-sub num-sub">
          {total} 条记忆 · 进程 {alive}/3
        </span>
        <button
          className="panel-close"
          title="关闭（Esc）"
          aria-label="关闭"
          onClick={() => invoke("hide_panel").catch(() => {})}
        >
          ✕
        </button>
      </header>

      <div className="panel-agents">
        {KNOWN.map(({ id, label }) => {
          const c = conns.find((x) => x.id === id);
          const n = ov?.agents.find((a) => a.agent_id === id)?.total ?? 0;
          const state = !c ? "…" : c.attached ? "已接入" : c.exists ? "未接入" : "未安装";
          return (
            <div key={id} className="panel-row">
              <span className={`dot ${c?.attached ? "dot-on" : "dot-off"}`} />
              <span className="panel-name">{label}</span>
              <span className="panel-num mono">{n}</span>
              <span
                className={`panel-state ${c?.attached ? "ok" : "warn"}`}
                title={c?.config_path}
              >
                {state}
              </span>
              <span className="panel-mode num-sub">{c?.format ?? ""}</span>
            </div>
          );
        })}
      </div>

      <div className="panel-foot">
        <button className="btn btn-sm btn-accent" onClick={openMain}>
          打开主窗口
        </button>
        <button
          className="btn btn-sm"
          onClick={() => invoke("quit_app").catch(() => {})}
        >
          退出
        </button>
      </div>
    </div>
  );
}
