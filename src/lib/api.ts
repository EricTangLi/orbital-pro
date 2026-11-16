/**
 * 前端与 Rust 的接口层。
 *
 * 为什么单独一层：
 * - 所有 `invoke` 集中，类型只写一遍；
 * - 非 Tauri 环境（纯浏览器预览）会给出**明确错误**，而不是静默空白。
 *
 * ⚠️ 术语：`invoke` = Tauri 的"前端调 Rust"通道（类似 RPC）。
 */

export type MemItem = {
  id: string;
  summary: string;
  agent_id: string;
  category: string;
  scope: string;
  importance: number;
  pinned: boolean;
  created_at: number;
  valid_to: number | null;
  deleted_at: number | null;
  superseded_by: string | null;
  has_source: boolean;
};

export type LayerCounts = { l0: number; l1: number; l2: number; l3: number };

export type ListResult = {
  items: MemItem[];
  total: number;
  page: number;
  page_size: number;
  counts: LayerCounts;
};

export type AgentStat = {
  agent_id: string;
  total: number;
  today: number;
  last_at: number | null;
};

/** 一天的写入量（趋势图）。 */
export type DayCount = { day: string; n: number };

export type Overview = {
  total: number;
  /** T-M10-05（ADR-048）：total 里"已被替代/已失效"的旧版本条数（界面加注用） */
  total_history: number;
  today: number;
  agents: AgentStat[];
  counts: LayerCounts;
  recent: MemItem[];
  pinned: number;
  embeddings_ok: number;
  embeddings_pending: number;
  db_version: number;
  /** null = conversations 表尚未迁移出来（v1 库），不是"没有积压" */
  pending_extract: number | null;
  duplicates: number;
  /** null = memory_conflicts 表尚未迁移出来（v2 旧库），不能显示成 0 */
  conflicts: number | null;
  untagged: number;
  stale: number;
  /** 待办类别数（侧栏角标与维护页标题同源） */
  todo_total: number;
  /** 近 7 天，已补 0 */
  trend: DayCount[];
};

export type MemDetail = {
  id: string;
  content: string;
  agent_id: string;
  category: string;
  scope: string;
  origin: string;
  importance: number;
  confidence: number;
  tags: string;
  entities: string;
  source_ref: string | null;
  session_id: string | null;
  created_at: number;
  updated_at: number;
  valid_from: number;
  valid_to: number | null;
  recorded_at: number;
  last_accessed_at: number | null;
  access_count: number;
  pinned: boolean;
  superseded_by: string | null;
  deleted_at: number | null;
  embedding_status: string;
};

export type FilterOptions = { agents: string[]; categories: string[] };

/** 编辑结果（T-M2-02 / docs/04 §10.1 承诺强度）。
 *  `action` 是分支唯一判据，**不要**用 note 文案做判断。 */
export type UpdateResult = {
  /** saved | superseded | merged_scope | duplicate */
  action: string;
  id: string;
  superseded_id: string | null;
  note: string;
  /** 撤销句柄（T-M2-07 的 5 秒撤销）；null = 本次没有写入（duplicate） */
  event_id: number | null;
};

/** 删除前的影响面（T-M2-03）。数字必须与执行删除时的真实影响一致。 */
export type DeleteImpact = {
  id: string;
  summary: string;
  pinned: boolean;
  already_deleted: boolean;
  access_count: number;
  /** 结构化属性条数（硬删会级联清掉） */
  facts: number;
  /** 关系边条数（软删不动它，靠查询过滤） */
  edges: number;
};

export function deleteImpact(id: string): Promise<DeleteImpact> {
  return invoke<DeleteImpact>("delete_impact", { id });
}

/** `soft` 可恢复；`hard` 不可逆（core 会先写 journal 快照）。返回事件 id（撤销句柄）。 */
export function forgetMemory(id: string, mode: "soft" | "hard"): Promise<number> {
  return invoke<number>("forget_memory", { id, mode });
}

export function restoreMemory(id: string): Promise<void> {
  return invoke<void>("restore_memory", { id });
}

// ---------------- 撤销 / 回滚（T-M2-07）----------------

export type UndoItem = {
  event_id: number;
  memory_id: string;
  note: string;
};

/** 撤销一条操作（编辑 / 内容变更 / 删除 / 失效 / 合并；硬删会被拒绝）。 */
export function undoEvent(eventId: number): Promise<UndoItem> {
  return invoke<UndoItem>("undo_event", { eventId });
}

