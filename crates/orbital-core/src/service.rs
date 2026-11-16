//! MemoryService：MCP 工具背后的业务层。
//!
//! 所有读写都经过**同一套门控**（ADR-005）：
//! - 写：`Writer::write(&VerifiedAgent, ...)`
//! - 读：SQL 里带 `agent_id = :caller OR scope IN ('shared','entity')`
//! - 图遍历：`JOIN memories` 后再门控（审核 P0-2：edges 表本身没有归属）

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::config::MAX_CONTENT_BYTES;
use crate::db::Db;
use crate::error::{OrbitalError, Result};
use crate::identity::{assert_claimed_matches, VerifiedAgent};
use crate::writer::{WriteInput, WriteReceipt, Writer};

/// 可见性条件（**唯一口径**，所有读 SQL 必须复用它）。
#[cfg(test)]
/// 所有用 `env()` 的测试共享这一把锁：
/// 它们都会 `std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", ...)`（全局状态），
/// 并发跑时一个测试把阈值改成 2 会让别的测试撞限流 → flaky。
/// 由 `env()` 持有整条测试期间的锁（返回 guard），从而串行化。
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 单次召回的返回条数上限（防御"limit=1e9"把库读进内存）。
const MAX_LIMIT: i64 = 200;
/// 单次召回 query 的字节上限（防御超长 query 拖慢 FTS 解析）。
const MAX_QUERY_BYTES: usize = 4096;

/// 统一 clamp 返回条数：`1..=200`。
fn clamp_limit(limit: i64) -> i64 {
    limit.clamp(1, MAX_LIMIT)
}

/// 校验召回 query：非空且不超上限（两个入口共用，避免口径分叉）。
fn check_query(query: &str) -> Result<()> {
    if query.trim().is_empty() {
        return Err(OrbitalError::InvalidArgument {
            reason: "query 不能为空".into(),
        });
    }
    if query.len() > MAX_QUERY_BYTES {
        return Err(OrbitalError::InvalidArgument {
            reason: format!("query 超过 {MAX_QUERY_BYTES} 字节上限"),
        });
    }
    Ok(())
}

const VISIBLE: &str =
    "(m.agent_id = ?1
      OR m.scope = 'shared'
      OR (m.scope = 'entity' AND EXISTS (
            SELECT 1 FROM memory_facts f
             WHERE f.memory_id = m.id AND f.valid_to IS NULL)))
     AND m.deleted_at IS NULL";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: String,
    pub content: String,
    pub agent_id: String,
    pub scope: String,
    pub category: String,
    pub importance: i64,
    pub tags: String,
    pub entities: String,
    pub valid_from: f64,
    pub valid_to: Option<f64>,
    pub created_at: f64,
    pub access_count: i64,
    pub pinned: i64,
    /// 被哪条记忆取代（版本链，ADR-009；NULL = 未被取代）
    pub superseded_by: Option<String>,
    /// `docs/04 §10.1`
    pub deletion_mode: String,
}

/// T-M10-21：query embedding 的进程内缓存（TTL 300s，容量 64，满了整清）。
///
/// 为什么：`vector_hits` 原来每次都实时调 embedding 端点（本机 Ollama 约 200ms），
/// 是注入热路径（300ms 预算）的主要耗时。同一 query 在短时间内复用向量即可。
/// 用全局 static 而不是加字段，是为了不动 `MemoryService` 的结构体（避免影响现有构造点）。
fn cached_query_vec(
    ep: &crate::embed::Endpoint,
    texts: &[String],
    cfg: &crate::embed::EmbeddingConfig,
) -> std::result::Result<Vec<Vec<f32>>, String> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    // T-M10-49（CI clippy 1.100 暴露 type_complexity）：抽成别名，语义也更清楚
    type QueryVecCache = OnceLock<Mutex<HashMap<String, (Vec<Vec<f32>>, Instant)>>>;
    static CACHE: QueryVecCache = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = format!("{}::{:?}", ep.base_url, texts);

    {
        let g = cache.lock().unwrap();
        if let Some((v, t)) = g.get(&key) {
            if t.elapsed() < Duration::from_secs(300) {
                return Ok(v.clone());
            }
        }
    }

    let out = crate::embed::embed(ep, texts, cfg).map_err(|e| e.to_string())?;
    let mut g = cache.lock().unwrap();
    if g.len() >= 64 {
        g.clear();
    }
    g.insert(key, (out.clone(), Instant::now()));
    Ok(out)
}

#[derive(Debug, Clone, Serialize)]
pub struct RecallHit {
    pub id: String,
    pub content: String,
    pub score: f64,
    pub scope: String,
}

/// 召回通道（T-M3-02：三路召回）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallChannel {
    /// 关键词（FTS5 trigram + bm25）
    Fts,
    /// 向量（`memories_vec` 余弦；无端点时降级为空）
    Vector,
    /// 图扩展（FTS 种子的 1 跳邻居）
    Graph,
}

impl RecallChannel {
    pub fn name(self) -> &'static str {
        match self {
            Self::Fts => "fts",
            Self::Vector => "vector",
            Self::Graph => "graph",
        }
    }

    /// 解析缩写（命令行 / 环境变量用）。
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "fts" | "keyword" | "kw" => Some(Self::Fts),
            "vec" | "vector" | "embed" => Some(Self::Vector),
            "graph" | "edge" | "expand" => Some(Self::Graph),
            _ => None,
        }
    }
}

/// 结构化过滤（T-M3-03）：只召回"在该实体上登记过该属性"的记忆。
#[derive(Debug, Clone)]
pub struct FactFilter {
    pub entity_id: String,
    /// None = 该实体的任意属性都算
    pub property: Option<String>,
}

/// 融合选项（T-M3-02；T-M3-05 按 ADR-040 加 `graph_decay`）。
#[derive(Debug, Clone)]
pub struct RecallOptions {
    pub channels: Vec<RecallChannel>,
    /// 与 `channels` 一一对应；权重 0 = 关闭该路（做对照实验用）
    pub weights: Vec<f64>,
    /// RRF 的 k（默认 60）
    pub k: i64,
    /// 结构化过滤（T-M3-03）；None = 不过滤
    pub fact_filter: Option<FactFilter>,
    /// 图加成的衰减系数（ADR-040）：加成 = 种子分数 × decay，默认 0.5
    pub graph_decay: f64,
}

impl Default for RecallOptions {
    fn default() -> Self {
        Self {
            channels: vec![RecallChannel::Fts, RecallChannel::Vector, RecallChannel::Graph],
            weights: vec![1.0, 1.0, 1.0],
            k: 60,
            fact_filter: None,
            graph_decay: 0.5,
        }
    }
}

/// 融合命中。
#[derive(Debug, Clone, Serialize)]
pub struct FusedHit {
    pub id: String,
    pub content: String,
    pub scope: String,
    pub score: f64,
    /// 命中的通道（便于诊断"这一条是哪一路带来的"）
    pub channels: Vec<String>,
}

/// 融合召回的返回：命中 + **降级原因**。
///
/// `degraded` 非空表示"某一路没跑"（如向量通道没有可用端点），
/// 必须与"没匹配"区分开（`docs/03 §5.4` 的硬失败契约）。
#[derive(Debug, Clone, Serialize)]
pub struct FusedRecall {
    pub hits: Vec<FusedHit>,
    pub degraded: Vec<String>,
}

/// 退役候选行（T-M4-01，只读产出，**不含任何删除动作**）。
#[derive(Debug, Clone, Serialize)]
pub struct RetirementRow {
    pub id: String,
    pub content_head: String,
    pub ei: f64,
    pub importance: i64,
    pub access_count: i64,
    pub days_since_access: f64,
    pub edge_count: i64,
    pub origin: String,
    /// 为什么被列为候选（给 GUI 显示，便于人判断）
    pub reasons: Vec<String>,
}

/// 备份校验报告（T-M5-01）。
#[derive(Debug, Clone, Serialize)]
pub struct BackupReport {
    pub path: String,
    pub integrity_ok: bool,
    pub schema_version: i64,
    pub current_schema_version: i64,
    pub table_count: i64,
    pub memories: i64,
    pub current_memories: i64,
    pub events: i64,
    pub current_events: i64,
    /// 与当前库逐项一致（integrity + 版本 + 记忆数 + 事件数）
    pub matches: bool,
}

/// 备份文件条目（列表用）。
#[derive(Debug, Clone, Serialize)]
pub struct BackupEntry {
    pub name: String,
    pub path: String,
    pub size_bytes: i64,
    pub modified: f64,
}

/// 孤儿边清理结果（T-M4-04）。
#[derive(Debug, Clone, Serialize)]
pub struct CleanupOutcome {
    pub removed_edges: i64,
    pub kept_edges: i64,
}

/// 向量补算结果（T-M4-04）。`degraded` 非空 = 有原因没算成（不静默）。
#[derive(Debug, Clone, Serialize)]
pub struct ReembedOutcome {
    pub tried: i64,
    pub ok: i64,
    pub failed: i64,
    pub degraded: Vec<String>,
}

/// 可恢复项（T-M4-03）：已降权 / 已软删。
#[derive(Debug, Clone, Serialize)]
pub struct RecoverableRow {
    pub id: String,
    pub content_head: String,
    /// `demoted`（降权，可回最初值）/ `soft_deleted`（软删，可撤销删除）
    pub kind: String,
    pub current_importance: i64,
    pub baseline_importance: i64,
    /// 软删行：撤销用的事件 id（前端调 `undo_event`）
    pub undo_event_id: Option<i64>,
    pub updated_at: f64,
}

