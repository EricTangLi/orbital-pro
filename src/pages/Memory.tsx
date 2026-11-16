/**
 * 记忆页（T-M2-01）—— 对齐 docs/03 §4.2 的主从双栏。
 *
 * 硬约束：
 * - 红线 3：**不是看板**——每行都能点进详情。
 * - 红线 4：只用 token。
 * - ADR-031：分层卡跨表统计，**不 GROUP BY layer**。
 * - 三态可区分（docs/03 §4.1 禁止三种情况都写"暂无数据"）。
 */

import { useCallback, useEffect, useMemo, useState } from "react";
import {
  type FilterOptions,
  type ListResult,
  type MemDetail,
  fmtDate,
  fmtNum,
  updateMemory,
  deleteImpact,
  forgetMemory,
  restoreMemory,
  togglePin,
  invalidateMemory,
  restoreValidity,
  effectiveImportance,
  type DeleteImpact,
  mergePreview,
  mergeMemories,
  undoEvent,
  relatedMemories,
  linkMemory,
  type RelatedItem,
  type MergePreview,
  invoke,
} from "../lib/api";

export default function MemoryPage(props: {
  initialTodo?: string | null;
  /** T-M10-05（ADR-048）：全局"历史版本"条数，仅在默认视图下加注（带筛选时口径不同，不能混用） */
  historyCount?: number;
}) {
  const { initialTodo = null, historyCount = 0 } = props;
  const [q, setQ] = useState("");
  const [agent, setAgent] = useState("all");
  const [category, setCategory] = useState("all");
  const [status, setStatus] = useState("current");
  // 待办筛选（从总览页点进来的）：与总览页的计数用**同一套 SQL 条件**，避免"总数 3、点进去 0"
  const [todo, setTodo] = useState<string | null>(initialTodo);
  useEffect(() => {
    setTodo(initialTodo);
    setPage(1);
  }, [initialTodo]);
  const [page, setPage] = useState(1);
  const [data, setData] = useState<ListResult | null>(null);
  const [opts, setOpts] = useState<FilterOptions>({ agents: [], categories: [] });
  const [selected, setSelected] = useState<string | null>(null);
  const [detail, setDetail] = useState<MemDetail | null>(null);
  // ---- T-M2-02 编辑态 ----
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");
  const [draftImp, setDraftImp] = useState(3);
  const [draftTags, setDraftTags] = useState<string[]>([]);
  const [tagInput, setTagInput] = useState("");
  const [saving, setSaving] = useState(false);
  const [feedback, setFeedback] = useState<string | null>(null);
  const [feedbackTone, setFeedbackTone] = useState("is-ok");
  // ---- T-M2-03 删除 ----
  const [delImpact, setDelImpact] = useState<DeleteImpact | null>(null);
  const [delConfirm, setDelConfirm] = useState("");
  const [deleting, setDeleting] = useState(false);
  // ---- T-M2-04 Pin / 失效 ----
  /** 读时计算的 ei（ADR-013：不落盘；只为让用户看到"它现在值多少"） */
  const [ei, setEi] = useState<number | null>(null);
  // ---- T-M2-05 合并重复 ----
  /** 合并弹窗（docs/03 §5.2 的三种决策：保留A删B / 保留B删A / 合并两者） */
  const [mergeOpen, setMergeOpen] = useState(false);
  const [mergeQ, setMergeQ] = useState("");
  const [mergeCands, setMergeCands] = useState<ListResult["items"]>([]);
  const [mergeTarget, setMergeTarget] = useState<ListResult["items"][number] | null>(null);
  const [mergePv, setMergePv] = useState<MergePreview | null>(null);
  const [merging, setMerging] = useState(false);
  /** T-M2-07 撤销窗口：操作成功后 **5 秒内**可撤销（eventId 是服务端返回的句柄）。
   *  5 秒后自动消失——超过窗口仍可从维护页「最近操作」撤销。 */
  const [undoable, setUndoable] = useState<{ eventId: number; label: string } | null>(null);
  useEffect(() => {
    if (!undoable) return;
    const t = setTimeout(() => setUndoable(null), 5000);
    return () => clearTimeout(t);
  }, [undoable]);
  // ---- T-M2-08 关系（JTBD J4：把两条记忆连起来）----
  const [related, setRelated] = useState<RelatedItem[]>([]);
  const [linkOpen, setLinkOpen] = useState(false);
  const [linkQ, setLinkQ] = useState("");
  const [linkCands, setLinkCands] = useState<ListResult["items"]>([]);
  const [linkTarget, setLinkTarget] = useState<ListResult["items"][number] | null>(null);
  const [linkType, setLinkType] = useState("semantic");
  const [linking, setLinking] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    setErr(null);
    try {
      const r = await invoke<ListResult>("list_memories", {
        agent: agent === "all" ? null : agent,
        category: category === "all" ? null : category,
        status,
        q: q.trim() || null,
        page,
        pageSize: 50,
        todo: todo,
      });
      setData(r);
    } catch (e) {
      setErr(String(e));
      setData(null);
    } finally {
      setLoading(false);
    }
  }, [agent, category, status, q, page, todo]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    invoke<FilterOptions>("filter_options").then(setOpts).catch(() => {});
  }, []);

  useEffect(() => {
    if (!selected) {
      setDetail(null);
      return;
    }
    invoke<MemDetail>("get_memory", { id: selected })
      .then(setDetail)
      .catch((e) => setErr(String(e)));
    // ei 是读时计算（ADR-013），与详情并行取，失败就显示"—"（不影响详情）
    setEi(null);
    effectiveImportance(selected).then(setEi).catch(() => setEi(null));
    // 关联列表（J4）：与详情并行取，失败就当空（不影响详情）
    relatedMemories(selected)
      .then(setRelated)
      .catch(() => setRelated([]));
  }, [selected]);

  // ---- T-M2-02 编辑动作 ----
  /** tags 在库里是 JSON 字符串（`docs/02`：TEXT 存数组），解析失败就当空。 */
  const parseTags = (raw: string): string[] => {
    try {
      const v = JSON.parse(raw);
      return Array.isArray(v) ? v.filter((x) => typeof x === "string") : [];
    } catch {
      return [];
    }
  };

  const startEdit = () => {
    if (!detail) return;
    setDraft(detail.content);
    setDraftImp(detail.importance);
    setDraftTags(parseTags(detail.tags));
    setTagInput("");
    setFeedback(null);
    setEditing(true);
  };

  const cancelEdit = () => {
    setEditing(false);
    setFeedback(null);
  };

  const addTag = () => {
    const t = tagInput.trim();
    if (!t) return;
    if (draftTags.includes(t)) {
      setTagInput("");
      return;
    }
    setDraftTags([...draftTags, t]);
    setTagInput("");
  };

  // ---- T-M2-03 删除动作 ----
  /** 打开删除弹窗：先去取真实影响面，弹窗里显示的就是这次删除的实际后果。 */
  const openDelete = async () => {
    if (!detail) return;
    setDelConfirm("");
    try {
      setDelImpact(await deleteImpact(detail.id));
    } catch (e) {
      setErr(String(e));
    }
  };

  const closeDelete = () => {
    setDelImpact(null);
    setDelConfirm("");
  };

  const doDelete = async (mode: "soft" | "hard") => {
    if (!detail || deleting) return;
    setDeleting(true);
    try {
      const eid = await forgetMemory(detail.id, mode);
      closeDelete();
      setFeedback(
        mode === "soft" ? "已删除（5 秒内可撤销；之后可从维护页撤销）" : "已彻底删除（不可撤销）",
      );
      setFeedbackTone(mode === "soft" ? "is-warn" : "is-danger");
      // ⚠️ 硬删不设撤销句柄：undo_event 会明确拒绝（数据已物理删除）
      if (mode === "soft") setUndoable({ eventId: eid, label: "删除" });
      // 软删后该条不再出现在默认（current）列表里，回到空详情
      setSelected(null);
      setDetail(null);
      void load();
    } catch (e) {
      setFeedback(String(e));
      setFeedbackTone("is-danger");
    } finally {
      setDeleting(false);
    }
  };

  const doRestore = async () => {
    if (!detail || deleting) return;
    setDeleting(true);
    try {
      await restoreMemory(detail.id);
      // ⚠️ 撤销软删只清 deleted_at，**不动 valid_to**（两个标记独立）。
      // 如果这条同时还被"标记失效"，恢复后依然不会被召回——必须如实说，
      // 不能只弹"已恢复"让用户以为它已经能用了（审计 2026-11-13 发现）。
      const stillInvalid = detail.valid_to !== null;
      setFeedback(
        stillInvalid
          ? "已撤销删除，但它仍处于「标记失效」状态（不会被召回）；要恢复召回请再点「恢复失效」"
          : "已恢复",
      );
      setFeedbackTone(stillInvalid ? "is-warn" : "is-ok");
      invoke<MemDetail>("get_memory", { id: detail.id }).then(setDetail).catch(() => {});
      void load();
    } catch (e) {
      setFeedback(String(e));
      setFeedbackTone("is-danger");
    } finally {
      setDeleting(false);
    }
  };

  // ---- T-M2-04 Pin / 失效动作 ----
  const doTogglePin = async () => {
    if (!detail) return;
    try {
      const eid = await togglePin(detail.id, !detail.pinned);
      setFeedback(detail.pinned ? "已取消 Pin" : "已 Pin（免疫降权，不会再被判成退役候选）");
      setFeedbackTone("is-ok");
      if (eid > 0) setUndoable({ eventId: eid, label: "Pin" });
      invoke<MemDetail>("get_memory", { id: detail.id }).then(setDetail).catch(() => {});
      effectiveImportance(detail.id).then(setEi).catch(() => {});
      void load();
    } catch (e) {
      setFeedback(String(e));
      setFeedbackTone("is-danger");
    }
  };

  const doInvalidate = async () => {
    if (!detail) return;
    try {
      const eid = await invalidateMemory(detail.id);
      setFeedback("已标记失效：不再被召回，但仍可在这里查到");
      setFeedbackTone("is-warn");
      setUndoable({ eventId: eid, label: "标记失效" });
      invoke<MemDetail>("get_memory", { id: detail.id }).then(setDetail).catch(() => {});
      void load();
    } catch (e) {
      setFeedback(String(e));
      setFeedbackTone("is-danger");
    }
  };

  const doRestoreValidity = async () => {
    if (!detail) return;
    try {
      await restoreValidity(detail.id);
      setFeedback("已恢复生效：重新可被召回");
      setFeedbackTone("is-ok");
      invoke<MemDetail>("get_memory", { id: detail.id }).then(setDetail).catch(() => {});
      void load();
    } catch (e) {
      setFeedback(String(e));
      setFeedbackTone("is-danger");
    }
  };

  /** T-M10-13：把 private 记忆提升为 shared。
   *
   * 为什么需要这个按钮：注入**只注入 `scope=shared`**（ADR-042，private 绝不进注入）。
   * 实测 4 个 agent 写的 89 条全是 private ⇒ 开了注入也是 0 条。
   *
   * 门禁（ADR-023 规则 1）：写 shared 必须 `origin=user_endorsed`；
   * core 的 `update` 在 `scope=shared` 时会**自动**把 origin 改成 user_endorsed，
   * 所以这里只需传 scope，不需要也不允许前端伪造 origin。
   */
  const doPromote = async () => {
    if (!detail) return;
    try {
      await updateMemory({ id: detail.id, scope: "shared" });
      setFeedback("已提升为共享：所有 agent 现在都能注入这条记忆");
      setFeedbackTone("is-ok");
      invoke<MemDetail>("get_memory", { id: detail.id }).then(setDetail).catch(() => {});
      void load();
    } catch (e) {
      setFeedback(String(e));
      setFeedbackTone("is-danger");
    }
  };

  // ---- T-M2-05 合并动作 ----
  /** 打开合并弹窗：默认给出候选列表，用户再点选"另一条"。 */
  const openMerge = () => {
    setMergeOpen(true);
    setMergeQ("");
    setMergeCands([]);
    setMergeTarget(null);
    setMergePv(null);
  };

  const closeMerge = () => {
    setMergeOpen(false);
    setMergeTarget(null);
    setMergePv(null);
  };

  /** 候选搜索：复用 `list_memories`（不新造接口），排除当前条与已删条。 */
  useEffect(() => {
    if (!mergeOpen || !detail) return;
    const t = setTimeout(() => {
      invoke<ListResult>("list_memories", {
        agent: null,
        category: null,
        status: "current",
        q: mergeQ.trim() || null,
        page: 1,
        pageSize: 20,
        todo: null,
      })
        .then((r) => setMergeCands(r.items.filter((m) => m.id !== detail.id)))
        .catch((e) => setErr(String(e)));
    }, 300);
    return () => clearTimeout(t);
  }, [mergeOpen, mergeQ, detail]);

  /** 选中候选 → 立刻取预览（数字来自 core 的 merge_preview，与真正合并同一套查询）。 */
  const pickMergeTarget = async (m: ListResult["items"][number]) => {
    if (!detail) return;
    setMergeTarget(m);
    setMergePv(null);
    try {
      setMergePv(await mergePreview(detail.id, m.id));
    } catch (e) {
      setErr(String(e));
    }
  };

  /** 「合并两者」：搬标签/实体/事实/关系，再软删另一条。 */
  const doMerge = async () => {
    if (!detail || !mergeTarget || merging) return;
    setMerging(true);
    try {
      const r = await mergeMemories(detail.id, mergeTarget.id);
      setFeedback(r.note);
      setFeedbackTone("is-ok");
      setUndoable({ eventId: r.event_id, label: "合并" });
      closeMerge();
      // 标签变了 → 详情重读；另一条被软删 → 列表刷新
      invoke<MemDetail>("get_memory", { id: detail.id }).then(setDetail).catch(() => {});
      void load();
    } catch (e) {
      setFeedback(String(e));
      setFeedbackTone("is-danger");
    } finally {
      setMerging(false);
    }
  };

  /**
   * 另外两种决策：只保留一条（**不搬任何东西**，等于软删另一条）。
   * 为什么不复用删除弹窗：这里的语境是"二选一"，用户已经看过两条对比，
   * 再让他确认一次影响面只是多一步点击；影响面（事实/关系数）已在合并预览里显示过。
   */
  const doKeepOnly = async (which: "keep" | "drop") => {
    if (!detail || !mergeTarget || merging) return;
    setMerging(true);
    try {
      if (which === "keep") {
        const eid = await forgetMemory(mergeTarget.id, "soft");
        setFeedback("已保留当前条，另一条已删除（可恢复）");
        setUndoable({ eventId: eid, label: "删除" });
      } else {
        const eid = await forgetMemory(detail.id, "soft");
        setFeedback("已保留另一条，当前条已删除（可恢复）");
        setUndoable({ eventId: eid, label: "删除" });
        setSelected(null);
        setDetail(null);
      }
      setFeedbackTone("is-warn");
      closeMerge();
      void load();
    } catch (e) {
      setFeedback(String(e));
      setFeedbackTone("is-danger");
    } finally {
      setMerging(false);
    }
  };

  /** 撤销上一步操作（T-M2-07：5 秒窗口内；合并也走同一条路径，由 core 分派逆操作）。 */
  const doUndoEvent = async () => {
    if (!undoable) return;
    try {
      const r = await undoEvent(undoable.eventId);
      setFeedback(r.note);
      setFeedbackTone("is-ok");
      setUndoable(null);
      void load();
      if (detail) {
        invoke<MemDetail>("get_memory", { id: detail.id }).then(setDetail).catch(() => {});
      }
    } catch (e) {
      setFeedback(String(e));
      setFeedbackTone("is-danger");
    }
  };

  // ---- T-M2-08 关系动作（J4）----
  const openLink = () => {
    setLinkOpen(true);
    setLinkQ("");
    setLinkCands([]);
    setLinkTarget(null);
  };
  const closeLink = () => {
    setLinkOpen(false);
    setLinkTarget(null);
  };
  // ---- T-M10-47：三个弹层的键盘出口 ----
  // 删除确认 / 合并 / 关联此前只有"点遮罩"一种关闭方式（`<div className="modal-mask" onClick>`），
  // 键盘用户打开后按 Esc 没有任何反应 —— 这正是"弹出对话框无法收回"观感的一半来源。
  // 这里统一接管 Esc，按"最上层优先"关闭（关联 → 合并 → 删除）。
  // 依赖数组只放"哪个弹层开着"：closeX 是每次渲染重建的普通函数，放进来会无限重注册。
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      if (linkOpen) {
        e.preventDefault();
        closeLink();
      } else if (mergeOpen) {
        e.preventDefault();
        closeMerge();
      } else if (delImpact) {
        e.preventDefault();
        closeDelete();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [linkOpen, mergeOpen, delImpact]);
  /** 候选搜索：复用 list_memories，排除当前条。 */
  useEffect(() => {
    if (!linkOpen || !detail) return;
    const t = setTimeout(() => {
      invoke<ListResult>("list_memories", {
        agent: null,
        category: null,
        status: "current",
        q: linkQ.trim() || null,
        page: 1,
        pageSize: 20,
        todo: null,
      })
        .then((r) => setLinkCands(r.items.filter((m) => m.id !== detail.id)))
        .catch((e) => setErr(String(e)));
    }, 300);
    return () => clearTimeout(t);
  }, [linkOpen, linkQ, detail]);

  const doLink = async () => {
    if (!detail || !linkTarget || linking) return;
    setLinking(true);
    try {
      await linkMemory(detail.id, linkTarget.id, linkType, 1.0);
      setFeedback(`已建立 ${linkType} 关系`);
      setFeedbackTone("is-ok");
      closeLink();
      relatedMemories(detail.id).then(setRelated).catch(() => {});
    } catch (e) {
      setFeedback(String(e));
      setFeedbackTone("is-danger");
    } finally {
      setLinking(false);
    }
  };

  const saveEdit = async () => {
    if (!detail || saving) return;
    const text = draft.trim();
    // 本地先校验，省一次往返；服务端仍会独立校验（不许只靠前端）
    if (!text) {
      setFeedback("内容不能为空");
      setFeedbackTone("is-danger");
      return;
    }
    const bytes = new TextEncoder().encode(text).length;
    if (bytes > 65536) {
      setFeedback(`超过单条上限 64KB（当前 ${bytes} 字节）`);
      setFeedbackTone("is-danger");
      return;
    }

    // 只提交**真的变了**的字段：
    // 只改重要度/标签却连带传 content，会触发版本链新建一条——那是白白多一版。
    const contentChanged = text !== detail.content;
    const impChanged = draftImp !== detail.importance;
    const tagsChanged =
      JSON.stringify(draftTags) !== JSON.stringify(parseTags(detail.tags));
    if (!contentChanged && !impChanged && !tagsChanged) {
      setFeedback("没有改动");
      setFeedbackTone("is-warn");
      return;
    }

    setSaving(true);
    try {
      const r = await updateMemory({
        id: detail.id,
        content: contentChanged ? text : null,
        importance: impChanged ? draftImp : null,
        tags: tagsChanged ? draftTags : null,
      });
      setFeedback(r.note);
      setFeedbackTone(r.action === "duplicate" ? "is-warn" : "is-ok");
      if (r.event_id !== null) setUndoable({ eventId: r.event_id, label: "编辑" });
      setEditing(false);
      if (r.id === detail.id) {
        // 只改元数据 / duplicate：原地刷新
        invoke<MemDetail>("get_memory", { id: r.id }).then(setDetail).catch(() => {});
      } else {
        // 内容变更走版本链 → 会产生新 id，切到新那条
        setSelected(r.id);
      }
      void load();
    } catch (e) {
      setFeedback(String(e));
      setFeedbackTone("is-danger");
    } finally {
      setSaving(false);
    }
  };

  const pages = useMemo(() => {
    if (!data) return 1;
    return Math.max(1, Math.ceil(data.total / data.page_size));
  }, [data]);

  const emptyText = useMemo(() => {
    const noFilter = !q && agent === "all" && category === "all";
    if (status === "deleted") return "回收站是空的。";
    if (status === "expired") return "没有已失效的记忆。";
    if (noFilter) return "还没有记忆。agent 写入后会显示在这里。";
    return "当前筛选条件下没有匹配的记忆——换个关键词或把筛选改成「全部」。";
  }, [q, agent, category, status]);

  return (
    <>
      <header className="page-head">
        <div>
          <h1>记忆</h1>
          <p className="desc">
            {data
              ? `${fmtNum(data.total)} 条 · 第 ${data.page}/${pages} 页${
                  // T-M10-05：只在默认视图（无筛选）加注——带筛选时 data.total 是筛选后的，
                  // 与全局 historyCount 不同源，混用会给出错误口径。
                  historyCount > 0 &&
                  !todo &&
                  agent === "all" &&
                  category === "all" &&
                  status === "current"
                    ? ` · 另有历史版本 ${fmtNum(historyCount)} 条（已被新版本替代，不计入召回）`
                    : ""
                }`
              : "读取中…"}
          </p>
        </div>
        <button className="btn" onClick={() => void load()} disabled={loading}>
          {loading ? "读取中…" : "刷新"}
        </button>
      </header>

      {err && (
        <div className="card card-danger err-line">
          <strong>读取失败</strong>：{err}
          <div className="num-sub">（不是"暂无数据"——数据源确实没读成功）</div>
        </div>
      )}

      <div className="card filters">
        <input
          className="input"
          placeholder="搜索内容…"
          value={q}
          onChange={(e) => {
            setQ(e.target.value);
            setPage(1);
          }}
        />
        <label className="kv-item">
          <span className="kv-k">Agent</span>
          <select
            className="input"
            value={agent}
            onChange={(e) => {
              setAgent(e.target.value);
              setPage(1);
            }}
          >
            <option value="all">全部</option>
            {opts.agents.map((a) => (
              <option key={a} value={a}>
                {a}
              </option>
            ))}
          </select>
        </label>
        <label className="kv-item">
          <span className="kv-k">类型</span>
          <select
            className="input"
            value={category}
            onChange={(e) => {
              setCategory(e.target.value);
              setPage(1);
            }}
          >
            <option value="all">全部</option>
            {opts.categories.map((c) => (
              <option key={c} value={c}>
                {c}
              </option>
            ))}
          </select>
        </label>
        <label className="kv-item">
          <span className="kv-k">状态</span>
          <select
            className="input"
            value={status}
            onChange={(e) => {
              setStatus(e.target.value);
              setPage(1);
            }}
          >
            <option value="current">当前</option>
            <option value="expired">已失效</option>
            <option value="deleted">已删除</option>
          </select>
        </label>
      </div>

      <div className="master-detail">
        <section className="card list-pane">
          {data && data.items.length === 0 ? (
            <div className="empty">{emptyText}</div>
          ) : (
            <ul className="rows">
              {data?.items.map((m) => (
                <li
                  key={m.id}
                  className={`mem-row ${selected === m.id ? "mem-row-active" : ""}`}
                  data-agent={m.agent_id}
                  onClick={() => setSelected(m.id)}
                  role="button"
                  tabIndex={0}
                  onKeyDown={(e) => e.key === "Enter" && setSelected(m.id)}
                >
                  <div className="mem-row-main">
                    {/* 行内 Pin 开关（T-M2-04）：点击只切 Pin，不切换选中行 */}
                    <button
                      type="button"
                      className={`pin-toggle ${m.pinned ? "pin-on" : ""}`}
                      title={m.pinned ? "取消 Pin" : "Pin（免疫降权）"}
                      onClick={(e) => {
                        e.stopPropagation();
                        void togglePin(m.id, !m.pinned)
                          .then(() => void load())
                          .catch((err) => setErr(String(err)));
                      }}
                    >
                      {m.pinned ? "★" : "☆"}
                    </button>
                    <span className="mem-row-summary">{m.summary || "(空内容)"}</span>
                  </div>
                  <div className="mem-row-meta num-sub">
                    {m.agent_id} · {fmtDate(m.created_at)} · {m.category} · imp {m.importance}
                    {m.superseded_by && " · 已被新版本取代"}
                  </div>
                </li>
              ))}
            </ul>
          )}

          <div className="pager">
            <button
              className="btn btn-sm"
              disabled={page <= 1}
              onClick={() => setPage((p) => Math.max(1, p - 1))}
            >
              上一页
            </button>
            <button
              className="btn btn-sm"
              disabled={page >= pages}
              onClick={() => setPage((p) => p + 1)}
            >
              下一页
            </button>
          </div>
        </section>

        <section className="card detail-pane">
          {!detail ? (
            <div className="empty">选左边任意一条，这里显示全文与元数据。</div>
          ) : (
            <>
              <div className="row-between">
                <span className="kv-k">详情</span>
                <span
                  className={`badge ${
                    detail.deleted_at ? "badge-danger" : detail.valid_to ? "badge-warn" : "badge-ok"
                  }`}
                >
                  {detail.deleted_at ? "已删除" : detail.valid_to ? "已失效" : "当前有效"}
                </span>
              </div>

              <p className="detail-content">{detail.content}</p>

              {/* T-M2-02 编辑：Enter 保存 / Esc 取消；结果文案按 action 分支（docs/04 §10.1） */}
              <div className="edit-bar">
                {!editing ? (
                  <>
                    <button className="btn btn-sm" onClick={startEdit}>
                      编辑
                    </button>
                    {detail.deleted_at ? (
                      <button className="btn btn-sm" onClick={doRestore} disabled={deleting}>
                        {deleting ? "恢复中…" : "恢复"}
                      </button>
                    ) : (
                      <>
                        <button
                          className={`btn btn-sm ${detail.pinned ? "btn-pin-on" : ""}`}
                          onClick={() => void doTogglePin()}
                        >
                          {detail.pinned ? "★ 已 Pin" : "☆ Pin"}
                        </button>
                        {detail.valid_to ? (
                          <button className="btn btn-sm" onClick={() => void doRestoreValidity()}>
                            恢复失效
                          </button>
                        ) : (
                          <button
                            className="btn btn-sm"
                            onClick={() => void doInvalidate()}
                            title="不再被召回，但历史仍可查"
                          >
                            标记失效
                          </button>
                        )}
                        <button className="btn btn-sm" onClick={openMerge}>
                          合并重复
                        </button>
                        <button className="btn btn-sm" onClick={openLink}>
                          建立关联
                        </button>
                        {detail.scope === "private" && (
                          <button
                            className="btn btn-sm"
                            onClick={() => void doPromote()}
                            title="提升后所有 agent 都能注入这条记忆（注入只注入 shared）"
                          >
                            提升为共享
                          </button>
                        )}
                        <button
                          className="btn btn-sm btn-danger-ghost"
                          onClick={() => void openDelete()}
                        >
                          删除
                        </button>
                      </>
                    )}
                  </>
                ) : (
                  <>
                    <button className="btn btn-sm" onClick={saveEdit} disabled={saving}>
                      {saving ? "保存中…" : "保存"}
                    </button>
                    <button className="btn btn-sm" onClick={cancelEdit} disabled={saving}>
                      取消
                    </button>
                    <span className="num-sub">Enter 保存 · Esc 取消</span>
                  </>
                )}
                {feedback && (
                  <span className={`edit-feedback ${feedbackTone}`}>{feedback}</span>
                )}
                {undoable !== null && (
                  <button className="btn btn-sm" onClick={() => void doUndoEvent()}>
                    撤销{undoable.label}（5 秒内）
                  </button>
                )}
              </div>
              {editing && (
                <textarea
                  className="edit-area"
                  value={draft}
                  onChange={(e) => setDraft(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && !e.shiftKey) {
                      e.preventDefault();
                      void saveEdit();
                    } else if (e.key === "Escape") {
                      e.preventDefault();
                      cancelEdit();
                    }
                  }}
                  rows={6}
                  autoFocus
                />
              )}
              {editing && (
                <div className="edit-meta">
                  <div className="edit-field">
                    <span className="kv-k">重要度</span>
                    <div className="imp-picker">
                      {[1, 2, 3, 4, 5].map((n) => (
                        <button
                          key={n}
                          type="button"
                          className={`imp-pick ${draftImp === n ? "imp-pick-on" : ""}`}
                          onClick={() => setDraftImp(n)}
                          title={`重要度 ${n}`}
                        >
                          {n}
                        </button>
                      ))}
                      <span className="num-sub">{draftImp} / 5</span>
                    </div>
                  </div>
                  <div className="edit-field">
                    <span className="kv-k">标签</span>
                    <div className="tag-editor">
                      {draftTags.length === 0 && <span className="num-sub">还没有标签</span>}
                      {draftTags.map((t) => (
                        <span key={t} className="tag-chip">
                          {t}
                          <button
                            type="button"
                            className="tag-del"
                            onClick={() => setDraftTags(draftTags.filter((x) => x !== t))}
                            title="删除标签"
                          >
                            ×
                          </button>
                        </span>
                      ))}
                      <input
                        className="tag-input"
                        placeholder="输入后回车添加"
                        value={tagInput}
                        onChange={(e) => setTagInput(e.target.value)}
                        onKeyDown={(e) => {
                          if (e.key === "Enter") {
                            e.preventDefault();
                            addTag();
                          }
                        }}
                      />
                    </div>
                  </div>
                </div>
              )}

              <dl className="kv">
                <div className="kv-item">
                  <dt className="kv-k">来源</dt>
                  <dd className="kv-v">
                    {detail.agent_id}
                    {detail.source_ref ? ` · ${detail.source_ref}` : ""}
                  </dd>
                </div>
                <div className="kv-item">
                  <dt className="kv-k">作用域</dt>
                  <dd className="kv-v">
                    {detail.scope} / {detail.origin}
                  </dd>
                </div>
                <div className="kv-item">
                  <dt className="kv-k">类型</dt>
                  <dd className="kv-v">{detail.category}</dd>
                </div>
                <div className="kv-item">
                  <dt className="kv-k">重要度</dt>
                  <dd className="kv-v">
                    {detail.importance} · 置信 {detail.confidence.toFixed(2)}
                    {/* ei 读时计算（ADR-013），显示当前值；Pin 时下限 1.0 */}
                    <span className="num-sub"> · 实效 {ei === null ? "—" : ei.toFixed(2)}</span>
                  </dd>
                </div>
                <div className="kv-item">
                  <dt className="kv-k">访问</dt>
                  <dd className="kv-v">
                    {detail.access_count} 次 · 最近 {fmtDate(detail.last_accessed_at, "datetime")}
                  </dd>
                </div>
                <div className="kv-item">
                  <dt className="kv-k">生效</dt>
                  <dd className="kv-v">
                    {fmtDate(detail.valid_from, "datetime")} 起 ·{" "}
                    {detail.valid_to ? `${fmtDate(detail.valid_to, "datetime")} 止` : "仍有效"}
                  </dd>
                </div>
                <div className="kv-item">
                  <dt className="kv-k">记录于</dt>
                  <dd className="kv-v">{fmtDate(detail.recorded_at, "datetime")}</dd>
                </div>
                <div className="kv-item">
                  <dt className="kv-k">向量</dt>
                  <dd className="kv-v mono">{detail.embedding_status}</dd>
                </div>
                {detail.superseded_by && (
                  <div className="kv-item">
                    <dt className="kv-k">被取代</dt>
                    <dd className="kv-v mono">{detail.superseded_by}</dd>
                  </div>
                )}
              </dl>

              {/* T-M2-08 J4：关联列表（双向）。点一条切过去，形成"顺着线索走"的路径 */}
              <div className="rel-block">
                <div className="kv-k">关联（{related.length}）</div>
                {related.length === 0 ? (
                  <div className="num-sub">还没有关联——点上方「建立关联」把两条记忆连起来。</div>
                ) : (
                  related.map((r) => (
                    <div key={`${r.id}-${r.edge_type}`} className="rel-row">
                      <span className="badge">{r.edge_type}</span>
                      <button className="rel-sum" onClick={() => setSelected(r.id)}>
                        {r.summary || r.id}
                      </button>
                    </div>
                  ))
                )}
              </div>

              <div className="detail-note num-sub">
                编辑走 <code>MemoryService::update</code>（ADR-034）：内容变更会**产生新版本**，
                旧版本保留可回滚；同内容判为"已存在"不重复写；超 64KB 直接拒绝。
                Pin / 删除 / 标记失效走 MCP 的受控路径，下一步接上。
              </div>
            </>
          )}
        </section>
      </div>

      {/* T-M2-03 删除弹窗：先给影响面，再让用户选"软删/彻底删" */}
      {delImpact && (
        <div className="modal-mask" onClick={closeDelete}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <div className="modal-head">
              <b>删除这条记忆？</b>
            </div>
            <p className="modal-summary">{delImpact.summary}</p>
            <dl className="kv">
              <div className="kv-item">
                <dt className="kv-k">结构化属性</dt>
                <dd className="kv-v num">{delImpact.facts} 条</dd>
              </div>
              <div className="kv-item">
                <dt className="kv-k">关系边</dt>
                <dd className="kv-v num">{delImpact.edges} 条</dd>
              </div>
              <div className="kv-item">
                <dt className="kv-k">访问次数</dt>
                <dd className="kv-v num">{delImpact.access_count} 次</dd>
              </div>
              {delImpact.pinned && (
                <div className="kv-item">
                  <dt className="kv-k">注意</dt>
                  <dd className="kv-v">这条被 Pin 过</dd>
                </div>
              )}
            </dl>

            <div className="modal-note num-sub">
              <b>软删</b>：只是隐藏，随时能恢复，关系边不动（查询时过滤）。
              <br />
              <b>彻底删</b>：物理删除，属性和关系边会被级联清掉；core 删除前会把整条快照写进
              journal，但**界面里无法恢复**。
            </div>

            <div className="modal-actions">
              <button className="btn btn-sm" onClick={closeDelete} disabled={deleting}>
                取消
              </button>
              <button
                className="btn btn-sm"
                onClick={() => void doDelete("soft")}
                disabled={deleting}
              >
                {deleting ? "处理中…" : "软删（可恢复）"}
              </button>
            </div>

            <div className="modal-danger">
              <label className="kv-k" htmlFor="hard-confirm">
                彻底删除需输入「删除」二字
              </label>
              <div className="modal-danger-row">
                <input
                  id="hard-confirm"
                  className="tag-input"
                  value={delConfirm}
                  onChange={(e) => setDelConfirm(e.target.value)}
                  placeholder="删除"
                />
                <button
                  className="btn btn-sm btn-danger"
                  disabled={delConfirm.trim() !== "删除" || deleting}
                  onClick={() => void doDelete("hard")}
                >
                  彻底删除
                </button>
              </div>
            </div>
          </div>
        </div>
      )}
      {/* T-M2-05 合并弹窗：选另一条 → 预览搬运量 → 三选一（docs/03 §5.2） */}
      {mergeOpen && detail && (
        <div className="modal-mask" onClick={closeMerge}>
          <div className="modal modal-wide" onClick={(e) => e.stopPropagation()}>
            <div className="modal-head">
              <b>合并重复</b>
            </div>
            <div className="merge-keep">
              <span className="kv-k">保留条</span>
              <p className="modal-summary">{detail.content.slice(0, 80)}</p>
            </div>

            <div className="edit-field">
              <span className="kv-k">另一条（要合并/删除的）</span>
              <input
                className="tag-input"
                value={mergeQ}
                onChange={(e) => setMergeQ(e.target.value)}
                placeholder="输入关键词搜索（300ms 防抖）"
                autoFocus
              />
            </div>

            <ul className="merge-cands">
              {mergeCands.length === 0 ? (
                <li className="merge-cand merge-cand-empty">
                  <span className="merge-cand-meta">输入关键词，列出可合并的条目</span>
                </li>
              ) : (
                mergeCands.map((m) => (
                  <li
                    key={m.id}
                    className={`merge-cand ${mergeTarget?.id === m.id ? "is-sel" : ""}`}
                    onClick={() => void pickMergeTarget(m)}
                  >
                    <div className="merge-cand-sum">{m.summary || "(空内容)"}</div>
                    <div className="merge-cand-meta num-sub">
                      {m.agent_id} · {fmtDate(m.created_at)} · {m.category} · imp {m.importance}
                    </div>
                  </li>
                ))
              )}
            </ul>

            {mergePv && (
              <div className="merge-preview">
                合并后：标签 <b>{mergePv.tags.length}</b> 个（搬来 {mergePv.tags_moved.length} 个）·
                关系 <b>{mergePv.edges_moved}</b> 条 · 事实 <b>{mergePv.facts_moved}</b> 条
                {mergePv.self_loops > 0 && (
                  <>（其中 {mergePv.self_loops} 条边会变成自环被丢弃）</>
                )}
              </div>
            )}

            <div className="merge-actions">
              <button
                className="btn btn-sm"
                onClick={() => void doKeepOnly("keep")}
                disabled={!mergeTarget || merging}
              >
                保留上面这条，删掉另一条
              </button>
              <button
                className="btn btn-sm"
                onClick={() => void doKeepOnly("drop")}
                disabled={!mergeTarget || merging}
              >
                保留另一条，删掉上面这条
              </button>
              <button
                className="btn btn-sm btn-accent"
                onClick={() => void doMerge()}
                disabled={!mergeTarget || !mergePv || merging}
              >
                {merging ? "合并中…" : "合并两者"}
              </button>
            </div>

            <p className="modal-note num-sub">
              「合并两者」= 把另一条的标签 / 实体 / 事实 / 关系搬过来，然后软删另一条；
              内容保留上面这条的原文（不拼接，避免破坏向量与去重键）。合并后可以撤销。
            </p>
          </div>
        </div>
      )}
      {/* T-M2-08 建立关联弹窗（J4） */}
      {linkOpen && detail && (
        <div className="modal-mask" onClick={closeLink}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <div className="modal-head">
              <b>建立关联</b>
            </div>
            <p className="modal-summary">{detail.content.slice(0, 80)}</p>

            <div className="edit-field">
              <span className="kv-k">连到哪一条</span>
              <input
                className="tag-input"
                value={linkQ}
                onChange={(e) => setLinkQ(e.target.value)}
                placeholder="输入关键词搜索（300ms 防抖）"
                autoFocus
              />
            </div>

            <ul className="merge-cands">
              {linkCands.length === 0 ? (
                <li className="merge-cand merge-cand-empty">
                  <span className="merge-cand-meta">输入关键词，列出可关联的条目</span>
                </li>
              ) : (
                linkCands.map((m) => (
                  <li
                    key={m.id}
                    className={`merge-cand ${linkTarget?.id === m.id ? "is-sel" : ""}`}
                    onClick={() => setLinkTarget(m)}
                  >
                    <div className="merge-cand-sum">{m.summary || "(空内容)"}</div>
                    <div className="merge-cand-meta num-sub">
                      {m.agent_id} · {fmtDate(m.created_at)}
                    </div>
                  </li>
                ))
              )}
            </ul>

            <div className="edit-field">
              <span className="kv-k">关系类型</span>
              <select value={linkType} onChange={(e) => setLinkType(e.target.value)}>
                <option value="semantic">semantic（语义相关）</option>
                <option value="causal">causal（因果）</option>
                <option value="entity">entity（同实体）</option>
                <option value="temporal">temporal（时间相邻）</option>
              </select>
            </div>

            <div className="merge-actions">
              <button className="btn btn-sm" onClick={closeLink} disabled={linking}>
                取消
              </button>
              <button
                className="btn btn-sm btn-accent"
                onClick={() => void doLink()}
                disabled={!linkTarget || linking}
              >
                {linking ? "建立中…" : "建立关系"}
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}