export type ActivityItem = {
  event_id: number;
  operation: string;
  memory_id: string;
  summary: string;
  agent_id: string;
  created_at: number;
  /** 服务端判定：是否可撤销（前端不许自己猜） */
  undoable: boolean;
  /** 已被撤销过 */
  undone: boolean;
};

/** 最近操作（维护页「最近操作」区块）。 */
export function listActivity(limit = 30): Promise<ActivityItem[]> {
  return invoke<ActivityItem[]>("list_activity", { limit });
}

// ---------------- Pin / 标记失效（T-M2-04）----------------

/** Pin 免疫：pinned 时 `effective_importance` 下限为 1.0，不会被判成退役候选。返回事件 id。 */
export function togglePin(id: string, pinned: boolean): Promise<number> {
  return invoke<number>("toggle_pin", { id, pinned });
}

/** 标记失效：行保留可查历史，但召回不再返回。返回事件 id（撤销句柄）。 */
export function invalidateMemory(id: string): Promise<number> {
  return invoke<number>("invalidate_memory", { id });
}

/** 清除失效标记 → 重新可被召回。 */
export function restoreValidity(id: string): Promise<void> {
  return invoke<void>("restore_validity", { id });
}

/** 读时计算的 effective_importance（ADR-013：不落盘，不用它做删除决策）。 */
export function effectiveImportance(id: string): Promise<number> {
  return invoke<number>("effective_importance", { id });
}

/** GUI 编辑一条记忆。走 core 的 MemoryService（ADR-034 过渡方案）。 */
export function updateMemory(args: {
  id: string;
  content?: string | null;
  importance?: number | null;
  tags?: string[] | null;
  pinned?: boolean | null;
  scope?: string | null;
}): Promise<UpdateResult> {
  return invoke<UpdateResult>("update_memory", args);
}

// ---------------- 合并重复（T-M2-05）----------------

/** 合并预览：数字与真正合并用**同一套查询**（core 的 merge_preview），
 *  否则会出现"弹窗说搬 3 条、实际搬 5 条"。 */
export type MergePreview = {
  keep_id: string;
  drop_id: string;
  keep_summary: string;
  drop_summary: string;
  keep_agent: string;
  drop_agent: string;
  /** 合并后的标签集合 */
  tags: string[];
  /** 真正从被并条搬来的标签 */
  tags_moved: string[];
  edges_moved: number;
  /** 会因变成自环而被丢弃的边数 */
  self_loops: number;
  facts_moved: number;
};

export function mergePreview(keepId: string, dropId: string): Promise<MergePreview> {
  return invoke<MergePreview>("merge_preview", { keepId, dropId });
}

export type MergeResult = {
  /** 撤销句柄：交给 undoMerge */
  event_id: number;
  keep_id: string;
  drop_id: string;
  tags: string[];
  tags_moved: string[];
  edges_moved: number;
  self_loops: number;
  facts_moved: number;
  note: string;
};

/** 合并两条重复：搬标签/实体/事实/关系 → 软删被并条（可撤销）。 */
export function mergeMemories(keepId: string, dropId: string): Promise<MergeResult> {
  return invoke<MergeResult>("merge_memories", { keepId, dropId });
}

export type UndoResult = {
  event_id: number;
  keep_id: string;
  drop_id: string;
  note: string;
};

/** 撤销一次合并：按 memory_events 里的逆操作清单还原两条。 */
export function undoMerge(eventId: number): Promise<UndoResult> {
  return invoke<UndoResult>("undo_merge", { eventId });
}

// ---------------- 维护页待办（T-M2-06）----------------

export type TodoItem = {
  /** duplicate | conflict | untagged | no-vector | stale */
  key: string;
  label: string;
  count: number;
  desc: string;
  /** groups（逐组决策）| conflicts（四选一）| list（跳记忆页带筛选） */
  action: string;
};

export type DuplicateGroup = {
  key: string;
  items: MemItem[];
};

export type ConflictItem = {
  id: number;
  entity_type: string;
  entity_id: string;
  property: string;
  memory_a: string;
  memory_b: string;
  value_a: string;
  value_b: string;
  summary_a: string;
  summary_b: string;
  agent_a: string;
  agent_b: string;
  created_at: number;
};

export type MaintenanceTodos = {
  items: TodoItem[];
  groups: DuplicateGroup[];
  conflicts: ConflictItem[];
  total_categories: number;
};

/** 维护页一次性取全（五类待办 + 重复组 + 冲突详情）。 */
export function maintenanceTodos(): Promise<MaintenanceTodos> {
  return invoke<MaintenanceTodos>("maintenance_todos");
}