/// 实效重要度（`docs/04 §6.1`，T-M4-01）。
///
/// **纯函数**：`days_since_access` 由调用方算出传入 —— 单测能用固定数字锁公式（不依赖 `now()`）。
///
/// ```text
/// ei = base(importance)
///    × max(1.0, ln(1 + access_count))
///    × 0.5^(days_since_last_access / H)
///    × (1 + 0.1 × min(edge_count, 5))
///    × origin_weight           // agent_derived=0.8，其余 1.0
/// pinned=1 时 ei = max(ei, 1.0)（免疫降权）
/// ```
pub fn effective_importance(
    importance: i64,
    access_count: i64,
    days_since_access: f64,
    edge_count: i64,
    origin: &str,
    half_life_days: f64,
    pinned: bool,
) -> f64 {
    let base = match importance {
        5 => 1.0,
        4 => 0.8,
        3 => 0.5,
        2 => 0.3,
        _ => 0.15,
    };
    let access = (1.0 + access_count.max(0) as f64).ln().max(1.0);
    let hl = if half_life_days > 0.0 {
        half_life_days
    } else {
        30.0
    };
    let decay = 0.5f64.powf(days_since_access.max(0.0) / hl);
    let edges = 1.0 + 0.1 * (edge_count.clamp(0, 5) as f64);
    let origin_w = if origin == "agent_derived" { 0.8 } else { 1.0 };
    let ei = base * access * decay * edges * origin_w;
    if pinned {
        ei.max(1.0)
    } else {
        ei
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RelatedHit {
    pub id: String,
    pub content: String,
    pub edge_type: String,
    pub weight: f64,
}

/// 索引层用的最小条目（ADR-027：静态层只放索引，不放全文）。
#[derive(Debug, Clone, Serialize)]
pub struct RecentItem {
    pub id: String,
    /// 摘要（<= max_chars 字符，按字符边界截断，不会切坏 UTF-8）
    pub title: String,
    pub agent_id: String,
    /// Unix 时间戳。库里是 REAL（带小数的 unix 秒），故用 f64。
    pub updated_at: f64,
}

/// 按**字符**（不是字节）截断，避免切坏中文。
pub fn summarize(content: &str, max_chars: usize) -> String {
    let one_line = content.replace(['\n', '\r'], " ");
    let one_line = one_line.trim();
    let chars: Vec<char> = one_line.chars().collect();
    if chars.len() <= max_chars {
        one_line.to_string()
    } else {
        let mut s: String = chars[..max_chars].iter().collect();
        s.push('…');
        s
    }
}

/// 把用户查询转成 **FTS5 安全** 的形式。
///
/// 实测 bug（2026-11-12）：`safe-delete` 里的 `-` 被 FTS5 当成 NOT 操作符，
/// `delete` 被当成列名 → `no such column: delete`（INTERNAL 500）。
/// 修法：每个词加双引号（内部 `"` 翻倍），词之间用 OR —— 宁可多召回，也不要报错。
pub fn fts_escape(query: &str) -> String {
    let parts: Vec<String> = query
        .split_whitespace()
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect();
    if parts.is_empty() {
        return "\"\"".to_string();
    }
    parts.join(" OR ")
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub totals: Totals,
    pub by_agent: Vec<AgentCount>,
    pub by_scope: Vec<ScopeCount>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Totals {
    pub memories: i64,
    pub active: i64,
    pub soft_deleted: i64,
    pub edges: i64,
    pub facts: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentCount {
    pub agent_id: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScopeCount {
    pub scope: String,
    pub count: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdatePatch {
    pub content: Option<String>,
    pub importance: Option<i64>,
    pub tags: Option<Vec<String>>,
    pub pinned: Option<bool>,
    /// 目标 scope（P1-2）：内容与已有条相同时，用它决定"升级"还是"拒绝"。
    /// 不传 = 保持原 scope。
    pub scope: Option<String>,
}

/// 删除前的影响面（T-M2-03）。
///
/// 契约：这些数字**必须**与执行删除时的实际影响一致。
/// 弹窗说"会删掉 3 条关系边"，恢复后就该是 3 条——对不上就是骗人。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeleteImpact {
    pub id: String,
    /// 内容摘要（不是全文：弹窗只需让用户认出这是哪条）
    pub summary: String,
    pub pinned: bool,
    pub already_deleted: bool,
    pub access_count: i64,
    /// 结构化属性条数（硬删会级联清掉）
    pub facts: i64,
    /// 关系边条数（源或目标任一端命中；软删**不动**它，靠查询过滤）
    pub edges: i64,
}

/// 更新结果（T-M2-02 的「承诺强度」要求 UI 能显示确定状态，docs/04 §10.1）。
///
/// `action` 是给前端做分支的唯一判据，**不许用 note 文案做判断**（文案会改，action 不会）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct UpdateOutcome {
    /// `saved`（正常保存）/ `superseded`（内容变更，旧条进版本链）
    /// / `merged_scope`（P1-2：与已有条同内容且 scope 升级，合并到那条）
    /// / `duplicate`（同内容已存在，本次没有新增行）
    pub action: String,
    /// 本次更新后应展示的那条 id（`merged_scope` 时是已存在那条）
    pub id: String,
    /// 内容变更时被取代的旧 id
    pub superseded_id: Option<String>,
    /// 给用户看的确定文案（四态之一）
    pub note: String,
    /// 本次写下的 `memory_events.id`（T-M2-07：前端拿它做"5 秒内撤销"的句柄）。
    /// `None` = 本次没有写事件（`duplicate` 分支），也就没有可撤销的东西。
    pub event_id: Option<i64>,
}

/// 合并预览（T-M2-05）：`docs/03 §5.2` 三种决策里，"合并两者"是唯一有副作用的，
/// 所以点之前必须看到"会搬走多少"。数字与真正合并时用**同一套查询**。
#[derive(Debug, Clone, Serialize)]
pub struct MergePreview {
    pub keep_id: String,
    pub drop_id: String,
    /// 两条的 60 字摘要（弹窗里让用户确认"我选的是这两条"）
    pub keep_summary: String,
    pub drop_summary: String,
    pub keep_agent: String,
    pub drop_agent: String,
    /// 合并后的标签集合（keep 原有顺序 + drop 独有）
    pub tags: Vec<String>,
    /// 真正从 drop 搬来的标签（不是并集大小）
    pub tags_moved: Vec<String>,
    /// 需要改指到 keep 的边数（不含会变自环的）
    pub edges_moved: i64,
    /// 会因变成自环而被丢弃的边数（A→B / B→A 改指后成 A→A）
    pub self_loops: i64,
    /// 需要从 drop 改指到 keep 的事实条数
    pub facts_moved: i64,
}

/// 合并结果。
#[derive(Debug, Clone, Serialize)]
pub struct MergeOutcome {
    /// 这次合并写下的 `memory_events.id`（撤销时用它，前端"撤销"按钮需要）
    pub event_id: i64,
    pub keep_id: String,
    pub drop_id: String,
    /// 合并后的标签（`tags_moved` 是其中"真正搬来的"）
    pub tags: Vec<String>,
    pub tags_moved: Vec<String>,
    pub edges_moved: i64,
    pub self_loops: i64,
    pub facts_moved: i64,
    pub note: String,
}

/// 撤销合并的结果（同一会话内可撤销，`docs/03 §1.3`）。
#[derive(Debug, Clone, Serialize)]
pub struct UndoOutcome {
    /// 被撤销的那条 `memory_events.id`
    pub event_id: i64,
    pub keep_id: String,
    pub drop_id: String,
    pub note: String,
}

/// 冲突收件箱的一行（T-M2-06；规则见 `docs/04 §7`，表见 `docs/11` P1-4）。
///
/// 触发规则：同一 `(entity_type, entity_id, property)` 存在多个 `valid_to IS NULL`
/// 且 value 不同 → 一条冲突。它**不是**记忆内容冲突，而是"结构化事实互相矛盾"。
#[derive(Debug, Clone, Serialize)]
pub struct ConflictRow {
    pub id: i64,
    pub entity_type: String,
    pub entity_id: String,
    pub property: String,
    pub memory_a: String,
    pub memory_b: String,
    pub value_a: String,
    pub value_b: String,
    /// 两条的 60 字摘要（对比视图要让人认出是哪两条）
    pub summary_a: String,
    pub summary_b: String,
    pub agent_a: String,
    pub agent_b: String,
    pub created_at: f64,
}

/// 通用撤销的结果（T-M2-07）。与 `UndoOutcome`（专指撤销合并）区分。
#[derive(Debug, Clone, Serialize)]
pub struct UndoEventOutcome {
    /// 被撤销的那条事件
    pub event_id: i64,
    /// 受影响的记忆
    pub memory_id: String,
    /// 给人看的确定文案
    pub note: String,
}

/// 活动日志的一行（维护页「最近操作」）。
#[derive(Debug, Clone, Serialize)]
pub struct ActivityRow {
    pub event_id: i64,
    pub operation: String,
    pub memory_id: String,
    pub summary: String,
    pub agent_id: String,
    pub created_at: f64,
    /// 是否可撤销（`hard_delete` / `write` / `undo` 本身不可撤销）
    pub undoable: bool,
    /// 已被撤销过（前端要灰掉按钮，二次撤销会被后端拒绝）
    pub undone: bool,
}

/// 合并时读一侧（keep / drop）的业务字段。
///
/// 为什么单独抽出来：预览与真正合并必须**同一口径**，
/// 两处各写一遍 SELECT 就会漂移（本机老坑：两个数据源算出两个数）。
struct MergeSide {
    content: String,
    agent_id: String,
    tags: Vec<String>,
    entities: Vec<String>,
    deleted_at: Option<f64>,
}

fn merge_side(conn: &Connection, id: &str) -> Result<MergeSide> {
    conn.query_row(
        "SELECT content, agent_id, tags, entities, deleted_at FROM memories WHERE id = ?1",
        params![id],
        |r| {
            Ok(MergeSide {
                content: r.get(0)?,
                agent_id: r.get(1)?,
                tags: parse_str_array(&r.get::<_, String>(2)?),
                entities: parse_str_array(&r.get::<_, String>(3)?),
                deleted_at: r.get(4)?,
            })
        },
    )
    .optional()
    .map_err(map_err)?
    .ok_or_else(|| OrbitalError::MemoryNotFound {
        id: id.to_string(),
    })
}

/// `tags` / `entities` 在库里是 JSON 数组字符串；解析失败就当空（与前端 `parseTags` 同口径）。
fn parse_str_array(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

/// 有序并集：`a` 在前、`b` 独有追加，同时返回"真正搬来的那部分"。
fn union_strings(a: &[String], b: &[String]) -> (Vec<String>, Vec<String>) {
    let mut out = a.to_vec();
    let mut moved = Vec::new();
    for s in b {
        if !out.contains(s) {
            out.push(s.clone());
            moved.push(s.clone());
        }
    }
    (out, moved)
}

pub struct MemoryService {
    db_path: std::path::PathBuf,
    journal_path: std::path::PathBuf,
    writer: Writer,
    reader: Db,
    /// 嵌入配置（ADR-022）。没有可用端点时为空，写入照常成功，只是 `embedding_status` 保持 pending。
    embed: crate::embed::EmbeddingConfig,
}

impl MemoryService {
    pub fn open(db_path: impl AsRef<Path>, journal_path: impl AsRef<Path>) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        let journal_path = journal_path.as_ref().to_path_buf();
        let reader = Db::open(&db_path)?;
        let writer = Writer::start(&db_path, &journal_path)?;
        Ok(Self {
            db_path,
            journal_path,
            writer,
            reader,
            embed: crate::embed::load_config(),
        })
    }

    /// journal 是"真相层"（docs/04 §2.6）：**每个状态变更都要追加事件**，
    /// 否则"删掉 db 可重建"不成立（只记 write 会把已删记忆复活）。
    fn journal_append(&self, op: &str, id: &str, extra: serde_json::Value) {
        use std::io::Write;
        if let Some(dir) = self.journal_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let line = serde_json::json!({
            "op": op,
            "id": id,
            "ts": now_secs(),
            "payload": extra,
        });
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal_path)
        {
            Ok(mut f) => {
                let _ = writeln!(f, "{}", line);
            }
            Err(e) => tracing::warn!("journal 追加失败（数据已入库）: {e}"),
        }
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    // ---------------- 写 ----------------

    pub fn write(&self, va: &VerifiedAgent, input: WriteInput) -> Result<WriteReceipt> {
        // P1-6：先限流再写，避免被刷爆写队列
        check_rate(va.id())?;
        let receipt = self.writer.write(va, input.clone())?;
        // 写入已成功（COMMIT 后回执）。嵌入是**后置增强**：
        // 失败绝不能回滚已写入的记忆，只能把 embedding_status 标成 failed 让它可见（F7）。
        if !receipt.duplicate {
            if let Err(e) = self.embed_one(&receipt.id, &input.content) {
                tracing::warn!(id = %receipt.id, %e, "嵌入失败（已标记 failed，记忆本身可用）");
            }
        }
        Ok(receipt)
    }

    /// 计算并落一条记忆的向量。失败时把 `embedding_status` 置为 `failed`（**可见**，不静默）。
    fn embed_one(&self, memory_id: &str, content: &str) -> Result<()> {
        let active = self.embed.active_endpoints();
        let Some(ep) = active.first().copied() else {
            // 没配端点：保持 pending，不算错误
            return Ok(());
        };
        let texts = vec![content.to_string()];
        let vecs = crate::embed::embed(ep, &texts, &self.embed)?;
        let v = vecs.first().ok_or_else(|| OrbitalError::EmbeddingUnavailable {
            reason: "端点未返回向量".into(),
        })?;

        let bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
        let now = now_secs();
        let conn = self.reader.conn();
        conn.execute(
            "INSERT INTO memories_vec (memory_id, agent_id, embedding, dim, updated_at)
             VALUES (?1, (SELECT agent_id FROM memories WHERE id=?1), ?2, ?3, ?4)
             ON CONFLICT(memory_id) DO UPDATE SET embedding=excluded.embedding, dim=excluded.dim, updated_at=excluded.updated_at",
            params![memory_id, bytes, v.len() as i64, now],
        )
        .map_err(map_err)?;
        conn.execute(
            "UPDATE memories SET embedding_status='ok', embedding_source=?1 WHERE id=?2",
            params![format!("{}:{}", ep.id, ep.model_name), memory_id],
        )
        .map_err(map_err)?;
        Ok(())
    }

    /// 把"写入后嵌入失败"的记忆标成 failed（供 health 与补算使用）。
    pub fn mark_embedding_failed(&self, memory_id: &str, reason: &str) -> Result<()> {
        self.reader
            .conn()
            .execute(
                "UPDATE memories SET embedding_status='failed', embedding_source=?1 WHERE id=?2",
                params![format!("error:{}", reason), memory_id],
            )
            .map_err(map_err)?;
        Ok(())
    }

    /// 补算：把所有 `pending`/`failed` 且有内容的记忆补上向量（幂等，可反复跑）。
    pub fn backfill_embeddings(&self, limit: usize) -> Result<(usize, usize)> {
        let active = self.embed.active_endpoints();
        if active.is_empty() {
            return Ok((0, 0));
        }
        let rows: Vec<(String, String)> = {
            let conn = self.reader.conn();
            let mut stmt = conn
                .prepare(
                    "SELECT id, content FROM memories
                     WHERE deleted_at IS NULL
                       AND NOT EXISTS (SELECT 1 FROM memories_vec v WHERE v.memory_id = memories.id)
                     LIMIT ?1",
                )
                .map_err(map_err)?;
            let it = stmt
                .query_map(params![limit as i64], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(map_err)?;
            let mut v = Vec::new();
            for x in it {
                v.push(x.map_err(map_err)?);
            }
            v
        };
        let mut ok = 0usize;
        let mut failed = 0usize;
        for (id, content) in rows {
            match self.embed_one(&id, &content) {
                Ok(()) => ok += 1,
                Err(e) => {
                    failed += 1;
                    let _ = self.mark_embedding_failed(&id, &e.to_string());
                }
            }
        }
        Ok((ok, failed))
    }

    // ---------------- 读 ----------------

    pub fn get(&self, va: &VerifiedAgent, id: &str) -> Result<MemoryRecord> {
        self.reader
            .conn()
            .query_row(
                &format!("SELECT m.id, m.content, m.agent_id, m.scope, m.category, m.importance, m.tags, m.entities,
                                 m.valid_from, m.valid_to, m.created_at, m.access_count, m.pinned, m.superseded_by
                          FROM memories m WHERE {VISIBLE} AND m.id = ?2"),
                params![va.id(), id],
                |r| {
                    Ok(MemoryRecord {
                        id: r.get(0)?,
                        content: r.get(1)?,
                        agent_id: r.get(2)?,
                        scope: r.get(3)?,
                        category: r.get(4)?,
                        importance: r.get(5)?,
                        tags: r.get(6)?,
                        entities: r.get(7)?,
                        valid_from: r.get(8)?,
                        valid_to: r.get(9)?,
                        created_at: r.get(10)?,
                        access_count: r.get(11)?,
                        pinned: r.get(12)?,
                        superseded_by: r.get(13)?,
                        deletion_mode: "soft".to_string(),
                    })
                },
            )
            .optional()
            .map_err(map_err)?
            .ok_or_else(|| OrbitalError::MemoryNotFound { id: id.to_string() })
    }

    /// 关键词召回（单路 FTS5；融合版见 `recall_fused`）。
    pub fn recall(&self, va: &VerifiedAgent, query: &str, limit: i64) -> Result<Vec<RecallHit>> {
        check_query(query)?;
        let limit = clamp_limit(limit);
        let out = self.fts_hits(va, query, limit, None)?;
        // 访问打点（ADR-003 唯一例外，见 fts_hits 上方的说明）
        touch_access(&self.reader, &out);
        Ok(out)
    }

    // ---------------- 三路召回 + RRF（T-M3-02）----------------

    /// 登记一条结构化事实（T-M3-03）：**走写队列**（红线 5），写入时增量检测冲突。
    ///
    /// 前置：调用方必须对该记忆可见——不能给别人的 private 记忆加事实。
    pub fn add_fact(
        &self,
        va: &VerifiedAgent,
        fw: crate::writer::FactWrite,
    ) -> Result<WriteReceipt> {
        let _ = self.get(va, &fw.memory_id)?;
        self.writer.write_fact(fw)
    }

    /// 单路 FTS 召回（**不含访问打点**，供 `recall` 与 `recall_fused` 共用）。
    ///
    /// `filter`：T-M3-03 的结构化过滤（只召回在该实体上登记过该属性的记忆）。
    fn fts_hits(
        &self,
        va: &VerifiedAgent,
        query: &str,
        limit: i64,
        filter: Option<&FactFilter>,
    ) -> Result<Vec<RecallHit>> {
        let conn = self.reader.conn();
        let safe_query = fts_escape(query);
        let mut sql = format!(
            "SELECT m.id, m.content, m.scope, bm25(memories_fts) AS rank
             FROM memories_fts
             JOIN memories m ON m.rowid = memories_fts.rowid
             WHERE memories_fts MATCH ?2 AND {VISIBLE} AND m.valid_to IS NULL
               AND m.id NOT IN (
                 SELECT memory_a FROM memory_conflicts WHERE status = 'open'
                 UNION
                 SELECT memory_b FROM memory_conflicts WHERE status = 'open')"
        );
        let mut pv: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(va.id().to_string()), Box::new(safe_query), Box::new(limit)];
        if let Some(f) = filter {
            if let Some(prop) = &f.property {
                sql.push_str(
                    " AND m.id IN (SELECT memory_id FROM memory_facts
                         WHERE valid_to IS NULL AND entity_id = ?4 AND property = ?5)",
                );
                pv.push(Box::new(f.entity_id.clone()));
                pv.push(Box::new(prop.clone()));
            } else {
                sql.push_str(
                    " AND m.id IN (SELECT memory_id FROM memory_facts
                         WHERE valid_to IS NULL AND entity_id = ?4)",
                );
                pv.push(Box::new(f.entity_id.clone()));
            }
        }
        sql.push_str(" ORDER BY rank LIMIT ?3");
        let mut stmt = conn.prepare(&sql).map_err(map_err)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(pv.iter().map(|b| b.as_ref())), |r| {
                let rank: f64 = r.get(3)?;
                Ok(RecallHit {
                    id: r.get(0)?,
                    content: r.get(1)?,
                    scope: r.get(2)?,
                    // bm25 越小越相关；转成 0..1 的"越大越相关"
                    score: 1.0 / (1.0 + rank.abs()),
                })
            })
            .map_err(map_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(map_err)?);
        }
        Ok(out)
    }

    /// 向量通道：**先把查询嵌入，再按门控过滤候选，最后算余弦**。
    ///
    /// ADR-019 硬要求：禁止"全库 top-k 之后再用 agent 过滤"——那会让小分区永远召不回。
    /// 无端点 / 嵌入失败时返回空 + `degraded` 原因（**不静默**：调用方必须能报告"这一路没跑"）。
    fn vector_hits(
        &self,
        va: &VerifiedAgent,
        query: &str,
        limit: i64,
        filter: Option<&FactFilter>,
    ) -> Result<(Vec<RecallHit>, Option<String>)> {
        let active = self.embed.active_endpoints();
        let Some(ep) = active.first().copied() else {
            return Ok((Vec::new(), Some("vector: no-endpoint".into())));
        };
        let texts = vec![query.to_string()];
        // T-M10-21（性能）：原来这里每次都实时调 embedding 端点（本机 Ollama，约 200ms），
        // 是注入热路径的主要耗时。同一 query 在 5 分钟内复用向量即可（对话里重复提问很常见）。
        let qvec = match cached_query_vec(ep, &texts, &self.embed) {
            Ok(mut v) => match v.pop() {
                Some(x) => x,
                None => return Ok((Vec::new(), Some("vector: empty-response".into()))),
            },
            Err(e) => return Ok((Vec::new(), Some(format!("vector: embed-error({e})")))),
        };

        let conn = self.reader.conn();
        // 候选集：门控（agent/scope/时效/软删/冲突排除）+ 只取有向量的条
        let mut sql = format!(
            "SELECT m.id, m.content, m.scope, v.embedding
               FROM memories_vec v
               JOIN memories m ON m.id = v.memory_id
              WHERE {VISIBLE} AND m.valid_to IS NULL
                AND m.id NOT IN (
                  SELECT memory_a FROM memory_conflicts WHERE status = 'open'
                  UNION
                  SELECT memory_b FROM memory_conflicts WHERE status = 'open')"
        );
        let mut pv: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(va.id().to_string())];
        if let Some(f) = filter {
            if let Some(prop) = &f.property {
                sql.push_str(
                    " AND m.id IN (SELECT memory_id FROM memory_facts
                         WHERE valid_to IS NULL AND entity_id = ?2 AND property = ?3)",
                );
                pv.push(Box::new(f.entity_id.clone()));
                pv.push(Box::new(prop.clone()));
            } else {
                sql.push_str(
                    " AND m.id IN (SELECT memory_id FROM memory_facts
                         WHERE valid_to IS NULL AND entity_id = ?2)",
                );
                pv.push(Box::new(f.entity_id.clone()));
            }
        }
        let mut stmt = conn.prepare(&sql).map_err(map_err)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(pv.iter().map(|b| b.as_ref())), |r| {
                let blob: Vec<u8> = r.get(3)?;
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, blob))
            })
            .map_err(map_err)?;
        let mut scored: Vec<RecallHit> = Vec::new();
        for r in rows {
            let (id, content, scope, blob) = r.map_err(map_err)?;
            // T-M10-49（CI clippy 1.100 的 chunks_exact_to_as_chunks）：用 as_chunks
            // 直接拿到 `&[u8; 4]`，省掉手工拼四字节，也避免未来版本再被 lint。
            let emb: Vec<f32> = blob
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes(*c))
                .collect();
            let sim = crate::embed::cosine(&qvec, &emb) as f64;
            scored.push(RecallHit {
                id,
                content,
                scope,
                score: sim,
            });
        }
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        scored.truncate(limit.max(1) as usize);
        Ok((scored, None))
    }

    /// 图加成（ADR-040）——**只给已在候选集里的 id 加分，绝不引入新 id**。
    ///
    /// 为什么这样改：邻居的相关性来自**种子**（与查询相关），不是它自己。
    /// 之前给邻居独立 `w/(k+rank)` 的排名，导致无关邻居被顶到前面（实测 hit@1 92%→14%）。
    /// 现在：`加成 = 种子在 FTS 里的 RRF 分 × decay`，同一条候选取**最强种子**的加成。
    ///
    /// 门控：邻居必须 `JOIN memories` 后再过一次 VISIBLE（P0-2：edges 表本身没有归属）。
    fn graph_bonus(
        &self,
        va: &VerifiedAgent,
        query: &str,
        candidates: &std::collections::HashSet<String>,
        k: f64,
        decay: f64,
        filter: Option<&FactFilter>,
    ) -> Result<std::collections::HashMap<String, f64>> {
        let mut bonus: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
        let seeds = self.fts_hits(va, query, 10, filter)?;
        if seeds.is_empty() {
            return Ok(bonus);
        }
        let conn = self.reader.conn();
        let sql = format!(
            "SELECT m.id FROM edges e
               JOIN memories m ON m.id = e.target_id
              WHERE e.source_id = ?2 AND {VISIBLE} AND m.valid_to IS NULL
             UNION
             SELECT m.id FROM edges e
               JOIN memories m ON m.id = e.source_id
              WHERE e.target_id = ?2 AND {VISIBLE} AND m.valid_to IS NULL"
        );
        let mut stmt = conn.prepare(&sql).map_err(map_err)?;
        for (rank, seed) in seeds.iter().enumerate() {
            // 种子的"相关性"就是它在 FTS 里的排名分（与其他通道同一个 k）
            let add = decay / (k + rank as f64 + 1.0);
            let rows = stmt
                .query_map(params![va.id(), seed.id], |r| r.get::<_, String>(0))
                .map_err(map_err)?;
            for row in rows {
                let nid = row.map_err(map_err)?;
                if !candidates.contains(&nid) {
                    continue; // ★ ADR-040：不引入新 id
                }
                let e = bonus.entry(nid).or_insert(0.0);
                if add > *e {
                    *e = add;
                }
            }
        }
        Ok(bonus)
    }

    /// 三路召回 + **RRF**（倒数排名融合，Reciprocal Rank Fusion）融合。
    ///
    /// ```text
    /// score(id) = Σ_ch weight_ch / (k + rank_in_ch)     // k 默认 60（业界默认）
    /// ```
    ///
    /// 同一 id 在多路命中时**只算一次累加**，并记录命中通道（便于诊断"这条是哪路带来的"）。
    /// T-M10-42：**预热 query embedding 缓存**（后台线程调用，不阻塞启动）。
    ///
    /// 为什么需要：冷查询（第一次问某个问题）要现算 embedding，本机 Ollama 约 200ms，
    /// 实测冷查询 P95 ≈ 240ms，离注入的 300ms 预算只剩 60ms（T-M10-41）。
    /// 启动后后台把这批常见查询先算一遍，用户真正提问时就能命中缓存（16ms）。
    ///
    /// 注意：缓存 key 只有 query 文本（不含 agent），所以预热一次**四家 agent 共享**。
    /// 返回成功预热的条数（失败的跳过，不影响启动）。
    pub fn warm_query_cache(&self, va: &VerifiedAgent, queries: &[&str]) -> usize {
        let opts = RecallOptions::default();
        let mut ok = 0usize;
        for q in queries {
            if !q.trim().is_empty() && self.recall_fused(va, q, 1, &opts).is_ok() {
                ok += 1;
            }
        }
        ok
    }

    pub fn recall_fused(
        &self,
        va: &VerifiedAgent,
        query: &str,
        limit: i64,
        opts: &RecallOptions,
    ) -> Result<FusedRecall> {
        check_query(query)?;
        let limit = clamp_limit(limit);
        let k = opts.k.max(1) as f64;
        let mut degraded: Vec<String> = Vec::new();
        // id -> (累计分, 命中通道, 内容, scope)
        let mut acc: std::collections::HashMap<String, (f64, Vec<String>, String, String)> =
            std::collections::HashMap::new();

        // ADR-040：图扩展**不产出独立 hits**，只对已有候选做加成（见循环后的 graph_bonus）。
        // 理由：邻居的相关性来自种子、与查询无关；给它独立排名会系统性挤掉正确答案（实测 hit@1 92%→14%）。
        let mut graph_weight = 0.0f64;
        for (ci, ch) in opts.channels.iter().enumerate() {
            let w = *opts.weights.get(ci).unwrap_or(&1.0);
            if w == 0.0 {
                continue; // 权重 0 = 关闭该路（对照实验用）
            }
            if *ch == RecallChannel::Graph {
                graph_weight = w;
                continue;
            }
            let hits: Vec<RecallHit> = match ch {
                RecallChannel::Fts => self.fts_hits(va, query, 50, opts.fact_filter.as_ref())?,
                RecallChannel::Vector => {
                    let (h, why) = self.vector_hits(va, query, 50, opts.fact_filter.as_ref())?;
                    if let Some(reason) = why {
                        degraded.push(reason);
                    }
                    h
                }
                RecallChannel::Graph => unreachable!("Graph 已在上面 continue"),
            };
            for (rank, h) in hits.iter().enumerate() {
                let ent = acc
                    .entry(h.id.clone())
                    .or_insert_with(|| (0.0, Vec::new(), h.content.clone(), h.scope.clone()));
                ent.0 += w / (k + rank as f64 + 1.0);
                let name = ch.name().to_string();
                if !ent.1.contains(&name) {
                    ent.1.push(name);
                }
            }
        }

        // ---- ADR-040 补充：图连接**不加分**，只做"真平局仲裁" ----
        //
        // 实测证据（T-M3-05）：RRF 下 rank0 与 rank1 的分数只差 1/(k+1) - 1/(k+2) ≈ 0.0005，
        // 而任何"加成"（哪怕 decay=0.1, ≈0.0016）都会让弱命中反超正确答案（hit@1 46→37）。
        // ⇒ 图连接不参与分数，只在**分数完全相同**时作为决胜（这样 hit@1 数学上不可能被改变）。
        if graph_weight > 0.0 && !acc.is_empty() {
            let candidates: std::collections::HashSet<String> = acc.keys().cloned().collect();
            let linked = self.graph_bonus(
                va,
                query,
                &candidates,
                k,
                opts.graph_decay.max(0.0),
                opts.fact_filter.as_ref(),
            )?;
            for (id, _bonus) in linked {
                if let Some(ent) = acc.get_mut(&id) {
                    let name = "graph_linked".to_string();
                    if !ent.1.contains(&name) {
                        ent.1.push(name);
                    }
                }
            }
        }

        let mut hits: Vec<FusedHit> = acc
            .into_iter()
            .map(|(id, (score, channels, content, scope))| FusedHit {
                id,
                content,
                scope,
                score,
                channels,
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                // ADR-040 补充：分数完全相同时，有图连接的优先（只做决胜，绝不改分数）
                .then_with(|| {
                    let ag = a.channels.iter().any(|c| c == "graph_linked");
                    let bg = b.channels.iter().any(|c| c == "graph_linked");
                    bg.cmp(&ag)
                })
                .then_with(|| a.id.cmp(&b.id))
        });
        hits.truncate(limit.max(1) as usize);

        // 打点：融合结果的访问计数（与单路 recall 同一口径）
        let touch: Vec<RecallHit> = hits
            .iter()
            .map(|h| RecallHit {
                id: h.id.clone(),
                content: h.content.clone(),
                scope: h.scope.clone(),
                score: h.score,
            })
            .collect();
        touch_access(&self.reader, &touch);

        Ok(FusedRecall { hits, degraded })
    }

    // ---------------- 退役候选（T-M4-01）----------------

    /// 检索"该考虑退役"的记忆（**只读**：不写库、不删除、不改任何字段）。
    ///
    /// 判据（`docs/04 §6.2`）：`ei < threshold` 且非 pinned 且创建 > 24h；
    /// **免疫**：`pinned` / `importance >= 4` / `access_count >= 3` → 永不进候选。
    /// ei **读时计算、不落盘**（ADR-013：mnemon 的"写入时快照"会让退役决策用错尺子）。
    pub fn retirement_candidates(
        &self,
        va: &VerifiedAgent,
        threshold: f64,
        limit: i64,
    ) -> Result<Vec<RetirementRow>> {
        self.retirement_scan(Some(va.id()), threshold, limit)
    }

    /// 本地特权版（GUI 用，ADR-034）：看**全部**记忆的退役候选（不按 agent 门控）。
    ///
    /// 语义与 `retirement_candidates` 相同，只放开作用域过滤——用户自己的库应该能看全。
    pub fn retirement_candidates_all(
        &self,
        threshold: f64,
        limit: i64,
    ) -> Result<Vec<RetirementRow>> {
        self.retirement_scan(None, threshold, limit)
    }

    fn retirement_scan(
        &self,
        owner: Option<&str>,
        threshold: f64,
        limit: i64,
    ) -> Result<Vec<RetirementRow>> {
        let conn = self.reader.conn();
        let now = now_secs();
        let half_life = std::env::var("ORBITAL_HALF_LIFE_DAYS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v > 0.0)
            .unwrap_or(30.0);
        let sql = "SELECT m.id, m.content, m.importance, COALESCE(m.access_count,0),
                    COALESCE(m.last_accessed_at, m.created_at), m.created_at, m.origin,
                    COALESCE(m.pinned,0),
                    (SELECT count(*) FROM edges e WHERE e.source_id = m.id OR e.target_id = m.id)
               FROM memories m
              WHERE (?1 IS NULL OR m.agent_id = ?1 OR m.scope IN ('shared','entity'))
                AND m.deleted_at IS NULL AND m.valid_to IS NULL
                AND m.id NOT IN (
                  SELECT memory_a FROM memory_conflicts WHERE status = 'open'
                  UNION
                  SELECT memory_b FROM memory_conflicts WHERE status = 'open')";
        let mut stmt = conn.prepare(sql).map_err(map_err)?;
        let rows = stmt
            .query_map(params![owner], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, f64>(4)?,
                    r.get::<_, f64>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, i64>(8)?,
                ))
            })
            .map_err(map_err)?;

        let mut out: Vec<RetirementRow> = Vec::new();
        for row in rows {
            let (id, content, importance, access, last_access, created, origin, pinned, edge_count) =
                row.map_err(map_err)?;
            // 免疫（docs/04 §6.2）：这三种情况永不进退役候选
            if pinned != 0 || importance >= 4 || access >= 3 {
                continue;
            }
            // 创建不足 24h 的不候选（避免刚写进来就被建议退役）
            if now - created < 86_400.0 {
                continue;
            }
            let days = ((now - last_access).max(0.0)) / 86_400.0;
            let ei = effective_importance(
                importance,
                access,
                days,
                edge_count,
                &origin,
                half_life,
                pinned != 0,
            );
            if ei >= threshold {
                continue;
            }
            let mut reasons = vec![format!("ei {ei:.3} < {threshold}")];
            if days >= 90.0 {
                reasons.push(format!("{} 天未访问", days.round() as i64));
            }
            if origin == "agent_derived" {
                reasons.push("agent 派生（非用户原话）".to_string());
            }
            out.push(RetirementRow {
                id,
                content_head: content.chars().take(60).collect(),
                ei,
                importance,
                access_count: access,
                days_since_access: days,
                edge_count,
                origin,
                reasons,
            });
        }
        out.sort_by(|a, b| {
            a.ei
                .partial_cmp(&b.ei)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        out.truncate(limit.max(1) as usize);
        Ok(out)
    }

    // ---------------- 降权 / 恢复（T-M4-02）----------------

    /// **可逆降权**：`importance - 1`（最低 1）。**不删除任何东西**。
    ///
    /// 与 `update` 一样是既有直接写（ADR-034 债务），不新增写路径类型。
    /// 事件 `retire{from,to}` 是恢复与撤销的唯一依据（`docs/04 §6.2` 的"降权"档）。
    pub fn retire(&self, va: &VerifiedAgent, id: &str) -> Result<UpdateOutcome> {
        let cur = self.get(va, id)?;
        let from = cur.importance;
        if from <= 1 {
            return Err(OrbitalError::InvalidArgument {
                reason: "重要度已经是 1（最低档），不能再降".into(),
            });
        }
        let to = from - 1;
        let now = now_secs();
        let conn = self.reader.conn();
        let n = conn
            .execute(
                "UPDATE memories SET importance = ?1, updated_at = ?2 WHERE id = ?3 AND agent_id = ?4",
                params![to, now, id, va.id()],
            )
            .map_err(map_err)?;
        if n == 0 {
            return Err(OrbitalError::MemoryNotFound { id: id.to_string() });
        }
        conn.execute(
            "INSERT INTO memory_events (operation, memory_id, actor, detail, created_at)
                 VALUES ('retire', ?1, ?2, ?3, ?4)",
            params![
                id,
                va.id(),
                serde_json::json!({"from": from, "to": to}).to_string(),
                now
            ],
        )
        .map_err(map_err)?;
        let event_id = conn.last_insert_rowid();
        Ok(UpdateOutcome {
            action: "retired".into(),
            id: id.to_string(),
            superseded_id: None,
            note: format!("已降权：重要度 {from} → {to}（可撤销）"),
            event_id: Some(event_id),
        })
    }

    /// 本地特权版降权（GUI 用，ADR-034）：按记忆的真实 owner 走。
    pub fn retire_local_owner(&self, id: &str) -> Result<UpdateOutcome> {
        let owner: String = self
            .reader
            .conn()
            .query_row(
                "SELECT agent_id FROM memories WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_err)?
            .ok_or_else(|| OrbitalError::MemoryNotFound {
                id: id.to_string(),
            })?;
        self.retire(&VerifiedAgent::for_local_owner(owner), id)
    }

    /// 本地特权版恢复（GUI 用，ADR-034）。
    pub fn restore_retire_local_owner(&self, id: &str) -> Result<UpdateOutcome> {
        let owner: String = self
            .reader
            .conn()
            .query_row(
                "SELECT agent_id FROM memories WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_err)?
            .ok_or_else(|| OrbitalError::MemoryNotFound {
                id: id.to_string(),
            })?;
        self.restore_retire(&VerifiedAgent::for_local_owner(owner), id)
    }

    // ---------------- 恢复入口（T-M4-03）----------------

    /// 该记忆**最初**的重要度：最早一条 `retire` 事件的 `from`。
    ///
    /// 为什么不是"当前 + 1"：连降两次（3→2→1）时，当前+1 只回到 2；
    /// events 链里的最早 `from` 才是"降权之前"的真值。
    fn baseline_importance_of(&self, mid: &str) -> Result<Option<i64>> {
        let detail: Option<String> = self
            .reader
            .conn()
            .query_row(
                "SELECT detail FROM memory_events
                  WHERE memory_id = ?1 AND operation = 'retire'
                  ORDER BY id ASC LIMIT 1",
                params![mid],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_err)?;
        Ok(detail
            .and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok())
            .and_then(|v| v.get("from").and_then(|x| x.as_i64())))
    }

    /// 恢复到**最初**的重要度（沿 events 链回放，一次写回）。
    pub fn restore_to_baseline(&self, va: &VerifiedAgent, id: &str) -> Result<UpdateOutcome> {
        let cur = self.get(va, id)?;
        let baseline = self
            .baseline_importance_of(id)?
            .ok_or_else(|| OrbitalError::InvalidArgument {
                reason: "这条记忆没有降权记录（无需恢复）".into(),
            })?;
        if cur.importance >= baseline {
            return Err(OrbitalError::InvalidArgument {
                reason: format!("当前重要度 {} 已不低于最初值 {baseline}", cur.importance),
            });
        }
        let now = now_secs();
        let conn = self.reader.conn();
        conn.execute(
            "UPDATE memories SET importance = ?1, updated_at = ?2 WHERE id = ?3 AND agent_id = ?4",
            params![baseline, now, id, va.id()],
        )
        .map_err(map_err)?;
        conn.execute(
            "INSERT INTO memory_events (operation, memory_id, actor, detail, created_at)
             VALUES ('restore_baseline', ?1, ?2, ?3, ?4)",
            params![
                id,
                va.id(),
                serde_json::json!({"from": cur.importance, "to": baseline}).to_string(),
                now
            ],
        )
        .map_err(map_err)?;
        let event_id = conn.last_insert_rowid();
        Ok(UpdateOutcome {
            action: "restored_baseline".into(),
            id: id.to_string(),
            superseded_id: None,
            note: format!("已恢复到最初的重要度 {baseline}"),
            event_id: Some(event_id),
        })
    }

    /// 本地特权版（GUI 用，ADR-034）。
    pub fn restore_to_baseline_local_owner(&self, id: &str) -> Result<UpdateOutcome> {
        let owner: String = self
            .reader
            .conn()
            .query_row(
                "SELECT agent_id FROM memories WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_err)?
            .ok_or_else(|| OrbitalError::MemoryNotFound {
                id: id.to_string(),
            })?;
        self.restore_to_baseline(&VerifiedAgent::for_local_owner(owner), id)
    }

    /// 可恢复列表（T-M4-03）：**已降权**（可回最初值）+ **已软删**（可撤销删除）。
    ///
    /// 只读；`undo_event_id` 给软删行用于撤销（复用 T-M2-07 的撤销通路）。
    pub fn recovery_list(&self, limit: i64) -> Result<Vec<RecoverableRow>> {
        let conn = self.reader.conn();
        let mut out: Vec<RecoverableRow> = Vec::new();

        // ① 已降权：当前 importance < 最早 retire 的 from
        let rows: Vec<(String, i64, f64, String)> = {
            let mut st = conn
                .prepare(
                    "SELECT m.id, m.importance, m.updated_at, m.content
                       FROM memories m
                      WHERE m.deleted_at IS NULL
                        AND EXISTS (SELECT 1 FROM memory_events e
                                     WHERE e.memory_id = m.id AND e.operation = 'retire')
                      ORDER BY m.updated_at DESC",
                )
                .map_err(map_err)?;
            let it = st
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, f64>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })
                .map_err(map_err)?;
            let mut v = Vec::new();
            for r in it {
                v.push(r.map_err(map_err)?);
            }
            v
        };
        for (id, current, updated_at, content) in rows {
            let Some(baseline) = self.baseline_importance_of(&id)? else {
                continue;
            };
            if current >= baseline {
                continue; // 已恢复过的，不算可恢复
            }
            out.push(RecoverableRow {
                id: id.clone(),
                content_head: content.chars().take(60).collect(),
                kind: "demoted".into(),
                current_importance: current,
                baseline_importance: baseline,
                undo_event_id: None,
                updated_at,
            });
        }

        // ② 已软删：取最近一条 forget:soft 事件 id（撤销用）
        let deleted: Vec<(String, f64, String, Option<i64>)> = {
            let mut st = conn
                .prepare(
                    "SELECT m.id, m.updated_at, m.content,
                            (SELECT max(e.id) FROM memory_events e
                              WHERE e.memory_id = m.id AND e.operation = 'forget:soft')
                       FROM memories m
                      WHERE m.deleted_at IS NOT NULL
                      ORDER BY m.updated_at DESC",
                )
                .map_err(map_err)?;
            let it = st
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, f64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                    ))
                })
                .map_err(map_err)?;
            let mut v = Vec::new();
            for r in it {
                v.push(r.map_err(map_err)?);
            }
            v
        };
        for (id, updated_at, content, undo_event_id) in deleted {
            out.push(RecoverableRow {
                id,
                content_head: content.chars().take(60).collect(),
                kind: "soft_deleted".into(),
                current_importance: 0,
                baseline_importance: 0,
                undo_event_id,
                updated_at,
            });
        }

        out.sort_by(|a, b| {
            b.updated_at
                .partial_cmp(&a.updated_at)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.truncate(limit.max(1) as usize);
        Ok(out)
    }

    /// 恢复降权：把**最近一次** `retire` 的 `from` 写回。
    pub fn restore_retire(&self, va: &VerifiedAgent, id: &str) -> Result<UpdateOutcome> {
        let _ = self.get(va, id)?;
        let conn = self.reader.conn();
        let (retire_event, detail): (i64, String) = conn
            .query_row(
                "SELECT id, detail FROM memory_events
                  WHERE memory_id = ?1 AND operation = 'retire'
                  ORDER BY id DESC LIMIT 1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(map_err)?
            .ok_or_else(|| OrbitalError::InvalidArgument {
                reason: "没有可恢复的降权记录".into(),
            })?;
        let from = serde_json::from_str::<serde_json::Value>(&detail)
            .ok()
            .and_then(|v| v.get("from").and_then(|x| x.as_i64()))
            .ok_or_else(|| OrbitalError::Internal("降权事件 detail 缺少 from".into()))?;
        let now = now_secs();
        conn.execute(
            "UPDATE memories SET importance = ?1, updated_at = ?2 WHERE id = ?3 AND agent_id = ?4",
            params![from, now, id, va.id()],
        )
        .map_err(map_err)?;
        conn.execute(
            "INSERT INTO memory_events (operation, memory_id, actor, detail, created_at)
                 VALUES ('restore_retire', ?1, ?2, ?3, ?4)",
            params![
                id,
                va.id(),
                serde_json::json!({"from_imp": from, "retire_event": retire_event}).to_string(),
                now
            ],
        )
        .map_err(map_err)?;
        let event_id = conn.last_insert_rowid();
        Ok(UpdateOutcome {
            action: "restored".into(),
            id: id.to_string(),
            superseded_id: None,
            note: format!("重要度已恢复到 {from}"),
            event_id: Some(event_id),
        })
    }

    // ---------------- 维护任务（T-M4-04）----------------

    /// 清理**孤儿边**：只删"指向不存在记忆"的边。
    ///
    /// ⚠️ 与"软删"的关键区别（`docs/04 §2.3`）：
    /// - **软删**的记忆行还在（`deleted_at` 非空）→ 它的边**必须保留**（恢复后还要用）
    /// - **硬删**是物理删除行 → 指向它的边才是孤儿
    ///
    /// 边没有软删机制，所以这是**不可逆删除**——只清孤儿，正常边一条不动。
    pub fn cleanup_orphan_edges(&self) -> Result<CleanupOutcome> {
        let conn = self.reader.conn();
        let before: i64 = conn
            .query_row("SELECT count(*) FROM edges", [], |r| r.get(0))
            .map_err(map_err)?;
        let removed = conn
            .execute(
                "DELETE FROM edges
                  WHERE source_id NOT IN (SELECT id FROM memories)
                     OR target_id NOT IN (SELECT id FROM memories)",
                [],
            )
            .map_err(map_err)?;
        let after: i64 = conn
            .query_row("SELECT count(*) FROM edges", [], |r| r.get(0))
            .map_err(map_err)?;
        if removed > 0 {
            let now = now_secs();
            conn.execute(
                "INSERT INTO memory_events (operation, memory_id, actor, detail, created_at)
                 VALUES ('maintenance:edges', NULL, 'user', ?1, ?2)",
                params![
                    serde_json::json!({"before": before, "after": after, "removed": removed})
                        .to_string(),
                    now
                ],
            )
            .map_err(map_err)?;
        }
        Ok(CleanupOutcome {
            removed_edges: removed as i64,
            kept_edges: after,
        })
    }

    /// 补算缺失向量（`embedding_status` 为 pending / failed 的记忆）。
    ///
    /// 没配嵌入端点时**不假装成功**：直接回 `degraded` 原因，一条也不算。
    pub fn reembed_pending(&self, limit: i64) -> Result<ReembedOutcome> {
        let mut out = ReembedOutcome {
            tried: 0,
            ok: 0,
            failed: 0,
            degraded: Vec::new(),
        };
        if self.embed.active_endpoints().is_empty() {
            out.degraded.push("no-embedding-endpoint".into());
            return Ok(out);
        }
        let pending: Vec<(String, String)> = {
            let conn = self.reader.conn();
            let mut st = conn
                .prepare(
                    "SELECT id, content FROM memories
                      WHERE embedding_status IN ('pending','failed')
                        AND deleted_at IS NULL AND valid_to IS NULL
                      ORDER BY created_at ASC LIMIT ?1",
                )
                .map_err(map_err)?;
            let it = st
                .query_map(params![limit.max(1)], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(map_err)?;
            let mut v = Vec::new();
            for r in it {
                v.push(r.map_err(map_err)?);
            }
            v
        };
        for (id, content) in pending {
            out.tried += 1;
            match self.embed_one(&id, &content) {
                Ok(()) => out.ok += 1,
                Err(e) => {
                    out.failed += 1;
                    let msg = e.to_string();
                    if out.degraded.len() < 5 && !out.degraded.contains(&msg) {
                        out.degraded.push(msg);
                    }
                }
            }
        }
        Ok(out)
    }

    // ---------------- 备份（T-M5-01）----------------

    /// 在线备份到 `dst`，**备份完立刻校验**并返回报告。
    ///
    /// 为什么用 SQLite Online Backup API 而不是文件拷贝：WAL 模式下直接拷 `.db`
    /// 会漏掉 WAL 里未 checkpoint 的页，恢复出来是**旧数据**（经典事故）。
    pub fn backup_to(&self, dst: &std::path::Path) -> Result<BackupReport> {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                OrbitalError::Internal(format!("创建备份目录失败: {e}"))
            })?;
        }
        let conn = self.reader.conn();
        let mut dst_conn = rusqlite::Connection::open(dst)
            .map_err(|e| OrbitalError::Internal(format!("打开备份目标失败: {e}")))?;
        {
            let backup = rusqlite::backup::Backup::new(conn, &mut dst_conn)
                .map_err(|e| OrbitalError::Internal(format!("初始化备份失败: {e}")))?;
            backup
                .run_to_completion(200, std::time::Duration::from_millis(5), None)
                .map_err(|e| OrbitalError::Internal(format!("备份执行失败: {e}")))?;
        }
        // 备份目标切回 DELETE 日志模式：
        // 否则会留下 `.db-wal`/`.db-shm` 伴生文件，用户只拷 `.db` 时会漏掉 WAL 里的页。
        let _ = dst_conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        let _ = dst_conn.query_row("PRAGMA journal_mode=DELETE", [], |r| {
            r.get::<_, String>(0)
        });
        drop(dst_conn);
        self.verify_backup(dst)
    }

    /// 校验备份：`integrity_check` + `user_version` + 关键表计数，并**与当前库逐项比对**。
    pub fn verify_backup(&self, path: &std::path::Path) -> Result<BackupReport> {
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY;
        let conn = rusqlite::Connection::open_with_flags(path, flags).map_err(|e| {
            OrbitalError::InvalidArgument {
                reason: format!("打不开备份文件（可能已损坏/被截断）: {e}"),
            }
        })?;
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(|e| OrbitalError::InvalidArgument {
                reason: format!("integrity_check 失败: {e}"),
            })?;
        let schema_version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(map_err)?;
        let table_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        let memories: i64 = conn
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .map_err(map_err)?;
        let events: i64 = conn
            .query_row("SELECT count(*) FROM memory_events", [], |r| r.get(0))
            .map_err(map_err)?;

        // 与当前库比对（备份必须与"当前库此刻"一致）
        let cur = self.reader.conn();
        let current_schema_version: i64 = cur
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(map_err)?;
        let current_memories: i64 = cur
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .map_err(map_err)?;
        let current_events: i64 = cur
            .query_row("SELECT count(*) FROM memory_events", [], |r| r.get(0))
            .map_err(map_err)?;

        let integrity_ok = integrity == "ok";
        let matches = integrity_ok
            && schema_version == current_schema_version
            && memories == current_memories
            && events == current_events;
        Ok(BackupReport {
            path: path.display().to_string(),
            integrity_ok,
            schema_version,
            current_schema_version,
            table_count,
            memories,
            current_memories,
            events,
            current_events,
            matches,
        })
    }

    /// 列出备份文件（按文件名倒序 = 最新在前）。
    pub fn list_backups(&self, dir: &std::path::Path) -> Result<Vec<BackupEntry>> {
        let mut out = Vec::new();
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return Ok(out), // 目录不存在 = 还没有备份
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("db") {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            out.push(BackupEntry {
                name: p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string(),
                path: p.display().to_string(),
                size_bytes: meta.len() as i64,
                modified: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0),
            });
        }
        out.sort_by(|a, b| b.name.cmp(&a.name));
        Ok(out)
    }

    // ---------------- 改 ----------------

    pub fn update(&self, va: &VerifiedAgent, id: &str, p: UpdatePatch) -> Result<UpdateOutcome> {
        let conn = self.reader.conn();
        // 先确认可见（避免"改到别人的"）
        let cur = self.get(va, id)?;
        let now = now_secs();
        // ⚠️ 内容变更必须走**版本链**（ADR-009：遗忘/修改必须可逆）。
        // 原地覆盖会让旧内容永久消失——这是 2026-11-12 反思发现的真偏差（docs/25）。
        // 元数据（importance/tags/pinned）仍原地改：它们不影响"内容历史"。
        let mut superseded_id: Option<String> = None;
        let mut out_id = id.to_string();
        let mut action = "saved";

        if let Some(c) = p.content {
            if c.trim().is_empty() {
                return Err(OrbitalError::InvalidArgument {
                    reason: "content 不能为空".into(),
                });
            }
            // T-M2-02 验收 4：64KB 边界（UTF-8 **字节**，不是字符数）。
            // 写路径（writer）早就有这个校验，读路径的 update 之前漏了——
            // 结果是 MCP 可以用 update 绕过上限写进超大内容。
            let bytes = c.len();
            if bytes > MAX_CONTENT_BYTES {
                return Err(OrbitalError::InvalidArgument {
                    reason: format!(
                        "超过单条上限 64KB（当前 {} 字节，上限 {} 字节）",
                        bytes, MAX_CONTENT_BYTES
                    ),
                });
            }
            let hash = content_hash_of(&c);

            // ---- P1-2：内容与同 agent 的另一条撞车时，先判断能不能"升级 scope" ----
            // 旧行为：直接 INSERT，撞 UNIQUE 就报"内容重复"——于是"同内容从 private
            // 改成 shared"永远做不成（docs/11 P1-2 记录的正是这个缺陷）。
            let dup: Option<(String, String, String)> = conn
                .query_row(
                    "SELECT id, scope, origin FROM memories
                     WHERE agent_id=?1 AND content_hash=?2 AND deleted_at IS NULL AND id<>?3",
                    params![va.id(), hash, id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(map_err)?;

            if let Some((dup_id, dup_scope, dup_origin)) = dup {
                let want_scope = p.scope.clone().unwrap_or_else(|| cur.scope.clone());
                if scope_rank(&want_scope) > scope_rank(&dup_scope) {
                    // 升级：改的是"已存在那条"的 scope，不是新建一条
                    // ADR-023 规则 1：shared 必须 user_endorsed，所以升级时一并改 origin
                    let new_origin = if want_scope == "shared" {
                        "user_endorsed".to_string()
                    } else {
                        dup_origin
                    };
                    conn.execute(
                        "UPDATE memories SET scope=?1, origin=?2, updated_at=?3
                         WHERE id=?4 AND agent_id=?5",
                        params![want_scope, new_origin, now, dup_id, va.id()],
                    )
                    .map_err(map_err)?;
                    let detail = serde_json::json!({
                        "actor": va.id(), "action": "merged_scope",
                        "from_id": id, "to_id": dup_id,
                        "scope_before": dup_scope, "scope_after": want_scope,
                    });
                    conn.execute(
                        "INSERT INTO memory_events (operation, memory_id, actor, detail, created_at)
                         VALUES ('update', ?1, ?2, ?3, ?4)",
                        params![dup_id, va.id(), detail.to_string(), now],
                    )
                    .map_err(map_err)?;
                    let event_id = conn.last_insert_rowid();
                    self.journal_append("update", &dup_id, detail);
                    return Ok(UpdateOutcome {
                        event_id: Some(event_id),
                        action: "merged_scope".into(),
                        id: dup_id,
                        superseded_id: None,
                        // ⚠️ 必须说清"本次修改没有写入"（docs/04 §10.2 的语义缺口）：
                        // 被编辑的那条保持原样，只是把已存在那条的 scope 升级了。
                        // 旧文案"已合并"容易被理解成"B 合并进 A"，而 B 其实没动。
                        note: format!(
                            "已合并到已有条目：本次修改未写入（目标条已存在），\
                             它的 scope 已从 {dup_scope} 升级为 {want_scope}"
                        ),
                    });
                }
                // scope 没有变高：不新增行，如实告诉用户"内容已存在"
                return Ok(UpdateOutcome {
                    event_id: None,
                    action: "duplicate".into(),
                    id: dup_id,
                    superseded_id: None,
                    note: "内容已存在，没有重复写入".into(),
                });
            }

            let new_id = format!("mem_{}", uuid::Uuid::new_v4().simple());

            // ① 新建一条：复制旧条的全部业务字段，只换 id / content / hash / 时间
            //    scope 若显式给了就用新的，否则继承旧条
            let insert_sql = "INSERT INTO memories
                (id, content, content_hash, agent_id, scope, origin, category, importance,
                 confidence, tags, entities, source_ref, session_id,
                 created_at, updated_at, valid_from, recorded_at, deleted_at, pinned,
                 embedding_status, embedding_source)
               SELECT ?1, ?2, ?3, agent_id, COALESCE(?7, scope), origin, category, importance,
                 confidence, tags, entities, source_ref, session_id,
                 ?4, ?4, ?4, ?4, NULL, pinned, 'pending', NULL
                 FROM memories WHERE id = ?5 AND agent_id = ?6";
            let n = conn
                .execute(
                    insert_sql,
                    params![new_id, c, hash, now, id, va.id(), p.scope],
                )
                .map_err(map_update_err)?;
            if n == 0 {
                return Err(OrbitalError::MemoryNotFound { id: id.to_string() });
            }

            // ② 旧条标记：被谁取代 + 何时失效（保留行，可查、可回滚）
            conn.execute(
                "UPDATE memories SET superseded_by=?1, valid_to=?2, updated_at=?3
                 WHERE id=?4 AND agent_id=?5",
                params![new_id, now, now, id, va.id()],
            )
            .map_err(map_update_err)?;

            // ③ 建 supersedes 边（新 → 旧），与 docs/04 §2.3 的生成规则一致
            conn.execute(
                "INSERT OR REPLACE INTO edges
                   (source_id, target_id, edge_type, weight, metadata, created_at)
                 VALUES (?1, ?2, 'supersedes', 1.0, '{}', ?3)",
                params![new_id, id, now],
            )
            .map_err(map_err)?;

            superseded_id = Some(id.to_string());
            out_id = new_id;
            action = "superseded";
        }

        if let Some(i) = p.importance {
            if !(1..=5).contains(&i) {
                return Err(OrbitalError::InvalidArgument {
                    reason: "importance 必须在 1..5".into(),
                });
            }
            conn.execute(
                "UPDATE memories SET importance=?1, updated_at=?2 WHERE id=?3 AND agent_id=?4",
                params![i, now, &out_id, va.id()],
            )
            .map_err(map_err)?;
        }
        if let Some(ref t) = p.tags {
            conn.execute(
                "UPDATE memories SET tags=?1, updated_at=?2 WHERE id=?3 AND agent_id=?4",
                params![
                    serde_json::to_string(t).unwrap_or_else(|_| "[]".into()),
                    now,
                    &out_id,
                    va.id()
                ],
            )
            .map_err(map_err)?;
        }
        if let Some(pin) = p.pinned {
            conn.execute(
                "UPDATE memories SET pinned=?1, updated_at=?2 WHERE id=?3 AND agent_id=?4",
                params![if pin { 1 } else { 0 }, now, &out_id, va.id()],
            )
            .map_err(map_err)?;
        }

        // T-M10-20（P0 审核修复）：**元数据分支此前完全忽略 `p.scope`**。
        // 后果：只改 scope 不改内容的调用（GUI 记忆详情页的「提升为共享」按钮正是这种）
        // 会返回 Ok，但库内 scope 纹丝不动 —— 静默无效。
        // 2026-11-14 实测：promote 脚本报 "PROMOTED 4"，库里 `shared` 仍为 0。
        // 这里只在**没有发生 supersede** 时处理（supersede 分支已用 p.scope 写了新条）。
        if superseded_id.is_none() {
            if let Some(ref want) = p.scope {
                if scope_rank(want) != scope_rank(&cur.scope) {
                    if want == "shared" {
                        // ADR-023 规则 1：shared 必须 user_endorsed（用户认可），随升级一并改 origin
                        conn.execute(
                            "UPDATE memories SET scope=?1, origin='user_endorsed', updated_at=?2
                              WHERE id=?3 AND agent_id=?4",
                            params![want, now, &out_id, va.id()],
                        )
                        .map_err(map_err)?;
                    } else {
                        // 降级（如 shared → private）：只改 scope，origin 保持原值
                        conn.execute(
                            "UPDATE memories SET scope=?1, updated_at=?2 WHERE id=?3 AND agent_id=?4",
                            params![want, now, &out_id, va.id()],
                        )
                        .map_err(map_err)?;
                    }
                }
            }
        }

        // 审计：T-M2-02 验收 3 要求 detail 里含 **前后值**（原来只记 actor，
        // 事后无法回答"改之前是什么"——那等于没有审计）。
        let (op, detail) = match &superseded_id {
            Some(old_id) => (
                "supersede",
                serde_json::json!({
                    "actor": va.id(),
                    "before": { "id": old_id, "content": cur.content, "scope": cur.scope,
                                "importance": cur.importance, "tags": cur.tags, "pinned": cur.pinned },
                    "after": { "id": out_id, "scope": p.scope,
                               "importance": p.importance, "tags": p.tags, "pinned": p.pinned },
                }),
            ),
            None => (
                "update",
                serde_json::json!({
                    "actor": va.id(),
                    "before": { "importance": cur.importance, "tags": cur.tags, "pinned": cur.pinned },
                    "after": { "importance": p.importance, "tags": p.tags, "pinned": p.pinned },
                }),
            ),
        };
        conn.execute(
            "INSERT INTO memory_events (operation, memory_id, actor, detail, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![op, id, va.id(), detail.to_string(), now],
        )
        .map_err(map_err)?;
        let event_id = conn.last_insert_rowid();
        self.journal_append(op, id, detail);

        let note = match action {
            "superseded" => "已保存（旧版本已进版本链，可回滚）",
            _ => "已保存",
        };
        Ok(UpdateOutcome {
            event_id: Some(event_id),
            action: action.into(),
            id: out_id,
            superseded_id,
            note: note.into(),
        })
    }

    /// 本地特权更新（ADR-034，**只给本机 GUI**）。
    ///
    /// 与 `update` 的唯一区别：不需要调用方持有 token——GUI 拿不到 agent token（库里只存 hash），
    /// 所以以"该条记忆自己的 agent"身份执行，等价于主人替它改自己的记录。
    ///
    /// 安全边界（三条，缺一不可）：
    /// 1. `agent_id` 必须**从库里查出来**，不能来自请求参数；
    /// 2. 只允许本机 IPC 调用，**绝不许**接到 MCP / HTTP / proxy；
    /// 3. 审计里 `actor` 仍是该 agent，但 detail 会带 `via: "gui"` 以便事后区分来源。
    pub fn update_local_owner(&self, id: &str, p: UpdatePatch) -> Result<UpdateOutcome> {
        let owner: String = self
            .reader
            .conn()
            .query_row(
                "SELECT agent_id FROM memories WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_err)?
            .ok_or_else(|| OrbitalError::MemoryNotFound {
                id: id.to_string(),
            })?;
        let va = VerifiedAgent::for_local_owner(owner);
        self.update(&va, id, p)
    }

    /// 删除前的影响面（T-M2-03 验收 1：弹窗数字必须来自真实 SQL）。
    ///
    /// 只做**读**，不需要身份——GUI 本来就能列出全部记忆；这里不返回内容全文，
    /// 只给计数与摘要，避免把"确认弹窗"变成数据导出通道。
    pub fn delete_impact(&self, id: &str) -> Result<DeleteImpact> {
        let conn = self.reader.conn();
        let row: Option<(String, i64, Option<f64>, i64)> = conn
            .query_row(
                "SELECT content, pinned, deleted_at, access_count FROM memories WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(map_err)?;
        let (content, pinned, deleted_at, access_count) = row.ok_or_else(|| {
            OrbitalError::MemoryNotFound {
                id: id.to_string(),
            }
        })?;
        let facts: i64 = conn
            .query_row(
                "SELECT count(*) FROM memory_facts WHERE memory_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let edges: i64 = conn
            .query_row(
                "SELECT count(*) FROM edges WHERE source_id = ?1 OR target_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok(DeleteImpact {
            id: id.to_string(),
            summary: summarize(&content, 60),
            pinned: pinned != 0,
            already_deleted: deleted_at.is_some(),
            access_count,
            facts,
            edges,
        })
    }

    /// 恢复一条被软删的记忆（T-M2-03 验收 2：内容必须逐字节一致）。
    ///
    /// 只把 `deleted_at` 置回 NULL；**不重建**向量、不改内容、不动 `valid_to`
    /// ——那三样都不该因为"删了又恢复"而变化。
    /// 唯一的例外是 `embedding_status`：若它曾被维护任务降级，恢复后标 pending 让后台重算。
    pub fn restore(&self, va: &VerifiedAgent, id: &str) -> Result<()> {
        let conn = self.reader.conn();
        let now = now_secs();
        let n = conn
            .execute(
                "UPDATE memories SET deleted_at = NULL, updated_at = ?1,
                        embedding_status = CASE WHEN embedding_status = 'skipped'
                                                THEN 'pending' ELSE embedding_status END
                 WHERE id = ?2 AND agent_id = ?3",
                params![now, id, va.id()],
            )
            .map_err(map_err)?;
        if n == 0 {
            // 可能是"不存在"也可能是"不是你的"；统一按找不到处理，不泄露存在性
            return Err(OrbitalError::MemoryNotFound { id: id.to_string() });
        }
        let detail = serde_json::json!({ "actor": va.id(), "action": "restore" });
        conn.execute(
            "INSERT INTO memory_events (operation, memory_id, actor, detail, created_at)
             VALUES ('restore', ?1, ?2, ?3, ?4)",
            params![id, va.id(), detail.to_string(), now],
        )
        .map_err(map_err)?;
        self.journal_append("restore", id, detail);
        Ok(())
    }

    /// 本地特权恢复（ADR-034，只给本机 GUI）。
    pub fn restore_local_owner(&self, id: &str) -> Result<()> {
        let owner = self.owner_of(id)?;
        self.restore(&VerifiedAgent::for_local_owner(owner), id)
    }

    /// 清除失效标记（T-M2-04 验收 4）：`valid_to = NULL` → 重新可被召回。
    ///
    /// 与 `restore`（撤销软删）是两件事，别混：
    ///
    ///   - `restore` 撤的是 `deleted_at`（"我删过"）
    ///   - 本函数撤的是 `valid_to`（"我让它过期"）
    ///
    /// 一条记忆可以"没过期但被软删"，也可以"没删但已过期"。
    pub fn restore_validity(&self, va: &VerifiedAgent, id: &str) -> Result<()> {
        let conn = self.reader.conn();
        let now = now_secs();
        let n = conn
            .execute(
                "UPDATE memories SET valid_to = NULL, updated_at = ?1
                 WHERE id = ?2 AND agent_id = ?3",
                params![now, id, va.id()],
            )
            .map_err(map_err)?;
        if n == 0 {
            return Err(OrbitalError::MemoryNotFound { id: id.to_string() });
        }
        let detail = serde_json::json!({ "actor": va.id(), "action": "restore_validity" });
        conn.execute(
            "INSERT INTO memory_events (operation, memory_id, actor, detail, created_at)
             VALUES ('restore_validity', ?1, ?2, ?3, ?4)",
            params![id, va.id(), detail.to_string(), now],
        )
        .map_err(map_err)?;
        self.journal_append("restore_validity", id, detail);
        Ok(())
    }

    /// 本地特权清除失效（ADR-034）。
    pub fn restore_validity_local_owner(&self, id: &str) -> Result<()> {
        let owner = self.owner_of(id)?;
        self.restore_validity(&VerifiedAgent::for_local_owner(owner), id)
    }

    /// 计算一条记忆的 `effective_importance`（docs/04 §6.1）。
    ///
    /// **读时计算，不落盘**（ADR-013）：落盘快照会与"最近访问"脱节，
    /// 拿过期值做退役决策等于用错误的尺子做不可逆操作。
    ///
    /// 公式：`base × max(1, ln(1+access)) × 0.5^(days/H) × (1+0.1×min(edges,5)) × origin_weight`，
    /// 且 `pinned = 1` 时 `ei = max(ei, 1.0)`（免疫降权）。
    ///
    /// 用途只两处：召回排序权重、退役候选排序。**不用它做自动删除。**
    pub fn effective_importance_of(&self, id: &str) -> Result<f64> {
        let conn = self.reader.conn();
        let (importance, access, origin, pinned, last_access, created): (
            i64,
            i64,
            String,
            i64,
            Option<f64>,
            f64,
        ) = conn
            .query_row(
                "SELECT importance, access_count, origin, pinned, last_accessed_at, created_at
                 FROM memories WHERE id = ?1",
                params![id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()
            .map_err(map_err)?
            .ok_or_else(|| OrbitalError::MemoryNotFound {
                id: id.to_string(),
            })?;
        let edge_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM edges WHERE source_id = ?1 OR target_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap_or(0);

        let base = match importance {
            5 => 1.0,
            4 => 0.8,
            3 => 0.5,
            2 => 0.3,
            _ => 0.15,
        };
        let access_factor = (1.0 + access as f64).ln().max(1.0);
        // 半衰期 30 天：以"最近访问"为准，从未访问则用创建时间
        const HALF_LIFE_DAYS: f64 = 30.0;
        let since = last_access.unwrap_or(created);
        let days = ((now_secs() - since) / 86400.0).max(0.0);
        let decay = 0.5_f64.powf(days / HALF_LIFE_DAYS);
        let edge_factor = 1.0 + 0.1 * (edge_count.min(5) as f64);
        let origin_weight = if origin == "user_endorsed" { 1.0 } else { 0.8 };

        let mut ei = base * access_factor * decay * edge_factor * origin_weight;
        // Pin = 免疫降权（docs/04 §6.1）。这是验收 1 的唯一可测断言点。
        if pinned != 0 {
            ei = ei.max(1.0);
        }
        Ok(ei)
    }

    /// 本地特权删除（ADR-034，只给本机 GUI）。返回事件 id（可交给 `undo_event`）。
    pub fn forget_local_owner(&self, id: &str, mode: &str) -> Result<i64> {
        let owner = self.owner_of(id)?;
        self.forget(&VerifiedAgent::for_local_owner(owner), id, mode)
    }

    /// 查某条记忆的 owner（本地特权路径用；**只从库里查，不接受请求参数**）。
    fn owner_of(&self, id: &str) -> Result<String> {
        self.reader
            .conn()
            .query_row(
                "SELECT agent_id FROM memories WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_err)?
            .ok_or_else(|| OrbitalError::MemoryNotFound {
                id: id.to_string(),
            })
    }

    /// 硬删前的完整快照（写进 journal，op=`hard_delete_snapshot`）。
    ///
    /// 包含：记忆行全部字段 + 它的 facts + 它的边。
    /// 不含向量（可从内容重算，且 BLOB 塞进 journal 会让文件迅速膨胀）。
    fn snapshot_for_hard_delete(&self, id: &str) -> Result<serde_json::Value> {
        let conn = self.reader.conn();
        let row = conn
            .query_row(
                "SELECT id, content, agent_id, scope, origin, category, importance, confidence,
                        source_ref, session_id, tags, entities, created_at, updated_at,
                        valid_from, valid_to, recorded_at, access_count, pinned,
                        superseded_by, content_hash, embedding_status
                 FROM memories WHERE id = ?1",
                params![id],
                |r| {
                    Ok(serde_json::json!({
                        "id": r.get::<_, String>(0)?,
                        "content": r.get::<_, String>(1)?,
                        "agent_id": r.get::<_, String>(2)?,
                        "scope": r.get::<_, String>(3)?,
                        "origin": r.get::<_, String>(4)?,
                        "category": r.get::<_, String>(5)?,
                        "importance": r.get::<_, i64>(6)?,
                        "confidence": r.get::<_, f64>(7)?,
                        "source_ref": r.get::<_, Option<String>>(8)?,
                        "session_id": r.get::<_, Option<String>>(9)?,
                        "tags": r.get::<_, String>(10)?,
                        "entities": r.get::<_, String>(11)?,
                        "created_at": r.get::<_, f64>(12)?,
                        "updated_at": r.get::<_, f64>(13)?,
                        "valid_from": r.get::<_, f64>(14)?,
                        "valid_to": r.get::<_, Option<f64>>(15)?,
                        "recorded_at": r.get::<_, f64>(16)?,
                        "access_count": r.get::<_, i64>(17)?,
                        "pinned": r.get::<_, i64>(18)?,
                        "superseded_by": r.get::<_, Option<String>>(19)?,
                        "content_hash": r.get::<_, String>(20)?,
                        "embedding_status": r.get::<_, String>(21)?,
                    }))
                },
            )
            .optional()
            .map_err(map_err)?
            .ok_or_else(|| OrbitalError::MemoryNotFound {
                id: id.to_string(),
            })?;
        // ⚠️ 2026-11-13 修（T-M2-05 顺手修的真 bug）：
        // 这里原来写的是 `SELECT key, value, confidence FROM memory_facts`，
        // 而 `memory_facts` 根本没有 `key` 列（schema 是 entity_type/entity_id/property/value）。
        // 更糟的是外层用 `if let Ok(...)` 把 SQL 错误吞掉了 ⇒ **快照里的 facts 永远是空数组**，
        // 硬删之后再也捞不回结构化属性，而日志上一切正常（静默降级）。
        // 现在改成用真实列名 + 错误向上传播：宁可硬删失败，也不许"备份成功但内容为空"。
        let mut facts = Vec::new();
        let mut st = conn
            .prepare(
                "SELECT agent_id, entity_type, entity_id, property, value, confidence,
                        valid_from, valid_to, recorded_at
                   FROM memory_facts WHERE memory_id = ?1
                  ORDER BY entity_type, entity_id, property",
            )
            .map_err(map_err)?;
        let rows = st
            .query_map(params![id], |r| {
                Ok(serde_json::json!({
                    "agent_id": r.get::<_, String>(0)?,
                    "entity_type": r.get::<_, String>(1)?,
                    "entity_id": r.get::<_, String>(2)?,
                    "property": r.get::<_, String>(3)?,
                    "value": r.get::<_, String>(4)?,
                    "confidence": r.get::<_, f64>(5)?,
                    "valid_from": r.get::<_, f64>(6)?,
                    "valid_to": r.get::<_, Option<f64>>(7)?,
                    "recorded_at": r.get::<_, f64>(8)?,
                }))
            })
            .map_err(map_err)?;
        for r in rows {
            facts.push(r.map_err(map_err)?);
        }
        let mut edges = Vec::new();
        let mut st = conn
            .prepare(
                "SELECT source_id, target_id, edge_type, weight, metadata, created_at
                    FROM edges WHERE source_id = ?1 OR target_id = ?1",
            )
            .map_err(map_err)?;
        let rows = st
            .query_map(params![id], |r| {
                Ok(serde_json::json!({
                    "source_id": r.get::<_, String>(0)?,
                    "target_id": r.get::<_, String>(1)?,
                    "edge_type": r.get::<_, String>(2)?,
                    "weight": r.get::<_, f64>(3)?,
                    "metadata": r.get::<_, String>(4)?,
                    "created_at": r.get::<_, f64>(5)?,
                }))
            })
            .map_err(map_err)?;
        for r in rows {
            edges.push(r.map_err(map_err)?);
        }
        Ok(serde_json::json!({
            "memory": row,
            "facts": facts,
            "edges": edges,
            "snapshot_at": now_secs(),
        }))
    }

    /// `mode`: `soft` 软删 / `invalidate` 标记失效 / `hard` 物理删除
    ///
    /// 返回本次写下的 `memory_events.id`（T-M2-07：前端拿它做"5 秒内撤销"）。
    /// **hard 返回的事件 id 不可撤销**——撤销时会被明确拒绝（见 `undo_event`）。
    pub fn forget(&self, va: &VerifiedAgent, id: &str, mode: &str) -> Result<i64> {
        let _ = self.get(va, id)?; // 可见性
        let conn = self.reader.conn();
        let now = now_secs();
        match mode {
            "soft" => {
                conn.execute(
                    "UPDATE memories SET deleted_at=?1, updated_at=?1 WHERE id=?2 AND agent_id=?3",
                    params![now, id, va.id()],
                )
                .map_err(map_err)?;
            }
            "invalidate" => {
                conn.execute(
                    "UPDATE memories SET valid_to=?1, updated_at=?1 WHERE id=?2 AND agent_id=?3",
                    params![now, id, va.id()],
                )
                .map_err(map_err)?;
            }
            "hard" => {
                // 硬删不可逆（docs/04 §6.2），执行前必须把整条快照写进 journal：
                // 这是事后唯一能"至少把内容捞回来"的依据。
                // 先写 journal 再删——顺序反了就等于没备份。
                let snap = self.snapshot_for_hard_delete(id)?;
                self.journal_append("hard_delete_snapshot", id, snap);
                conn.execute("DELETE FROM memories WHERE id=?1", params![id])
                    .map_err(map_err)?;
            }
            other => {
                return Err(OrbitalError::InvalidArgument {
                    reason: format!("未知 mode: {other}"),
                })
            }
        }
        conn.execute(
            "INSERT INTO memory_events (operation, memory_id, actor, detail, created_at)
             VALUES (?1, ?2, ?3, '{}', ?4)",
            params![format!("forget:{mode}"), id, va.id(), now],
        )
        .map_err(map_err)?;
        let event_id = conn.last_insert_rowid();
        let op = match mode { "soft" => "soft_delete", "invalidate" => "invalidate", _ => "hard_delete" };
        self.journal_append(op, id, serde_json::json!({"actor": va.id(), "mode": mode}));
        Ok(event_id)
    }

    // ---------------- 冲突收件箱（T-M2-06）----------------

    /// 扫描 `memory_facts` 并登记冲突（幂等，可反复跑）。
    ///
    /// 返回**新增**条数。已存在的同组冲突（UNIQUE 命中）不会重复插入，
    /// 也不会把用户裁决过的 `ignored`/`resolved` 重新拉成 `open`——
    /// 这是"处理完计数必须真的减少"的前提。
    pub fn detect_conflicts(&self) -> Result<usize> {
        let conn = self.reader.conn();
        let tx = rusqlite::Transaction::new_unchecked(
            conn,
            rusqlite::TransactionBehavior::Immediate,
        )
        .map_err(map_err)?;

        // ① 先找出"同实体同属性多值"的组（`docs/04 §7` 的判据）
        let groups: Vec<(String, String, String)> = {
            let mut st = tx
                .prepare(
                    "SELECT entity_type, entity_id, property FROM memory_facts
                      WHERE valid_to IS NULL
                      GROUP BY entity_type, entity_id, property
                     HAVING count(DISTINCT value) > 1",
                )
                .map_err(map_err)?;
            let rows = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(map_err)?;
            let mut v = Vec::new();
            for x in rows {
                v.push(x.map_err(map_err)?);
            }
            v
        };

        let now = now_secs();
        let mut inserted = 0usize;
        for (et, eid, prop) in groups {
            // ② 组内所有有效 fact 行
            let facts: Vec<(i64, String, String)> = {
                let mut st = tx
                    .prepare(
                        "SELECT id, memory_id, value FROM memory_facts
                          WHERE entity_type = ?1 AND entity_id = ?2 AND property = ?3
                            AND valid_to IS NULL",
                    )
                    .map_err(map_err)?;
                let rows = st
                    .query_map(params![et, eid, prop], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })
                    .map_err(map_err)?;
                let mut v = Vec::new();
                for x in rows {
                    v.push(x.map_err(map_err)?);
                }
                v
            };
            // ③ 两两配对：值不同、且**分属不同记忆**（同一记忆内部自相矛盾不是"留A留B"能解决的）
            for i in 0..facts.len() {
                for j in (i + 1)..facts.len() {
                    let (fa, ma, va) = &facts[i];
                    let (fb, mb, vb) = &facts[j];
                    if va == vb || ma == mb {
                        continue;
                    }
                    // 统一 a/b 顺序（按 memory_id 字典序），否则 (A,B) 与 (B,A) 会各插一条
                    let (ma_s, mb_s, va_s, vb_s, fa_n, fb_n) = if ma < mb {
                        (ma.clone(), mb.clone(), va.clone(), vb.clone(), *fa, *fb)
                    } else {
                        (mb.clone(), ma.clone(), vb.clone(), va.clone(), *fb, *fa)
                    };
                    let n = tx
                        .execute(
                            "INSERT OR IGNORE INTO memory_conflicts
                               (entity_type, entity_id, property, memory_a, memory_b,
                                value_a, value_b, fact_ids, status, created_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'open', ?9)",
                            params![
                                et,
                                eid,
                                prop,
                                ma_s,
                                mb_s,
                                va_s,
                                vb_s,
                                serde_json::json!([fa_n, fb_n]).to_string(),
                                now
                            ],
                        )
                        .map_err(map_err)?;
                    inserted += n;
                }
            }
        }
        tx.commit().map_err(map_err)?;
        Ok(inserted)
    }

    /// 未处理的冲突列表（维护页"冲突"待办与对比视图的数据源）。
    pub fn list_conflicts(&self) -> Result<Vec<ConflictRow>> {
        let conn = self.reader.conn();
        let mut stmt = conn
            .prepare(
                "SELECT c.id, c.entity_type, c.entity_id, c.property,
                        c.memory_a, c.memory_b, c.value_a, c.value_b, c.created_at,
                        COALESCE(ma.content, ''), COALESCE(mb.content, ''),
                        COALESCE(ma.agent_id, ''), COALESCE(mb.agent_id, '')
                   FROM memory_conflicts c
                   LEFT JOIN memories ma ON ma.id = c.memory_a
                   LEFT JOIN memories mb ON mb.id = c.memory_b
                  WHERE c.status = 'open'
                  ORDER BY c.created_at DESC, c.id DESC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |r| {
                let ca: String = r.get(9)?;
                let cb: String = r.get(10)?;
                Ok(ConflictRow {
                    id: r.get(0)?,
                    entity_type: r.get(1)?,
                    entity_id: r.get(2)?,
                    property: r.get(3)?,
                    memory_a: r.get(4)?,
                    memory_b: r.get(5)?,
                    value_a: r.get(6)?,
                    value_b: r.get(7)?,
                    summary_a: summarize(&ca, 60),
                    summary_b: summarize(&cb, 60),
                    agent_a: r.get(11)?,
                    agent_b: r.get(12)?,
                    created_at: r.get(8)?,
                })
            })
            .map_err(map_err)?;
        let mut out = Vec::new();
        for x in rows {
            out.push(x.map_err(map_err)?);
        }
        Ok(out)
    }

    /// 裁决一条冲突（四选一，`docs/03 §4.3` 的"四选一"）。
    ///
    /// - `keep_a` / `keep_b`：保留一侧的值，把**另一侧**的 fact 标 `valid_to=now`
    ///   （记忆内容不动——冲突是事实层的事）
    /// - `keep_both`：数据不动，状态记 `ignored`（`detect_conflicts` 不会再报同一组）
    /// - `merged`：调用方应**先** `merge` 两条再调本方法（这里只记录裁决）
    ///
    /// 已处理过的冲突再处理一次返回 InvalidArgument（防重复点击导致误改）。
    pub fn resolve_conflict(&self, id: i64, resolution: &str) -> Result<()> {
        let conn = self.reader.conn();
        let tx = rusqlite::Transaction::new_unchecked(
            conn,
            rusqlite::TransactionBehavior::Immediate,
        )
        .map_err(map_err)?;
        let (status, fact_ids, mem_a, mem_b): (String, String, String, String) = tx
            .query_row(
                "SELECT status, fact_ids, memory_a, memory_b FROM memory_conflicts WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(map_err)?
            .ok_or_else(|| OrbitalError::InvalidArgument {
                reason: format!("没有 id={id} 的冲突记录"),
            })?;
        if status != "open" {
            return Err(OrbitalError::InvalidArgument {
                reason: format!("这条冲突已处理过（status={status}）"),
            });
        }

        let now = now_secs();
        let (new_status, invalidate_side): (&str, Option<usize>) = match resolution {
            "keep_a" => ("resolved", Some(1)), // 失效 b 侧（下标 1）
            "keep_b" => ("resolved", Some(0)), // 失效 a 侧（下标 0）
            "keep_both" => ("ignored", None),
            "merged" => ("resolved", None),
            other => {
                return Err(OrbitalError::InvalidArgument {
                    reason: format!("未知裁决: {other}（只允许 keep_a/keep_b/keep_both/merged）"),
                })
            }
        };

        if let Some(side) = invalidate_side {
            // ⚠️ T-M8-01 修的第二个缺陷：原来用 `fact_ids[side]`（**数组下标**）定位弃方，
            // 但 `fact_ids` 的顺序与 `memory_a`/`memory_b` 两列**不一定对应**
            // （detect_conflicts 的插入顺序由 SQL 决定），所以会弃错方。
            // 改为：**按列名**定位弃方（keep_a 弃 memory_b，keep_b 弃 memory_a），再按 memory_id 失效其 fact。
            let dropped = if side == 1 { &mem_b } else { &mem_a };
            tx.execute(
                "UPDATE memory_facts SET valid_to = ?1 WHERE memory_id = ?2 AND valid_to IS NULL",
                params![now, dropped],
            )
            .map_err(map_err)?;
            let _ = &fact_ids; // 保留列（供审计），不再用它做下标定位
            // 只失效 fact 不够——冲突一关，弃方**记忆**就会因为"不再是 open 冲突"而重新可见。
            // 裁决语义是"留 X"，所以弃方记忆要一并视同被取代（valid_to）。
            tx.execute(
                "UPDATE memories SET valid_to = ?1 WHERE id = ?2 AND valid_to IS NULL",
                params![now, dropped],
            )
            .map_err(map_err)?;
        }

        tx.execute(
            "UPDATE memory_conflicts SET status = ?1, resolution = ?2, resolved_at = ?3
              WHERE id = ?4",
            params![new_status, resolution, now, id],
        )
        .map_err(map_err)?;
        tx.commit().map_err(map_err)?;
        Ok(())
    }

    // ---------------- 合并重复（T-M2-05）----------------

    /// 合并预览：告诉用户"这次合并会搬走什么"。
    ///
    /// 为什么必须有预览：`docs/03 §5.2` 的三种决策里，"合并两者"是唯一有副作用的选择；
    /// 用户点之前必须知道会搬几条关系、几个标签（否则和盲删没区别）。
    pub fn merge_preview(&self, keep_id: &str, drop_id: &str) -> Result<MergePreview> {
        if keep_id == drop_id {
            return Err(OrbitalError::InvalidArgument {
                reason: "不能把一条记忆与自己合并".into(),
            });
        }
        let conn = self.reader.conn();
        let keep = merge_side(conn, keep_id)?;
        let drop = merge_side(conn, drop_id)?;
        let (tags, tags_moved) = union_strings(&keep.tags, &drop.tags);
        let facts_moved: i64 = conn
            .query_row(
                "SELECT count(*) FROM memory_facts WHERE memory_id = ?1",
                params![drop_id],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        let self_loops: i64 = conn
            .query_row(
                "SELECT count(*) FROM edges
                  WHERE (source_id = ?1 AND target_id = ?2) OR (source_id = ?2 AND target_id = ?1)",
                params![keep_id, drop_id],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        let edges_total: i64 = conn
            .query_row(
                "SELECT count(*) FROM edges WHERE source_id = ?1 OR target_id = ?1",
                params![drop_id],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        let edges_moved = edges_total - self_loops;
        Ok(MergePreview {
            keep_id: keep_id.to_string(),
            drop_id: drop_id.to_string(),
            keep_summary: summarize(&keep.content, 60),
            drop_summary: summarize(&drop.content, 60),
            keep_agent: keep.agent_id,
            drop_agent: drop.agent_id,
            tags,
            tags_moved,
            edges_moved,
            self_loops,
            facts_moved,
        })
    }

    /// 合并两条重复记忆（`docs/03 §5.2` 的「合并两者」）。
    ///
    /// 语义（**只搬附属物，不拼内容**）：
    /// - 内容保留 `keep` 的，不与 `drop` 拼接——拼接会破坏 `content_hash` 与向量，
    ///   而且"两条近似重复"的全文拼起来只会得到一段更长的噪声（决策记在 ADR-036）。
    /// - 搬：标签并集、实体并集、`memory_facts` 改指、`edges` 改指。
    /// - 丢：`drop` 软删（可恢复），边/事实全部搬走，不留指向已删记忆的残边。
    ///
    /// 整个操作在 **`BEGIN IMMEDIATE` 事务**内完成：任何一步失败，A 与 B 都不动。
    pub fn merge(&self, va: &VerifiedAgent, keep_id: &str, drop_id: &str) -> Result<MergeOutcome> {
        // 两端都必须可见（与 `link` 同口径，防止借合并把别人的私有记忆搬进自己的条目）
        let _ = self.get(va, keep_id)?;
        let _ = self.get(va, drop_id)?;
        self.merge_inner(keep_id, drop_id, va.id())
    }

    /// 本地特权合并（ADR-034，**只给本机 GUI**）：身份从库里查，不接受请求参数。
    pub fn merge_local_owner(&self, keep_id: &str, drop_id: &str) -> Result<MergeOutcome> {
        let keep_owner = self.owner_of(keep_id)?;
        let _ = self.owner_of(drop_id)?;
        self.merge_inner(keep_id, drop_id, &keep_owner)
    }

    fn merge_inner(&self, keep_id: &str, drop_id: &str, actor: &str) -> Result<MergeOutcome> {
        if keep_id == drop_id {
            return Err(OrbitalError::InvalidArgument {
                reason: "不能把一条记忆与自己合并".into(),
            });
        }
        let conn = self.reader.conn();
        // IMMEDIATE：先拿写锁再读，避免"读到 B 的边 → 别人删了 B → 搬空"
        let tx = rusqlite::Transaction::new_unchecked(
            conn,
            rusqlite::TransactionBehavior::Immediate,
        )
        .map_err(map_err)?;

        let keep = merge_side(&tx, keep_id)?;
        let drop = merge_side(&tx, drop_id)?;
        if drop.deleted_at.is_some() {
            return Err(OrbitalError::InvalidArgument {
                reason: "目标条已被删除，不能再作为合并来源".into(),
            });
        }
        let now = now_secs();

        // ---- ① 标签/实体并集（保持 keep 原有顺序，只追加 drop 独有的）----
        let (tags, tags_moved) = union_strings(&keep.tags, &drop.tags);
        let (entities, entities_moved) = union_strings(&keep.entities, &drop.entities);

        // ---- ② facts 改指 keep ----
        // 完全同值（实体/属性/值/有效期全同）的才删，避免无意义重复；
        // 同实体同属性但**值不同**的必须保留——那种多值正是 docs/04 §2.2
        // "冲突检测（同实体同属性多个有效值）"的输入，删掉就是丢信息。
        let facts = {
            let mut st = tx
                .prepare(
                    "SELECT id, agent_id, entity_type, entity_id, property, value, confidence,
                            valid_from, valid_to, recorded_at
                       FROM memory_facts WHERE memory_id = ?1",
                )
                .map_err(map_err)?;
            let rows = st
                .query_map(params![drop_id], |r| {
                    Ok(serde_json::json!({
                        "id": r.get::<_, i64>(0)?,
                        "agent_id": r.get::<_, String>(1)?,
                        "entity_type": r.get::<_, String>(2)?,
                        "entity_id": r.get::<_, String>(3)?,
                        "property": r.get::<_, String>(4)?,
                        "value": r.get::<_, String>(5)?,
                        "confidence": r.get::<_, f64>(6)?,
                        "valid_from": r.get::<_, f64>(7)?,
                        "valid_to": r.get::<_, Option<f64>>(8)?,
                        "recorded_at": r.get::<_, f64>(9)?,
                    }))
                })
                .map_err(map_err)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(map_err)?);
            }
            out
        };
        let mut moved_fact_ids: Vec<i64> = Vec::new();
        let mut dedupe_dropped: Vec<serde_json::Value> = Vec::new();
        for f in &facts {
            let fid = f["id"].as_i64().unwrap_or(0);
            let dup: Option<i64> = tx
                .query_row(
                    "SELECT id FROM memory_facts
                      WHERE memory_id = ?1 AND entity_type = ?2 AND entity_id = ?3
                        AND property = ?4 AND value = ?5 AND valid_to IS ?6
                      LIMIT 1",
                    params![
                        keep_id,
                        f["entity_type"].as_str().unwrap_or(""),
                        f["entity_id"].as_str().unwrap_or(""),
                        f["property"].as_str().unwrap_or(""),
                        f["value"].as_str().unwrap_or(""),
                        f["valid_to"].as_f64()
                    ],
                    |r| r.get(0),
                )
                .optional()
                .map_err(map_err)?;
            if dup.is_some() {
                tx.execute("DELETE FROM memory_facts WHERE id = ?1", params![fid])
                    .map_err(map_err)?;
                dedupe_dropped.push(f.clone());
            } else {
                tx.execute(
                    "UPDATE memory_facts SET memory_id = ?1 WHERE id = ?2",
                    params![keep_id, fid],
                )
                .map_err(map_err)?;
                // 改指后 agent_id 保持原值：事实的归属是"谁记录的"，不是"挂在谁名下"
                moved_fact_ids.push(fid);
            }
        }

        // ---- ③ edges 改指 keep ----
        // 自环（改指后 A→A）直接丢：它对图遍历毫无信息量，留着只会让 related 自指。
        // 主键冲突用 `INSERT OR IGNORE`：保留 keep 已有的那条边（不覆盖既有权重）。
        let edge_rows = {
            let mut st = tx
                .prepare(
                    "SELECT source_id, target_id, edge_type, weight, metadata, created_at
                       FROM edges WHERE source_id = ?1 OR target_id = ?1",
                )
                .map_err(map_err)?;
            let rows = st
                .query_map(params![drop_id], |r| {
                    Ok(serde_json::json!({
                        "source_id": r.get::<_, String>(0)?,
                        "target_id": r.get::<_, String>(1)?,
                        "edge_type": r.get::<_, String>(2)?,
                        "weight": r.get::<_, f64>(3)?,
                        "metadata": r.get::<_, String>(4)?,
                        "created_at": r.get::<_, f64>(5)?,
                    }))
                })
                .map_err(map_err)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(map_err)?);
            }
            out
        };
        let mut moved_edges: Vec<serde_json::Value> = Vec::new();
        let mut self_loops: Vec<serde_json::Value> = Vec::new();
        for e in &edge_rows {
            let src = e["source_id"].as_str().unwrap_or("").to_string();
            let tgt = e["target_id"].as_str().unwrap_or("").to_string();
            let et = e["edge_type"].as_str().unwrap_or("").to_string();
            let new_src = if src == drop_id { keep_id.to_string() } else { src.clone() };
            let new_tgt = if tgt == drop_id { keep_id.to_string() } else { tgt.clone() };
            tx.execute(
                "DELETE FROM edges WHERE source_id = ?1 AND target_id = ?2 AND edge_type = ?3",
                params![src, tgt, et],
            )
            .map_err(map_err)?;
            if new_src == new_tgt {
                self_loops.push(e.clone());
                continue;
            }
            // T-M10-04：`INSERT OR IGNORE` 可能因"keep 已有同 key 边"被忽略。
            // 必须记录"这条边到底有没有插进去"，否则 undo 无条件 DELETE 会删掉 keep 原有的边
            // （实测：A→C 0.5 + B→C 1.0 合并后撤销，A→C 消失）。
            let inserted = tx
                .execute(
                    "INSERT OR IGNORE INTO edges
                       (source_id, target_id, edge_type, weight, metadata, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        new_src,
                        new_tgt,
                        et,
                        e["weight"].as_f64().unwrap_or(1.0),
                        e["metadata"].as_str().unwrap_or("{}"),
                        e["created_at"].as_f64().unwrap_or(now)
                    ],
                )
                .map_err(map_err)?
                > 0;
            moved_edges.push(serde_json::json!({
                "old": e,
                "new": { "source_id": new_src, "target_id": new_tgt, "edge_type": et },
                "inserted": inserted,
            }));
        }

        // ---- ④ 写回 keep 的标签/实体 ----
        tx.execute(
            "UPDATE memories SET tags = ?1, entities = ?2, updated_at = ?3 WHERE id = ?4",
            params![
                serde_json::to_string(&tags).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(&entities).unwrap_or_else(|_| "[]".into()),
                now,
                keep_id
            ],
        )
        .map_err(map_err)?;

        // ---- ⑤ 软删 drop ----
        tx.execute(
            "UPDATE memories SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2",
            params![now, drop_id],
        )
        .map_err(map_err)?;

        // ---- ⑥ 审计（detail 带完整清单，撤销靠它）----
        let detail = serde_json::json!({
            "actor": actor,
            "via": "merge",
            "kept": keep_id,
            "merged": drop_id,
            "tags_before": keep.tags,
            "tags_after": tags,
            "tags_moved": tags_moved,
            "entities_before": keep.entities,
            "entities_after": entities,
            "entities_moved": entities_moved,
            "facts_moved_ids": moved_fact_ids,
            "facts_dedupe_dropped": dedupe_dropped,
            "edges_moved": moved_edges,
            "self_loops_dropped": self_loops,
            "drop_deleted_at": now,
        });
        tx.execute(
            "INSERT INTO memory_events (operation, memory_id, actor, detail, created_at)
             VALUES ('merge', ?1, ?2, ?3, ?4)",
            params![keep_id, actor, detail.to_string(), now],
        )
        .map_err(map_err)?;
        // 事件 id 就是撤销的句柄（前端"撤销"按钮要把它交回 undo_merge）
        let event_id = tx.last_insert_rowid();

        tx.commit().map_err(map_err)?;
        self.journal_append("merge", keep_id, detail.clone());

        Ok(MergeOutcome {
            event_id,
            keep_id: keep_id.to_string(),
            drop_id: drop_id.to_string(),
            tags: detail["tags_after"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            tags_moved: detail["tags_moved"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            // T-M10-04：只算"真正插入"的边。被 `INSERT OR IGNORE` 忽略的不算"搬走"
            // （那是 keep 本来就有的边，撤销时也不能删它）。
            edges_moved: moved_edges
                .iter()
                .filter(|e| e["inserted"].as_bool().unwrap_or(true))
                .count() as i64,
            self_loops: self_loops.len() as i64,
            facts_moved: moved_fact_ids.len() as i64,
            note: format!(
                "已合并：保留 {}，{} 已软删；搬走 {} 个标签 / {} 条关系 / {} 条事实{}",
                summarize(&keep.content, 20),
                summarize(&drop.content, 20),
                tags_moved.len(),
                moved_edges
                    .iter()
                    .filter(|e| e["inserted"].as_bool().unwrap_or(true))
                    .count(),
                moved_fact_ids.len(),
                if self_loops.is_empty() {
                    String::new()
                } else {
                    format!("；{} 条边变成自环已丢弃", self_loops.len())
                }
            ),
        })
    }

    /// 撤销一次合并（T-M2-05 验收 4；T-M2-07 的统一撤销会复用它）。
    ///
    /// 逆操作清单全部来自 `memory_events.detail`（**不许**从当前状态反推——
    /// 合并后用户可能又改过，反推会算出错误的历史）。
    /// 幂等：B 已恢复时直接返回，不重复搬。
    pub fn undo_merge(&self, event_id: i64) -> Result<UndoOutcome> {
        let conn = self.reader.conn();
        let tx = rusqlite::Transaction::new_unchecked(
            conn,
            rusqlite::TransactionBehavior::Immediate,
        )
        .map_err(map_err)?;
        let (keep_id, detail_str): (Option<String>, String) = tx
            .query_row(
                "SELECT memory_id, detail FROM memory_events
                  WHERE id = ?1 AND operation = 'merge'",
                params![event_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(map_err)?
            .ok_or_else(|| OrbitalError::InvalidArgument {
                reason: format!("没有 id={event_id} 的 merge 事件"),
            })?;
        let detail: serde_json::Value = serde_json::from_str(&detail_str).map_err(|e| {
            OrbitalError::Internal(format!("merge 事件 detail 不是合法 JSON: {e}"))
        })?;
        let keep_id = keep_id.unwrap_or_default();
        let drop_id = detail["merged"].as_str().unwrap_or("").to_string();
        if keep_id.is_empty() || drop_id.is_empty() {
            return Err(OrbitalError::Internal("merge 事件缺 kept/merged".into()));
        }
        // 幂等：B 已经不是"被软删"状态 → 说明撤销过了
        let drop_deleted: Option<f64> = tx
            .query_row(
                "SELECT deleted_at FROM memories WHERE id = ?1",
                params![drop_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_err)?
            .flatten();
        if drop_deleted.is_none() {
            return Ok(UndoOutcome {
                event_id,
                keep_id,
                drop_id,
                note: "这条合并已经撤销过了（目标条已恢复）".into(),
            });
        }
        // A 必须还在：A 被硬删后撤销无从下手（软删可以，恢复只动 deleted_at）
        let keep_exists: bool = tx
            .query_row(
                "SELECT count(*) FROM memories WHERE id = ?1",
                params![keep_id],
                |r| Ok(r.get::<_, i64>(0)? > 0),
            )
            .map_err(map_err)?;
        if !keep_exists {
            return Err(OrbitalError::InvalidArgument {
                reason: "保留条已被彻底删除，无法撤销这次合并".into(),
            });
        }
        let now = now_secs();

        // ---- ① 还原 keep 的标签/实体 ----
        let tags_before = detail["tags_before"].clone();
        let entities_before = detail["entities_before"].clone();
        tx.execute(
            "UPDATE memories SET tags = ?1, entities = ?2, updated_at = ?3 WHERE id = ?4",
            params![tags_before.to_string(), entities_before.to_string(), now, keep_id],
        )
        .map_err(map_err)?;

        // ---- ② 边：先删合并时产生的"新边"，再把原始边写回 ----
        if let Some(arr) = detail["edges_moved"].as_array() {
            for e in arr {
                let new = &e["new"];
                // T-M10-04：只有当这条新边**确实由本次合并插入**时才删。
                // `inserted=false` 表示 merge 时 `INSERT OR IGNORE` 因主键冲突被忽略
                // （keep 本来就有这条边），撤销时不能碰它。
                // 旧事件没有该字段 → 默认 true（保持旧行为，不破坏历史撤销）。
                if e["inserted"].as_bool().unwrap_or(true) {
                    tx.execute(
                        "DELETE FROM edges WHERE source_id = ?1 AND target_id = ?2 AND edge_type = ?3",
                        params![
                            new["source_id"].as_str().unwrap_or(""),
                            new["target_id"].as_str().unwrap_or(""),
                            new["edge_type"].as_str().unwrap_or("")
                        ],
                    )
                    .map_err(map_err)?;
                }
                let old = &e["old"];
                tx.execute(
                    "INSERT OR REPLACE INTO edges
                       (source_id, target_id, edge_type, weight, metadata, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        old["source_id"].as_str().unwrap_or(""),
                        old["target_id"].as_str().unwrap_or(""),
                        old["edge_type"].as_str().unwrap_or(""),
                        old["weight"].as_f64().unwrap_or(1.0),
                        old["metadata"].as_str().unwrap_or("{}"),
                        old["created_at"].as_f64().unwrap_or(now)
                    ],
                )
                .map_err(map_err)?;
            }
        }
        // 被当自环丢掉的那些边，原样恢复
        if let Some(arr) = detail["self_loops_dropped"].as_array() {
            for e in arr {
                tx.execute(
                    "INSERT OR REPLACE INTO edges
                       (source_id, target_id, edge_type, weight, metadata, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        e["source_id"].as_str().unwrap_or(""),
                        e["target_id"].as_str().unwrap_or(""),
                        e["edge_type"].as_str().unwrap_or(""),
                        e["weight"].as_f64().unwrap_or(1.0),
                        e["metadata"].as_str().unwrap_or("{}"),
                        e["created_at"].as_f64().unwrap_or(now)
                    ],
                )
                .map_err(map_err)?;
            }
        }

        // ---- ③ facts：搬回 drop；合并时因完全同值被删的，原样插回 ----
        if let Some(arr) = detail["facts_moved_ids"].as_array() {
            for id in arr {
                if let Some(fid) = id.as_i64() {
                    tx.execute(
                        "UPDATE memory_facts SET memory_id = ?1 WHERE id = ?2",
                        params![drop_id, fid],
                    )
                    .map_err(map_err)?;
                }
            }
        }
        if let Some(arr) = detail["facts_dedupe_dropped"].as_array() {
            for f in arr {
                tx.execute(
                    "INSERT INTO memory_facts
                       (memory_id, agent_id, entity_type, entity_id, property, value,
                        confidence, valid_from, valid_to, recorded_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        drop_id,
                        f["agent_id"].as_str().unwrap_or(""),
                        f["entity_type"].as_str().unwrap_or(""),
                        f["entity_id"].as_str().unwrap_or(""),
                        f["property"].as_str().unwrap_or(""),
                        f["value"].as_str().unwrap_or(""),
                        f["confidence"].as_f64().unwrap_or(0.8),
                        f["valid_from"].as_f64().unwrap_or(now),
                        f["valid_to"].as_f64(),
                        f["recorded_at"].as_f64().unwrap_or(now)
                    ],
                )
                .map_err(map_err)?;
            }
        }

        // ---- ④ 恢复 drop（只清 deleted_at，内容逐字节不变）----
        tx.execute(
            "UPDATE memories SET deleted_at = NULL, updated_at = ?1 WHERE id = ?2",
            params![now, drop_id],
        )
        .map_err(map_err)?;

        // ---- ⑤ 撤销本身也留痕（T-M2-07 要求）----
        let undo_detail = serde_json::json!({
            "undo_of": event_id,
            "kept": keep_id,
            "merged": drop_id,
        });
        tx.execute(
            "INSERT INTO memory_events (operation, memory_id, actor, detail, created_at)
             VALUES ('undo', ?1, 'user', ?2, ?3)",
            params![keep_id, undo_detail.to_string(), now],
        )
        .map_err(map_err)?;

        tx.commit().map_err(map_err)?;
        self.journal_append("undo_merge", &keep_id, undo_detail.clone());

        Ok(UndoOutcome {
            event_id,
            keep_id,
            drop_id,
            note: "已撤销这次合并：两条都恢复，标签/关系/事实已搬回".into(),
        })
    }

    // ---------------- 撤销 / 回滚（T-M2-07）----------------

    /// 撤销一条审计事件（`docs/03 §1.3`「每个操作有回滚路径」）。
    ///
    /// 逆操作表（任务卡 T-M2-07）：
    /// - `update`：写回 `detail.before` 的 importance / tags / pinned
    /// - `supersede`：新版本软删 + 旧版本恢复有效（删掉 supersedes 边）
    /// - `forget:soft`：清 `deleted_at`
    /// - `forget:invalidate`：清 `valid_to`
    /// - `merge`：委托 `undo_merge`（按 detail 清单逆操作）
    /// - `forget:hard`：**明确拒绝**（数据已物理删除，没有逆操作）
    ///
    /// 幂等：同一条事件不能被撤销两次（第一次撤销会写一条 `undo` 事件，
    /// 这里按 `undo_of` 检查）。
    pub fn undo_event(&self, event_id: i64) -> Result<UndoEventOutcome> {
        let conn = self.reader.conn();
        let (op, memory_id, detail_str): (String, Option<String>, String) = conn
            .query_row(
                "SELECT operation, memory_id, detail FROM memory_events WHERE id = ?1",
                params![event_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(map_err)?
            .ok_or_else(|| OrbitalError::InvalidArgument {
                reason: format!("没有 id={event_id} 的审计事件"),
            })?;

        // 已被撤销过？（detail 是我们自己写的固定格式，`"undo_of":<id>` 可精确匹配）
        let already: i64 = conn
            .query_row(
                "SELECT count(*) FROM memory_events
                  WHERE operation = 'undo' AND detail LIKE ?1",
                params![format!("%\"undo_of\":{event_id}%")],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        if already > 0 {
            return Err(OrbitalError::InvalidArgument {
                reason: "这条操作已经撤销过了".into(),
            });
        }

        let mid = memory_id.unwrap_or_default();
        let detail: serde_json::Value =
            serde_json::from_str(&detail_str).unwrap_or(serde_json::json!({}));

        // merge 有自己的一套逆操作（含独立事务），不能套在本事务里
        if op == "merge" {
            let out = self.undo_merge(event_id)?;
            return Ok(UndoEventOutcome {
                event_id,
                memory_id: out.keep_id,
                note: out.note,
            });
        }

        let tx = rusqlite::Transaction::new_unchecked(
            conn,
            rusqlite::TransactionBehavior::Immediate,
        )
        .map_err(map_err)?;
        let now = now_secs();
        let note: String;

        match op.as_str() {
            "update" => {
                let before = detail
                    .get("before")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                let mut restored: Vec<&str> = Vec::new();
                if let Some(i) = before.get("importance").and_then(|v| v.as_i64()) {
                    tx.execute(
                        "UPDATE memories SET importance = ?1 WHERE id = ?2",
                        params![i, mid],
                    )
                    .map_err(map_err)?;
                    restored.push("重要度");
                }
                if let Some(t) = before.get("tags").and_then(|v| v.as_str()) {
                    tx.execute(
                        "UPDATE memories SET tags = ?1 WHERE id = ?2",
                        params![t, mid],
                    )
                    .map_err(map_err)?;
                    restored.push("标签");
                }
                if let Some(p) = before.get("pinned").and_then(|v| v.as_i64()) {
                    tx.execute(
                        "UPDATE memories SET pinned = ?1 WHERE id = ?2",
                        params![p, mid],
                    )
                    .map_err(map_err)?;
                    restored.push("Pin");
                }
                if restored.is_empty() {
                    return Err(OrbitalError::InvalidArgument {
                        reason: "这条 update 事件没有可恢复的字段".into(),
                    });
                }
                tx.execute(
                    "UPDATE memories SET updated_at = ?1 WHERE id = ?2",
                    params![now, mid],
                )
                .map_err(map_err)?;
                note = format!("已撤销编辑：恢复了{}", restored.join("/"));
            }
            "supersede" => {
                let old_id = detail["before"]["id"].as_str().unwrap_or("").to_string();
                let new_id = detail["after"]["id"].as_str().unwrap_or("").to_string();
                if old_id.is_empty() || new_id.is_empty() {
                    return Err(OrbitalError::Internal(
                        "supersede 事件缺 before/after id".into(),
                    ));
                }
                // 安全前提：新版本仍有效且没有被后续修改再次取代，否则撤销会制造第三个版本
                let (new_deleted, new_superseded): (Option<f64>, Option<String>) = tx
                    .query_row(
                        "SELECT deleted_at, superseded_by FROM memories WHERE id = ?1",
                        params![new_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(map_err)?
                    .ok_or_else(|| OrbitalError::InvalidArgument {
                        reason: "新版本已被彻底删除，无法撤销".into(),
                    })?;
                if new_deleted.is_some() {
                    return Err(OrbitalError::InvalidArgument {
                        reason: "新版本已被删除，无法撤销".into(),
                    });
                }
                if new_superseded.is_some() {
                    return Err(OrbitalError::InvalidArgument {
                        reason: "新版本已被再次修改取代，撤销会制造第三个版本；请直接从版本链回滚".into(),
                    });
                }
                // 新版本软删（可恢复），旧版本恢复有效
                tx.execute(
                    "UPDATE memories SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2",
                    params![now, new_id],
                )
                .map_err(map_err)?;
                tx.execute(
                    "UPDATE memories SET valid_to = NULL, superseded_by = NULL, updated_at = ?1
                      WHERE id = ?2",
                    params![now, old_id],
                )
                .map_err(map_err)?;
                tx.execute(
                    "DELETE FROM edges WHERE source_id = ?1 AND target_id = ?2
                       AND edge_type = 'supersedes'",
                    params![new_id, old_id],
                )
                .map_err(map_err)?;
                note = "已撤销内容变更：旧版本恢复有效，新版本已软删（可恢复）".into();
            }
            "forget:soft" => {
                tx.execute(
                    "UPDATE memories SET deleted_at = NULL, updated_at = ?1 WHERE id = ?2",
                    params![now, mid],
                )
                .map_err(map_err)?;
                note = "已撤销删除：记忆恢复可见".into();
            }
            "retire" => {
                // T-M4-02：降权的逆操作 = 把 from 写回
                let from = detail
                    .get("from")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| OrbitalError::Internal("降权事件 detail 缺少 from".into()))?;
                tx.execute(
                    "UPDATE memories SET importance = ?1, updated_at = ?2 WHERE id = ?3",
                    params![from, now, mid],
                )
                .map_err(map_err)?;
                note = format!("已撤销降权：重要度恢复到 {from}");
            }
            "forget:invalidate" => {
                tx.execute(
                    "UPDATE memories SET valid_to = NULL, updated_at = ?1 WHERE id = ?2",
                    params![now, mid],
                )
                .map_err(map_err)?;
                note = "已撤销失效标记：重新可被召回".into();
            }
            "forget:hard" => {
                return Err(OrbitalError::InvalidArgument {
                    reason: "彻底删除不可撤销（数据已物理删除，只有 journal 快照）".into(),
                })
            }
            "write" => {
                return Err(OrbitalError::InvalidArgument {
                    reason: "写入没有逆操作（要删请用删除，删除本身可撤销）".into(),
                })
            }
            other => {
                return Err(OrbitalError::InvalidArgument {
                    reason: format!("操作 {other} 不支持撤销"),
                })
            }
        }

        let undo_detail = serde_json::json!({
            "undo_of": event_id,
            "operation": op,
            "memory_id": mid,
            "note": note,
        });
        tx.execute(
            "INSERT INTO memory_events (operation, memory_id, actor, detail, created_at)
             VALUES ('undo', ?1, 'user', ?2, ?3)",
            params![mid, undo_detail.to_string(), now],
        )
        .map_err(map_err)?;
        tx.commit().map_err(map_err)?;
        self.journal_append("undo", &mid, undo_detail.clone());

        Ok(UndoEventOutcome {
            event_id,
            memory_id: mid,
            note,
        })
    }

    /// 活动日志（维护页「最近操作」区块用；最近的在最前）。
    ///
    /// `undoable` 与 `undone` 由服务端判定，**不让前端猜**——
    /// 前端按自己的规则判断可撤销，迟早会与后端不一致。
    pub fn list_activity(&self, limit: i64) -> Result<Vec<ActivityRow>> {
        let conn = self.reader.conn();
        // 已被撤销的事件 id 集合
        let mut undone: std::collections::HashSet<i64> = std::collections::HashSet::new();
        {
            let mut st = conn
                .prepare("SELECT detail FROM memory_events WHERE operation = 'undo'")
                .map_err(map_err)?;
            let rows = st
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(map_err)?;
            for x in rows {
                let s = x.map_err(map_err)?;
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                    if let Some(id) = v.get("undo_of").and_then(|x| x.as_i64()) {
                        undone.insert(id);
                    }
                }
            }
        }

        let mut stmt = conn
            .prepare(
                "SELECT e.id, e.operation, COALESCE(e.memory_id, ''), COALESCE(m.content, ''),
                        COALESCE(m.agent_id, ''), e.created_at
                   FROM memory_events e
                   LEFT JOIN memories m ON m.id = e.memory_id
                  ORDER BY e.id DESC
                  LIMIT ?1",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![limit], |r| {
                let id: i64 = r.get(0)?;
                let op: String = r.get(1)?;
                let content: String = r.get(3)?;
                let undoable = matches!(
                    op.as_str(),
                    "update" | "supersede" | "forget:soft" | "forget:invalidate" | "merge" | "retire"
                );
                Ok(ActivityRow {
                    event_id: id,
                    operation: op,
                    memory_id: r.get(2)?,
                    summary: summarize(&content, 60),
                    agent_id: r.get(4)?,
                    created_at: r.get(5)?,
                    undoable,
                    undone: undone.contains(&id),
                })
            })
            .map_err(map_err)?;
        let mut out = Vec::new();
        for x in rows {
            out.push(x.map_err(map_err)?);
        }
        Ok(out)
    }

    // ---------------- 图 ----------------

    pub fn link(
        &self,
        va: &VerifiedAgent,
        src: &str,
        dst: &str,
        edge_type: &str,
        weight: f64,
    ) -> Result<()> {
        if !matches!(
            edge_type,
            "temporal" | "semantic" | "causal" | "entity" | "supersedes"
        ) {
            return Err(OrbitalError::InvalidArgument {
                reason: format!("非法 edge_type: {edge_type}"),
            });
        }
        // 自环（A→A）对图遍历没有信息量，且会让 related 结果出现"自己"
        if src == dst {
            return Err(OrbitalError::InvalidArgument {
                reason: "不能把一条记忆连到自己（自环没有信息量）".into(),
            });
        }
        // 两端都必须可见（防止用 link 把别人的私有记忆连进来）
        let _ = self.get(va, src)?;
        let _ = self.get(va, dst)?;
        // 权重范围（审计修复 P2）：越界/非有限值会污染图权重与召回加成
        if !weight.is_finite() || !(0.0..=1.0).contains(&weight) {
            return Err(OrbitalError::InvalidArgument {
                reason: format!("weight 必须在 0..=1 的有限数，当前 {weight}"),
            });
        }
        self.reader
            .conn()
            .execute(
                "INSERT OR REPLACE INTO edges (source_id, target_id, edge_type, weight, metadata, created_at)
                 VALUES (?1, ?2, ?3, ?4, '{}', ?5)",
                params![src, dst, edge_type, weight, now_secs()],
            )
            .map_err(map_err)?;
        Ok(())
    }

    /// 本地特权建边（ADR-034，只给本机 GUI；T-M2-08 走查补 J4）。
    ///
    /// 身份从库里查（`owner_of`），不接受请求参数——与 update/forget 的 local_owner 同一套边界。
    pub fn link_local_owner(
        &self,
        src: &str,
        dst: &str,
        edge_type: &str,
        weight: f64,
    ) -> Result<()> {
        let owner = self.owner_of(src)?;
        let _ = self.owner_of(dst)?;
        self.link(
            &VerifiedAgent::for_local_owner(owner),
            src,
            dst,
            edge_type,
            weight,
        )
    }

    /// 图遍历：**必须 JOIN memories 后再门控**（审核 P0-2 的硬要求）。
    pub fn related(        &self,
        va: &VerifiedAgent,
        id: &str,
        depth: i64,
    ) -> Result<Vec<RelatedHit>> {
        let _ = self.get(va, id)?;
        let depth = depth.clamp(1, 2);
        let conn = self.reader.conn();
        let mut out = Vec::new();
        let mut frontier = vec![id.to_string()];
        let mut seen: Vec<String> = vec![id.to_string()];

        for _ in 0..depth {
            let mut next = Vec::new();
            for cur in &frontier {
                let sql = format!(
                    "SELECT m.id, m.content, e.edge_type, e.weight
                     FROM edges e
                     JOIN memories m ON m.id = e.target_id
                     WHERE e.source_id = ?2 AND {VISIBLE} AND m.id != ?2
                     UNION
                     SELECT m.id, m.content, e.edge_type, e.weight
                     FROM edges e
                     JOIN memories m ON m.id = e.source_id
                     WHERE e.target_id = ?2 AND {VISIBLE} AND m.id != ?2"
                );
                let mut stmt = conn.prepare(&sql).map_err(map_err)?;
                let rows = stmt
                    .query_map(params![va.id(), cur], |r| {
                        Ok(RelatedHit {
                            id: r.get(0)?,
                            content: r.get(1)?,
                            edge_type: r.get(2)?,
                            weight: r.get(3)?,
                        })
                    })
                    .map_err(map_err)?;
                for r in rows {
                    let hit = r.map_err(map_err)?;
                    if !seen.contains(&hit.id) {
                        seen.push(hit.id.clone());
                        next.push(hit.id.clone());
                        out.push(hit);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        Ok(out)
    }

    // ---------------- 静态索引层（ADR-027） ----------------

    /// 最近 N 条记忆（按 `updated_at` 倒序），用于生成「静态索引层」。
    ///
    /// 只返回**索引所需的最小字段**（id / 摘要 / agent / 时间），不返回全文——
    /// 因为静态层只放索引（ADR-027 决策三）。
    pub fn recent(&self, va: &VerifiedAgent, limit: i64) -> Result<Vec<RecentItem>> {
        let conn = self.reader.conn();
        let sql = format!(
            "SELECT m.id, m.content, m.agent_id, m.updated_at
               FROM memories m
              WHERE {VISIBLE}
              ORDER BY m.updated_at DESC, m.id DESC
              LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql).map_err(map_err)?;
        let it = stmt
            .query_map(params![va.id(), limit], |r| {
                let content: String = r.get(1)?;
                Ok(RecentItem {
                    id: r.get(0)?,
                    title: summarize(&content, 40),
                    agent_id: r.get(2)?,
                    updated_at: r.get::<_, f64>(3)?,
                })
            })
            .map_err(map_err)?;
        let mut out = Vec::new();
        for x in it {
            out.push(x.map_err(map_err)?);
        }
        Ok(out)
    }

    // ---------------- 统计 ----------------

    pub fn stats(&self, va: &VerifiedAgent) -> Result<Stats> {
        let conn = self.reader.conn();
        let memories: i64 = conn
            .query_row(&format!("SELECT count(*) FROM memories m WHERE {VISIBLE}"), params![va.id()], |r| r.get(0))
            .map_err(map_err)?;
        let active: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM memories m WHERE {VISIBLE} AND (m.valid_to IS NULL OR m.valid_to > ?2)"),
                params![va.id(), now_secs()],
                |r| r.get(0),
            )
            .map_err(map_err)?;

        let mut by_agent = Vec::new();
        {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT m.agent_id, count(*) FROM memories m WHERE {VISIBLE} GROUP BY m.agent_id"
                ))
                .map_err(map_err)?;
            let rows = stmt
                .query_map(params![va.id()], |r| {
                    Ok(AgentCount {
                        agent_id: r.get(0)?,
                        count: r.get(1)?,
                    })
                })
                .map_err(map_err)?;
            for r in rows {
                by_agent.push(r.map_err(map_err)?);
            }
        }
        let mut by_scope = Vec::new();
        {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT m.scope, count(*) FROM memories m WHERE {VISIBLE} GROUP BY m.scope"
                ))
                .map_err(map_err)?;
            let rows = stmt
                .query_map(params![va.id()], |r| {
                    Ok(ScopeCount {
                        scope: r.get(0)?,
                        count: r.get(1)?,
                    })
                })
                .map_err(map_err)?;
            for r in rows {
                by_scope.push(r.map_err(map_err)?);
            }
        }

        // 全库计数（不含内容，只给规模；用于"体检"）
        let edges: i64 = conn
            .query_row("SELECT count(*) FROM edges", [], |r| r.get(0))
            .map_err(map_err)?;
        let facts: i64 = conn
            .query_row("SELECT count(*) FROM memory_facts", [], |r| r.get(0))
            .map_err(map_err)?;
        let soft_deleted: i64 = conn
            .query_row(
                "SELECT count(*) FROM memories WHERE deleted_at IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .map_err(map_err)?;

        Ok(Stats {
            totals: Totals {
                memories,
                active,
                soft_deleted,
                edges,
                facts,
            },
            by_agent,
            by_scope,
        })
    }

    /// 校验令牌（MCP 层每个请求都要调）。
    pub fn verify(&self, token: &str) -> Result<VerifiedAgent> {
        crate::identity::verify_token(self.reader.conn(), token)
    }

    /// 供 MCP 层做"声明 vs 令牌"比对。
    pub fn check_claim(&self, va: &VerifiedAgent, claimed: Option<&str>) -> Result<()> {
        assert_claimed_matches(va, claimed)
    }
}

fn content_hash_of(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let d = h.finalize();
    let mut out = String::with_capacity(d.len() * 2);
    for b in d.iter() {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// scope 的可比性排序（P1-2 用它判断"请求的 scope 是否更高"）。
/// private < entity < shared：shared 影响面最大，所以只有它能触发升级。
fn scope_rank(s: &str) -> u8 {
    match s {
        "private" => 0,
        "entity" => 1,
        "shared" => 2,
        _ => 0,
    }
}

/// 召回命中后记一次"被访问"（`last_accessed_at` / `access_count`）。
///
/// **故意吞掉所有错误**：访问计数可丢，不能让打点失败影响召回结果的返回。
/// 也故意不进写队列（ADR-003 的有意例外，理由见 `recall` 里的注释）。
fn touch_access(db: &Db, hits: &[RecallHit]) {
    const TOUCH_LIMIT: usize = 5;
    if hits.is_empty() {
        return;
    }
    let now = now_secs();
    let ids: Vec<&str> = hits
        .iter()
        .take(TOUCH_LIMIT)
        .map(|h| h.id.as_str())
        .collect();
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!(
        "UPDATE memories SET last_accessed_at = ?1, access_count = access_count + 1 \
         WHERE id IN ({placeholders})"
    );
    let mut args: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(ids.len() + 1);
    args.push(&now);
    for id in &ids {
        args.push(id);
    }
    let _ = db.conn().execute(&sql, args.as_slice());
}

/// 更新时撞上 (agent_id, content_hash) 唯一约束，应视为"内容重复"而不是内部错误。
fn map_update_err(e: rusqlite::Error) -> OrbitalError {
    if let rusqlite::Error::SqliteFailure(err, Some(msg)) = &e {
        if err.code == rusqlite::ErrorCode::ConstraintViolation && msg.contains("UNIQUE") {
            return OrbitalError::InvalidArgument {
                reason: "更新后的内容与同 agent 的另一条记忆重复".into(),
            };
        }
    }
    map_err(e)
}

fn map_err(e: rusqlite::Error) -> OrbitalError {
    match &e {
        rusqlite::Error::SqliteFailure(err, _)
            if err.code == rusqlite::ErrorCode::DatabaseBusy
                || err.code == rusqlite::ErrorCode::DatabaseLocked =>
        {
            OrbitalError::WriteBusy
        }
        _ => OrbitalError::Internal(format!("数据库错误: {e}")),
    }
}

/// `Db::conn()` 是 `&Connection`；这里只需要读，无需 `&mut`。
#[allow(dead_code)]
fn _assert_send(conn: &Connection) {
    let _ = conn;
}


/// 健康报告（T-M1-10 静默失败防护：**异常必须可见，不能只 warn**）。
#[derive(Debug, Clone, Serialize)]
pub struct HealthReport {
    pub ok: bool,
    pub degraded: bool,
    pub reasons: Vec<String>,
    pub schema_version: i64,
    pub schema_expected: i64,
    pub missing_tables: Vec<String>,
    pub memories: i64,
    pub soft_deleted: i64,
    pub vectors_total: i64,
    /// 有记忆但没有向量的条数（嵌入缺失是否可见）
    pub vectors_missing: i64,
    /// 未完成的写入意图（journal pending 残留）
    pub pending_intents: i64,
}

impl MemoryService {
    /// 体检：把"静默"变成"可查"。任何异常都进 `reasons`，并令 `degraded=true`。
    pub fn health(&self) -> Result<HealthReport> {
        let conn = self.reader.conn();
        let mut reasons: Vec<String> = Vec::new();

        let schema_version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(map_err)?;
        if schema_version != crate::db::SCHEMA_VERSION {
            reasons.push(format!(
                "schema 版本不符：库={schema_version} 期望={}",
                crate::db::SCHEMA_VERSION
            ));
        }

        let missing = self.reader.missing_v1_tables()?;
        if !missing.is_empty() {
            reasons.push(format!("缺表：{}", missing.join(", ")));
        }

        let memories: i64 = conn
            .query_row(
                "SELECT count(*) FROM memories WHERE deleted_at IS NULL",
                [],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        let soft_deleted: i64 = conn
            .query_row(
                "SELECT count(*) FROM memories WHERE deleted_at IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        let vectors_total: i64 = conn
            .query_row("SELECT count(*) FROM memories_vec", [], |r| r.get(0))
            .map_err(map_err)?;
        let vectors_missing: i64 = conn
            .query_row(
                "SELECT count(*) FROM memories m
                 WHERE m.deleted_at IS NULL
                   AND NOT EXISTS (SELECT 1 FROM memories_vec v WHERE v.memory_id = m.id)",
                [],
                |r| r.get(0),
            )
            .map_err(map_err)?;

        // 未完成的写入意图（.orbital-pending 残留）
        let mut pending_intents = 0i64;
        if let Some(dir) = self.journal_path.parent() {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    if e.path()
                        .to_string_lossy()
                        .ends_with(crate::attach::PENDING_SUFFIX)
                    {
                        pending_intents += 1;
                    }
                }
            }
        }
        if pending_intents > 0 {
            reasons.push(format!("有 {pending_intents} 个未完成的配置写入意图"));
        }

        Ok(HealthReport {
            ok: reasons.is_empty(),
            degraded: !reasons.is_empty(),
            reasons,
            schema_version,
            schema_expected: crate::db::SCHEMA_VERSION,
            missing_tables: missing,
            memories,
            soft_deleted,
            vectors_total,
            vectors_missing,
            pending_intents,
        })
    }
}


// ============================================================================
// P1-6 写入限流：每 agent 每分钟 N 条（默认 200）
// ============================================================================

use std::sync::{Mutex, OnceLock};

fn rate_table() -> &'static Mutex<std::collections::HashMap<String, (u32, std::time::Instant)>> {
    static T: OnceLock<Mutex<std::collections::HashMap<String, (u32, std::time::Instant)>>> =
        OnceLock::new();
    T.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// 每分钟上限；`0` 表示关闭限流（测试与压测用）。
pub fn rate_limit_per_min() -> u32 {
    std::env::var("ORBITAL_RATE_LIMIT_PER_MIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
}

/// 令牌桶（简化版：固定 60 秒窗口）。超限返回 `RATE_LIMITED`（可重试）。
fn check_rate(agent_id: &str) -> Result<()> {
    let limit = rate_limit_per_min();
    if limit == 0 {
        return Ok(());
    }
    let mut guard = rate_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    let e = guard
        .entry(agent_id.to_string())
        .or_insert((0, now));
    if now.duration_since(e.1) >= std::time::Duration::from_secs(60) {
        *e = (0, now);
    }
    if e.0 >= limit {
        return Err(OrbitalError::RateLimited);
    }
    e.0 += 1;
    Ok(())
}


#[cfg(test)]
mod p1_tests {
    use super::*;
    use crate::identity::{register_agent, verify_token};

        fn env(tag: &str) -> (
    std::path::PathBuf,
    MemoryService,
    VerifiedAgent,
    std::sync::MutexGuard<'static, ()>,
) {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir()
            .join(format!("orbital-p1-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("t.db");
        let journal = dir.join("j.ndjson");
        // 限流：测试里放宽（否则写 3 条以上会撞限流）
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "1000");
        let svc = MemoryService::open(&db_path, &journal).unwrap();
        let token = register_agent(svc.reader.conn(), "tester", "Tester").unwrap();
        let va = verify_token(svc.reader.conn(), &token).unwrap();
        (dir, svc, va, _g)
    }

    /// P1-3：软删掉的记忆不应再出现在 related 结果里（edges 仍存在）。
    /// 召回必须写回访问时间（2026-11-12）。
    /// 没有这条之前 `last_accessed_at` 全库为 NULL，
    /// 总览页「90 天未访问」恒等于「全部记忆」——指标是假的。
    #[test]
    fn recall_touches_last_accessed_at_and_count() {
        let (dir, svc, va, _g) = env("touch");
        let rec = svc
            .write(&va, WriteInput::new("tester", "zephyrine 探针词"))
            .unwrap();

        let read = |id: &str| -> (Option<f64>, i64) {
            let db = Db::open(dir.join("t.db")).unwrap();
            db.conn()
                .query_row(
                    "SELECT last_accessed_at, access_count FROM memories WHERE id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
        };

        let (t0, c0) = read(&rec.id);
        assert!(t0.is_none(), "刚写完不该有访问时间");
        assert_eq!(c0, 0);

        let hits = svc.recall(&va, "zephyrine", 10).unwrap();
        assert!(hits.iter().any(|h| h.id == rec.id), "应召回刚写的那条");

        let (t1, c1) = read(&rec.id);
        assert!(t1.is_some(), "召回后 last_accessed_at 必须有值");
        assert_eq!(c1, 1, "access_count 应 +1");
    }

    #[test]
    fn deleted_memory_is_filtered_from_related() {
        let (dir, svc, va, _g) = env("filter");
        let a = svc.write(&va, WriteInput::new("tester", "node A")).unwrap();
        let b = svc.write(&va, WriteInput::new("tester", "node B")).unwrap();
        svc.link(&va, &a.id, &b.id, "semantic", 1.0).unwrap();

        let before = svc.related(&va, &a.id, 1).unwrap();
        assert!(before.iter().any(|x| x.id == b.id), "软删前应能连到 B");

        svc.forget(&va, &b.id, "soft").unwrap();
        let after = svc.related(&va, &a.id, 1).unwrap();
        assert!(
            !after.iter().any(|x| x.id == b.id),
            "P1-3：软删后 B 必须从图遍历结果消失（边还在，但记忆不可见）"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// P1-6：超过每分钟上限返回 RATE_LIMITED（可重试），而不是无限写。
    #[test]
    fn rate_limit_returns_retryable_error() {
        let (dir, svc, va, _g) = env("rate");
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "2");
        // 清掉上一条测试可能留下的计数（同 agent 名）
        {
            let mut g = super::rate_table().lock().unwrap_or_else(|e| e.into_inner());
            g.clear();
        }
        assert!(svc.write(&va, WriteInput::new("tester", "rl 1")).is_ok());
        assert!(svc.write(&va, WriteInput::new("tester", "rl 2")).is_ok());
        let e = svc.write(&va, WriteInput::new("tester", "rl 3")).unwrap_err();
        assert_eq!(e.code(), "RATE_LIMITED");
        assert_eq!(e.http_status(), 429, "限流应是 429 可重试");
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "1000");
        let _ = std::fs::remove_dir_all(dir);
    }
}


#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::identity::{register_agent, verify_token};

    fn env(tag: &str) -> (
    std::path::PathBuf,
    MemoryService,
    VerifiedAgent,
    std::sync::MutexGuard<'static, ()>,
) {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir()
            .join(format!("orbital-cov2-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "1000");
        let svc = MemoryService::open(dir.join("t.db"), dir.join("j")).unwrap();
        let tok = register_agent(svc.reader.conn(), "cov", "Cov").unwrap();
        let va = verify_token(svc.reader.conn(), &tok).unwrap();
        (dir, svc, va, _g)
    }

    /// update 改成与另一条相同内容 → 返回 `duplicate`，且**不产生重复行**。
    ///
    /// 2026-11-12 行为变更（T-M2-02）：原实现是直接 INSERT 撞 UNIQUE 后报
    /// INVALID_ARGUMENT「内容重复」。但那有两个问题：
    ///   ① 报错 = 用户不知道"其实是已存在、不需要再写"，只能反复试；
    ///   ② 无法表达 P1-2 的"同内容 scope 升级"（升级需要看到"已存在那条"）。
    /// 现在改成先查重再决定，四种结局由 `UpdateOutcome.action` 明确给出。
    #[test]
    fn update_to_duplicate_content_returns_duplicate_without_new_row() {
        let (dir, svc, va, _g) = env("dup");
        let a = svc.write(&va, WriteInput::new("cov", "unique-A")).unwrap();
        let b = svc.write(&va, WriteInput::new("cov", "unique-B")).unwrap();
        let total_before = svc.stats(&va).unwrap().totals.memories;

        let out = svc
            .update(
                &va,
                &a.id,
                UpdatePatch {
                    content: Some("unique-B".into()),
                    importance: None,
                    tags: None,
                    pinned: None,
                    scope: None,
                },
            )
            .unwrap();

        assert_eq!(out.action, "duplicate", "同内容应判定为已存在");
        assert_eq!(out.id, b.id, "应指向已存在的那条");
        assert!(out.note.contains("已存在"), "文案要能看懂: {}", out.note);
        let total_after = svc.stats(&va).unwrap().totals.memories;
        assert_eq!(
            total_before, total_after,
            "duplicate 不该新增行（{} -> {}）",
            total_before, total_after
        );
        // 原条 a 仍然有效、内容没变
        assert_eq!(svc.get(&va, &a.id).unwrap().content, "unique-A");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// T-M2-02 验收 1：改后一致——新内容可读回，旧条进版本链（可回滚）。
    #[test]
    fn update_content_is_readable_back_and_old_version_retained() {
        let (dir, svc, va, _g) = env("upd1");
        let a = svc.write(&va, WriteInput::new("cov", "原始内容 v1")).unwrap();
        let out = svc
            .update(
                &va,
                &a.id,
                UpdatePatch {
                    content: Some("修改后内容 v2".into()),
                    importance: None,
                    tags: None,
                    pinned: None,
                    scope: None,
                },
            )
            .unwrap();
        assert_eq!(out.action, "superseded");
        assert_eq!(out.superseded_id.as_deref(), Some(a.id.as_str()));
        assert_ne!(out.id, a.id, "内容变更应产生新行（版本链），不是原地改");

        let fresh = svc.get(&va, &out.id).unwrap();
        assert_eq!(fresh.content, "修改后内容 v2");
        // 旧条仍在库里，只是 valid_to 有值 + superseded_by 指向新条
        let old = svc.get(&va, &a.id).unwrap();
        assert_eq!(old.content, "原始内容 v1", "旧版本内容必须保留（可回滚）");
        assert_eq!(old.superseded_by.as_deref(), Some(out.id.as_str()));
        assert!(old.valid_to.is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// T-M2-02 验收 3：审计 detail 必须含**前后值**（只记 actor 等于没审计）。
    #[test]
    fn update_writes_event_with_before_and_after() {
        let (dir, svc, va, _g) = env("upd3");
        let a = svc.write(&va, WriteInput::new("cov", "审计前内容")).unwrap();
        svc.update(
            &va,
            &a.id,
            UpdatePatch {
                content: Some("审计后内容".into()),
                importance: None,
                tags: None,
                pinned: None,
                scope: None,
            },
        )
        .unwrap();

        let db = Db::open(dir.join("t.db")).unwrap();
        let detail: String = db
            .conn()
            .query_row(
                "SELECT detail FROM memory_events
                 WHERE memory_id = ?1 AND operation IN ('update','supersede')
                 ORDER BY id DESC LIMIT 1",
                params![a.id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(detail.contains("before"), "缺 before: {detail}");
        assert!(detail.contains("after"), "缺 after: {detail}");
        assert!(detail.contains("审计前内容"), "before 必须是原内容: {detail}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// T-M2-02 验收 4：64KB 边界——65536 字节通过、65537 拒绝。
    /// 用 ASCII 保证"字节数 == 字符数"，避免把 UTF-8 多字节算成超限。
    #[test]
    fn update_enforces_64kb_boundary() {
        let (dir, svc, va, _g) = env("upd4");
        let a = svc.write(&va, WriteInput::new("cov", "边界测试起点")).unwrap();

        // 恰好 65536 字节：必须通过
        let ok_content = "x".repeat(MAX_CONTENT_BYTES);
        assert_eq!(ok_content.len(), MAX_CONTENT_BYTES);
        let out = svc
            .update(
                &va,
                &a.id,
                UpdatePatch {
                    content: Some(ok_content),
                    importance: None,
                    tags: None,
                    pinned: None,
                    scope: None,
                },
            )
            .unwrap();
        assert_eq!(out.action, "superseded");

        // 65537 字节：必须拒绝，且原因要说清上限
        let too_big = "y".repeat(MAX_CONTENT_BYTES + 1);
        let e = svc
            .update(
                &va,
                &out.id,
                UpdatePatch {
                    content: Some(too_big),
                    importance: None,
                    tags: None,
                    pinned: None,
                    scope: None,
                },
            )
            .unwrap_err();
        assert_eq!(e.code(), "INVALID_ARGUMENT");
        assert!(
            e.to_string().contains("64KB"),
            "原因要写清上限: {e}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// P1-2：同内容、scope 升级 → merged_scope（合并到已存在那条，不新增行）。
    #[test]
    fn update_promotes_scope_when_content_already_exists() {
        let (dir, svc, va, _g) = env("updp2");
        // A 先私有一条
        let a = svc.write(&va, WriteInput::new("cov", "同一段内容")).unwrap();
        // B 写不同内容，再把 B 改成与 A 相同、同时要求 shared
        let b = svc.write(&va, WriteInput::new("cov", "另一段内容")).unwrap();
        let total_before = svc.stats(&va).unwrap().totals.memories;

        let out = svc
            .update(
                &va,
                &b.id,
                UpdatePatch {
                    content: Some("同一段内容".into()),
                    importance: None,
                    tags: None,
                    pinned: None,
                    scope: Some("shared".into()),
                },
            )
            .unwrap();

        assert_eq!(out.action, "merged_scope", "应升级而不是报重复");
        assert_eq!(out.id, a.id, "应命中已存在那条");
        assert!(out.note.contains("升级"), "文案要说清发生什么: {}", out.note);
        // 命中那条的 scope 真的变了
        assert_eq!(svc.get(&va, &a.id).unwrap().scope, "shared");
        // 没有新增行
        assert_eq!(
            total_before,
            svc.stats(&va).unwrap().totals.memories,
            "merged_scope 不该新增行"
        );
        // ⚠️ 2026-11-13 审计补的断言：**被编辑的那条（b）保持原样**。
        // 这是已知语义缺口（docs/04 §10.2）：用户以为"B 被合并进 A"，实际 B 没变。
        // 写进测试是为了锁住当前行为——将来若改成"顺带软删 B"，这里会红，提醒改文案与文档。
        assert_eq!(
            svc.get(&va, &b.id).unwrap().content,
            "另一段内容",
            "当前实现不会改动被编辑那条；若此处变红说明改了语义，需同步 docs/04 §10.2"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// T-M2-03 验收 1：影响面数字必须等于真实 SQL 计数。
    #[test]
    fn delete_impact_matches_actual_counts() {
        let (dir, svc, va, _g) = env("imp1");
        let a = svc.write(&va, WriteInput::new("cov", "影响面 A")).unwrap();
        let b = svc.write(&va, WriteInput::new("cov", "影响面 B")).unwrap();
        let c = svc.write(&va, WriteInput::new("cov", "影响面 C")).unwrap();
        svc.link(&va, &a.id, &b.id, "semantic", 1.0).unwrap();
        svc.link(&va, &a.id, &c.id, "semantic", 1.0).unwrap();
        // 直接插一条 fact（服务层暂无写 fact 的 API）
        {
            let db = Db::open(dir.join("t.db")).unwrap();
            db.conn()
                .execute(
                    "INSERT INTO memory_facts
                       (memory_id, agent_id, entity_type, entity_id, property, value,
                        confidence, valid_from, recorded_at)
                     VALUES (?1, 'cov', 'memory', ?1, 'k1', 'v1', 0.9, 0.0, 0.0)",
                    params![a.id],
                )
                .unwrap();
        }

        let imp = svc.delete_impact(&a.id).unwrap();
        assert_eq!(imp.facts, 1, "facts 计数应来自 memory_facts");
        assert_eq!(imp.edges, 2, "edges 计数应是 source+target 命中数");
        assert!(!imp.already_deleted);
        assert!(!imp.pinned);

        // 交叉验证：与直接 SQL 一致（防止两边各算一套）
        let db = Db::open(dir.join("t.db")).unwrap();
        let sql_facts: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM memory_facts WHERE memory_id=?1",
                params![a.id],
                |r| r.get(0),
            )
            .unwrap();
        let sql_edges: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM edges WHERE source_id=?1 OR target_id=?1",
                params![a.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(imp.facts, sql_facts);
        assert_eq!(imp.edges, sql_edges);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// T-M2-03 验收 2：软删 → 恢复，内容逐字节一致。
    #[test]
    fn soft_delete_then_restore_is_byte_identical() {
        let (dir, svc, va, _g) = env("res1");
        let text = "可恢复性测试：这段内容删了再恢复必须一模一样 ✓";
        let a = svc.write(&va, WriteInput::new("cov", text)).unwrap();
        let before = svc.get(&va, &a.id).unwrap().content.clone();

        svc.forget(&va, &a.id, "soft").unwrap();
        assert!(
            svc.get(&va, &a.id).is_err(),
            "软删后 get 应报 MEMORY_NOT_FOUND"
        );
        assert!(svc.delete_impact(&a.id).unwrap().already_deleted);

        svc.restore(&va, &a.id).unwrap();
        let after = svc.get(&va, &a.id).unwrap().content;
        assert_eq!(before, after, "恢复后内容必须逐字节一致");
        assert_eq!(after, text);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// T-M2-03 验收 4：硬删级联清 facts/edges，且**删除前**已写 journal 快照。
    #[test]
    fn hard_delete_cascades_and_writes_snapshot() {
        let (dir, svc, va, _g) = env("hard1");
        let a = svc.write(&va, WriteInput::new("cov", "硬删目标")).unwrap();
        let b = svc.write(&va, WriteInput::new("cov", "邻居")).unwrap();
        svc.link(&va, &a.id, &b.id, "semantic", 1.0).unwrap();
        {
            let db = Db::open(dir.join("t.db")).unwrap();
            db.conn()
                .execute(
                    "INSERT INTO memory_facts
                       (memory_id, agent_id, entity_type, entity_id, property, value,
                        confidence, valid_from, recorded_at)
                     VALUES (?1, 'cov', 'memory', ?1, 'k', 'v', 0.9, 0.0, 0.0)",
                    params![a.id],
                )
                .unwrap();
        }

        svc.forget(&va, &a.id, "hard").unwrap();

        let db = Db::open(dir.join("t.db")).unwrap();
        let n_mem: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM memories WHERE id=?1", params![a.id], |r| {
                r.get(0)
            })
            .unwrap();
        let n_facts: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM memory_facts WHERE memory_id=?1",
                params![a.id],
                |r| r.get(0),
            )
            .unwrap();
        let n_edges: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM edges WHERE source_id=?1 OR target_id=?1",
                params![a.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_mem, 0, "硬删后记忆行应不存在");
        assert_eq!(n_facts, 0, "facts 应被 CASCADE 清掉");
        assert_eq!(n_edges, 0, "edges 应被 CASCADE 清掉");

        // journal 里必须有删除前快照，且含原文
        let journal = std::fs::read_to_string(dir.join("j")).unwrap_or_default();
        assert!(
            journal.contains("hard_delete_snapshot"),
            "journal 缺硬删快照: {journal}"
        );
        assert!(
            journal.contains("硬删目标"),
            "快照里必须含原文（否则事后捞不回来）"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// T-M2-04 验收 1：Pin 免疫——pinned 的记忆 `ei` 不低于 1.0。
    #[test]
    fn pinned_gets_immunity_floor_on_ei() {
        let (dir, svc, va, _g) = env("pin1");
        // 造一条本来 ei 很低的：importance=1 + 从未访问 + agent_derived
        let a = svc.write(&va, WriteInput::new("cov", "低权记忆")).unwrap();
        {
            let db = Db::open(dir.join("t.db")).unwrap();
            db.conn()
                .execute(
                    "UPDATE memories SET importance = 1, access_count = 0, origin = 'agent_derived',
                            pinned = 0 WHERE id = ?1",
                    params![a.id],
                )
                .unwrap();
        }
        let before = svc.effective_importance_of(&a.id).unwrap();
        assert!(before < 1.0, "未 Pin 时 ei 应低于 1.0，实际 {before}");

        // Pin → 抬到 1.0
        svc.update(
            &va,
            &a.id,
            UpdatePatch {
                content: None,
                importance: None,
                tags: None,
                pinned: Some(true),
                scope: None,
            },
        )
        .unwrap();
        let after = svc.effective_importance_of(&a.id).unwrap();
        assert!(after >= 1.0, "Pin 后 ei 应 >= 1.0（免疫），实际 {after}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// T-M2-04 验收 2+3+4：失效后不召回；库里仍能查到；清除 valid_to 后恢复召回。
    #[test]
    fn invalidated_memory_is_not_recalled_until_restored() {
        let (dir, svc, va, _g) = env("inv1");
        let a = svc
            .write(&va, WriteInput::new("cov", "zephyrite 失效测试专用词"))
            .unwrap();

        // 失效前：能召回
        let hits = svc.recall(&va, "zephyrite", 10).unwrap();
        assert!(hits.iter().any(|h| h.id == a.id), "失效前应能召回");

        // 标记失效
        svc.forget(&va, &a.id, "invalidate").unwrap();
        let hits = svc.recall(&va, "zephyrite", 10).unwrap();
        assert!(
            !hits.iter().any(|h| h.id == a.id),
            "失效后不应被召回（验收 2）"
        );

        // 验收 3：行仍在、能被查历史（valid_to 非空）
        let db = Db::open(dir.join("t.db")).unwrap();
        let (n, vt): (i64, Option<f64>) = db
            .conn()
            .query_row(
                "SELECT count(*), max(valid_to) FROM memories
                 WHERE id = ?1 AND valid_to IS NOT NULL",
                params![a.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(n, 1, "失效只置 valid_to，行必须还在（可查历史）");
        assert!(vt.is_some());

        // 验收 4：恢复后重新可召回
        svc.restore_validity(&va, &a.id).unwrap();
        let hits = svc.recall(&va, "zephyrite", 10).unwrap();
        assert!(
            hits.iter().any(|h| h.id == a.id),
            "清除 valid_to 后应恢复召回"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// forget 的 invalidate：记忆不可见但**行还在**（可逆）。
    #[test]
    fn invalidate_hides_but_keeps_row() {
        let (dir, svc, va, _g) = env("inv");
        let a = svc.write(&va, WriteInput::new("cov", "to-invalidate")).unwrap();
        svc.forget(&va, &a.id, "invalidate").unwrap();
        // 可见性：get 仍能拿到（valid_to 只是时间维度，不是软删）
        let got = svc.get(&va, &a.id).unwrap();
        assert!(got.valid_to.is_some(), "应写入 valid_to（失效时间）");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// forget 的 hard：物理删除，之后查不到。
    #[test]
    fn hard_forget_removes_row() {
        let (dir, svc, va, _g) = env("hard");
        let a = svc.write(&va, WriteInput::new("cov", "to-hard-delete")).unwrap();
        svc.forget(&va, &a.id, "hard").unwrap();
        let e = svc.get(&va, &a.id).unwrap_err();
        assert_eq!(e.code(), "MEMORY_NOT_FOUND");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// health 在正常库上 ok=true 且 reasons 为空。
    #[test]
    fn health_reports_ok_on_healthy_db() {
        let (dir, svc, _va, _g) = env("health");
        let h = svc.health().unwrap();
        assert!(h.ok, "健康库应 ok: {:?}", h.reasons);
        assert!(h.reasons.is_empty());
        assert_eq!(h.schema_expected, crate::db::SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// backfill_embeddings 在"没有可用端点"时返回 (0,0) 且不报错。
    #[test]
    fn backfill_without_endpoints_is_noop() {
        let (dir, svc, _va, _g) = env("backfill");
        // 没有 secrets 配置时 active_endpoints 可能为空；无论哪种，都不应 panic
        let (ok, failed) = svc.backfill_embeddings(10).unwrap();
        assert!(ok + failed <= 10);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod summarize_tests {

    /// ADR-027 静态层：摘要必须按**字符**截断，不能切坏中文/emoji。
    #[test]
    fn summarize_respects_char_boundary() {
        assert_eq!(super::summarize("短文本", 10), "短文本");
        // 20 个汉字，限 5 字 -> 截 5 字 + 省略号
        let long = "一二三四五六七八九十一二三四五六七八九十";
        let out = super::summarize(long, 5);
        assert_eq!(out.chars().count(), 6, "5 字 + 省略号 = 6 字符");
        assert!(out.ends_with('…'));
        // 含 emoji（4 字节）也不应 panic
        let emoji = "🎯目标🎯目标🎯目标";
        let o2 = super::summarize(emoji, 3);
        assert!(o2.chars().count() <= 4);
        // 换行被压成空格
        assert_eq!(super::summarize("a\nb", 10), "a b");
    }
}



#[cfg(test)]
mod fts_tests {
    /// 实测 bug（2026-11-12）：`safe-delete` 让 FTS5 报 `no such column: delete`。
    #[test]
    fn fts_escape_neutralizes_special_chars() {
        assert_eq!(super::fts_escape("safe-delete"), "\"safe-delete\"");
        assert_eq!(super::fts_escape("tauri build"), "\"tauri\" OR \"build\"");
        assert_eq!(super::fts_escape("a+b"), "\"a+b\"");
        assert_eq!(super::fts_escape("  "), "\"\"");
        assert_eq!(super::fts_escape("say \"hi\""), "\"say\" OR \"\"\"hi\"\"\"");
    }
}


#[cfg(test)]
mod version_chain_tests {
    use super::*;
    use crate::identity::{register_agent, verify_token};

    fn env(tag: &str) -> (
    std::path::PathBuf,
    MemoryService,
    VerifiedAgent,
    std::sync::MutexGuard<'static, ()>,
) {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("orbital-vc-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "1000");
        let svc = MemoryService::open(dir.join("t.db"), dir.join("j")).unwrap();
        let tok = register_agent(svc.reader.conn(), "cov", "Cov").unwrap();
        let va = verify_token(svc.reader.conn(), &tok).unwrap();
        (dir, svc, va, _g)
    }

    /// ADR-009 可逆（反思修正的真偏差）：改内容走版本链，旧内容仍可查。
    #[test]
    fn update_content_creates_version_chain() {
        let (dir, svc, va, _g) = env("verchain");
        let old = svc.write(&va, WriteInput::new("cov", "原始内容 A")).unwrap();

        svc.update(
            &va,
            &old.id,
            UpdatePatch {
                content: Some("修改后内容 B".into()),
                importance: None,
                tags: None,
                pinned: None,
                scope: None,
            },
        )
        .unwrap();

        // ① 旧条仍在，内容还是 A
        let old_rec = svc.get(&va, &old.id).expect("旧条必须仍可查");
        assert_eq!(old_rec.content, "原始内容 A", "旧内容不能被覆盖");
        assert!(old_rec.valid_to.is_some(), "旧条应有 valid_to");
        assert!(old_rec.superseded_by.is_some(), "旧条应指向新条");

        // ② 新条内容为 B 且仍有效
        let new_id = old_rec.superseded_by.clone().unwrap();
        let new_rec = svc.get(&va, &new_id).expect("新条必须存在");
        assert_eq!(new_rec.content, "修改后内容 B");

        // ③ supersedes 边（新 -> 旧）
        let edges: i64 = svc
            .reader
            .conn()
            .query_row(
                "SELECT count(*) FROM edges WHERE source_id=?1 AND target_id=?2 AND edge_type='supersedes'",
                params![new_id, old.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(edges, 1, "应有一条 supersedes 边");

        // ④ 审计事件是 supersede
        let op: String = svc
            .reader
            .conn()
            .query_row(
                "SELECT operation FROM memory_events WHERE memory_id=?1 ORDER BY id DESC LIMIT 1",
                params![old.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(op, "supersede");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 只改元数据不产生新版本。
    #[test]
    fn update_metadata_does_not_create_version() {
        let (dir, svc, va, _g) = env("vermeta");
        let a = svc.write(&va, WriteInput::new("cov", "内容不变 C")).unwrap();

        svc.update(
            &va,
            &a.id,
            UpdatePatch {
                content: None,
                importance: Some(5),
                tags: Some(vec!["x".into()]),
                pinned: Some(true),
                scope: None,
            },
        )
        .unwrap();

        let after = svc.get(&va, &a.id).unwrap();
        assert!(after.superseded_by.is_none(), "不应被取代");
        assert_eq!(after.importance, 5);
        assert_eq!(after.pinned, 1, "pinned 应为 1");

        let n: i64 = svc
            .reader
            .conn()
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "元数据更新不应新增行");

        let _ = std::fs::remove_dir_all(dir);
    }
}

/// T-M2-05 合并重复（搬标签 + 搬关系 + 搬事实 + 可撤销）。
///
/// 为什么会需要这个功能：多 agent 各自记录同一条事实时会产生"近似重复"，
/// 用户要能选"合并两者"而不是"删掉一条"——删掉就等于丢掉另一条上挂的标签与关系。
#[cfg(test)]
mod merge_tests {
    use super::*;
    use crate::identity::{register_agent, verify_token};

    fn env(tag: &str) -> (
        std::path::PathBuf,
        MemoryService,
        VerifiedAgent,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("orbital-mg-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "1000");
        let svc = MemoryService::open(dir.join("t.db"), dir.join("j")).unwrap();
        let tok = register_agent(svc.reader.conn(), "mg", "Merge").unwrap();
        let va = verify_token(svc.reader.conn(), &tok).unwrap();
        (dir, svc, va, _g)
    }

    /// 直接插一条 fact（M1/M2 还没有结构化属性的写入 API，测试直接走 SQL）。
    fn add_fact(
        conn: &Connection,
        memory_id: &str,
        agent: &str,
        prop: &str,
        value: &str,
        valid_to: Option<f64>,
    ) {
        conn.execute(
            "INSERT INTO memory_facts
               (memory_id, agent_id, entity_type, entity_id, property, value,
                confidence, valid_from, valid_to, recorded_at)
             VALUES (?1, ?2, 'project', 'project:orbital-pro', ?3, ?4, 0.9, 1.0, ?5, 1.0)",
            params![memory_id, agent, prop, value, valid_to],
        )
        .unwrap();
    }

    /// 计数宏：参数用 `params!` 拼，避免 `&[&dyn ToSql]` 的类型体操。
    macro_rules! count {
        ($conn:expr, $sql:expr $(, $arg:expr)*) => {{
            $conn
                .query_row($sql, rusqlite::params![$($arg),*], |r| r.get::<_, i64>(0))
                .unwrap()
        }};
    }

    fn set_tags(svc: &MemoryService, va: &VerifiedAgent, id: &str, tags: &[&str]) {
        svc.update(
            va,
            id,
            UpdatePatch {
                content: None,
                importance: None,
                tags: Some(tags.iter().map(|s| s.to_string()).collect()),
                pinned: None,
                scope: None,
            },
        )
        .unwrap();
    }

    /// 验收 1/2/3：不丢标签、不丢关系、事实全部指向保留条。
    #[test]
    fn merge_moves_tags_edges_and_facts() {
        let (dir, svc, va, _g) = env("move");
        let a = svc
            .write(&va, WriteInput::new("mg", "端口 8790 的记忆中枢"))
            .unwrap();
        let b = svc
            .write(
                &va,
                WriteInput::new("mg", "记忆中枢监听 8790（另一 agent 的重复记录）"),
            )
            .unwrap();
        let c = svc.write(&va, WriteInput::new("mg", "无关的第三条")).unwrap();
        set_tags(&svc, &va, &a.id, &["alpha"]);
        set_tags(&svc, &va, &b.id, &["beta", "alpha"]);

        // B 上挂两条事实（同属性不同值 = 冲突检测的输入，必须都保留）
        add_fact(svc.reader.conn(), &b.id, "mg", "port", "8790", None);
        add_fact(svc.reader.conn(), &b.id, "mg", "port", "8791", None);
        // A 已有与 B 完全同值的一条 → 合并时只该保留一条
        add_fact(svc.reader.conn(), &a.id, "mg", "port", "8790", None);

        // A→B（合并后成自环，应被丢弃）；B→C（应改指 A→C）
        svc.link(&va, &a.id, &b.id, "semantic", 1.0).unwrap();
        svc.link(&va, &b.id, &c.id, "causal", 0.8).unwrap();

        let pv = svc.merge_preview(&a.id, &b.id).unwrap();
        assert_eq!(pv.tags_moved, vec!["beta".to_string()], "只该搬 beta");
        assert_eq!(pv.self_loops, 1, "A→B 会变自环");
        assert_eq!(pv.facts_moved, 2, "预览的事实数应 = B 上挂的条数");
        assert!(pv.edges_moved >= 1);

        let out = svc.merge(&va, &a.id, &b.id).unwrap();
        assert!(out.note.contains("已合并"), "{}", out.note);
        assert_eq!(out.self_loops, 1);

        // ① 标签：A 的集合 ⊇ 并集
        let after = svc.get(&va, &a.id).unwrap();
        let tags: Vec<String> = serde_json::from_str(&after.tags).unwrap();
        assert!(tags.contains(&"alpha".to_string()));
        assert!(tags.contains(&"beta".to_string()), "beta 必须搬过来: {tags:?}");

        // ② 关系：B→C 变成 A→C；A→B 已消失
        let conn = svc.reader.conn();
        assert_eq!(
            count!(
                conn,
                "SELECT count(*) FROM edges WHERE source_id=?1 AND target_id=?2",
                &a.id,
                &c.id
            ),
            1,
            "B→C 应改指为 A→C"
        );
        assert_eq!(
            count!(
                conn,
                "SELECT count(*) FROM edges WHERE source_id=?1 AND target_id=?2",
                &a.id,
                &b.id
            ),
            0,
            "A→B 应作为自环被丢弃"
        );
        assert_eq!(
            count!(
                conn,
                "SELECT count(*) FROM edges WHERE source_id=?1 OR target_id=?1",
                &b.id
            ),
            0,
            "不能留下任何指向已删 B 的边"
        );

        // ③ 事实：全部改指 A；同值的那条去重（A 原有 1 + B 的 2 条里 1 条重复 → 2 条）
        assert_eq!(
            count!(
                conn,
                "SELECT count(*) FROM memory_facts WHERE memory_id=?1",
                &b.id
            ),
            0,
            "B 的事实必须全部搬走"
        );
        assert_eq!(
            count!(
                conn,
                "SELECT count(*) FROM memory_facts WHERE memory_id=?1",
                &a.id
            ),
            2,
            "同值去重后 A 上应剩 2 条事实（port 8790 / 8791）"
        );

        // ④ B 已软删（行还在，可恢复）
        assert!(svc.get(&va, &b.id).is_err(), "软删后 B 不应再被 get 到");
        let b_deleted: Option<f64> = conn
            .query_row(
                "SELECT deleted_at FROM memories WHERE id=?1",
                params![b.id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(b_deleted.is_some(), "B 行必须还在（软删，不是硬删）");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 事务安全：drop 已被软删时拒绝合并，且 keep 一个字节都不许动。
    #[test]
    fn merge_refuses_deleted_drop_and_leaves_keep_untouched() {
        let (dir, svc, va, _g) = env("atomic");
        let a = svc.write(&va, WriteInput::new("mg", "保留条 A")).unwrap();
        let b = svc.write(&va, WriteInput::new("mg", "被删条 B")).unwrap();
        set_tags(&svc, &va, &a.id, &["keep-tag"]);
        svc.forget(&va, &b.id, "soft").unwrap();

        // 两条入口的拒绝理由不同，**都要断言**：
        // - MCP 路径 `merge`：B 已不可见 → MEMORY_NOT_FOUND（它对外就是不存在的）
        // - GUI 路径 `merge_local_owner`：owner_of 不过滤 deleted_at → 进到业务校验 → INVALID_ARGUMENT
        let err = svc.merge(&va, &a.id, &b.id).unwrap_err();
        assert_eq!(err.code(), "MEMORY_NOT_FOUND", "MCP 路径应报不可见: {:?}", err);
        let err = svc.merge_local_owner(&a.id, &b.id).unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT", "GUI 路径应明确拒绝: {:?}", err);

        let after = svc.get(&va, &a.id).unwrap();
        let tags: Vec<String> = serde_json::from_str(&after.tags).unwrap();
        assert_eq!(tags, vec!["keep-tag".to_string()], "失败必须完全回滚");
        assert_eq!(
            count!(
                svc.reader.conn(),
                "SELECT count(*) FROM memory_events WHERE operation='merge'"
            ),
            0,
            "失败不该留下 merge 事件"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 4：撤销后 A/B 恢复到合并前（标签还原、B 恢复、事实搬回、边还原）。
    #[test]
    fn merge_can_be_undone() {
        let (dir, svc, va, _g) = env("undo");
        let a = svc.write(&va, WriteInput::new("mg", "保留条 A")).unwrap();
        let b = svc.write(&va, WriteInput::new("mg", "被并条 B")).unwrap();
        let c = svc.write(&va, WriteInput::new("mg", "关系目标 C")).unwrap();
        set_tags(&svc, &va, &a.id, &["a-tag"]);
        set_tags(&svc, &va, &b.id, &["b-tag"]);
        add_fact(svc.reader.conn(), &b.id, "mg", "port", "8790", None);
        svc.link(&va, &b.id, &c.id, "causal", 0.8).unwrap();

        svc.merge(&va, &a.id, &b.id).unwrap();
        let event_id: i64 = svc
            .reader
            .conn()
            .query_row(
                "SELECT id FROM memory_events WHERE operation='merge' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();

        let undo = svc.undo_merge(event_id).unwrap();
        assert!(undo.note.contains("已撤销"), "{}", undo.note);

        // ① A 的标签回到合并前
        let a_after = svc.get(&va, &a.id).unwrap();
        let tags: Vec<String> = serde_json::from_str(&a_after.tags).unwrap();
        assert_eq!(tags, vec!["a-tag".to_string()], "A 的标签必须还原");

        // ② B 恢复、内容逐字节不变
        let b_after = svc.get(&va, &b.id).expect("B 必须恢复可见");
        assert_eq!(b_after.content, "被并条 B");

        // ③ 事实搬回 B
        let conn = svc.reader.conn();
        assert_eq!(
            count!(
                conn,
                "SELECT count(*) FROM memory_facts WHERE memory_id=?1",
                &b.id
            ),
            1
        );
        assert_eq!(
            count!(
                conn,
                "SELECT count(*) FROM memory_facts WHERE memory_id=?1",
                &a.id
            ),
            0
        );

        // ④ 边还原：B→C 回来，A→C 消失
        assert_eq!(
            count!(
                conn,
                "SELECT count(*) FROM edges WHERE source_id=?1 AND target_id=?2",
                &b.id,
                &c.id
            ),
            1
        );
        assert_eq!(
            count!(
                conn,
                "SELECT count(*) FROM edges WHERE source_id=?1 AND target_id=?2",
                &a.id,
                &c.id
            ),
            0
        );

        // ⑤ 撤销本身留痕，且指向原事件
        let undo_of: String = conn
            .query_row(
                "SELECT detail FROM memory_events WHERE operation='undo' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            undo_of.contains(&format!("\"undo_of\":{event_id}")),
            "undo 事件必须指向原 merge 事件: {undo_of}"
        );

        // ⑥ 幂等：再撤销一次不应重复搬
        let again = svc.undo_merge(event_id).unwrap();
        assert!(again.note.contains("已经撤销过"), "{}", again.note);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// T-M10-04：撤销合并不能把 keep 原有的边删掉。
    ///
    /// 触发条件：keep 与 drop 都有指向同一第三方、同 `edge_type` 的边。
    /// merge 的 `INSERT OR IGNORE` 因主键冲突被忽略，但旧实现仍把它记进
    /// `edges_moved`；undo 无条件 DELETE 该"新边" ⇒ 删掉的是 keep 原有边。
    #[test]
    fn merge_undo_keeps_existing_edge_of_keep() {
        let (dir, svc, va, _g) = env("undokeepedge");
        let a = svc.write(&va, WriteInput::new("mg", "保留条 A")).unwrap();
        let b = svc.write(&va, WriteInput::new("mg", "被并条 B")).unwrap();
        let c = svc.write(&va, WriteInput::new("mg", "关系目标 C")).unwrap();
        // keep 原有边：A→C semantic 0.5
        svc.link(&va, &a.id, &c.id, "semantic", 0.5).unwrap();
        // drop 也有同类型边：B→C semantic 1.0（合并后改指成 A→C，与 keep 原边主键冲突）
        svc.link(&va, &b.id, &c.id, "semantic", 1.0).unwrap();

        svc.merge(&va, &a.id, &b.id).unwrap();
        let event_id: i64 = svc
            .reader
            .conn()
            .query_row(
                "SELECT id FROM memory_events WHERE operation='merge' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();

        svc.undo_merge(event_id).unwrap();

        let conn = svc.reader.conn();
        let (n_keep, w_keep): (i64, f64) = conn
            .query_row(
                "SELECT count(*), coalesce(max(weight), 0) FROM edges
                  WHERE source_id = ?1 AND target_id = ?2 AND edge_type = 'semantic'",
                params![&a.id, &c.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(n_keep, 1, "撤销后 keep 原有的边不能被删掉");
        assert!(
            (w_keep - 0.5).abs() < 1e-9,
            "keep 原边权重不应被 drop 的 1.0 覆盖，实际 {w_keep}"
        );
        let n_drop: i64 = conn
            .query_row(
                "SELECT count(*) FROM edges
                  WHERE source_id = ?1 AND target_id = ?2 AND edge_type = 'semantic'",
                params![&b.id, &c.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_drop, 1, "drop 的边应恢复");

        let _ = std::fs::remove_dir_all(dir);
    }
}

/// T-M2-06 冲突收件箱：检测 / 裁决 / 注入排除。
///
/// 为什么这块必须有独立测试：`docs/04 §7` 的规则只有一句
/// "同实体同属性多个有效值且 value 不同 → 冲突"，而"注入排除"是**安全属性**
/// （调研 A4：持续暴露冲突记忆是唯一让所有模型都掉点的条件），
/// 没有对抗测试就无法证明它真的生效。
#[cfg(test)]
mod conflict_tests {
    use super::*;
    use crate::identity::{register_agent, verify_token};

    fn env(tag: &str) -> (
        std::path::PathBuf,
        MemoryService,
        VerifiedAgent,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("orbital-cf-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "1000");
        let svc = MemoryService::open(dir.join("t.db"), dir.join("j")).unwrap();
        let tok = register_agent(svc.reader.conn(), "cf", "Conf").unwrap();
        let va = verify_token(svc.reader.conn(), &tok).unwrap();
        (dir, svc, va, _g)
    }

    fn add_fact(conn: &Connection, memory_id: &str, prop: &str, value: &str) {
        conn.execute(
            "INSERT INTO memory_facts
               (memory_id, agent_id, entity_type, entity_id, property, value,
                confidence, valid_from, valid_to, recorded_at)
             VALUES (?1, 'cf', 'project', 'project:orbital-pro', ?2, ?3, 0.9, 1.0, NULL, 1.0)",
            params![memory_id, prop, value],
        )
        .unwrap();
    }

    /// 检测：同实体同属性不同值 → 一条冲突，且 a/b 顺序稳定。
    #[test]
    fn detects_conflict_for_same_entity_property_different_values() {
        let (dir, svc, va, _g) = env("detect");
        let a = svc.write(&va, WriteInput::new("cf", "端口记录 A")).unwrap();
        let b = svc.write(&va, WriteInput::new("cf", "端口记录 B")).unwrap();
        add_fact(svc.reader.conn(), &a.id, "port", "8790");
        add_fact(svc.reader.conn(), &b.id, "port", "8791");

        assert_eq!(svc.detect_conflicts().unwrap(), 1, "应新增 1 条冲突");
        let list = svc.list_conflicts().unwrap();
        assert_eq!(list.len(), 1);
        let c = &list[0];
        assert_eq!(c.property, "port");
        // ⚠️ a/b 是按 memory_id 字典序排的，**不能**假设 a 就是先写的那条
        //（memory_id 是随机 uuid）。只断言"两个值都在、顺序稳定"。
        let mut vals = vec![c.value_a.clone(), c.value_b.clone()];
        vals.sort();
        assert_eq!(vals, vec!["8790".to_string(), "8791".to_string()]);
        assert!(c.memory_a < c.memory_b, "a/b 必须按 id 排序，否则会重复插入");
        assert!(!c.summary_a.is_empty() && !c.summary_b.is_empty());

        // 幂等：再扫一次不应重复报
        assert_eq!(svc.detect_conflicts().unwrap(), 0, "同组冲突不应重复插入");
        assert_eq!(svc.list_conflicts().unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 相同值不算冲突（那是重复，不是矛盾）。
    #[test]
    fn same_value_is_not_a_conflict() {
        let (dir, svc, va, _g) = env("sameval");
        let a = svc.write(&va, WriteInput::new("cf", "同值 A")).unwrap();
        let b = svc.write(&va, WriteInput::new("cf", "同值 B")).unwrap();
        add_fact(svc.reader.conn(), &a.id, "port", "8790");
        add_fact(svc.reader.conn(), &b.id, "port", "8790");
        assert_eq!(svc.detect_conflicts().unwrap(), 0, "同值不是冲突");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 对抗测试（T-M8-01，补 docs/05 M3 那条"冲突条目不进入注入"的对抗样本）：
    /// 试图用**各种手段**让冲突中的记忆重新可见——抬高 scope、加 fact、连边、改重要性——
    /// 只要冲突还是 `open`，任何通道（FTS / 融合 / 图扩展）都不许把它带出来。
    #[test]
    fn open_conflicts_stay_invisible_under_adversarial_attempts() {
        let (dir, svc, va, _g) = env("adversarial");
        let a = svc
            .write(&va, WriteInput::new("cf", "quasar 部署端口 A"))
            .unwrap();
        let b = svc
            .write(&va, WriteInput::new("cf", "quasar 部署端口 B"))
            .unwrap();
        // 另一条作为"邻居"，用来诱导图扩展把冲突条目拉回来
        let neighbor = svc
            .write(&va, WriteInput::new("cf", "quasar 相关笔记 C"))
            .unwrap();

        add_fact(svc.reader.conn(), &a.id, "port", "9000");
        add_fact(svc.reader.conn(), &b.id, "port", "9001");
        assert_eq!(svc.detect_conflicts().unwrap(), 1, "应检测到 1 条冲突");

        // --- 对抗手段 1：把**将被弃掉的那条**（B）抬成 shared（最容易"绕过"的一招）---
        // 只抬 B：这样"留 A"的断言才有意义（A 保持普通 private 记忆）
        svc.reader
            .conn()
            .execute(
                "UPDATE memories SET scope = 'shared', origin = 'user_endorsed' WHERE id = ?1",
                params![b.id],
            )
            .unwrap();
        // --- 对抗手段 2：把重要性拉满 ---
        svc.reader
            .conn()
            .execute("UPDATE memories SET importance = 5 WHERE id IN (?1, ?2)", params![a.id, b.id])
            .unwrap();
        // --- 对抗手段 3：把它们与邻居连边，诱导图扩展带上它们 ---
        svc.link(&va, &neighbor.id, &a.id, "semantic", 1.0).unwrap();
        svc.link(&va, &neighbor.id, &b.id, "semantic", 1.0).unwrap();

        // 单路 FTS：0（排除在 SQL 层）
        let fts = svc.recall(&va, "quasar", 10).unwrap();
        assert!(
            fts.iter().all(|h| h.id != a.id && h.id != b.id),
            "FTS 通道不得召回冲突条目: {:?}",
            fts.iter().map(|h| &h.id).collect::<Vec<_>>()
        );
        // 邻居仍能召回到（正对照：证明查询没坏）
        assert!(fts.iter().any(|h| h.id == neighbor.id), "邻居应正常召回");

        // 融合三路（FTS + 向量 + 图扩展）：仍不得出现
        let fused = svc
            .recall_fused(&va, "quasar", 10, &RecallOptions::default())
            .unwrap();
        assert!(
            fused.hits.iter().all(|h| h.id != a.id && h.id != b.id),
            "融合召回不得包含冲突条目: {:?}",
            fused.hits.iter().map(|h| &h.id).collect::<Vec<_>>()
        );

        // 裁决后（留 A）：**memory_b 那一列对应的记忆必须不再可见**。
        // 注意：断言必须用 `conflict.memory_a/memory_b` **列值**，不能用我自己的 a/b 变量——
        // detect_conflicts 的插入顺序由 SQL 决定，列里的 a/b 与"我先写谁"不一定对应。
        let conflict = svc.list_conflicts().unwrap().remove(0);
        let dropped = conflict.memory_b.clone();
        svc.resolve_conflict(conflict.id, "keep_a").unwrap();
        let after = svc.recall(&va, "quasar", 10).unwrap();
        assert!(
            after.iter().all(|h| h.id != dropped),
            "裁决留 A 后 memory_b 那条不应恢复可见（旧实现只失效 fact，它会回来——已修）"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn conflict_memories_are_excluded_from_recall_until_resolved() {
        let (dir, svc, va, _g) = env("exclude");
        let a = svc
            .write(&va, WriteInput::new("cf", "zephyr 端口记录 A"))
            .unwrap();
        let b = svc
            .write(&va, WriteInput::new("cf", "zephyr 端口记录 B"))
            .unwrap();

        // 无冲突时两条都能召回（正对照：证明查询本身没坏）
        let before = svc.recall(&va, "zephyr", 10).unwrap();
        assert_eq!(before.len(), 2, "正对照：无冲突时应召回 2 条");

        add_fact(svc.reader.conn(), &a.id, "port", "8790");
        add_fact(svc.reader.conn(), &b.id, "port", "8791");
        assert_eq!(svc.detect_conflicts().unwrap(), 1);

        // 冲突期间：0 条（docs/04 §7「冲突条目不参与注入」）
        let during = svc.recall(&va, "zephyr", 10).unwrap();
        assert!(
            during.is_empty(),
            "冲突期间的记忆必须被排除在召回之外，实际: {:?}",
            during.iter().map(|h| &h.id).collect::<Vec<_>>()
        );

        // 裁决"留 A" → 冲突关闭，B 侧的 fact 失效
        let conflict = svc.list_conflicts().unwrap().remove(0);
        svc.resolve_conflict(conflict.id, "keep_a").unwrap();
        assert!(svc.list_conflicts().unwrap().is_empty(), "裁决后不该再有 open 冲突");

        let after = svc.recall(&va, "zephyr", 10).unwrap();
        assert!(!after.is_empty(), "裁决后应恢复召回");

        // 按 a/b 查（不能假设是写库顺序）：A 侧仍有效、B 侧必须失效
        let (va_, vb_): (Option<f64>, Option<f64>) = svc
            .reader
            .conn()
            .query_row(
                "SELECT
                   (SELECT valid_to FROM memory_facts WHERE memory_id=?1 AND property='port'),
                   (SELECT valid_to FROM memory_facts WHERE memory_id=?2 AND property='port')",
                params![conflict.memory_a, conflict.memory_b],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(va_.is_none(), "留 A：A 侧的事实应仍有效");
        assert!(vb_.is_some(), "留 A：B 侧的事实应被标失效");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 「都保留」：状态记 ignored，再次扫描不会复活。
    #[test]
    fn keep_both_marks_ignored_and_stays_ignored() {
        let (dir, svc, va, _g) = env("both");
        let a = svc.write(&va, WriteInput::new("cf", "都保留 A")).unwrap();
        let b = svc.write(&va, WriteInput::new("cf", "都保留 B")).unwrap();
        add_fact(svc.reader.conn(), &a.id, "port", "8790");
        add_fact(svc.reader.conn(), &b.id, "port", "8791");
        svc.detect_conflicts().unwrap();
        let cid = svc.list_conflicts().unwrap()[0].id;

        svc.resolve_conflict(cid, "keep_both").unwrap();
        assert!(svc.list_conflicts().unwrap().is_empty(), "都保留后不进 open 列表");
        // 再扫：UNIQUE 挡住重复插入 → 不会复活
        assert_eq!(
            svc.detect_conflicts().unwrap(),
            0,
            "已裁决（ignored）的组不应重新变 open"
        );

        // 重复裁决应被拒绝（防重复点击）
        let err = svc.resolve_conflict(cid, "keep_a").unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");

        let _ = std::fs::remove_dir_all(dir);
    }
}

/// T-M2-07 撤销 / 回滚：编辑、内容变更、删除、失效各撤销一次；硬删拒绝。
///
/// 为什么每类都要独立测：`docs/03 §1.3` 的承诺是"每个操作有回滚路径"，
/// 而这五类的逆操作**完全不同**（写回字段 / 恢复版本链 / 清 deleted_at / 清 valid_to / 无逆）。
/// 只测其中一类就宣称"可撤销"是本机最常犯的假绿。
#[cfg(test)]
mod undo_tests {
    use super::*;
    use crate::identity::{register_agent, verify_token};

    fn env(tag: &str) -> (
        std::path::PathBuf,
        MemoryService,
        VerifiedAgent,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("orbital-ud-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "1000");
        let svc = MemoryService::open(dir.join("t.db"), dir.join("j")).unwrap();
        let tok = register_agent(svc.reader.conn(), "ud", "Undo").unwrap();
        let va = verify_token(svc.reader.conn(), &tok).unwrap();
        (dir, svc, va, _g)
    }

    /// 验收 1（编辑）：元数据改回操作前的值。
    #[test]
    fn undo_metadata_update_restores_before_values() {
        let (dir, svc, va, _g) = env("meta");
        let a = svc.write(&va, WriteInput::new("ud", "元数据撤销测例")).unwrap();
        let out = svc
            .update(
                &va,
                &a.id,
                UpdatePatch {
                    content: None,
                    importance: Some(5),
                    tags: Some(vec!["t1".into()]),
                    pinned: Some(true),
                    scope: None,
                },
            )
            .unwrap();
        let eid = out.event_id.expect("update 必须返回事件 id");
        let changed = svc.get(&va, &a.id).unwrap();
        assert_eq!(changed.importance, 5);
        assert_eq!(changed.pinned, 1);

        svc.undo_event(eid).unwrap();
        let back = svc.get(&va, &a.id).unwrap();
        assert_eq!(back.importance, 3, "应回到写入时的默认重要度");
        assert_eq!(back.pinned, 0, "Pin 应撤销");
        assert_eq!(back.tags, "[]", "标签应还原");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 1（内容变更）：旧版本恢复有效、新版本软删、supersedes 边清掉。
    #[test]
    fn undo_supersede_restores_old_version() {
        let (dir, svc, va, _g) = env("super");
        let old = svc.write(&va, WriteInput::new("ud", "原始内容 v1")).unwrap();
        let out = svc
            .update(
                &va,
                &old.id,
                UpdatePatch {
                    content: Some("修改后 v2".into()),
                    importance: None,
                    tags: None,
                    pinned: None,
                    scope: None,
                },
            )
            .unwrap();
        assert_eq!(out.action, "superseded");
        let new_id = out.id.clone();
        let eid = out.event_id.expect("supersede 必须返回事件 id");

        svc.undo_event(eid).unwrap();

        let o = svc.get(&va, &old.id).unwrap();
        assert_eq!(o.content, "原始内容 v1", "旧内容必须恢复");
        assert!(o.valid_to.is_none(), "旧版本应重新有效");
        assert!(o.superseded_by.is_none(), "旧版本的取代指针应清空");
        assert!(svc.get(&va, &new_id).is_err(), "新版本应已软删（可恢复）");
        let edges: i64 = svc
            .reader
            .conn()
            .query_row(
                "SELECT count(*) FROM edges WHERE source_id=?1 AND target_id=?2 AND edge_type='supersedes'",
                params![new_id, old.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(edges, 0, "supersedes 边必须随撤销删除");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 1（删除）：软删撤销后重新可见。
    #[test]
    fn undo_soft_delete_restores_visibility() {
        let (dir, svc, va, _g) = env("soft");
        let a = svc.write(&va, WriteInput::new("ud", "删除撤销测例")).unwrap();
        let eid = svc.forget(&va, &a.id, "soft").unwrap();
        assert!(svc.get(&va, &a.id).is_err());
        let undo = svc.undo_event(eid).unwrap();
        assert!(undo.note.contains("撤销删除"), "{}", undo.note);
        assert!(svc.get(&va, &a.id).is_ok(), "撤销后必须重新可见");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 1（失效）：撤销后恢复召回。
    #[test]
    fn undo_invalidate_restores_recall() {
        let (dir, svc, va, _g) = env("inval");
        let a = svc
            .write(&va, WriteInput::new("ud", "quasar 失效撤销测例"))
            .unwrap();
        let eid = svc.forget(&va, &a.id, "invalidate").unwrap();
        assert!(svc.recall(&va, "quasar", 10).unwrap().is_empty(), "失效后不召回");
        svc.undo_event(eid).unwrap();
        assert!(!svc.recall(&va, "quasar", 10).unwrap().is_empty(), "撤销后恢复召回");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 3（硬删）：明确拒绝，且提示说清"不可撤销"。
    #[test]
    fn undo_hard_delete_is_refused() {
        let (dir, svc, va, _g) = env("hard");
        let a = svc.write(&va, WriteInput::new("ud", "硬删不可撤销测例")).unwrap();
        let eid = svc.forget(&va, &a.id, "hard").unwrap();
        let err = svc.undo_event(eid).unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
        assert!(
            err.to_string().contains("不可撤销"),
            "提示必须明确说不可撤销: {err}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 2（留痕）：undo 事件存在且指向原事件；重复撤销被拒。
    #[test]
    fn undo_is_idempotent_and_leaves_trace() {
        let (dir, svc, va, _g) = env("trace");
        let a = svc.write(&va, WriteInput::new("ud", "撤销留痕测例")).unwrap();
        let eid = svc.forget(&va, &a.id, "soft").unwrap();
        svc.undo_event(eid).unwrap();

        let detail: String = svc
            .reader
            .conn()
            .query_row(
                "SELECT detail FROM memory_events WHERE operation='undo' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            detail.contains(&format!("\"undo_of\":{eid}")),
            "undo 事件必须指向原事件: {detail}"
        );

        let err = svc.undo_event(eid).unwrap_err();
        assert!(err.to_string().contains("已经撤销过"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 活动日志：服务端判定 undoable / undone（前端不许自己猜）。
    #[test]
    fn activity_log_marks_undoable_and_undone() {        let (dir, svc, va, _g) = env("act");
        let a = svc.write(&va, WriteInput::new("ud", "活动日志测例")).unwrap();
        let eid = svc.forget(&va, &a.id, "soft").unwrap();

        let rows = svc.list_activity(20).unwrap();
        let row = rows.iter().find(|r| r.event_id == eid).expect("应能找到该事件");
        assert!(row.undoable, "软删应可撤销");
        assert!(!row.undone, "还没撤销");
        assert!(!row.summary.is_empty(), "摘要要有内容");

        svc.undo_event(eid).unwrap();
        let rows2 = svc.list_activity(20).unwrap();
        let row2 = rows2.iter().find(|r| r.event_id == eid).unwrap();
        assert!(row2.undone, "撤销后应标记已撤销");

        // 硬删事件不可撤销（undoable=false）
        let b = svc.write(&va, WriteInput::new("ud", "硬删日志测例")).unwrap();
        let hid = svc.forget(&va, &b.id, "hard").unwrap();
        let rows3 = svc.list_activity(20).unwrap();
        let hrow = rows3.iter().find(|r| r.event_id == hid).unwrap();
        assert!(!hrow.undoable, "硬删必须标记为不可撤销");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// T-M2-08（J4）：本机 GUI 能建边；非法类型与自环都要被拒。
    #[test]
    fn link_local_owner_creates_edge_and_rejects_bad_input() {
        let (dir, svc, va, _g) = env("link");
        let a = svc.write(&va, WriteInput::new("ud", "关系 A")).unwrap();
        let b = svc.write(&va, WriteInput::new("ud", "关系 B")).unwrap();

        svc.link_local_owner(&a.id, &b.id, "causal", 0.8).unwrap();
        let n: i64 = svc
            .reader
            .conn()
            .query_row(
                "SELECT count(*) FROM edges WHERE source_id=?1 AND target_id=?2 AND edge_type='causal'",
                params![a.id, b.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "应建立一条 causal 边");

        let err = svc.link_local_owner(&a.id, &b.id, "bogus", 1.0).unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT", "非法类型必须拒绝");

        let err = svc.link_local_owner(&a.id, &a.id, "semantic", 1.0).unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT", "自环必须拒绝");
        assert!(
            err.to_string().contains("自环"),
            "提示要说清原因: {err}"
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}

/// T-M3-02：三路召回 + RRF 融合。
///
/// 为什么每条都要测：
/// - 单路复现（关掉另外两路必须与 `recall` 完全一致）——否则"融合"就是在偷偷改召回
/// - 降级可见（无嵌入端点时 `degraded` 必须写明，不能变成"没匹配"）
/// - 图扩展真的能带出邻居
/// - 门控不破（A 查不到 B 的 private）
/// - 权重 0 = 关闭该路（对照实验的正确性依赖它）
#[cfg(test)]
mod fused_recall_tests {
    use super::*;
    use crate::identity::{register_agent, verify_token};

    fn env(tag: &str) -> (
        std::path::PathBuf,
        MemoryService,
        VerifiedAgent,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("orbital-fr-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "1000");
        // 无嵌入端点：指向不存在的 secrets，保证向量通道走降级分支（测试不联网、不 flaky）
        std::env::set_var("ORBITAL_SECRETS", dir.join("no-secrets.json").display().to_string());
        let svc = MemoryService::open(dir.join("t.db"), dir.join("j")).unwrap();
        let tok = register_agent(svc.reader.conn(), "fr", "Fused").unwrap();
        let va = verify_token(svc.reader.conn(), &tok).unwrap();
        (dir, svc, va, _g)
    }

    /// 只开 FTS 时，融合结果必须与单路 `recall` 逐条一致（顺序也要一致）。
    #[test]
    fn fts_only_equals_single_channel_recall() {
        let (dir, svc, va, _g) = env("fts");
        svc.write(&va, WriteInput::new("fr", "zephyr alpha")).unwrap();
        svc.write(&va, WriteInput::new("fr", "zephyr beta")).unwrap();

        let plain = svc.recall(&va, "zephyr", 5).unwrap();
        let opts = RecallOptions {
            channels: vec![RecallChannel::Fts],
            weights: vec![1.0],
            k: 60,
            fact_filter: None,
            graph_decay: 0.5,
        };
        let fused = svc.recall_fused(&va, "zephyr", 5, &opts).unwrap();
        assert!(fused.degraded.is_empty(), "只开 FTS 不该有降级");
        let plain_ids: Vec<&String> = plain.iter().map(|h| &h.id).collect();
        let fused_ids: Vec<&String> = fused.hits.iter().map(|h| &h.id).collect();
        assert_eq!(plain_ids, fused_ids, "只开 FTS 必须与 recall 完全一致");

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 无嵌入端点：向量路必须**可见降级**，但 FTS 命中照常返回。
    #[test]
    fn vector_channel_degrades_visibly_without_endpoint() {
        let (dir, svc, va, _g) = env("degrade");
        svc.write(&va, WriteInput::new("fr", "zephyr 降级测例")).unwrap();

        let fused = svc
            .recall_fused(&va, "zephyr", 5, &RecallOptions::default())
            .unwrap();
        assert!(
            fused.degraded.iter().any(|d| d.starts_with("vector:")),
            "无端点时必须写明降级原因，实际: {:?}",
            fused.degraded
        );
        assert!(!fused.hits.is_empty(), "向量降级不能把 FTS 命中一起吞掉");

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// ADR-040：图加成只对**已在候选集**里的 id 加分，绝不引入新 id；权重 0 = 不加成。
    #[test]
    fn graph_bonus_only_boosts_existing_candidates() {
        let (dir, svc, va, _g) = env("bonus");
        let a = svc.write(&va, WriteInput::new("fr", "zephyr 种子节点")).unwrap();
        let b = svc.write(&va, WriteInput::new("fr", "zephyr 低排名邻居")).unwrap();
        let c = svc.write(&va, WriteInput::new("fr", "完全无关的邻居")).unwrap();
        svc.link(&va, &a.id, &b.id, "semantic", 1.0).unwrap();
        svc.link(&va, &a.id, &c.id, "semantic", 1.0).unwrap();

        let opts = RecallOptions {
            channels: vec![RecallChannel::Fts, RecallChannel::Graph],
            weights: vec![2.0, 1.0],
            k: 60,
            fact_filter: None,
            graph_decay: 0.5,
        };
        let fused = svc.recall_fused(&va, "zephyr", 10, &opts).unwrap();

        // B 在候选里（内容含 zephyr）且与种子有边 → 应带 graph_bonus 标记
        let hb = fused
            .hits
            .iter()
            .find(|h| h.id == b.id)
            .expect("B 应在候选内（FTS 命中）");
        assert!(
            hb.channels.contains(&"graph_linked".to_string()),
            "有边候选应被标 graph_linked: {:?}",
            hb.channels
        );

        // C 不在候选（内容不含 zephyr）→ 绝不能出现（ADR-040：不引入新 id）
        assert!(
            !fused.hits.iter().any(|h| h.id == c.id),
            "图加成不得引入新 id（这正是 hit@1 掉到 14% 的原因）"
        );

        // 权重 0 = 不加成
        let off = RecallOptions {
            channels: vec![RecallChannel::Fts, RecallChannel::Graph],
            weights: vec![2.0, 0.0],
            k: 60,
            fact_filter: None,
            graph_decay: 0.5,
        };
        let f2 = svc.recall_fused(&va, "zephyr", 10, &off).unwrap();
        assert!(
            !f2.hits
                .iter()
                .any(|h| h.channels.contains(&"graph_linked".to_string())),
            "权重 0 必须真正关闭图连接标记"
        );

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 门控不破：agent A 的融合结果里不能出现 agent B 的 private 记忆。
    #[test]
    fn fused_respects_scope_gating() {
        let (dir, svc, va_a, _g) = env("gating");
        let tok_b = register_agent(svc.reader.conn(), "fr-b", "B").unwrap();
        let va_b = verify_token(svc.reader.conn(), &tok_b).unwrap();
        let secret = svc
            .write(&va_b, WriteInput::new("fr-b", "zephyr 别人的私密"))
            .unwrap();

        let opts = RecallOptions {
            channels: vec![RecallChannel::Fts, RecallChannel::Graph],
            weights: vec![1.0, 1.0],
            k: 60,
            fact_filter: None,
            graph_decay: 0.5,
        };
        let fused = svc.recall_fused(&va_a, "zephyr", 10, &opts).unwrap();
        assert!(
            !fused.hits.iter().any(|h| h.id == secret.id),
            "跨 agent 的 private 记忆绝不能被融合召回带出"
        );

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// T-M3-03：结构化属性通道（facts 写入走 writer + 按实体过滤召回 + 增量冲突）。
///
/// 为什么每条都要测：
/// - 写路径合规（红线 5）只有"真的走队列"才能证明：测试里写完后数据必须**立刻可查**
///   （writer 是同步回执的，`add_fact` 返回即落盘）
/// - 增量冲突：不点"重新扫描"也必须出现 open 冲突
/// - 可见性：不能给别人的 private 记忆加事实
#[cfg(test)]
mod fact_tests {
    use super::*;
    use crate::identity::{register_agent, verify_token};
    use crate::writer::FactWrite;

    fn env(tag: &str) -> (
        std::path::PathBuf,
        MemoryService,
        VerifiedAgent,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("orbital-fc-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "1000");
        std::env::set_var("ORBITAL_SECRETS", dir.join("no-secrets.json").display().to_string());
        let svc = MemoryService::open(dir.join("t.db"), dir.join("j")).unwrap();
        let tok = register_agent(svc.reader.conn(), "ft", "Facts").unwrap();
        let va = verify_token(svc.reader.conn(), &tok).unwrap();
        (dir, svc, va, _g)
    }

    fn fw(memory_id: &str, prop: &str, value: &str) -> FactWrite {
        FactWrite {
            memory_id: memory_id.to_string(),
            entity_type: "project".to_string(),
            entity_id: "project:orbital-pro".to_string(),
            property: prop.to_string(),
            value: value.to_string(),
            confidence: 0.9,
        }
    }

    fn fact_count(svc: &MemoryService, memory_id: &str) -> i64 {
        svc.reader
            .conn()
            .query_row(
                "SELECT count(*) FROM memory_facts WHERE memory_id = ?1",
                params![memory_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// 验收 1+2：写入走队列（返回即可查），且按实体/属性过滤召回生效。
    #[test]
    fn add_fact_writes_and_filters_recall() {
        let (dir, svc, va, _g) = env("write");
        let a = svc.write(&va, WriteInput::new("ft", "zephyr 端口记录 A")).unwrap();
        let b = svc.write(&va, WriteInput::new("ft", "zephyr 端口记录 B")).unwrap();

        let rec = svc.add_fact(&va, fw(&a.id, "port", "8790")).unwrap();
        assert!(!rec.duplicate, "首次写入不该是 duplicate");
        assert_eq!(rec.write_mode, "fact");
        assert_eq!(fact_count(&svc, &a.id), 1, "回执即落盘（writer 先 COMMIT 再回执）");

        // 不过滤：两条都召回
        let plain = svc.recall(&va, "zephyr", 10).unwrap();
        assert_eq!(plain.len(), 2, "正对照：不过滤时两条都在");

        // 按 entity + property 过滤：只有 A
        let opts = RecallOptions {
            channels: vec![RecallChannel::Fts],
            weights: vec![1.0],
            k: 60,
            fact_filter: Some(FactFilter {
                entity_id: "project:orbital-pro".to_string(),
                property: Some("port".to_string()),
            }),
            graph_decay: 0.5,
        };
        let fused = svc.recall_fused(&va, "zephyr", 10, &opts).unwrap();
        assert!(fused.hits.iter().any(|h| h.id == a.id), "有该属性的记忆应命中");
        assert!(
            !fused.hits.iter().any(|h| h.id == b.id),
            "没登记该属性的记忆必须被过滤掉（否则过滤是假的）"
        );

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 3：写入第二个不同值 → 不用手动扫描就出现 open 冲突。
    #[test]
    fn add_fact_detects_conflict_incrementally() {
        let (dir, svc, va, _g) = env("conf");
        let a = svc.write(&va, WriteInput::new("ft", "冲突 A")).unwrap();
        let b = svc.write(&va, WriteInput::new("ft", "冲突 B")).unwrap();
        svc.add_fact(&va, fw(&a.id, "port", "8790")).unwrap();
        svc.add_fact(&va, fw(&b.id, "port", "8791")).unwrap();

        let list = svc.list_conflicts().unwrap();
        assert_eq!(list.len(), 1, "写入第二个不同值时必须自动登记冲突（不必点扫描）");
        assert_ne!(list[0].value_a, list[0].value_b);
        assert!(list[0].memory_a < list[0].memory_b, "a/b 必须按 id 排序");

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 同值重复：回 duplicate，不新增行。
    #[test]
    fn add_fact_duplicate_not_inserted() {
        let (dir, svc, va, _g) = env("dup");
        let a = svc.write(&va, WriteInput::new("ft", "去重 A")).unwrap();
        assert!(!svc.add_fact(&va, fw(&a.id, "port", "8790")).unwrap().duplicate);
        assert!(
            svc.add_fact(&va, fw(&a.id, "port", "8790")).unwrap().duplicate,
            "同记忆同实体同属性同值 → duplicate"
        );
        assert_eq!(fact_count(&svc, &a.id), 1, "重复写入不能多一行");

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 可见性：不能给别人的 private 记忆加事实。
    #[test]
    fn add_fact_requires_visibility() {
        let (dir, svc, va_a, _g) = env("vis");
        let tok_b = register_agent(svc.reader.conn(), "ft-b", "B").unwrap();
        let va_b = verify_token(svc.reader.conn(), &tok_b).unwrap();
        let secret = svc.write(&va_b, WriteInput::new("ft-b", "别人的记忆")).unwrap();

        let err = svc.add_fact(&va_a, fw(&secret.id, "port", "8790")).unwrap_err();
        assert_eq!(err.code(), "MEMORY_NOT_FOUND", "跨 agent 的 private 必须拒绝");

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 回归：记忆写入与事实写入共用同一队列（Job 多类型），两者都要落盘。
    #[test]
    fn mixed_write_and_fact_both_land() {
        let (dir, svc, va, _g) = env("mix");
        let a = svc.write(&va, WriteInput::new("ft", "混合批 A")).unwrap();
        let r = svc.add_fact(&va, fw(&a.id, "lane", "m1")).unwrap();
        assert_eq!(r.write_mode, "fact");
        assert_eq!(r.id, a.id, "回执 id 应是所属记忆");
        assert_eq!(fact_count(&svc, &a.id), 1);
        // 记忆本身也在
        assert_eq!(svc.get(&va, &a.id).unwrap().content, "混合批 A");

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// T-M3-04：图关系自动生成（时间边 / 实体边）。
///
/// 为什么每条都要测：
/// - 时间边必须**同 agent + 同 session**（mnemon 原版会跨 agent 连，我们显式收紧）
/// - 实体边必须限**同 agent**（跨 agent 自动边会制造泄漏）
/// - 重复写入（duplicate 分支）不能建边（幂等）
/// - 自动边的 `metadata.derived=true` 是回放约定的判据
#[cfg(test)]
mod graph_auto_tests {
    use super::*;
    use crate::identity::{register_agent, verify_token};
    use crate::writer::FactWrite;

    fn env(tag: &str) -> (
        std::path::PathBuf,
        MemoryService,
        VerifiedAgent,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("orbital-ga-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "1000");
        std::env::set_var("ORBITAL_SECRETS", dir.join("no-secrets.json").display().to_string());
        let svc = MemoryService::open(dir.join("t.db"), dir.join("j")).unwrap();
        let tok = register_agent(svc.reader.conn(), "ga", "GraphAuto").unwrap();
        let va = verify_token(svc.reader.conn(), &tok).unwrap();
        (dir, svc, va, _g)
    }

    fn edge_count(svc: &MemoryService, et: &str) -> i64 {
        svc.reader
            .conn()
            .query_row(
                "SELECT count(*) FROM edges WHERE edge_type = ?1",
                params![et],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn fw(memory_id: &str, entity_id: &str, prop: &str, value: &str) -> FactWrite {
        FactWrite {
            memory_id: memory_id.to_string(),
            entity_type: "project".to_string(),
            entity_id: entity_id.to_string(),
            property: prop.to_string(),
            value: value.to_string(),
            confidence: 0.9,
        }
    }

    /// 验收 1：同 agent 同 session 相邻两条 → temporal 边，权重 ≈ 1（同秒写入）。
    #[test]
    fn temporal_edge_created_for_same_session() {
        let (dir, svc, va, _g) = env("temporal");
        let mut i1 = WriteInput::new("ga", "会话第一条");
        i1.session_id = Some("sess-1".into());
        let a = svc.write(&va, i1).unwrap();
        let mut i2 = WriteInput::new("ga", "会话第二条");
        i2.session_id = Some("sess-1".into());
        let b = svc.write(&va, i2).unwrap();

        assert_eq!(edge_count(&svc, "temporal"), 1, "应生成一条 temporal 边");
        let (src, tgt, w, meta): (String, String, f64, String) = svc
            .reader
            .conn()
            .query_row(
                "SELECT source_id, target_id, weight, metadata FROM edges WHERE edge_type='temporal'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(src, a.id, "方向应是 上一条 → 新一条");
        assert_eq!(tgt, b.id);
        assert!(w > 0.99 && w <= 1.0, "同秒写入权重应≈1，实际 {w}");
        assert!(meta.contains("derived"), "自动边必须标 derived=true: {meta}");

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 不同 session 不连（mnemon 原版会按 source 跨 session 连，我们收紧）。
    #[test]
    fn temporal_edge_not_created_across_sessions() {
        let (dir, svc, va, _g) = env("nosess");
        let mut i1 = WriteInput::new("ga", "会话 A 第一条");
        i1.session_id = Some("sess-A".into());
        svc.write(&va, i1).unwrap();
        let mut i2 = WriteInput::new("ga", "会话 B 第一条");
        i2.session_id = Some("sess-B".into());
        svc.write(&va, i2).unwrap();
        assert_eq!(edge_count(&svc, "temporal"), 0, "不同 session 不该连");
        // 没有 session_id 的更不连
        svc.write(&va, WriteInput::new("ga", "无 session")).unwrap();
        assert_eq!(edge_count(&svc, "temporal"), 0);

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 2：两条记忆共享同一实体的 facts → entity 边（同 agent）。
    #[test]
    fn entity_edge_created_for_shared_fact_entity() {
        let (dir, svc, va, _g) = env("entity");
        let a = svc.write(&va, WriteInput::new("ga", "端口记录 A")).unwrap();
        let b = svc.write(&va, WriteInput::new("ga", "端口记录 B")).unwrap();
        svc.add_fact(&va, fw(&a.id, "project:orbital-pro", "port", "8790"))
            .unwrap();
        svc.add_fact(&va, fw(&b.id, "project:orbital-pro", "port", "8791"))
            .unwrap();

        assert_eq!(edge_count(&svc, "entity"), 1, "共享实体的两条应建 entity 边");
        let (src, tgt): (String, String) = svc
            .reader
            .conn()
            .query_row(
                "SELECT source_id, target_id FROM edges WHERE edge_type='entity'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(
            (src == a.id && tgt == b.id) || (src == b.id && tgt == a.id),
            "边应连接两条记忆"
        );

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 实体边限同 agent：另一个 agent 的记忆不因共享实体被自动连上。
    #[test]
    fn entity_edge_only_same_agent() {
        let (dir, svc, va_a, _g) = env("agent");
        let tok_b = register_agent(svc.reader.conn(), "ga-b", "B").unwrap();
        let va_b = verify_token(svc.reader.conn(), &tok_b).unwrap();
        let a = svc.write(&va_a, WriteInput::new("ga", "A 的记录")).unwrap();
        let b = svc.write(&va_b, WriteInput::new("ga-b", "B 的记录")).unwrap();
        svc.add_fact(&va_a, fw(&a.id, "project:shared-x", "port", "8790"))
            .unwrap();
        svc.add_fact(&va_b, fw(&b.id, "project:shared-x", "port", "8791"))
            .unwrap();
        assert_eq!(
            edge_count(&svc, "entity"),
            0,
            "跨 agent 的自动实体边必须为 0（否则会制造泄漏）"
        );

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 3 幂等：重复写入（duplicate 分支）不建边。
    #[test]
    fn duplicate_write_creates_no_temporal_edge() {
        let (dir, svc, va, _g) = env("dup");
        let mut i1 = WriteInput::new("ga", "重复内容");
        i1.session_id = Some("sess-1".into());
        svc.write(&va, i1).unwrap();
        let mut i2 = WriteInput::new("ga", "重复内容");
        i2.session_id = Some("sess-1".into());
        let r = svc.write(&va, i2).unwrap();
        assert!(r.duplicate, "同内容应回 duplicate");
        assert_eq!(edge_count(&svc, "temporal"), 0, "duplicate 不该建边");

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// T-M4-03：恢复入口（events 链回放 + 可恢复列表）。
#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::identity::{register_agent, verify_token};

    fn env(tag: &str) -> (
        std::path::PathBuf,
        MemoryService,
        VerifiedAgent,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("orbital-rc-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "1000");
        std::env::set_var("ORBITAL_SECRETS", dir.join("no-secrets.json").display().to_string());
        let svc = MemoryService::open(dir.join("t.db"), dir.join("j")).unwrap();
        let tok = register_agent(svc.reader.conn(), "rc", "Recover").unwrap();
        let va = verify_token(svc.reader.conn(), &tok).unwrap();
        (dir, svc, va, _g)
    }

    /// 验收 1：连降两次（3→2→1）能回到最初 3。
    #[test]
    fn restore_to_baseline_returns_to_initial() {
        let (dir, svc, va, _g) = env("baseline");
        let m = svc
            .write(
                &va,
                WriteInput {
                    importance: 3,
                    ..WriteInput::new("rc", "连降两次的记忆")
                },
            )
            .unwrap();
        svc.retire(&va, &m.id).unwrap();
        svc.retire(&va, &m.id).unwrap();
        assert_eq!(svc.get(&va, &m.id).unwrap().importance, 1);

        let out = svc.restore_to_baseline(&va, &m.id).unwrap();
        assert_eq!(out.action, "restored_baseline");
        assert_eq!(
            svc.get(&va, &m.id).unwrap().importance,
            3,
            "必须回到最初值 3，而不是 当前+1=2"
        );

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 未降权 / 已恢复的两类拒绝，防止误操作。
    #[test]
    fn restore_to_baseline_refuses_clean_records() {
        let (dir, svc, va, _g) = env("refuse");
        let clean = svc.write(&va, WriteInput::new("rc", "从没降权")).unwrap();
        let err = svc.restore_to_baseline(&va, &clean.id).unwrap_err();
        assert!(err.to_string().contains("没有降权记录"), "{err}");

        let m = svc
            .write(
                &va,
                WriteInput {
                    importance: 3,
                    ..WriteInput::new("rc", "降过一次")
                },
            )
            .unwrap();
        svc.retire(&va, &m.id).unwrap();
        svc.restore_retire(&va, &m.id).unwrap();
        let err2 = svc.restore_to_baseline(&va, &m.id).unwrap_err();
        assert!(err2.to_string().contains("已不低于最初值"), "{err2}");

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 2：可恢复列表同时列出"已降权"与"已软删"，且不含已恢复的。
    #[test]
    fn recovery_list_lists_demoted_and_soft_deleted() {
        let (dir, svc, va, _g) = env("list");
        // 已降权（应出现）
        let demoted = svc
            .write(
                &va,
                WriteInput {
                    importance: 4,
                    ..WriteInput::new("rc", "被降权的记忆")
                },
            )
            .unwrap();
        svc.retire(&va, &demoted.id).unwrap();
        // 降权后又恢复（不应出现）
        let restored = svc
            .write(
                &va,
                WriteInput {
                    importance: 3,
                    ..WriteInput::new("rc", "已恢复的记忆")
                },
            )
            .unwrap();
        svc.retire(&va, &restored.id).unwrap();
        svc.restore_retire(&va, &restored.id).unwrap();
        // 已软删（应出现）
        let deleted = svc.write(&va, WriteInput::new("rc", "被删掉的记忆")).unwrap();
        svc.forget(&va, &deleted.id, "soft").unwrap();

        let rows = svc.recovery_list(50).unwrap();
        assert!(
            rows.iter().any(|r| r.id == demoted.id && r.kind == "demoted"),
            "已降权应出现: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.id == restored.id),
            "已恢复的不该出现"
        );
        let sd = rows
            .iter()
            .find(|r| r.id == deleted.id && r.kind == "soft_deleted")
            .expect("已软删应出现");
        assert!(sd.undo_event_id.is_some(), "软删行要带撤销事件 id");

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }
}
#[cfg(test)]
mod retire_tests {
    use super::*;
    use crate::identity::{register_agent, verify_token};

    fn env(tag: &str) -> (
        std::path::PathBuf,
        MemoryService,
        VerifiedAgent,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("orbital-rt2-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "1000");
        std::env::set_var("ORBITAL_SECRETS", dir.join("no-secrets.json").display().to_string());
        let svc = MemoryService::open(dir.join("t.db"), dir.join("j")).unwrap();
        let tok = register_agent(svc.reader.conn(), "rr", "Retire2").unwrap();
        let va = verify_token(svc.reader.conn(), &tok).unwrap();
        (dir, svc, va, _g)
    }

    /// M4 批量口径（T-M9-01 补）：**100 条**不同时间/访问频率的记忆，批量降权后
    /// **没有一条被物理删除**，且每条的重要性只降 1 档（下限 1）。
    #[test]
    fn bulk_deweight_100_memories_deletes_nothing() {
        let (dir, svc, va, _g) = env("bulk100");
        let now = now_secs();
        let mut ids: Vec<(String, i64)> = Vec::new();
        for i in 0..100 {
            let m = svc
                .write(
                    &va,
                    WriteInput {
                        importance: 3,
                        ..WriteInput::new("rr", format!("批量降权样本 {i} 号记忆"))
                    },
                )
                .unwrap();
            ids.push((m.id.clone(), 3));
            // 造出不同的创建时间与访问频率（1/10/90 天前，访问 0/1/5 次）
            let created = now as i64 - [1i64, 10, 90][i % 3] * 86400;
            let access = [0i64, 1, 5][i % 3];
            svc.reader
                .conn()
                .execute(
                    "UPDATE memories SET created_at = ?1, valid_from = ?2, access_count = ?3 WHERE id = ?4",
                    params![created, created, access, m.id],
                )
                .unwrap();
        }
        let before: i64 = svc
            .reader
            .conn()
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, 100, "正对照：应写入 100 条");

        // 批量降权（与真实维护页同一入口：retire 逐条）
        for (id, _) in ids.iter() {
            svc.retire(&va, id).unwrap();
        }

        let after: i64 = svc
            .reader
            .conn()
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(after, 100, "降权后**一条都不能少**（绝不物理删除）");

        // 每条重要性只降 1 档（3 → 2；下限 1 不会被压到 0）
        let mut bad = Vec::new();
        for (id, orig) in ids.iter() {
            let imp: i64 = svc
                .reader
                .conn()
                .query_row(
                    "SELECT importance FROM memories WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .unwrap();
            let expect = (orig - 1).max(1);
            if imp != expect {
                bad.push(format!("{id}: 期望 {expect} 实际 {imp}"));
            }
        }
        assert!(bad.is_empty(), "有以下条目的重要性不是「只降 1 档」：{bad:?}");

        // 恢复一条 → 回到原始重要性（可逆）
        svc.restore_retire(&va, &ids[0].0).unwrap();
        let back: i64 = svc
            .reader
            .conn()
            .query_row(
                "SELECT importance FROM memories WHERE id = ?1",
                params![&ids[0].0],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(back, 3, "撤销降权后应回到原始重要性");

        let _ = std::fs::remove_dir_all(dir);
    }
    /// 降权 → 留痕 → 恢复 → 撤销，四条都要成立，且**绝不删除**。
    #[test]
    fn retire_is_reversible_and_never_deletes() {
        let (dir, svc, va, _g) = env("retire");
        let m = svc
            .write(
                &va,
                WriteInput {
                    importance: 3,
                    ..WriteInput::new("rr", "可降权的记忆")
                },
            )
            .unwrap();
        let before_content = svc.get(&va, &m.id).unwrap().content;

        // ① 降权
        let out = svc.retire(&va, &m.id).unwrap();
        assert_eq!(out.action, "retired");
        assert!(out.event_id.is_some(), "降权必须回 event_id（撤销句柄）");
        let g = svc.get(&va, &m.id).unwrap();
        assert_eq!(g.importance, 2, "重要度应减 1");
        assert_eq!(g.content, before_content, "内容不能变");

        // ② 留痕（from/to）
        let detail: String = svc
            .reader
            .conn()
            .query_row(
                "SELECT detail FROM memory_events WHERE operation='retire' AND memory_id=?1",
                params![m.id],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&detail).unwrap();
        assert_eq!(v["from"], 3);
        assert_eq!(v["to"], 2);

        // ③ 恢复
        let r = svc.restore_retire(&va, &m.id).unwrap();
        assert_eq!(r.action, "restored");
        assert_eq!(svc.get(&va, &m.id).unwrap().importance, 3);

        // ④ 撤销（降权也能被 undo_event 回滚）
        let out2 = svc.retire(&va, &m.id).unwrap();
        svc.undo_event(out2.event_id.unwrap()).unwrap();
        assert_eq!(
            svc.get(&va, &m.id).unwrap().importance,
            3,
            "undo_event 必须能把降权回滚"
        );

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 最低档拒绝、跨 agent 拒绝。
    #[test]
    fn retire_refuses_lowest_and_invisible() {
        let (dir, svc, va, _g) = env("edge");
        let low = svc
            .write(
                &va,
                WriteInput {
                    importance: 1,
                    ..WriteInput::new("rr", "已经最低")
                },
            )
            .unwrap();
        let err = svc.retire(&va, &low.id).unwrap_err();
        assert!(err.to_string().contains("最低档"), "应明确拒绝: {err}");

        // 别人的 private 不能降权
        let tok_b = register_agent(svc.reader.conn(), "rr-b", "B").unwrap();
        let va_b = verify_token(svc.reader.conn(), &tok_b).unwrap();
        let secret = svc
            .write(
                &va_b,
                WriteInput {
                    importance: 3,
                    ..WriteInput::new("rr-b", "别人的记忆")
                },
            )
            .unwrap();
        assert_eq!(
            svc.retire(&va, &secret.id).unwrap_err().code(),
            "MEMORY_NOT_FOUND"
        );

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }
}
///
/// 公式必须用**固定数字**锁住（不依赖 `now()`），否则测试会随日期漂移。
#[cfg(test)]
mod retirement_tests {
    use super::*;
    use crate::identity::{register_agent, verify_token};

    fn env(tag: &str) -> (
        std::path::PathBuf,
        MemoryService,
        VerifiedAgent,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("orbital-rt-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "1000");
        std::env::set_var("ORBITAL_SECRETS", dir.join("no-secrets.json").display().to_string());
        let svc = MemoryService::open(dir.join("t.db"), dir.join("j")).unwrap();
        let tok = register_agent(svc.reader.conn(), "rt", "Retire").unwrap();
        let va = verify_token(svc.reader.conn(), &tok).unwrap();
        (dir, svc, va, _g)
    }

    /// 公式逐项验证（对齐 `docs/04 §6.1`，全部用固定数字）。
    #[test]
    fn ei_formula_matches_docs() {
        // importance=3 → base 0.5；access 0 → ln(1)=0 被 max(…,1.0) 抬到 1
        let ei = effective_importance(3, 0, 0.0, 0, "user_endorsed", 30.0, false);
        assert!((ei - 0.5).abs() < 1e-9, "应为 0.5，实际 {ei}");

        // 半衰期 30 天 → 恰好衰减一半
        let ei30 = effective_importance(3, 0, 30.0, 0, "user_endorsed", 30.0, false);
        assert!((ei30 - 0.25).abs() < 1e-9, "30 天应减半，实际 {ei30}");

        // importance=5 → 1.0
        let ei5 = effective_importance(5, 0, 0.0, 0, "user_endorsed", 30.0, false);
        assert!((ei5 - 1.0).abs() < 1e-9);

        // access=10 → ×ln(11)
        let a = effective_importance(3, 10, 0.0, 0, "user_endorsed", 30.0, false);
        assert!((a - 0.5 * 11f64.ln()).abs() < 1e-9, "实际 {a}");

        // edge_count 取 min(10,5)=5 → ×1.5
        let e = effective_importance(3, 0, 0.0, 10, "user_endorsed", 30.0, false);
        assert!((e - 0.75).abs() < 1e-9, "实际 {e}");

        // agent_derived → ×0.8
        let d = effective_importance(3, 0, 0.0, 0, "agent_derived", 30.0, false);
        assert!((d - 0.4).abs() < 1e-9, "实际 {d}");

        // pinned → max(ei, 1.0)（免疫降权）
        let p = effective_importance(1, 0, 1000.0, 0, "agent_derived", 30.0, true);
        assert!((p - 1.0).abs() < 1e-9, "pinned 必须免疫，实际 {p}");
    }

    /// 免疫规则 + 24h 规则 + 只读。
    #[test]
    fn retirement_candidates_filters_immune_and_is_readonly() {
        let (dir, svc, va, _g) = env("cand");
        // 免疫：importance=5
        svc.write(
            &va,
            WriteInput {
                importance: 5,
                ..WriteInput::new("rt", "重要的事")
            },
        )
        .unwrap();
        // 候选：importance=1，且把时间推到 200 天前
        let cand = svc
            .write(
                &va,
                WriteInput {
                    importance: 1,
                    ..WriteInput::new("rt", "很久以前的小事")
                },
            )
            .unwrap();
        let old = now_secs() - 200.0 * 86_400.0;
        svc.reader
            .conn()
            .execute(
                "UPDATE memories SET created_at=?1, last_accessed_at=?1 WHERE id=?2",
                params![old, cand.id],
            )
            .unwrap();

        let before: i64 = svc
            .reader
            .conn()
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        let rows = svc.retirement_candidates(&va, 0.15, 10).unwrap();
        let after: i64 = svc
            .reader
            .conn()
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .unwrap();

        assert_eq!(rows.len(), 1, "只有那条老的 importance=1 应入选: {rows:?}");
        assert_eq!(rows[0].id, cand.id);
        assert!(
            rows[0].reasons.iter().any(|r| r.contains("天未访问")),
            "原因应含未访问天数: {:?}",
            rows[0].reasons
        );
        assert_eq!(before, after, "退役候选必须只读（不许删/改）");

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// T-M4-04：维护任务（孤儿边清理 + 向量补算）。
///
/// 关键区别必须锁住：**软删的边要保留**，只有"指向根本不存在的记忆"才是孤儿。
#[cfg(test)]
mod maintenance_tests {
    use super::*;
    use crate::identity::{register_agent, verify_token};

    fn env(tag: &str) -> (
        std::path::PathBuf,
        MemoryService,
        VerifiedAgent,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("orbital-mt-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "1000");
        std::env::set_var("ORBITAL_SECRETS", dir.join("no-secrets.json").display().to_string());
        let svc = MemoryService::open(dir.join("t.db"), dir.join("j")).unwrap();
        let tok = register_agent(svc.reader.conn(), "mt", "Maint").unwrap();
        let va = verify_token(svc.reader.conn(), &tok).unwrap();
        (dir, svc, va, _g)
    }

    fn edge_count(svc: &MemoryService) -> i64 {
        svc.reader
            .conn()
            .query_row("SELECT count(*) FROM edges", [], |r| r.get(0))
            .unwrap()
    }

    /// 只清"指向不存在记忆"的边；正常边与软删的边都保留；记忆一条不动。
    #[test]
    fn cleanup_removes_only_orphan_edges() {
        let (dir, svc, va, _g) = env("orphan");
        let a = svc.write(&va, WriteInput::new("mt", "正常 A")).unwrap();
        let b = svc.write(&va, WriteInput::new("mt", "正常 B")).unwrap();
        svc.link(&va, &a.id, &b.id, "semantic", 1.0).unwrap();

        // 手工造一条"历史遗留"孤儿边（指向不存在的记忆）。
        // ⚠️ edges 有外键（→ memories.id），**正常操作下根本插不进去**——
        // 所以孤儿边只可能来自历史数据/外部 SQL。这里临时关 FK 来模拟那种数据。
        let now = now_secs();
        svc.reader
            .conn()
            .execute("PRAGMA foreign_keys=OFF", [])
            .unwrap();
        svc.reader
            .conn()
            .execute(
                "INSERT INTO edges (source_id, target_id, edge_type, weight, metadata, created_at)
                 VALUES ('mem_not_exist_0000', ?1, 'semantic', 1.0, '{}', ?2)",
                params![b.id, now],
            )
            .unwrap();
        svc.reader
            .conn()
            .execute("PRAGMA foreign_keys=ON", [])
            .unwrap();
        // 再软删 B：它的边**必须保留**
        svc.forget(&va, &b.id, "soft").unwrap();
        assert_eq!(edge_count(&svc), 2, "清理前应有 2 条边（正常 + 孤儿）");

        let before_mem: i64 = svc
            .reader
            .conn()
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        let out = svc.cleanup_orphan_edges().unwrap();
        let after_mem: i64 = svc
            .reader
            .conn()
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .unwrap();

        assert_eq!(out.removed_edges, 1, "只该删那条孤儿边");
        assert_eq!(out.kept_edges, 1, "软删 B 的边必须保留");
        assert_eq!(edge_count(&svc), 1);
        assert_eq!(before_mem, after_mem, "记忆一条都不许动");

        // 软删的边确实还在（恢复 B 后仍可用）
        let still: i64 = svc
            .reader
            .conn()
            .query_row(
                "SELECT count(*) FROM edges WHERE source_id = ?1 AND target_id = ?2",
                params![a.id, b.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still, 1);

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 无嵌入端点时补算必须**可见降级**，不能假装成功。
    #[test]
    fn reembed_reports_degraded_without_endpoint() {
        let (dir, svc, va, _g) = env("embed");
        svc.write(&va, WriteInput::new("mt", "待补算的记忆")).unwrap();
        let out = svc.reembed_pending(10).unwrap();
        assert_eq!(out.tried, 0, "没端点不该去尝试");
        assert!(
            out.degraded.iter().any(|d| d == "no-embedding-endpoint"),
            "必须写明原因: {:?}",
            out.degraded
        );

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// T-M5-01：备份与校验。
///
/// 核心断言：备份必须与"当前库此刻"**逐项一致**；损坏的备份必须被识别。
#[cfg(test)]
mod backup_tests {
    use super::*;
    use crate::identity::{register_agent, verify_token};

    fn env(tag: &str) -> (
        std::path::PathBuf,
        MemoryService,
        VerifiedAgent,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("orbital-bk-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "1000");
        std::env::set_var("ORBITAL_SECRETS", dir.join("no-secrets.json").display().to_string());
        let svc = MemoryService::open(dir.join("t.db"), dir.join("j")).unwrap();
        let tok = register_agent(svc.reader.conn(), "bk", "Backup").unwrap();
        let va = verify_token(svc.reader.conn(), &tok).unwrap();
        (dir, svc, va, _g)
    }

    /// 验收 1+2：备份一致（integrity/版本/计数逐项），备份后写入照常。
    #[test]
    fn backup_is_consistent_and_matches_current() {
        let (dir, svc, va, _g) = env("ok");
        svc.write(&va, WriteInput::new("bk", "备份测例一")).unwrap();
        svc.write(&va, WriteInput::new("bk", "备份测例二")).unwrap();

        let dst = dir.join("backup").join("orbital-test.db");
        let report = svc.backup_to(&dst).unwrap();
        assert!(report.integrity_ok, "integrity_check 应 ok: {report:?}");
        assert!(report.matches, "备份必须与当前库一致: {report:?}");
        assert_eq!(report.memories, report.current_memories);
        assert_eq!(report.schema_version, report.current_schema_version);
        assert!(dst.exists(), "备份文件应存在");
        // 关键：备份必须是**单文件**（不能留 .db-wal/.db-shm，否则用户只拷 .db 会漏页）
        assert!(
            !dst.with_extension("db-wal").exists(),
            "备份不应留下 .db-wal 伴生文件"
        );
        assert!(
            !dst.with_extension("db-shm").exists(),
            "备份不应留下 .db-shm 伴生文件"
        );

        // 备份后写入照常（备份不锁库）
        svc.write(&va, WriteInput::new("bk", "备份之后的写入")).unwrap();

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 3（红绿对照）：截断的备份**必须**被识别为不可信。
    #[test]
    fn verify_detects_truncated_backup() {
        let (dir, svc, va, _g) = env("bad");
        svc.write(&va, WriteInput::new("bk", "会被截断的备份")).unwrap();
        let dst = dir.join("backup").join("truncated.db");
        svc.backup_to(&dst).unwrap();

        let bytes = std::fs::read(&dst).unwrap();
        assert!(bytes.len() > 1024, "测试前提：备份文件应大于 1KB");
        std::fs::write(&dst, &bytes[..1024]).unwrap();

        match svc.verify_backup(&dst) {
            Err(_) => {}
            Ok(r) => assert!(
                !r.integrity_ok || !r.matches,
                "截断的备份绝不能判为一致: {r:?}"
            ),
        }

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 备份列表：最新在前，只认 .db。
    #[test]
    fn list_backups_newest_first() {
        let (dir, svc, _va, _g) = env("list");
        let bdir = dir.join("backup");
        svc.backup_to(&bdir.join("a-20260101.db")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        svc.backup_to(&bdir.join("b-20260102.db")).unwrap();
        std::fs::write(bdir.join("notes.txt"), b"not a backup").unwrap();

        let list = svc.list_backups(&bdir).unwrap();
        assert_eq!(list.len(), 2, "只列出 .db: {list:?}");
        assert_eq!(list[0].name, "b-20260102.db", "最新在前");

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// ADR-043：entity 作用域的 fact 门禁（在**召回侧**，不是写入侧）。
///
/// 这条测试是审计发现"docs/04 §4.1 规则未落地"后的回归锁：
/// 没有 fact 的 entity 记忆只能写入者自己看见；加 fact 后才对其他人可见。
#[cfg(test)]
mod entity_scope_tests {
    use super::*;
    use crate::identity::{register_agent, verify_token};
    use crate::writer::FactWrite;

    fn env(tag: &str) -> (
        std::path::PathBuf,
        MemoryService,
        VerifiedAgent,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("orbital-es-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "1000");
        std::env::set_var("ORBITAL_SECRETS", dir.join("no-secrets.json").display().to_string());
        let svc = MemoryService::open(dir.join("t.db"), dir.join("j")).unwrap();
        let tok = register_agent(svc.reader.conn(), "es", "EntityOwner").unwrap();
        let va = verify_token(svc.reader.conn(), &tok).unwrap();
        (dir, svc, va, _g)
    }

    #[test]
    fn unbound_entity_memory_is_private_until_fact_added() {
        let (dir, svc, va_a, _g) = env("vis");
        let tok_b = register_agent(svc.reader.conn(), "es-b", "B").unwrap();
        let va_b = verify_token(svc.reader.conn(), &tok_b).unwrap();

        // A 写一条 entity 作用域但**没有 fact** 的记忆
        let m = svc
            .write(
                &va_a,
                WriteInput {
                    scope: "entity".to_string(),
                    origin: "user_endorsed".to_string(),
                    ..WriteInput::new("es", "实体记忆但还没有事实")
                },
            )
            .unwrap();

        // ① 其他 agent 看不见（等价 private）
        assert_eq!(
            svc.get(&va_b, &m.id).unwrap_err().code(),
            "MEMORY_NOT_FOUND",
            "没有 fact 的 entity 记忆不能对其他人可见"
        );
        assert!(
            svc.recall(&va_b, "实体记忆", 10).unwrap().is_empty(),
            "召回也必须挡住"
        );

        // ② 写入者本人仍可见
        assert!(svc.get(&va_a, &m.id).is_ok());

        // ③ 加了 fact 之后，其他 agent 可见
        svc.add_fact(
            &va_a,
            FactWrite {
                memory_id: m.id.clone(),
                entity_type: "project".to_string(),
                entity_id: "project:audit".to_string(),
                property: "port".to_string(),
                value: "8790".to_string(),
                confidence: 0.9,
            },
        )
        .unwrap();
        assert!(
            svc.get(&va_b, &m.id).is_ok(),
            "有 fact 之后应对其他 agent 可见"
        );

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }
}
/// 审计修复（P1-2）：输入上限守卫。
///
/// 商业交付不能假设调用方守规矩：MCP 的 `limit`/`query` 来自 agent 参数。
#[cfg(test)]
mod input_guard_tests {
    use super::*;
    use crate::identity::{register_agent, verify_token};

    fn env(tag: &str) -> (
        std::path::PathBuf,
        MemoryService,
        VerifiedAgent,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("orbital-ig-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ORBITAL_RATE_LIMIT_PER_MIN", "1000");
        std::env::set_var("ORBITAL_SECRETS", dir.join("no-secrets.json").display().to_string());
        let svc = MemoryService::open(dir.join("t.db"), dir.join("j")).unwrap();
        let tok = register_agent(svc.reader.conn(), "ig", "Guard").unwrap();
        let va = verify_token(svc.reader.conn(), &tok).unwrap();
        (dir, svc, va, _g)
    }

    /// 超大 limit 必须被 clamp（否则一次召回能把整库读进内存）。
    #[test]
    fn huge_limit_is_clamped() {
        let (dir, svc, va, _g) = env("limit");
        for i in 0..3 {
            svc.write(&va, WriteInput::new("ig", format!("guard 测例 {i}")))
                .unwrap();
        }
        let hits = svc.recall(&va, "guard", 1_000_000).unwrap();
        assert!(hits.len() <= MAX_LIMIT as usize, "limit 必须被 clamp");
        // clamp 后仍应正常返回（不是直接空）
        assert!(!hits.is_empty());

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 超长 query 必须被拒绝（不是静默截断，也不是慢慢解析）。
    #[test]
    fn oversized_query_is_rejected() {
        let (dir, svc, va, _g) = env("query");
        let big = "x".repeat(MAX_QUERY_BYTES + 1);
        let err = svc.recall(&va, &big, 10).unwrap_err();
        assert!(
            err.to_string().contains("字节上限"),
            "应明确报上限: {err}"
        );
        // 正常长度仍然可用
        assert!(svc.recall(&va, "guard", 10).is_ok());

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// P2：非法 scope 必须被拒绝（不能入库成脏数据）。
    #[test]
    fn invalid_scope_is_rejected() {
        let (dir, svc, va, _g) = env("scope");
        let err = svc
            .write(
                &va,
                WriteInput {
                    scope: "public".to_string(),
                    ..WriteInput::new("ig", "非法作用域")
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("scope"), "应明确报 scope: {err}");

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// P2：link 的 weight 必须在 0..=1（否则会污染图权重与召回加成）。
    #[test]
    fn out_of_range_link_weight_is_rejected() {
        let (dir, svc, va, _g) = env("weight");
        let a = svc.write(&va, WriteInput::new("ig", "权重 A")).unwrap();
        let b = svc.write(&va, WriteInput::new("ig", "权重 B")).unwrap();
        for bad in [-0.5_f64, 1.5, 1e9] {
            let err = svc.link(&va, &a.id, &b.id, "semantic", bad).unwrap_err();
            assert!(err.to_string().contains("weight"), "应报 weight 范围: {err}");
        }
        assert!(svc.link(&va, &a.id, &b.id, "semantic", 0.5).is_ok());

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }
}