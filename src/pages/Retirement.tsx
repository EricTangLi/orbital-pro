/**
 * 退役页（T-M4-05）：候选 / 可恢复 两个 tab。
 *
 * 为什么单独一页：维护页只展示前 8 条（M4-01/03 的临时版），完整列表需要分页 + 批量操作。
 * 铁律：**降权 ≠ 删除**。每一条降权都可撤销（5 秒页内撤销条 + 活动日志兜底）。
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  type RecoverableRow,
  type RetirementRow,
  recoveryList,
  retirementCandidates,
  retireMemory,
  restoreToBaseline,
  undoEvent,
} from "../lib/api";

const PAGE_SIZE = 10;

export default function RetirementPage({ onChanged }: { onChanged: () => void }) {
  const [tab, setTab] = useState<"candidates" | "recovery">("candidates");
  const [cands, setCands] = useState<RetirementRow[]>([]);
  const [rec, setRec] = useState<RecoverableRow[]>([]);
  const [page, setPage] = useState(0);
  const [checked, setChecked] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [undoable, setUndoable] = useState<{ eventIds: number[]; label: string } | null>(null);

  const load = useCallback(async () => {
    try {
      const [c, r] = await Promise.all([retirementCandidates(), recoveryList()]);
      setCands(c);
      setRec(r);
      setChecked(new Set());
      setErr(null);
    } catch (e) {
      setErr(String(e));
    }
  }, []);
  useEffect(() => {
    void load();
  }, [load]);

  // 5 秒页内撤销（与 Memory 页同一模式：超时后只能去活动日志撤销）
  useEffect(() => {
    if (!undoable) return;
    const t = setTimeout(() => setUndoable(null), 5000);
    return () => clearTimeout(t);
  }, [undoable]);

  const pages = Math.max(1, Math.ceil(cands.length / PAGE_SIZE));
  const view = useMemo(
    () => cands.slice(page * PAGE_SIZE, page * PAGE_SIZE + PAGE_SIZE),
    [cands, page],
  );

  const doRetire = async (ids: string[]) => {
    if (busy || ids.length === 0) return;
    if (
      ids.length > 1 &&
      !window.confirm(
        `将降权 ${ids.length} 条记忆（只降 1 档重要度、不删除，每条都可撤销）。继续？`,
      )
    ) {
      return;
    }
    setBusy(true);
    try {
      const eventIds: number[] = [];
      for (const id of ids) {
        const out = await retireMemory(id);
        if (out.event_id) eventIds.push(out.event_id);
      }
      setUndoable({ eventIds, label: `降权 ${ids.length} 条` });
      setMsg(`已降权 ${ids.length} 条（5 秒内可撤销）`);
      await load();
      onChanged();
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  const doUndo = async () => {
    if (!undoable) return;
    setBusy(true);
    try {
      for (const eid of undoable.eventIds) {
        await undoEvent(eid);
      }
      setMsg("已撤销降权：重要度回到原值");
      setUndoable(null);
      await load();
      onChanged();
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  const doRestore = async (r: RecoverableRow) => {
    if (busy) return;
    setBusy(true);
    try {
      if (r.kind === "demoted") {
        const out = await restoreToBaseline(r.id);
        setMsg(out.note);
      } else if (r.undo_event_id) {
        await undoEvent(r.undo_event_id);
        setMsg("已撤销删除：记忆恢复可见");
      } else {
        setMsg("这条没有可撤销的删除记录");
      }
      await load();
      onChanged();
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <header className="page-head">
        <div>
          <h1>退役</h1>
          <p className="desc">
            {cands.length} 条候选 · {rec.length} 条可恢复（降权只降 1 档，「永不」删除）
          </p>
        </div>
        <div className="row-actions">
          {undoable && (
            <button className="btn btn-sm btn-accent" disabled={busy} onClick={() => void doUndo()}>
              撤销{undoable.label}
            </button>
          )}
          <button className="btn btn-sm" onClick={() => void load()}>
            刷新
          </button>
        </div>
      </header>

      {err && (
        <section className="card">
          <div className="err-line">读取失败：{err}</div>
        </section>
      )}
      {msg && (
        <section className="card">
          <div className="num-sub">{msg}</div>
        </section>
      )}

      <section className="card">
        <div className="card-head">
          <span className="row-actions">
            <button
              className={`btn btn-sm${tab === "candidates" ? " btn-accent" : ""}`}
              onClick={() => setTab("candidates")}
            >
              候选（{cands.length}）
            </button>
            <button
              className={`btn btn-sm${tab === "recovery" ? " btn-accent" : ""}`}
              onClick={() => setTab("recovery")}
            >
              可恢复（{rec.length}）
            </button>
          </span>
          {tab === "candidates" && (
            <button
              className="btn btn-sm"
              disabled={busy || checked.size === 0}
              onClick={() => void doRetire([...checked])}
            >
              批量降权（{checked.size}）
            </button>
          )}
        </div>

        {tab === "candidates" ? (
          cands.length === 0 ? (
            <div className="num-sub" style={{ padding: "10px 14px" }}>
              没有候选：24 小时内新建、被 Pin、重要性 ≥4 或访问过 ≥3 次的记忆「永不」进候选。
            </div>
          ) : (
            <div style={{ padding: "4px 14px 12px" }}>
              {view.map((r) => (
                <div key={r.id} className="act-row">
                  <input
                    type="checkbox"
                    checked={checked.has(r.id)}
                    onChange={(e) => {
                      const next = new Set(checked);
                      if (e.target.checked) next.add(r.id);
                      else next.delete(r.id);
                      setChecked(next);
                    }}
                  />
                  <span className="act-sum">{r.content_head || r.id}</span>
                  <span className="num-sub">
                    ei {r.ei.toFixed(3)} · 重要性 {r.importance} · {r.reasons.join(" / ")}
                  </span>
                  <button className="btn btn-sm" disabled={busy} onClick={() => void doRetire([r.id])}>
                    降权
                  </button>
                </div>
              ))}
              <div className="row-actions" style={{ marginTop: 8 }}>
                <button className="btn btn-sm" disabled={page <= 0} onClick={() => setPage(page - 1)}>
                  上一页
                </button>
                <span className="num-sub">
                  {page + 1} / {pages}
                </span>
                <button
                  className="btn btn-sm"
                  disabled={page >= pages - 1}
                  onClick={() => setPage(page + 1)}
                >
                  下一页
                </button>
              </div>
            </div>
          )
        ) : rec.length === 0 ? (
          <div className="num-sub" style={{ padding: "10px 14px" }}>
            没有可恢复项（降权过或软删过的记忆会出现在这里）
          </div>
        ) : (
          <div style={{ padding: "4px 14px 12px" }}>
            {rec.map((r) => (
              <div key={`${r.kind}-${r.id}`} className="act-row">
                <span className="act-sum">{r.content_head || r.id}</span>
                <span className="num-sub">
                  {r.kind === "demoted"
                    ? `已降权：当前 ${r.current_importance}，最初 ${r.baseline_importance}`
                    : "已删除（软删，可撤销）"}
                </span>
                <button className="btn btn-sm" disabled={busy} onClick={() => void doRestore(r)}>
                  {r.kind === "demoted" ? "恢复" : "撤销删除"}
                </button>
              </div>
            ))}
          </div>
        )}
      </section>
    </>
  );
}