/** 退役候选行（T-M4-01，只读）。 */
export type RetirementRow = {
  id: string;
  content_head: string;
  ei: number;
  importance: number;
  access_count: number;
  days_since_access: number;
  edge_count: number;
  origin: string;
  reasons: string[];
};

/** 退役候选（按 ei 公式算，只读；免疫规则在 core）。 */
export function retirementCandidates(threshold?: number): Promise<RetirementRow[]> {
  return invoke<RetirementRow[]>("retirement_candidates", { threshold });
}

/** 可逆降权（importance-1）：返回 event_id，可撤销。 */
export function retireMemory(id: string): Promise<UpdateResult> {
  return invoke<UpdateResult>("retire_memory", { id });
}

/** 恢复降权：把最近一次降权的原值写回。 */
export function restoreRetireMemory(id: string): Promise<UpdateResult> {
  return invoke<UpdateResult>("restore_retire_memory", { id });
}

/** 可恢复项（T-M4-03）：已降权 / 已软删。 */
export type RecoverableRow = {
  id: string;
  content_head: string;
  /** demoted = 已降权；soft_deleted = 已软删 */
  kind: string;
  current_importance: number;
  baseline_importance: number;
  undo_event_id: number | null;
  updated_at: number;
};

/** 可恢复列表（只读）。 */
export function recoveryList(): Promise<RecoverableRow[]> {
  return invoke<RecoverableRow[]>("recovery_list");
}

/** 恢复到最初重要度（沿 events 链回放）。 */
export function restoreToBaseline(id: string): Promise<UpdateResult> {
  return invoke<UpdateResult>("restore_to_baseline_memory", { id });
}

/** 清理孤儿边（T-M4-04）：只删指向不存在记忆的边。 */
export function cleanupOrphanEdges(): Promise<{ removed_edges: number; kept_edges: number }> {
  return invoke("cleanup_orphan_edges");
}

/** 补算缺失向量（T-M4-04）：无端点时回 degraded。 */
export function reembedPending(): Promise<{
  tried: number;
  ok: number;
  failed: number;
  degraded: string[];
}> {
  return invoke("reembed_pending");
}

/** 备份校验报告（T-M5-01）。 */
export type BackupReport = {
  path: string;
  integrity_ok: boolean;
  schema_version: number;
  current_schema_version: number;
  table_count: number;
  memories: number;
  current_memories: number;
  events: number;
  current_events: number;
  /** integrity + 版本 + 记忆数 + 事件数 全一致 */
  matches: boolean;
};

export type BackupEntry = {
  name: string;
  path: string;
  size_bytes: number;
  modified: number;
};

/** 立即备份（Online Backup API）+ 自动校验。 */
export function createBackup(): Promise<BackupReport> {
  return invoke<BackupReport>("create_backup");
}

/** 备份列表（最新在前）。 */
export function listBackups(): Promise<BackupEntry[]> {
  return invoke<BackupEntry[]>("list_backups");
}

/** 校验一个备份文件。 */
export function verifyBackup(path: string): Promise<BackupReport> {
  return invoke<BackupReport>("verify_backup", { path });
}

/** 恢复第一步的结果（T-M5-02）。 */
export type StageReport = {
  pending_path: string;
  schema_version: number;
  memories: number;
  /** 必须重启应用才生效 */
  needs_restart: boolean;
};

/** 从备份恢复（第一步：校验并落成"待恢复"，不改当前库，重启生效）。 */
export function stageRestore(path: string): Promise<StageReport> {
  return invoke<StageReport>("stage_restore", { path });
}

/** launcher 运行态（T-M5-03）：pid / port / 重启次数。 */
export type LauncherEntry = {
  role: string;
  pid: number;
  port: number;
  started_at: number;
  restarts: number;
};

export type LauncherEvent = {
  at: number;
  role: string;
  event: "restart" | "gave_up";
  exit_code: number | null;
  restarts: number;
};

/** 读取 launcher 运行态（含重启次数）。 */
export function launcherState(): Promise<LauncherEntry[]> {
  return invoke<LauncherEntry[]>("launcher_state");
}

/** 读取最近的重启事件。 */
export function launcherEvents(limit = 10): Promise<LauncherEvent[]> {
  return invoke<LauncherEvent[]>("launcher_events", { limit });
}

/** 诊断包导出结果（T-M5-04）。 */
export type DiagReport = {
  dir: string;
  files: string[];
  /** 命中疑似凭据并整行脱敏的行数 */
  redacted_lines: number;
};

/** 导出诊断包（日志 + launcher + db-info + env，已脱敏）。 */
export function exportDiagnostics(): Promise<DiagReport> {
  return invoke<DiagReport>("export_diagnostics");
}

/** 端口探测结论（T-M5-05）。 */
export type PortProbe = {
  port: number;
  /** 端口表里这个端口应该由谁提供 */
  expected: string;
  listening: boolean;
  /** 实际探测到的角色 */
  served_by: string | null;
  /** 与端口表一致 */
  ok: boolean;
  note: string;
};

/** 端口占用一览。 */
export function portProbes(): Promise<PortProbe[]> {
  return invoke<PortProbe[]>("port_probes");
}

/** 首启引导信息（T-M6-02）。 */
export type OnboardingInfo = {
  data_dir: string;
  db_path: string;
  mcp_path: string | null;
  mcp_available: boolean;
  embedding_configured: boolean;
  ports: { core: number; proxy: number; recall: number };
};

/** 读取首启引导所需信息（数据目录 / MCP 二进制 / 嵌入配置状态）。 */
export function onboardingInfo(): Promise<OnboardingInfo> {
  return invoke<OnboardingInfo>("onboarding_info");
}

/** 一个 agent 的接入现状（T-M6-04）。 */
export type AgentConnection = {
  id: string;
  display_name: string;
  config_path: string;
  format: string;
  exists: boolean;
  attached: boolean;
  supports_mcp: boolean;
  note: string;
};

/** 探测 4 个内置 agent 的接入状态。 */
export function agentConnections(): Promise<AgentConnection[]> {
  return invoke<AgentConnection[]>("agent_connections");
}

/** 启动时 schema 迁移失败的原因（null = 正常）。T-M6-05 */
export function schemaIssue(): Promise<string | null> {
  return invoke<string | null>("schema_issue");
}

/** 一键接入（会先备份原配置；幂等；**不回传令牌**）。 */
export function attachAgent(id: string): Promise<{
  agent_id: string;
  config_path: string;
  changed: boolean;
  backup_path: string | null;
}> {
  return invoke("attach_agent", { id });
}

/** 重新扫描冲突（幂等），返回新增条数。 */
export function detectConflicts(): Promise<number> {
  return invoke<number>("detect_conflicts");
}

/** 裁决冲突：keep_a | keep_b | keep_both | merged（merged 需先合并两条）。 */
export function resolveConflict(id: number, resolution: string): Promise<void> {
  return invoke<void>("resolve_conflict", { id, resolution });
}

// ---------------- 关系（T-M2-08 补 J4：把两条记忆连起来）----------------

export type RelatedItem = {
  id: string;
  summary: string;
  edge_type: string;
  weight: number;
};

/** 某条记忆的关联（双向）。 */
export function relatedMemories(id: string): Promise<RelatedItem[]> {
  return invoke<RelatedItem[]>("related_memories", { id });
}

/** 建立一条关系。edge_type 只接受 temporal/semantic/causal/entity/supersedes。 */
export function linkMemory(
  sourceId: string,
  targetId: string,
  edgeType: string,
  weight = 1.0,
): Promise<void> {
  return invoke<void>("link_memory", { sourceId, targetId, edgeType, weight });
}

export type ProcStatus = {
  role: string;
  alive: boolean;
  pid: number | null;
  port: number | null;
  version: string | null;
  error: string | null;
};

declare global {
  interface Window {
    __TAURI__?: {
      core?: {
        invoke: <T>(cmd: string, args?: Record<string, unknown>) => Promise<T>;
      };
    };
  }
}

export function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const c = window.__TAURI__?.core;
  if (!c) {
    return Promise.reject(
      new Error("不在 Tauri 环境中运行（纯浏览器预览下 window.__TAURI__ 不存在）"),
    );
  }
  return c.invoke<T>(cmd, args);
}

/** Unix 秒 → 本地 `MM-DD` / `MM-DD HH:mm` / `HH:mm`。 */
export function fmtDate(sec: number | null, mode: "date" | "datetime" | "time" = "date"): string {
  if (!sec) return "—";
  const d = new Date(sec * 1000);
  const p = (n: number) => String(n).padStart(2, "0");
  const md = `${p(d.getMonth() + 1)}-${p(d.getDate())}`;
  const hm = `${p(d.getHours())}:${p(d.getMinutes())}`;
  if (mode === "time") return hm;
  if (mode === "datetime") return `${md} ${hm}`;
  return md;
}

/** 相对时间：刚刚 / 3 分钟前 / 2 小时前 / 3 天前。 */
export function relTime(sec: number | null): string {
  if (!sec) return "—";
  const diff = Date.now() / 1000 - sec;
  if (diff < 60) return "刚刚";
  if (diff < 3600) return `${Math.floor(diff / 60)} 分钟前`;
  if (diff < 86400) return `${Math.floor(diff / 3600)} 小时前`;
  return `${Math.floor(diff / 86400)} 天前`;
}

/** 数字千分位（1583 → 1,583）。 */
export function fmtNum(n: number): string {
  return n.toLocaleString("zh-CN");
}

/** 单向体检结果（T-M9-01，PRD J8）。status: pass | fail | waived */
export interface CheckItem {
  id: string;
  title: string;
  /** 判据（人话）：这一项在验什么 */
  criterion: string;
  status: "pass" | "fail" | "waived";
  /** 明细：实测值 / why waived */
  detail: string;
}

/** 一键体检：逐项结果（每项自带判据与明细）。 */
export function healthCheckup(): Promise<CheckItem[]> {
  return invoke<CheckItem[]>("health_checkup");
}

/** 体检小结（total/pass/fail/waived）。 */
export function healthSummary(): Promise<{
  total: number;
  pass: number;
  fail: number;
  waived: number;
}> {
  return invoke("health_summary");
}

// ===== T-M10-06（J6）：注入统计（读 proxy 的 metrics JSONL，只读）=====

/** 一天的注入汇总（趋势柱）。 */
export type DayInjection = {
  day: string;
  rows: number;
  hits: number;
  tokens: number;
};

/** 注入统计。`empty` 时看 `reason`：no-dir / no-file / no-rows / all-off */
export type InjectionStats = {
  empty: boolean;
  reason: string;
  days: number;
  total_rows: number;
  injected_rows: number;
  hits_total: number;
  tokens_total: number;
  cache_hit_rows: number;
  skipped_rows: number;
  /** T-M10-40：其中 status=off（注入被关/无命中） */
  off_rows: number;
  /** T-M10-40：其中真的超时（区别于"没命中"） */
  timeout_rows: number;
  p50_ms: number;
  p95_ms: number;
  bad_lines: number;
  daily: DayInjection[];
  dir: string;
};

/** 注入统计（窗口 1/7/30 天）。 */
export function injectionStats(days = 7): Promise<InjectionStats> {
  return invoke<InjectionStats>("injection_stats", { days });
}

// ===== T-M10-11（ADR-051）：上游与注入配置 =====

/** proxy 配置（文件 > 环境变量 > 默认）。`exists` = 配置文件是否已写出。 */
export type ProxyConfig = {
  upstream: string;
  inject: boolean;
  core_url: string;
  path: string;
  exists: boolean;
};

export function proxyConfig(): Promise<ProxyConfig> {
  return invoke<ProxyConfig>("proxy_config");
}

/** 保存并重启 proxy（kill 子进程 → 启动器 watchdog 自动拉起读新配置）。 */
export function saveProxyConfig(
  upstream: string,
  inject: boolean,
): Promise<{ saved: boolean; restarted: boolean; path: string }> {
  return invoke("save_proxy_config", { upstream, inject });
}

// ===== T-M10-26：一键接入状态检测（只读）=====

export type InjectState = {
  id: string;
  name: string;
  deployed: boolean;
  /** file / hook / plugin / extension */
  kind: string;
  path: string;
  detail: string;
  /** T-M10-37：安装位置是**怎么找到的**（where 命令 / 常见目录 / 用户目录） */
  probe_source: string | null;
};

export function injectStatus(): Promise<InjectState[]> {
  return invoke<InjectState[]>("inject_status");
}

/** T-M10-27：部署注入适配器（本版只支持 workbuddy）。 */
export function deployInject(id: string): Promise<{
  ok: boolean;
  agent: string;
  path: string;
  backup: string;
  block_chars: number;
}> {
  return invoke("deploy_inject", { id });
}

export function removeInject(id: string): Promise<{
  ok: boolean;
  removed: boolean;
  path?: string;
  backup?: string;
  note?: string;
}> {
  return invoke("remove_inject", { id });
}

/** 用该 agent 身份调一次 /recall，确认"能召回"（不是只看文件写没写）。 */
export function verifyInject(id: string): Promise<{
  ok: boolean;
  agent: string;
  recall_count: number;
  note: string;
}> {
  return invoke("verify_inject", { id });
}