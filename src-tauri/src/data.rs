//! 记忆总览的只读查询（T-M2-01）。
//!
//! **为什么是只读**：红线 5 禁止绕过 `db::writer` 做 INSERT/UPDATE；
//! 读不受限，且 SQLite WAL 支持多读者。GUI 只读，写入仍走 MCP/core。
//!
//! **分层计数口径（ADR-031 决策三）**：
//! - L0 = `SELECT count(*) FROM conversations`
//! - L1 = `SELECT count(*) FROM memories WHERE deleted_at IS NULL`
//! - **禁止 `GROUP BY layer`**（该字段已在 schema v2 删除）
//!
//! **中文摘要必须按字符边界截断**（旧坑：按字节切会切坏 UTF-8）。

use rusqlite::{params, Connection};
use std::path::PathBuf;

/// 单条记忆（列表用，不含全文——全文靠 `get_memory` 单独取）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemItem {
    pub id: String,
    /// 摘要（按字符截断，不是字节）
    pub summary: String,
    pub agent_id: String,
    pub category: String,
    pub scope: String,
    pub importance: i64,
    pub pinned: bool,
    pub created_at: f64,
    /// 有效期末（NULL = 仍有效）
    pub valid_to: Option<f64>,
    pub deleted_at: Option<f64>,
    pub superseded_by: Option<String>,
    pub has_source: bool,
}

/// 分层计数（跨表，ADR-031）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct LayerCounts {
    pub l0: i64,
    pub l1: i64,
    pub l2: i64,
    pub l3: i64,
}

/// 列表结果（服务端分页）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ListResult {
    pub items: Vec<MemItem>,
    pub total: i64,
    pub page: i64,
    pub page_size: i64,
    pub counts: LayerCounts,
}

/// 一条记忆的完整详情。
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemDetail {
    pub id: String,
    pub content: String,
    pub agent_id: String,
    pub category: String,
    pub scope: String,
    pub origin: String,
    pub importance: i64,
    pub confidence: f64,
    pub tags: String,
    pub entities: String,
    pub source_ref: Option<String>,
    pub session_id: Option<String>,
    pub created_at: f64,
    pub updated_at: f64,
    pub valid_from: f64,
    pub valid_to: Option<f64>,
    pub recorded_at: f64,
    pub last_accessed_at: Option<f64>,
    pub access_count: i64,
    pub pinned: bool,
    pub superseded_by: Option<String>,
    pub deleted_at: Option<f64>,
    pub embedding_status: String,
}

pub fn db_path() -> PathBuf {
    if let Ok(p) = std::env::var("ORBITAL_DB_PATH") {
        return PathBuf::from(p);
    }
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("OrbitalPro").join("data").join("orbital.db")
}

fn open_ro() -> Result<Connection, String> {
    let p = db_path();
    if !p.exists() {
        return Err(format!("数据库不存在：{}（先启动 core 或跑一次 --register）", p.display()));
    }
    Connection::open_with_flags(
        &p,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("打开数据库失败：{e}"))
}

/// 按**字符**截断（不是字节）——旧坑：按字节切会切坏 UTF-8。
fn summarize(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (n, c) in s.chars().enumerate() {
        if n >= max_chars {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out.replace('\n', " ").trim().to_string()
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 分层计数（跨表，ADR-031 决策三）。
fn layer_counts(conn: &Connection) -> LayerCounts {
    let l0: i64 = conn
        .query_row("SELECT count(*) FROM conversations", [], |r| r.get(0))
        .unwrap_or(0);
    let l1: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories WHERE deleted_at IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    // L2/L3 尚未建表（M3 才引入 scene/persona）——如实返回 0，不假装有
    LayerCounts { l0, l1, l2: 0, l3: 0 }
}

/// 记忆列表（服务端分页 + 四筛选）。
///
/// `status`: `current`（默认）/ `expired`（已失效）/ `deleted`
#[tauri::command]
pub fn list_memories(
    agent: Option<String>,
    category: Option<String>,
    status: Option<String>,
    q: Option<String>,
    page: Option<i64>,
    page_size: Option<i64>,
    todo: Option<String>,
) -> Result<ListResult, String> {
    let conn = open_ro()?;
    let page = page.unwrap_or(1).max(1);
    let page_size = page_size.unwrap_or(50).clamp(1, 200);

    // 状态条件（三态必须可区分，docs/03 §4.1 禁止都用"暂无数据"）
    let status = status.unwrap_or_else(|| "current".into());
    let status_cond = match status.as_str() {
        "deleted" => "deleted_at IS NOT NULL".to_string(),
        "expired" => "deleted_at IS NULL AND valid_to IS NOT NULL".to_string(),
        _ => "deleted_at IS NULL AND valid_to IS NULL".to_string(),
    };

    let mut where_parts = vec![status_cond];
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(a) = agent.as_ref().filter(|s| !s.is_empty() && s.as_str() != "all") {
        where_parts.push("agent_id = ?".into());
        args.push(Box::new(a.clone()));
    }
    if let Some(c) = category.as_ref().filter(|s| !s.is_empty() && s.as_str() != "all") {
        where_parts.push("category = ?".into());
        args.push(Box::new(c.clone()));
    }
    if let Some(k) = q.as_ref().filter(|s| !s.trim().is_empty()) {
        where_parts.push("content LIKE ? ESCAPE '\\'".into());
        // 审计修复：LIKE 的 %/_ 必须转义，否则搜 "50%" 会匹配到全部结果
        args.push(Box::new(format!("%{}%", escape_like(k.trim()))));
    }
    // 待办类筛选：总览页的"我该做什么"每条点进来都要落到同一个判定上，
    // 否则会出现"总览说 3 条缺向量、点进去列表是 0"的矛盾。
    if let Some(t) = todo.as_ref().filter(|s| !s.is_empty()) {
        let cond = match t.as_str() {
            "no-vector" => "embedding_status IN ('pending','failed')",
            "untagged" => "(tags = '[]' OR tags = '' OR tags IS NULL)",
            "stale" => "(last_accessed_at IS NULL
                         OR last_accessed_at < cast(strftime('%s','now') as real) - 90*86400)",
            // 重复 = 同 content_hash 还有另一条**有效**记忆（跨 agent；版本链旧条不算）
            "duplicate" => "EXISTS (SELECT 1 FROM memories m2
                              WHERE m2.deleted_at IS NULL AND m2.valid_to IS NULL
                                AND m2.content_hash = memories.content_hash
                                AND m2.id <> memories.id)",
            _ => "",
        };
        if !cond.is_empty() {
            where_parts.push(cond.into());
        }
    }

    let where_sql = where_parts.join(" AND ");

    let total: i64 = conn
        .query_row(
            &format!("SELECT count(*) FROM memories WHERE {where_sql}"),
            rusqlite::params_from_iter(args.iter().map(|b| b.as_ref())),
            |r| r.get(0),
        )
        .map_err(|e| format!("计数失败：{e}"))?;

    let sql = format!(
        "SELECT id, content, agent_id, category, scope, importance, pinned, created_at,
                valid_to, deleted_at, superseded_by, source_ref
         FROM memories WHERE {where_sql}
         ORDER BY created_at DESC LIMIT ? OFFSET ?"
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| format!("准备查询失败：{e}"))?;
    let mut all: Vec<Box<dyn rusqlite::ToSql>> = args;
    all.push(Box::new(page_size));
    all.push(Box::new((page - 1) * page_size));

    let items = stmt
        .query_map(
            rusqlite::params_from_iter(all.iter().map(|b| b.as_ref())),
            |r| {
                let content: String = r.get(1)?;
                let source_ref: Option<String> = r.get(11)?;
                Ok(MemItem {
                    id: r.get(0)?,
                    summary: summarize(&content, 80),
                    agent_id: r.get(2)?,
                    category: r.get(3)?,
                    scope: r.get(4)?,
                    importance: r.get(5)?,
                    pinned: r.get::<_, i64>(6)? != 0,
                    created_at: r.get(7)?,
                    valid_to: r.get(8)?,
                    deleted_at: r.get(9)?,
                    superseded_by: r.get(10)?,
                    has_source: source_ref.map(|s| !s.is_empty()).unwrap_or(false),
                })
            },
        )
        .map_err(|e| format!("查询失败：{e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("读取行失败：{e}"))?;

    Ok(ListResult {
        items,
        total,
        page,
        page_size,
        counts: layer_counts(&conn),
    })
}

/// 单条详情（点列表行时调用）。
#[tauri::command]
pub fn get_memory(id: String) -> Result<MemDetail, String> {
    let conn = open_ro()?;
    conn.query_row(
        "SELECT id, content, agent_id, category, scope, origin, importance, confidence,
                tags, entities, source_ref, session_id, created_at, updated_at,
                valid_from, valid_to, recorded_at, last_accessed_at, access_count,
                pinned, superseded_by, deleted_at, embedding_status
         FROM memories WHERE id = ?1",
        params![id],
        |r| {
            Ok(MemDetail {
                id: r.get(0)?,
                content: r.get(1)?,
                agent_id: r.get(2)?,
                category: r.get(3)?,
                scope: r.get(4)?,
                origin: r.get(5)?,
                importance: r.get(6)?,
                confidence: r.get(7)?,
                tags: r.get(8)?,
                entities: r.get(9)?,
                source_ref: r.get(10)?,
                session_id: r.get(11)?,
                created_at: r.get(12)?,
                updated_at: r.get(13)?,
                valid_from: r.get(14)?,
                valid_to: r.get(15)?,
                recorded_at: r.get(16)?,
                last_accessed_at: r.get(17)?,
                access_count: r.get(18)?,
                pinned: r.get::<_, i64>(19)? != 0,
                superseded_by: r.get(20)?,
                deleted_at: r.get(21)?,
                embedding_status: r.get(22)?,
            })
        },
    )
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => format!("记忆 {id} 不存在"),
        other => format!("查询失败：{other}"),
    })
}

/// 筛选可选项（agent / category 的真实取值，不写死）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct FilterOptions {
    pub agents: Vec<String>,
    pub categories: Vec<String>,
}

#[tauri::command]
pub fn filter_options() -> Result<FilterOptions, String> {
    let conn = open_ro()?;
    let q = |sql: &str| -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(mut st) = conn.prepare(sql) {
            if let Ok(rows) = st.query_map([], |r| r.get::<_, String>(0)) {
                for x in rows.flatten() {
                    out.push(x);
                }
            }
        }
        out
    };
    Ok(FilterOptions {
        agents: q("SELECT DISTINCT agent_id FROM memories ORDER BY agent_id"),
        categories: q("SELECT DISTINCT category FROM memories ORDER BY category"),
    })
}

/// 供后续"最近活跃"用（当前未使用，保留避免 dead_code 告警）。
#[allow(dead_code)]
fn _now() -> f64 {
    now_secs()
}

/// 每个 agent 的记忆条数（侧栏 Agent 行与总览页的 Agent 卡片用）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentStat {
    pub agent_id: String,
    pub total: i64,
    pub today: i64,
    pub last_at: Option<f64>,
}

/// 总览页一次性取全的数据（避免前端发 5 个请求）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Overview {
    pub total: i64,
    /// T-M10-05（ADR-048）：`total` 里"已被替代/已失效"的旧版本条数。
    ///
    /// 口径：`deleted_at IS NULL`（与 `total` 同）**且**（`superseded_by IS NOT NULL` 或 `valid_to IS NOT NULL`）。
    /// 为什么单列一个字段而不是改 `total`：`total` 被总览、托盘、体检、备份校验四处消费，
    /// 改语义会连锁；界面用「当前有效 N · 历史版本 M」把差异显式化（`docs/29 §4.3` 方案 B）。
    pub total_history: i64,
    pub today: i64,
    pub agents: Vec<AgentStat>,
    pub counts: LayerCounts,
    /// 最近 8 条（点进详情用）
    pub recent: Vec<MemItem>,
    pub pinned: i64,
    pub embeddings_ok: i64,
    pub embeddings_pending: i64,
    pub db_version: i64,
    // ---- 2026-11-12 新增（docs/26 §2 的 B 组指标）----
    /// 待抽取积压（conversations 表不存在时为 None，避免把"表还没迁移"显示成 0）
    pub pending_extract: Option<i64>,
    /// 重复内容组数（**跨 agent** 同 content_hash 且两条都有效）。
    /// 口径修正（T-M2-06）：原按 `同 agent` 统计有两个问题——
    /// ① `ux_mem_agent_content` 保证同 agent 同 hash 只有一条有效 ⇒ 该指标恒为 0；
    /// ② 版本链的旧条（`valid_to` 非空）会误算成重复。
    pub duplicates: i64,
    /// 未处理的冲突条数（memory_conflicts.status='open'，docs/04 §7）。
    /// `None` = 表还不存在（v2 旧库、尚未迁移），**不能显示成 0**
    /// —— 与 `pending_extract` 同一条原则：把"没迁移"显示成"没问题"是假绿。
    pub conflicts: Option<i64>,
    /// 未打标签（tags 为空数组）
    pub untagged: i64,
    /// 长期未访问（从未访问 或 90 天以上）
    pub stale: i64,
    /// **待办类别数**（五类里"有数据"的类别数）—— 侧栏角标与维护页标题都用它，
    /// 保证"角标 4 vs 页面待办 6"这类不一致不会再出现（旧项目坑 #8）。
    pub todo_total: i64,
    /// 近 7 天每日写入数（含 0 填充）
    pub trend: Vec<DayCount>,
}

/// 一天的写入量（趋势图用）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DayCount {
    /// YYYY-MM-DD（本地时区）
    pub day: String,
    pub n: i64,
}

/// 今天 00:00 的 Unix 秒（本地时区）。
fn today_start() -> f64 {
    // 不引 chrono：用本地时间偏移手工算当天 0 点
    let now = now_secs();
    let days = (now / 86400.0).floor();
    days * 86400.0
}

#[tauri::command]
pub fn overview() -> Result<Overview, String> {
    let conn = open_ro()?;

    // ⚠️ 2026-11-12 修「跨时区日界」bug：
    // 原实现是 `(now / 86400).floor() * 86400` —— 那是 **UTC 日界**，注释却写"本地时区"。
    // 东八区实证差异：本地口径今日 50 条，UTC 口径 47 条（本地 08:00 前写的 3 条被算进"昨天"）。
    // 改由 SQLite 自己算 localtime 的当日 0 点（system 本地时区，不引 chrono）。
    // 失败才回退到旧的 UTC 算法（保证不会因为一条 SQL 让整个总览挂掉）。
    let today: f64 = conn
        .query_row(
            "SELECT cast(strftime('%s', date('now','localtime')) as real)",
            [],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| today_start());

    let total: i64 = conn
        .query_row("SELECT count(*) FROM memories WHERE deleted_at IS NULL", [], |r| r.get(0))
        .map_err(|e| format!("计数失败：{e}"))?;
    // T-M10-05（ADR-048）：total 里有多少是"版本链旧条/已失效"——界面用它把口径说清。
    let total_history: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories
              WHERE deleted_at IS NULL
                AND (superseded_by IS NOT NULL OR valid_to IS NOT NULL)",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("计数失败：{e}"))?;
    let today_n: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories WHERE deleted_at IS NULL AND created_at >= ?1",
            params![today],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let pinned: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories WHERE deleted_at IS NULL AND pinned = 1",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let embeddings_ok: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories WHERE deleted_at IS NULL AND embedding_status = 'ok'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let embeddings_pending: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories WHERE deleted_at IS NULL AND embedding_status IN ('pending','failed')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let db_version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);

    // ---------- B 组指标（docs/26 §2）----------
    // conversations 表是 schema v2 才有的；v1 库上它不存在。
    // 这里必须先查表存在性：**表不存在要返回 None，不能返回 0**
    // ——否则"迁移还没跑到"会被显示成"没有积压"，属于把未知冒充成已知。
    let has_conversations: bool = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='conversations'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false);
    let pending_extract: Option<i64> = if has_conversations {
        conn.query_row(
            "SELECT count(*) FROM conversations WHERE extracted_at IS NULL",
            [],
            |r| r.get(0),
        )
        .ok()
    } else {
        None
    };
    // 重复组（T-M2-06 口径修正）：
    // - 必须排除 `valid_to IS NOT NULL` 的版本链旧条（它们 hash 相同是设计使然）
    // - 同 agent 同 hash 因唯一索引不可能同时有两条有效 ⇒ 命中必然是跨 agent
    //   （docs/04 §4.5：跨 agent 相同内容是"重要信号"，这里只统计不自动合并）
    let duplicates: i64 = conn
        .query_row(
            "SELECT count(*) FROM (
               SELECT content_hash FROM memories
               WHERE deleted_at IS NULL AND valid_to IS NULL
               GROUP BY content_hash HAVING count(*) > 1)",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let conflicts: Option<i64> = if conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='memory_conflicts'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    {
        conn.query_row(
            "SELECT count(*) FROM memory_conflicts WHERE status = 'open'",
            [],
            |r| r.get(0),
        )
        .ok()
    } else {
        None
    };
    let untagged: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories WHERE deleted_at IS NULL
               AND (tags = '[]' OR tags = '' OR tags IS NULL)",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let stale: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories WHERE deleted_at IS NULL
               AND (last_accessed_at IS NULL
                    OR last_accessed_at < cast(strftime('%s','now') as real) - 90*86400)",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    // 近 7 天趋势：递归 CTE 生成日期序列再 LEFT JOIN，
    // 保证"那天没有写入"表现为 0 而不是缺一行（趋势图最怕缺行导致折线错位）
    let mut trend: Vec<DayCount> = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        "WITH RECURSIVE days(d) AS (
           SELECT date('now','localtime','-6 days')
           UNION ALL SELECT date(d,'+1 day') FROM days WHERE d < date('now','localtime')
         )
         SELECT days.d, count(m.id) FROM days
         LEFT JOIN memories m
           ON date(m.created_at,'unixepoch','localtime') = days.d AND m.deleted_at IS NULL
         GROUP BY days.d ORDER BY days.d",
    ) {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok(DayCount {
                day: r.get(0)?,
                n: r.get(1)?,
            })
        }) {
            for x in rows.flatten() {
                trend.push(x);
            }
        }
    }

    // 每个 agent 的条数 + 今日 + 最近时间
    let mut agents: Vec<AgentStat> = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT agent_id, count(*) AS n,
                        sum(CASE WHEN created_at >= ?1 THEN 1 ELSE 0 END) AS t,
                        max(created_at) AS last
                 FROM memories WHERE deleted_at IS NULL
                 GROUP BY agent_id ORDER BY n DESC",
            )
            .map_err(|e| format!("准备 agent 统计失败：{e}"))?;
        let rows = stmt
            .query_map(params![today], |r| {
                Ok(AgentStat {
                    agent_id: r.get(0)?,
                    total: r.get(1)?,
                    today: r.get(2)?,
                    last_at: r.get(3)?,
                })
            })
            .map_err(|e| format!("agent 统计失败：{e}"))?;
        for a in rows.flatten() {
            agents.push(a);
        }
    }

    // 最近 8 条
    let mut recent: Vec<MemItem> = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT id, content, agent_id, category, scope, importance, pinned, created_at,
                        valid_to, deleted_at, superseded_by, source_ref
                 FROM memories WHERE deleted_at IS NULL
                 ORDER BY created_at DESC LIMIT 8",
            )
            .map_err(|e| format!("准备最近查询失败：{e}"))?;
        let rows = stmt
            .query_map([], |r| {
                let content: String = r.get(1)?;
                let source_ref: Option<String> = r.get(11)?;
                Ok(MemItem {
                    id: r.get(0)?,
                    summary: summarize(&content, 70),
                    agent_id: r.get(2)?,
                    category: r.get(3)?,
                    scope: r.get(4)?,
                    importance: r.get(5)?,
                    pinned: r.get::<_, i64>(6)? != 0,
                    created_at: r.get(7)?,
                    valid_to: r.get(8)?,
                    deleted_at: r.get(9)?,
                    superseded_by: r.get(10)?,
                    has_source: source_ref.map(|s| !s.is_empty()).unwrap_or(false),
                })
            })
            .map_err(|e| format!("最近查询失败：{e}"))?;
        for m in rows.flatten() {
            recent.push(m);
        }
    }

    // 待办类别数：与维护页「待处理 N 项」同源（侧栏角标必须等于页面数，旧坑 #8）
    let mut todo_total = 0i64;
    if duplicates > 0 {
        todo_total += 1;
    }
    if conflicts.unwrap_or(0) > 0 {
        todo_total += 1;
    }
    if untagged > 0 {
        todo_total += 1;
    }
    if embeddings_pending > 0 {
        todo_total += 1;
    }
    if stale > 0 {
        todo_total += 1;
    }

    Ok(Overview {
        total,
        total_history,
        today: today_n,
        agents,
        counts: layer_counts(&conn),
        recent,
        pinned,
        embeddings_ok,
        embeddings_pending,
        db_version,
        pending_extract,
        duplicates,
        conflicts,
        untagged,
        stale,
        todo_total,
        trend,
    })
}

// ---------------- 写通道（ADR-034 过渡方案）----------------

/// GUI 的编辑结果。`action` 是前端的唯一分支判据（docs/04 §10.1 承诺强度）。
#[derive(serde::Serialize)]
pub struct UpdateResult {
    pub action: String,
    pub id: String,
    pub superseded_id: Option<String>,
    pub note: String,
    /// 本次写下的 `memory_events.id`（T-M2-07 的"5 秒内撤销"句柄；None = 没有可撤销的写入）
    pub event_id: Option<i64>,
}

/// GUI 懒初始化的 MemoryService。
///
/// 为什么用 `OnceLock`：`MemoryService::open` 会启动写线程并持有连接，
/// 每次点击都开关一次既慢又会反复启停线程。
/// 为什么包 `Mutex`：SQLite 连接不是 `Sync`，且 `update` 内部走 `&self` 的连接。
static EDITOR: std::sync::OnceLock<std::sync::Mutex<orbital_core::service::MemoryService>> =
    std::sync::OnceLock::new();

fn editor() -> Result<&'static std::sync::Mutex<orbital_core::service::MemoryService>, String> {
    if let Some(e) = EDITOR.get() {
        return Ok(e);
    }
    let db = db_path();
    if !db.exists() {
        return Err(format!("数据库不存在：{}", db.display()));
    }
    let journal = db.with_file_name("journal.ndjson");
    let svc = orbital_core::service::MemoryService::open(&db, &journal)
        .map_err(|e| format!("打开记忆服务失败：{e}"))?;
    // 竞态：另一个线程可能已抢先设置；`set` 失败就用已有的那个（丢弃新建的会 Drop 掉写线程，安全）
    let _ = EDITOR.set(std::sync::Mutex::new(svc));
    EDITOR
        .get()
        .ok_or_else(|| "记忆服务初始化失败".to_string())
}

/// GUI 编辑一条记忆（T-M2-02）。
///
/// 走 `MemoryService::update_local_owner`（唯一实现）——**绝不**在这里另写版本链 SQL，
/// 否则两份实现必然漂移（ADR-034 判据 ②）。
#[tauri::command]
pub fn update_memory(
    id: String,
    content: Option<String>,
    importance: Option<i64>,
    tags: Option<Vec<String>>,
    pinned: Option<bool>,
    scope: Option<String>,
) -> Result<UpdateResult, String> {
    let patch = orbital_core::service::UpdatePatch {
        content,
        importance,
        tags,
        pinned,
        scope,
    };
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    let out = guard
        .update_local_owner(&id, patch)
        .map_err(|e| e.to_string())?;
    Ok(UpdateResult {
        action: out.action,
        id: out.id,
        superseded_id: out.superseded_id,
        note: out.note,
        event_id: out.event_id,
    })
}

// ---------------- 删除 / 恢复（T-M2-03）----------------

/// 删除前的影响面（弹窗数字来自这里，必须与真实 SQL 一致）。
#[derive(serde::Serialize)]
pub struct ImpactResult {
    pub id: String,
    pub summary: String,
    pub pinned: bool,
    pub already_deleted: bool,
    pub access_count: i64,
    pub facts: i64,
    pub edges: i64,
}

#[tauri::command]
pub fn delete_impact(id: String) -> Result<ImpactResult, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    let imp = guard.delete_impact(&id).map_err(|e| e.to_string())?;
    Ok(ImpactResult {
        id: imp.id,
        summary: imp.summary,
        pinned: imp.pinned,
        already_deleted: imp.already_deleted,
        access_count: imp.access_count,
        facts: imp.facts,
        edges: imp.edges,
    })
}

/// 删除一条记忆。`mode`：`soft`（默认，可恢复）/ `hard`（不可逆；core 会先写 journal 快照）。
///
/// 返回 `memory_events.id`（T-M2-07 的撤销句柄）；hard 返回的 id 调用 `undo_event` 会被拒绝。
#[tauri::command]
pub fn forget_memory(id: String, mode: Option<String>) -> Result<i64, String> {
    let m = mode.unwrap_or_else(|| "soft".into());
    // 破坏性操作只接受两个确定值，别让拼写错误变成"未知 mode"
    if m != "soft" && m != "hard" {
        return Err(format!("不支持的删除方式：{m}（只允许 soft / hard）"));
    }
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    guard.forget_local_owner(&id, &m).map_err(|e| e.to_string())
}

/// 恢复一条被软删的记忆（内容逐字节不变）。
#[tauri::command]
pub fn restore_memory(id: String) -> Result<(), String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    guard.restore_local_owner(&id).map_err(|e| e.to_string())
}

// ---------------- Pin / 标记失效（T-M2-04）----------------

/// 切换 Pin（免疫降权：pinned 时 `ei` 下限为 1.0）。
///
/// 复用 `update_local_owner` 而不再造一条写路径——Pin 只是 `pinned` 字段的变更，
/// 走同一条实现才能保证审计与版本链行为一致。
#[tauri::command]
pub fn toggle_pin(id: String, pinned: bool) -> Result<i64, String> {
    let patch = orbital_core::service::UpdatePatch {
        content: None,
        importance: None,
        tags: None,
        pinned: Some(pinned),
        scope: None,
    };
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    // 返回事件 id：Pin 也是可撤销操作（T-M2-07）
    let out = guard
        .update_local_owner(&id, patch)
        .map_err(|e| e.to_string())?;
    Ok(out.event_id.unwrap_or(0))
}

/// 标记失效：`valid_to = now`。行保留（可查历史），但**召回不再返回**。
#[tauri::command]
pub fn invalidate_memory(id: String) -> Result<i64, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    guard
        .forget_local_owner(&id, "invalidate")
        .map_err(|e| e.to_string())
}

/// 清除失效标记：`valid_to = NULL` → 重新可被召回。
#[tauri::command]
pub fn restore_validity(id: String) -> Result<(), String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    guard
        .restore_validity_local_owner(&id)
        .map_err(|e| e.to_string())
}

/// 读时计算 `effective_importance`（docs/04 §6.1，ADR-013：不落盘）。
///
/// 目前只给详情面板显示用；**不用它做任何自动删除决策**。
#[tauri::command]
pub fn effective_importance(id: String) -> Result<f64, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    guard
        .effective_importance_of(&id)
        .map_err(|e| e.to_string())
}

// ---------------- 合并重复（T-M2-05）----------------

/// 合并预览（"合并两者"之前的预览：会搬走多少标签/关系/事实）。
#[derive(serde::Serialize)]
pub struct MergePreviewResult {
    pub keep_id: String,
    pub drop_id: String,
    pub keep_summary: String,
    pub drop_summary: String,
    pub keep_agent: String,
    pub drop_agent: String,
    pub tags: Vec<String>,
    pub tags_moved: Vec<String>,
    pub edges_moved: i64,
    pub self_loops: i64,
    pub facts_moved: i64,
}

/// 预览与执行必须用同一套查询（core 的 `merge_preview`），
/// 否则会出现"弹窗说搬 3 条、实际搬 5 条"——这就是旧的"两个数据源"坑。
#[tauri::command]
pub fn merge_preview(keep_id: String, drop_id: String) -> Result<MergePreviewResult, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    let p = guard
        .merge_preview(&keep_id, &drop_id)
        .map_err(|e| e.to_string())?;
    Ok(MergePreviewResult {
        keep_id: p.keep_id,
        drop_id: p.drop_id,
        keep_summary: p.keep_summary,
        drop_summary: p.drop_summary,
        keep_agent: p.keep_agent,
        drop_agent: p.drop_agent,
        tags: p.tags,
        tags_moved: p.tags_moved,
        edges_moved: p.edges_moved,
        self_loops: p.self_loops,
        facts_moved: p.facts_moved,
    })
}

#[derive(serde::Serialize)]
pub struct MergeResult {
    /// 撤销句柄：前端"撤销"按钮把它交回 `undo_merge`
    pub event_id: i64,
    pub keep_id: String,
    pub drop_id: String,
    pub tags: Vec<String>,
    pub tags_moved: Vec<String>,
    pub edges_moved: i64,
    pub self_loops: i64,
    pub facts_moved: i64,
    pub note: String,
}

/// 合并两条重复（搬标签/实体/事实/关系，然后软删被并条）。
///
/// 走 `MemoryService::merge_local_owner`（ADR-034 的本地特权模式）：
/// 身份从库里查，不接受请求参数；**不要**在 src-tauri 里另写合并 SQL。
#[tauri::command]
pub fn merge_memories(keep_id: String, drop_id: String) -> Result<MergeResult, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    let out = guard
        .merge_local_owner(&keep_id, &drop_id)
        .map_err(|e| e.to_string())?;
    Ok(MergeResult {
        event_id: out.event_id,
        keep_id: out.keep_id,
        drop_id: out.drop_id,
        tags: out.tags,
        tags_moved: out.tags_moved,
        edges_moved: out.edges_moved,
        self_loops: out.self_loops,
        facts_moved: out.facts_moved,
        note: out.note,
    })
}

#[derive(serde::Serialize)]
pub struct UndoResult {
    pub event_id: i64,
    pub keep_id: String,
    pub drop_id: String,
    pub note: String,
}

/// 撤销一次合并（基于 `memory_events` 的逆操作清单）。
///
/// 只给本机 GUI：撤销是"回滚账本"，不该由外部 agent 触发。
#[tauri::command]
pub fn undo_merge(event_id: i64) -> Result<UndoResult, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    let out = guard.undo_merge(event_id).map_err(|e| e.to_string())?;
    Ok(UndoResult {
        event_id: out.event_id,
        keep_id: out.keep_id,
        drop_id: out.drop_id,
        note: out.note,
    })
}

// ---------------- 维护页待办（T-M2-06）----------------

/// 一类待办（维护页一行）。`action` 决定前端怎么展开：
/// - `groups`：逐组决策（重复记忆，复用 T-M2-05 的合并 / 保留一条）
/// - `conflicts`：冲突四选一
/// - `list`：跳到记忆页并带**同一个** todo 筛选（判定条件必须与这里同一套，
///   否则又会出现"总览说 3 条、点进去 0 条"）
#[derive(serde::Serialize)]
pub struct TodoItem {
    pub key: String,
    pub label: String,
    pub count: i64,
    pub desc: String,
    pub action: String,
}

/// 重复组：同一 `content_hash` 的多条**有效**记忆（跨 agent 才可能出现，见 overview.duplicates 注释）。
#[derive(serde::Serialize)]
pub struct DuplicateGroup {
    /// content_hash 前 10 位（前端做 React key，不泄露内容）
    pub key: String,
    pub items: Vec<MemItem>,
}

/// 冲突行（字段与 core 的 `ConflictRow` 一致；这里走只读连接直接查，
/// 不必为了"读"去启动 MemoryService）。
#[derive(serde::Serialize)]
pub struct ConflictItem {
    pub id: i64,
    pub entity_type: String,
    pub entity_id: String,
    pub property: String,
    pub memory_a: String,
    pub memory_b: String,
    pub value_a: String,
    pub value_b: String,
    pub summary_a: String,
    pub summary_b: String,
    pub agent_a: String,
    pub agent_b: String,
    pub created_at: f64,
}

#[derive(serde::Serialize)]
pub struct MaintenanceTodos {
    pub items: Vec<TodoItem>,
    pub groups: Vec<DuplicateGroup>,
    pub conflicts: Vec<ConflictItem>,
    /// 待办类别数（= 侧栏角标 = 页面「待处理 N 项」）
    pub total_categories: i64,
}

fn has_table(conn: &rusqlite::Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
        params![name],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

fn mem_items_by_hash(conn: &rusqlite::Connection, hash: &str) -> Result<Vec<MemItem>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, content, agent_id, category, scope, importance, pinned, created_at,
                    valid_to, deleted_at, superseded_by, source_ref
               FROM memories
              WHERE deleted_at IS NULL AND valid_to IS NULL AND content_hash = ?1
              ORDER BY created_at DESC",
        )
        .map_err(|e| format!("准备查询失败：{e}"))?;
    let rows = stmt
        .query_map(params![hash], |r| {
            let content: String = r.get(1)?;
            let source_ref: Option<String> = r.get(11)?;
            Ok(MemItem {
                id: r.get(0)?,
                summary: summarize(&content, 80),
                agent_id: r.get(2)?,
                category: r.get(3)?,
                scope: r.get(4)?,
                importance: r.get(5)?,
                pinned: r.get::<_, i64>(6)? != 0,
                created_at: r.get(7)?,
                valid_to: r.get(8)?,
                deleted_at: r.get(9)?,
                superseded_by: r.get(10)?,
                has_source: source_ref.map(|s| !s.is_empty()).unwrap_or(false),
            })
        })
        .map_err(|e| format!("查询失败：{e}"))?;
    let mut v = Vec::new();
    for x in rows {
        v.push(x.map_err(|e| format!("读取行失败：{e}"))?);
    }
    Ok(v)
}

/// 维护页一次性取全：五类待办 + 重复组 + 冲突详情。
#[tauri::command]
pub fn maintenance_todos() -> Result<MaintenanceTodos, String> {
    let conn = open_ro()?;

    // ① 重复组（条件与 overview.duplicates 保持一致）
    let hashes: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT content_hash FROM memories
                  WHERE deleted_at IS NULL AND valid_to IS NULL
                  GROUP BY content_hash HAVING count(*) > 1
                  ORDER BY max(created_at) DESC",
            )
            .map_err(|e| format!("准备重复组查询失败：{e}"))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| format!("查询重复组失败：{e}"))?;
        let mut v = Vec::new();
        for x in rows {
            v.push(x.map_err(|e| format!("读重复组失败：{e}"))?);
        }
        v
    };
    let mut groups = Vec::new();
    for h in &hashes {
        groups.push(DuplicateGroup {
            key: h.chars().take(10).collect(),
            items: mem_items_by_hash(&conn, h)?,
        });
    }

    // ② 冲突（v2 旧库没有表 → 空列表；`conflicts` 单独返回 None 由 overview 表达）
    let conflicts = if has_table(&conn, "memory_conflicts") {
        let mut stmt = conn
            .prepare(
                "SELECT c.id, c.entity_type, c.entity_id, c.property,
                        c.memory_a, c.memory_b, c.value_a, c.value_b, c.created_at,
                        COALESCE(ma.content,''), COALESCE(mb.content,''),
                        COALESCE(ma.agent_id,''), COALESCE(mb.agent_id,'')
                   FROM memory_conflicts c
                   LEFT JOIN memories ma ON ma.id = c.memory_a
                   LEFT JOIN memories mb ON mb.id = c.memory_b
                  WHERE c.status = 'open'
                  ORDER BY c.created_at DESC, c.id DESC",
            )
            .map_err(|e| format!("准备冲突查询失败：{e}"))?;
        let rows = stmt
            .query_map([], |r| {
                let ca: String = r.get(9)?;
                let cb: String = r.get(10)?;
                Ok(ConflictItem {
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
            .map_err(|e| format!("查询冲突失败：{e}"))?;
        let mut v = Vec::new();
        for x in rows {
            v.push(x.map_err(|e| format!("读冲突失败：{e}"))?);
        }
        v
    } else {
        Vec::new()
    };

    // ③ 三个计数（条件与 list_memories 的 todo 筛选一致）
    let untagged: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories WHERE deleted_at IS NULL AND valid_to IS NULL
               AND (tags = '[]' OR tags = '' OR tags IS NULL)",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("统计未打标签失败：{e}"))?;
    let no_vector: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories WHERE deleted_at IS NULL AND valid_to IS NULL
               AND embedding_status IN ('pending','failed')",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("统计缺向量失败：{e}"))?;
    let stale: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories WHERE deleted_at IS NULL AND valid_to IS NULL
               AND (last_accessed_at IS NULL
                    OR last_accessed_at < cast(strftime('%s','now') as real) - 90*86400)",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("统计长期未访问失败：{e}"))?;

    // ④ 组装（顺序与 docs/03 §4.3 一致：重复 → 冲突 → 标签 → 向量 → 访问）
    let mut items: Vec<TodoItem> = Vec::new();
    if !groups.is_empty() {
        items.push(TodoItem {
            key: "duplicate".into(),
            label: "重复记忆".into(),
            count: groups.len() as i64,
            desc: "同一内容在多个 agent 各存了一条，可逐组决策".into(),
            action: "groups".into(),
        });
    }
    if !conflicts.is_empty() {
        items.push(TodoItem {
            key: "conflict".into(),
            label: "冲突事实".into(),
            count: conflicts.len() as i64,
            desc: "同一实体的同一属性有两个不同值；冲突期间不会注入".into(),
            action: "conflicts".into(),
        });
    }
    if untagged > 0 {
        items.push(TodoItem {
            key: "untagged".into(),
            label: "缺标签".into(),
            count: untagged,
            desc: "没有标签的记忆难以被按主题找回".into(),
            action: "list".into(),
        });
    }
    if no_vector > 0 {
        items.push(TodoItem {
            key: "no-vector".into(),
            label: "缺向量".into(),
            count: no_vector,
            desc: "语义召回用不上（关键词召回不受影响）".into(),
            action: "list".into(),
        });
    }
    if stale > 0 {
        items.push(TodoItem {
            key: "stale".into(),
            label: "长期未访问".into(),
            count: stale,
            desc: "90 天没有被召回过的记忆，可考虑标记失效".into(),
            action: "list".into(),
        });
    }
    let total_categories = items.len() as i64;

    Ok(MaintenanceTodos {
        items,
        groups,
        conflicts,
        total_categories,
    })
}

/// 启动时把库迁移到当前 schema（v2 → v3 补建 `memory_conflicts`）。
///
/// 为什么必须显式做：`overview` / `maintenance_todos` 都走**只读连接**，不会触发迁移；
/// 而 `MemoryService` 是懒初始化的（第一次写操作才创建）。
/// 结果是：升级后如果用户只读不写，维护页会一直显示"冲突表尚未迁移"——
/// 看起来像功能没做，实际只是没人触发迁移。
pub fn ensure_migrated() -> Result<(), String> {
    let db = db_path();
    if !db.exists() {
        return Ok(()); // 首启还没有库，等第一次写入时创建
    }
    // 打开（并丢弃句柄）即完成迁移；写线程常驻，本来也需要它
    let _ = editor()?;
    Ok(())
}

/// 退役候选（T-M4-01，**只读**）：按 ei 公式列出"该考虑退役"的记忆。
///
/// 免疫规则（pinned / importance≥4 / access≥3）在 core 里，这里不重复实现。
#[tauri::command]
pub fn retirement_candidates(threshold: Option<f64>) -> Result<Vec<serde_json::Value>, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    guard
        .retirement_candidates_all(threshold.unwrap_or(0.15), 50)
        .map(|rows| {
            rows.into_iter()
                .filter_map(|r| serde_json::to_value(r).ok())
                .collect()
        })
        .map_err(|e| e.to_string())
}

/// 可逆降权（T-M4-02）：`importance - 1` + 事件留痕；返回 event_id 供撤销。
#[tauri::command]
pub fn retire_memory(id: String) -> Result<UpdateResult, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    let out = guard.retire_local_owner(&id).map_err(|e| e.to_string())?;
    Ok(UpdateResult {
        action: out.action,
        id: out.id,
        superseded_id: out.superseded_id,
        note: out.note,
        event_id: out.event_id,
    })
}

/// 恢复降权（T-M4-02）：把最近一次降权的原值写回。
#[tauri::command]
pub fn restore_retire_memory(id: String) -> Result<UpdateResult, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    let out = guard
        .restore_retire_local_owner(&id)
        .map_err(|e| e.to_string())?;
    Ok(UpdateResult {
        action: out.action,
        id: out.id,
        superseded_id: out.superseded_id,
        note: out.note,
        event_id: out.event_id,
    })
}

/// 可恢复列表（T-M4-03）：已降权（可回最初值）+ 已软删（可撤销删除）。**只读**。
#[tauri::command]
pub fn recovery_list() -> Result<Vec<serde_json::Value>, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    guard
        .recovery_list(50)
        .map(|rows| {
            rows.into_iter()
                .filter_map(|r| serde_json::to_value(r).ok())
                .collect()
        })
        .map_err(|e| e.to_string())
}

/// 恢复到最初重要度（沿 events 链回放；T-M4-03）。
#[tauri::command]
pub fn restore_to_baseline_memory(id: String) -> Result<UpdateResult, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    let out = guard
        .restore_to_baseline_local_owner(&id)
        .map_err(|e| e.to_string())?;
    Ok(UpdateResult {
        action: out.action,
        id: out.id,
        superseded_id: out.superseded_id,
        note: out.note,
        event_id: out.event_id,
    })
}

// ---------------- 备份（T-M5-01）----------------

fn backup_dir() -> std::path::PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(base)
        .join("OrbitalPro")
        .join("backups")
}

/// `YYYYMMDD-HHMMSS`（UTC），用于备份文件名。
fn ts_name() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 简单公历换算（与 proxy 的 utc_day 同思路，只需可读）
    let days = secs / 86_400;
    let mut rem = secs % 86_400;
    let h = rem / 3600;
    rem %= 3600;
    let mi = rem / 60;
    let s = rem % 60;
    let mut y = 1970i64;
    let mut d = days as i64;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let dy = if leap { 366 } else { 365 };
        if d < dy {
            break;
        }
        d -= dy;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mdays = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0;
    while m < 12 && d >= mdays[m] {
        d -= mdays[m];
        m += 1;
    }
    format!(
        "{y:04}{:02}{:02}-{:02}{:02}{:02}",
        m + 1,
        d + 1,
        h,
        mi,
        s
    )
}

/// 从备份恢复（T-M5-02 第一步）：校验 + 落成"待恢复"，**不改当前库**。
/// 真正替换发生在下次启动早期（`apply_pending_restore`）。
#[tauri::command]
pub fn stage_restore(path: String) -> Result<serde_json::Value, String> {
    let db = db_path();
    let r = orbital_core::db::stage_restore(&db, std::path::Path::new(&path))
        .map_err(|e| e.to_string())?;
    serde_json::to_value(r).map_err(|e| e.to_string())
}

/// 立即备份（Online Backup API）+ 自动校验。
#[tauri::command]
pub fn create_backup() -> Result<serde_json::Value, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    let path = backup_dir().join(format!("orbital-{}.db", ts_name()));
    let report = guard.backup_to(&path).map_err(|e| e.to_string())?;
    serde_json::to_value(report).map_err(|e| e.to_string())
}

/// 备份列表（最新在前）。
#[tauri::command]
pub fn list_backups() -> Result<Vec<serde_json::Value>, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    let list = guard.list_backups(&backup_dir()).map_err(|e| e.to_string())?;
    Ok(list
        .into_iter()
        .filter_map(|b| serde_json::to_value(b).ok())
        .collect())
}

/// 校验一个备份文件（integrity + 版本 + 计数逐项比对）。
#[tauri::command]
pub fn verify_backup(path: String) -> Result<serde_json::Value, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    let report = guard
        .verify_backup(std::path::Path::new(&path))
        .map_err(|e| e.to_string())?;
    serde_json::to_value(report).map_err(|e| e.to_string())
}

// ---------------- Agent 接入（T-M6-04）----------------

/// 探测 4 个内置 agent 的配置路径与接入状态。
#[tauri::command]
pub fn agent_connections() -> Vec<serde_json::Value> {
    orbital_core::adapter::detect_connections()
        .into_iter()
        .filter_map(|c| serde_json::to_value(c).ok())
        .collect()
}

/// T-M10-08/09：接入串行化锁。
///
/// 实测症状：点一次「一键接入」后，`sha256(配置里的 token)` 与库里 `agents.token_hash` 不一致，
/// 且库 hash 每次点击都变——最可能是**同一次点击触发多次 attach**（并发进入，都想"重写"，
/// 各自 `register_agent` 轮换一次令牌，最后写入配置的是其中一个，库里留下另一个）。
///
/// 串行化后：第二个进来的调用会看到"配置已写好且 env 完整" → 走幂等路径 → 不再轮换令牌。
static ATTACH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 一键接入一个 agent：写它的 MCP 配置（**会先备份原文件**，幂等）。
///
/// 注意：**不回传明文令牌**（红线 9）——令牌已写进该 agent 的配置里，界面只需知道成功与否。
#[tauri::command]
pub fn attach_agent(id: String) -> Result<serde_json::Value, String> {
    // 串行化：并发/重复点击时第二次会走幂等路径，不会二次轮换令牌
    let _guard = ATTACH_LOCK
        .lock()
        .map_err(|_| "接入锁异常（上一次接入失败在锁内 panic）".to_string())?;
    let specs = orbital_core::adapter::builtin_adapters();
    let spec = specs
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("未知 agent: {id}"))?;
    if !spec.supports_mcp {
        return Err(format!("{} 不支持 MCP 接入（应走 proxy）", spec.display_name));
    }
    // mcp 二进制：安装版与主程序同目录；开发版在 target/{debug,release} 同级
    let mcp = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("orbital-mcp.exe")))
        .filter(|p| p.exists())
        .ok_or_else(|| {
            "找不到 orbital-mcp.exe（安装包应自带；开发环境请先 cargo build -p orbital-mcp）"
                .to_string()
        })?;
    let db = orbital_core::db::Db::open(db_path()).map_err(|e| e.to_string())?;
    let out = orbital_core::attach::attach_by_spec(&db, &spec, &mcp.display().to_string())
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "agent_id": out.agent_id,
        "config_path": out.config_path.display().to_string(),
        "changed": out.changed,
        "backup_path": out.backup_path.map(|p| p.display().to_string()),
    }))
}

// ===== T-M10-11（ADR-051）：上游与注入配置 =====

/// `%LOCALAPPDATA%\OrbitalPro\proxy.conf.json`（与 proxy 端同路径）。
fn proxy_conf_path() -> std::path::PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(base)
        .join("OrbitalPro")
        .join("proxy.conf.json")
}

/// 读当前生效的 proxy 配置（**文件 > 环境变量 > 默认**，与 proxy 启动逻辑一致）。
#[tauri::command]
pub fn proxy_config() -> Result<serde_json::Value, String> {
    let p = proxy_conf_path();
    let parsed: Option<serde_json::Value> = std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());

    let upstream = parsed
        .as_ref()
        .and_then(|v| v.get("upstream"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| std::env::var("ORBITAL_UPSTREAM").ok())
        .unwrap_or_default();
    let inject = parsed
        .as_ref()
        .and_then(|v| v.get("inject"))
        .and_then(|v| v.as_bool())
        .unwrap_or_else(|| {
            std::env::var("ORBITAL_INJECT")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
        });
    let core_url = parsed
        .as_ref()
        .and_then(|v| v.get("core_url"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| std::env::var("ORBITAL_CORE_URL").ok())
        .unwrap_or_else(|| "http://127.0.0.1:8799".to_string());

    Ok(serde_json::json!({
        "upstream": upstream,
        "inject": inject,
        "core_url": core_url,
        "path": p.display().to_string(),
        "exists": p.exists(),
    }))
}

/// 保存 proxy 配置并**重启 proxy**（kill 子进程 → 启动器 watchdog 自动拉起新进程读新配置）。
///
/// 不做"热重载"：proxy 的 upstream/inject 在启动时解析，重读配置需要新进程；
/// 而启动器已有指数退避的自动重启，复用它比新写一套 reload 逻辑更稳（ADR-051）。
#[tauri::command]
pub fn save_proxy_config(
    state: tauri::State<'_, crate::LauncherState>,
    upstream: String,
    inject: bool,
    core_url: Option<String>,
) -> Result<serde_json::Value, String> {
    let p = proxy_conf_path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败：{e}"))?;
    }
    let body = serde_json::json!({
        "upstream": upstream.trim(),
        "inject": inject,
        "core_url": core_url
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:8799".to_string()),
    });
    let text = serde_json::to_string_pretty(&body).map_err(|e| e.to_string())?;
    // 原子写：先写 tmp 再 rename（与 attach 的写盘策略一致）
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, text.as_bytes()).map_err(|e| format!("写临时文件失败：{e}"))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("替换配置文件失败：{e}"))?;

    let restarted = {
        let mut guard = state
            .0
            .lock()
            .map_err(|_| "启动器状态锁异常（上次启动失败在锁内 panic）".to_string())?;
        match guard.as_mut() {
            Some(l) => l.restart_role("proxy"),
            None => false,
        }
    };

    Ok(serde_json::json!({
        "saved": true,
        "restarted": restarted,
        "path": p.display().to_string(),
    }))
}

/// 一个 agent 的注入适配器部署状态。
#[derive(serde::Serialize)]
pub struct InjectState {
    pub id: String,
    pub name: String,
    /// 适配器是否已部署
    pub deployed: bool,
    /// 适配器类型（file / hook / plugin / extension）
    pub kind: String,
    /// 落点路径（给人看）
    pub path: String,
    /// 状态说明（已装/未装/装了但配置没挂）
    pub detail: String,
    /// T-M10-37：这个 agent 的安装位置是**怎么找到的**（PATH / 常见目录 / 用户目录）
    pub probe_source: Option<String>,
}

fn home() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into()))
}

fn localappdata() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into()))
}

// ===== T-M10-37：agent 安装位置探测 =====
//
// 背景（用户 2026-11-15 提出的真问题）：一键接入此前**只查标准位置的文件在不在**，
// 装在非标准位置就找不到；DSH/Hermes 还依赖 CLI 在 PATH。换台电脑大概率失败。
//
// 三级策略（从最可靠到最兜底）：
//   1. `where <cmd>` —— 官方 CLI，找到即用（PATH 上的才是真在用的那个）
//   2. 常见安装目录（用环境变量拼，随机器自适应）
//   3. 用户级配置目录（`~/.xxx`）
//
// 找不到返回 None —— 调用方必须给出**明确错误**，不许静默失败（本机地雷：静默失效最难查）。

/// `where <name>` 返回第一个命中的**目录**（exe 所在目录）。
fn which_exe_dir(name: &str) -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("where")
        .arg(name)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next()?.trim();
    if first.is_empty() {
        return None;
    }
    std::path::Path::new(first)
        .parent()
        .map(|p| p.to_path_buf())
}

/// 探测某个 agent 的 home 目录，返回 (目录, 判据说明)。
fn probe_agent_home(agent: &str) -> Option<(std::path::PathBuf, String)> {
    let mut cands: Vec<(std::path::PathBuf, String)> = Vec::new();

    match agent {
        "hermes" => {
            if let Some(d) = which_exe_dir("hermes") {
                // where hermes → <home>\bin\hermes.exe ⇒ home 是父目录
                if let Some(home) = d.parent() {
                    cands.push((home.to_path_buf(), "where hermes（PATH）".into()));
                }
                cands.push((d, "where hermes（exe 目录）".into()));
            }
            cands.push((localappdata().join("hermes"), "LOCALAPPDATA\\hermes".into()));
            cands.push((home().join(".hermes"), "~/.hermes".into()));
        }
        "dsh" => {
            if let Some(d) = which_exe_dir("dsh") {
                // where dsh → <install>\resources\runtime\cli\bin\dsh.cmd ⇒ 上溯 5 级是安装根
                let mut p = d.clone();
                for _ in 0..5 {
                    if let Some(parent) = p.parent() {
                        p = parent.to_path_buf();
                    }
                }
                cands.push((p, "where dsh（PATH）".into()));
            }
            cands.push((
                localappdata().join("Programs").join("DeepSeek Harness"),
                "LOCALAPPDATA\\Programs\\DeepSeek Harness".into(),
            ));
        }
        "pi" => {
            // T-M10-37b：pi 的**配置**在 `~/.pi`（用户目录），而 `where pi` 找到的是**程序目录**。
            // 首版把 where 放最前，导致 pi 被误判"未安装"（真机截图发现）。
            // ⇒ 用户目录优先，PATH 只作为"pi 确实装了"的旁证。
            cands.push((home().join(".pi"), "~/.pi".into()));
            cands.push((
                home().join("AppData").join("Roaming").join("pi"),
                "%APPDATA%\\pi".into(),
            ));
            if let Some(d) = which_exe_dir("pi") {
                cands.push((d, "where pi（PATH）".into()));
            }
        }
        "workbuddy" => {
            if let Some(d) = which_exe_dir("workbuddy") {
                cands.push((d, "where workbuddy（PATH）".into()));
            }
            cands.push((home().join(".workbuddy"), "~/.workbuddy".into()));
            cands.push((localappdata().join("WorkBuddy"), "LOCALAPPDATA\\WorkBuddy".into()));
        }
        _ => {}
    }

    for (dir, src) in cands {
        if dir.exists() {
            return Some((dir, src));
        }
    }
    None
}

/// T-M10-26：检测四家 agent 的注入适配器是否已部署（**只读**，不动任何配置）。
///
/// 每家的判据都不是"文件存在"这么弱：
/// - DSH：插件目录存在 **且** profile 配置里挂了它
/// - Hermes：脚本存在 **且** config.yaml 的 hooks.pre_llm_call 指向它
/// - pi：扩展文件存在
/// - WorkBuddy：MEMORY.md 里有 `<!-- orbital:begin` 受控区块
#[tauri::command]
pub fn inject_status() -> Result<Vec<InjectState>, String> {
    let mut out = Vec::new();

    // ---- DSH ----
    let dsh_profile = home().join(".dsh").join("profiles").join("desktop");
    let dsh_plugin = dsh_profile.join("node_modules").join("dsh-orbital-inject");
    // T-M10-26（修误报）：DSH 插件的挂载点**不是** cordis.patch.yml，而是
    // profile/package.json 的 dependencies（`dsh plugin add` 写它）+ 包内 `dsh.bundle.patch`。
    // 之前查 cordis.patch.yml 会误报"已装但未挂载"（实测 2026-11-14 截图发现）。
    let dsh_mounted = std::fs::read_to_string(dsh_profile.join("package.json"))
        .map(|s| s.contains("dsh-orbital-inject"))
        .unwrap_or(false)
        || std::fs::read_to_string(dsh_profile.join("cordis.patch.yml"))
            .map(|s| s.contains("orbital-inject"))
            .unwrap_or(false);
    let dsh_plugin_ok = dsh_plugin.exists();
    out.push(InjectState {
        id: "dsh".into(),
        name: "DSH".into(),
        deployed: dsh_plugin_ok && dsh_mounted,
        kind: "plugin".into(),
        path: dsh_plugin.display().to_string(),
        detail: match (dsh_plugin_ok, dsh_mounted) {
            (true, true) => "插件已装且已挂载（重启 DSH 后生效）".into(),
            (true, false) => "插件已装但 profile 未挂载".into(),
            (false, _) => "未安装".into(),
        },
        probe_source: probe_agent_home("dsh").map(|(_, s)| s),
    });

    // ---- Hermes ----
    // T-M10-37：用**探测**到的 home（where hermes / 常见目录 / 用户目录），不再硬编码
    let (hermes_home, hermes_src) = probe_agent_home("hermes")
        .unwrap_or((localappdata().join("hermes"), "默认位置（探测未命中）".into()));
    let hermes_script = hermes_home.join("agent-hooks").join("orbital-recall.py");
    let hermes_cfg = hermes_home.join("config.yaml");
    let hermes_script_ok = hermes_script.exists();
    let hermes_mounted = std::fs::read_to_string(&hermes_cfg)
        .map(|s| s.contains("orbital-recall.py"))
        .unwrap_or(false);
    out.push(InjectState {
        id: "hermes".into(),
        name: "Hermes".into(),
        deployed: hermes_script_ok && hermes_mounted,
        kind: "hook".into(),
        path: hermes_script.display().to_string(),
        detail: match (hermes_script_ok, hermes_mounted) {
            (true, true) => "脚本已装且已注册 pre_llm_call".into(),
            (true, false) => "脚本已装但 config.yaml 未注册".into(),
            (false, _) => "未安装".into(),
        },
        probe_source: Some(hermes_src),
    });

    // ---- pi ----
    let (pi_home, pi_src) = probe_agent_home("pi")
        .unwrap_or((home().join(".pi"), "默认位置（探测未命中）".into()));
    let pi_ext = pi_home
        .join("agent")
        .join("extensions")
        .join("orbital.ts");
    let pi_ok = pi_ext.exists();
    out.push(InjectState {
        id: "pi".into(),
        name: "pi".into(),
        deployed: pi_ok,
        kind: "extension".into(),
        path: pi_ext.display().to_string(),
        detail: if pi_ok {
            "扩展已装（/reload 或重启 pi 生效）".into()
        } else {
            "未安装".into()
        },
        probe_source: Some(pi_src),
    });

    // ---- WorkBuddy ----
    let (wb_dir, wb_src) = probe_agent_home("workbuddy")
        .unwrap_or((home().join(".workbuddy"), "默认位置（探测未命中）".into()));
    let mut wb_md = None;
    if let Ok(entries) = std::fs::read_dir(&wb_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("user-") && name.ends_with("-personal") {
                let p = e.path().join("MEMORY.md");
                if p.exists() {
                    wb_md = Some(p);
                    break;
                }
            }
        }
    }
    let wb_block = wb_md
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.contains("<!-- orbital:begin"))
        .unwrap_or(false);
    out.push(InjectState {
        id: "workbuddy".into(),
        name: "WorkBuddy".into(),
        deployed: wb_block,
        kind: "file".into(),
        path: wb_md
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "未找到 MEMORY.md".into()),
        detail: if wb_block {
            "MEMORY.md 已写入受控区块".into()
        } else if wb_md.is_some() {
            "MEMORY.md 存在但没有 Orbital 区块".into()
        } else {
            format!("未找到 MEMORY.md（探测来源：{wb_src}）")
        },
        probe_source: Some(wb_src),
    });

    Ok(out)
}

// ===== T-M10-27：一键接入（部署 / 移除 / 自检）=====
//
// 本版**只做 WorkBuddy**（最安全：只改 MEMORY.md 的受控区块，不动其他配置）。
// 风险控制：① 写前必建 `.bak.orbital-<时间戳>` 备份；② 只替换 `<!-- orbital:begin -->`…`<!-- orbital:end -->`
// 这一段，绝不重写区块外字节；③ 区块放文件开头且 ≤300 字符（WorkBuddy 用户级 MEMORY.md 有 4000 硬上限）。

const WB_BEGIN: &str = "<!-- orbital:begin";
const WB_END: &str = "<!-- orbital:end -->";
const WB_BLOCK_BUDGET: usize = 300;

fn wb_memory_path() -> Option<std::path::PathBuf> {
    let wb = home().join(".workbuddy");
    for e in std::fs::read_dir(&wb).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("user-") && name.ends_with("-personal") {
            let p = e.path().join("MEMORY.md");
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

fn now_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

fn backup_file(path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let bak = path.with_extension(format!("md.bak.orbital-{}", now_stamp()));
    std::fs::copy(path, &bak).map_err(|e| format!("备份失败：{e}"))?;
    Ok(bak)
}

/// 生成 WorkBuddy 的受控区块（≤300 字符，只放索引不放全文）。
fn wb_block() -> String {
    let s = format!(
        "{WB_BEGIN} · Orbital 自动维护，勿手编辑 -->\n\
         记忆细节在我电脑上的 Orbital Pro 应用（记忆页可搜）。\n\
         需要更多时用 MCP 工具 memory_recall(query) / memory_get(id)。\n\
         {WB_END}\n"
    );
    if s.chars().count() > WB_BLOCK_BUDGET {
        return format!("{WB_BEGIN} · Orbital 记忆库（索引已降级） -->\n{WB_END}\n");
    }
    s
}

/// 部署注入适配器（分发）。本版支持 workbuddy / hermes。
#[tauri::command]
pub fn deploy_inject(id: String) -> Result<serde_json::Value, String> {
    match id.as_str() {
        "workbuddy" => deploy_workbuddy(),
        "hermes" => deploy_hermes(),
        "pi" => deploy_pi(),
        "dsh" => deploy_dsh(),
        other => Err(format!("本版支持 workbuddy / hermes / pi / dsh（收到 {other}）")),
    }
}

fn deploy_workbuddy() -> Result<serde_json::Value, String> {
    let path = wb_memory_path().ok_or("找不到 WorkBuddy 的 user-*-personal/MEMORY.md")?;
    let bak = backup_file(&path)?;

    let original = std::fs::read_to_string(&path).unwrap_or_default();
    let block = wb_block();
    // 已有区块 → 原位替换；没有 → 插到开头（插末尾必被 4000 截断）
    let new_content = match (original.find(WB_BEGIN), original.find(WB_END)) {
        (Some(b), Some(e)) if e > b => format!(
            "{}{}{}",
            &original[..b],
            block,
            &original[e + WB_END.len()..]
        ),
        _ => format!("{block}\n{original}"),
    };
    // 原子写：tmp + rename（同目录）
    let tmp = path.with_extension("md.orbital-tmp");
    std::fs::write(&tmp, new_content.as_bytes()).map_err(|e| format!("写临时文件失败：{e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("替换失败：{e}"))?;

    Ok(serde_json::json!({
        "ok": true,
        "agent": "workbuddy",
        "path": path.display().to_string(),
        "backup": bak.display().to_string(),
        "block_chars": block.chars().count(),
    }))
}

/// 移除注入适配器（分发）。本版支持 workbuddy / hermes。
#[tauri::command]
pub fn remove_inject(id: String) -> Result<serde_json::Value, String> {
    match id.as_str() {
        "workbuddy" => remove_workbuddy(),
        "hermes" => remove_hermes(),
        "pi" => remove_pi(),
        "dsh" => remove_dsh(),
        other => Err(format!("本版支持 workbuddy / hermes / pi / dsh（收到 {other}）")),
    }
}

fn remove_workbuddy() -> Result<serde_json::Value, String> {
    let path = wb_memory_path().ok_or("找不到 WorkBuddy 的 user-*-personal/MEMORY.md")?;
    let original = std::fs::read_to_string(&path).unwrap_or_default();
    let (Some(b), Some(e)) = (original.find(WB_BEGIN), original.find(WB_END)) else {
        return Ok(serde_json::json!({ "ok": true, "removed": false, "note": "没有 Orbital 区块" }));
    };
    if e <= b {
        return Ok(serde_json::json!({ "ok": true, "removed": false, "note": "区块不完整，未动" }));
    }
    let bak = backup_file(&path)?;
    // 去掉区块 + 紧随的一个空行
    let mut rest = format!("{}{}", &original[..b], &original[e + WB_END.len()..]);
    while rest.starts_with('\n') || rest.starts_with("\r\n") {
        rest = rest.trim_start_matches(['\r', '\n']).to_string();
    }
    let tmp = path.with_extension("md.orbital-tmp");
    std::fs::write(&tmp, rest.as_bytes()).map_err(|e| format!("写临时文件失败：{e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("替换失败：{e}"))?;
    Ok(serde_json::json!({
        "ok": true,
        "removed": true,
        "path": path.display().to_string(),
        "backup": bak.display().to_string(),
    }))
}

/// 自检：用该 agent 的身份**直查库**，确认"这个身份能召回"。
///
/// T-M10-28（修 T-M10-27 的缺陷）：原实现用裸 `TcpStream` 手写 HTTP GET，
/// 解析响应时可能失败（实测点了按钮无任何反馈）。
/// 现改为直接调 `MemoryService::recall_fused`——与 8799 端点**同一实现**，
/// 但去掉了网络与手写协议解析这两层风险。
#[tauri::command]
pub fn verify_inject(id: String) -> Result<serde_json::Value, String> {
    use orbital_core::identity::VerifiedAgent;
    use orbital_core::service::{MemoryService, RecallOptions};

    tracing::info!(agent = %id, "verify_inject: start");
    // T-M10-28b：**不用共享的 `editor()` 锁**——同步 IPC 里拿那把锁可能与设置页并发调用
    // （overview / list_memories）互相等待，导致前端 `await` 一直 pending、页面无反馈。
    // 改用**独立连接**（SQLite WAL 允许多连接）；只读查询，无写竞争。
    let db = db_path();
    if !db.exists() {
        return Err(format!("数据库不存在：{}", db.display()));
    }
    let journal = db.with_file_name("journal.ndjson");
    let svc = MemoryService::open(&db, &journal).map_err(|e| format!("打开记忆服务失败：{e}"))?;

    let va = VerifiedAgent::for_local_owner(id.clone());
    let opts = RecallOptions::default();
    let fused = svc
        .recall_fused(&va, "Orbital 项目", 3, &opts)
        .map_err(|e| format!("召回失败：{e}"))?;

    let n = fused.hits.len();
    tracing::info!(agent = %id, hits = n, "verify_inject: done");
    Ok(serde_json::json!({
        "ok": true,
        "agent": id,
        "recall_count": n,
        "degraded": fused.degraded,
        "note": if n > 0 {
            format!("能召回 {n} 条（可见性与检索都正常）")
        } else {
            "召回 0 条：该身份下没有匹配记忆（共享记忆为空时属正常）".to_string()
        },
    }))
}

/// T-M10-30：Hermes 的 hook 脚本（编译期嵌入，部署时写到 `%LOCALAPPDATA%\hermes\agent-hooks\`）。
///
/// 用 `include_str!` 而不是运行时读文件：v1 的脚本随二进制走，**不存在"安装包缺脚本"**
/// 这种半成品状态（T-M10-19 的教训：tauri build 不重编根 workspace 的 proxy，导致包内是旧版）。
const HERMES_HOOK_SCRIPT: &str = include_str!("../scripts/orbital-recall.py");

fn hermes_home() -> std::path::PathBuf {
    localappdata().join("hermes")
}

/// 部署 Hermes 注入：写脚本 + 用**官方 CLI**注册（红线：不手改 config.yaml）。
#[tauri::command]
pub fn deploy_hermes() -> Result<serde_json::Value, String> {
    let home = hermes_home();
    let hooks_dir = home.join("agent-hooks");
    std::fs::create_dir_all(&hooks_dir).map_err(|e| format!("创建目录失败：{e}"))?;
    let script = hooks_dir.join("orbital-recall.py");

    // 写脚本（已存在则备份后覆盖）
    let backup = if script.exists() {
        Some(backup_file(&script)?)
    } else {
        None
    };
    std::fs::write(&script, HERMES_HOOK_SCRIPT).map_err(|e| format!("写脚本失败：{e}"))?;

    // 注册：hermes config set hooks.pre_llm_call '<json>'
    let hermes = home.join("bin").join("hermes.exe");
    if !hermes.exists() {
        return Err(format!("找不到 hermes CLI：{}", hermes.display()));
    }
    let config_path = script.display().to_string().replace('\\', "/");
    let value = format!(
        r#"[{{"command": "{}", "timeout": 5}}]"#,
        config_path
    );
    let out = std::process::Command::new(&hermes)
        .args(["config", "set", "hooks.pre_llm_call", &value])
        .output()
        .map_err(|e| format!("调 hermes CLI 失败：{e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(format!(
            "hermes config set 失败：{}{}",
            stdout.trim(),
            err.trim()
        ));
    }

    Ok(serde_json::json!({
        "ok": true,
        "agent": "hermes",
        "path": script.display().to_string(),
        "backup": backup.map(|p| p.display().to_string()),
        "note": "脚本已写入并用官方 CLI 注册；Hermes 下一轮对话即生效",
    }))
}

/// 移除 Hermes 注入：清空 `hooks.pre_llm_call`（脚本保留，便于以后一键恢复）。
#[tauri::command]
pub fn remove_hermes() -> Result<serde_json::Value, String> {
    let home = hermes_home();
    let hermes = home.join("bin").join("hermes.exe");
    if !hermes.exists() {
        return Err(format!("找不到 hermes CLI：{}", hermes.display()));
    }
    let out = std::process::Command::new(&hermes)
        .args(["config", "set", "hooks.pre_llm_call", "[]"])
        .output()
        .map_err(|e| format!("调 hermes CLI 失败：{e}"))?;
    if !out.status.success() {
        return Err(format!(
            "hermes config set 失败：{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(serde_json::json!({
        "ok": true,
        "removed": true,
        "agent": "hermes",
        "note": "已清空 hooks.pre_llm_call（脚本保留，可一键恢复）",
    }))
}

/// T-M10-31：pi 的扩展（编译期嵌入）。部署时写到 `~/.pi/agent/extensions/orbital.ts`。
const PI_EXT_SCRIPT: &str = include_str!("../scripts/orbital-pi.ts");

fn pi_ext_path() -> std::path::PathBuf {
    home().join(".pi").join("agent").join("extensions").join("orbital.ts")
}

/// 部署 pi 注入：写扩展文件（pi 自动发现 `extensions/` 目录，`/reload` 后生效）。
#[tauri::command]
pub fn deploy_pi() -> Result<serde_json::Value, String> {
    let path = pi_ext_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败：{e}"))?;
    }
    // 已存在则备份（改名，不删除）
    let backup = if path.exists() {
        let bak = path.with_extension(format!(
            "ts.bak.orbital-{}",
            now_stamp()
        ));
        std::fs::copy(&path, &bak).map_err(|e| format!("备份失败：{e}"))?;
        Some(bak)
    } else {
        None
    };
    std::fs::write(&path, PI_EXT_SCRIPT).map_err(|e| format!("写扩展失败：{e}"))?;
    Ok(serde_json::json!({
        "ok": true,
        "agent": "pi",
        "path": path.display().to_string(),
        "backup": backup.map(|p| p.display().to_string()),
        "note": "扩展已写入；在 pi 里执行 /reload 或重启 pi 生效",
    }))
}

/// 移除 pi 注入：把扩展**改名**为 `.disabled`（保留可恢复，不删除）。
#[tauri::command]
pub fn remove_pi() -> Result<serde_json::Value, String> {
    let path = pi_ext_path();
    if !path.exists() {
        return Ok(serde_json::json!({ "ok": true, "removed": false, "note": "扩展不存在" }));
    }
    let disabled = path.with_extension(format!("ts.disabled-{}", now_stamp()));
    std::fs::rename(&path, &disabled).map_err(|e| format!("改名失败：{e}"))?;
    Ok(serde_json::json!({
        "ok": true,
        "removed": true,
        "agent": "pi",
        "note": format!("已改名为 {}（可手动改回恢复）", disabled.display()),
    }))
}

/// T-M10-33：DSH 注入插件包（编译期嵌入三个文件）。
const DSH_PLUGIN_PKG: &str = include_str!("../scripts/dsh-orbital-inject/package.json");
const DSH_PLUGIN_PATCH: &str = include_str!("../scripts/dsh-orbital-inject/cordis.patch.yml");
const DSH_PLUGIN_LIB: &str = include_str!("../scripts/dsh-orbital-inject/lib/index.js");

/// 部署 DSH 注入：把插件包写到临时目录 → `dsh plugin add`（官方命令）。
///
/// 为什么不直接写 profile 的 node_modules：DSH 的插件安装是 `dsh plugin add` 的职责，
/// 它负责 link + 写 package.json 依赖 + 生成 bundle 层；手写会破坏它的状态。
#[tauri::command]
pub fn deploy_dsh() -> Result<serde_json::Value, String> {
    // T-M10-33b：**不能用 `%TEMP%`**——`dsh plugin add` 会以 link 方式指向这个目录，
    // 而 Temp 可能被系统/清理工具删掉，插件随之失效。
    // 改用持久目录：`%LOCALAPPDATA%\OrbitalPro\adapters\dsh-orbital-inject`。
    let dir = localappdata()
        .join("OrbitalPro")
        .join("adapters")
        .join("dsh-orbital-inject");
    std::fs::create_dir_all(dir.join("lib")).map_err(|e| format!("创建目录失败：{e}"))?;
    std::fs::write(dir.join("package.json"), DSH_PLUGIN_PKG)
        .map_err(|e| format!("写 package.json 失败：{e}"))?;
    std::fs::write(dir.join("cordis.patch.yml"), DSH_PLUGIN_PATCH)
        .map_err(|e| format!("写 cordis.patch.yml 失败：{e}"))?;
    std::fs::write(dir.join("lib").join("index.js"), DSH_PLUGIN_LIB)
        .map_err(|e| format!("写 index.js 失败：{e}"))?;

    let out = std::process::Command::new("cmd")
        .args([
            "/c",
            "dsh",
            "plugin",
            "--profile",
            "desktop",
            "add",
            &dir.display().to_string(),
        ])
        .output()
        .map_err(|e| format!("调 dsh CLI 失败：{e}"))?;
    if !out.status.success() {
        return Err(format!(
            "dsh plugin add 失败：{}{}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(serde_json::json!({
        "ok": true,
        "agent": "dsh",
        "path": dir.display().to_string(),
        "note": "插件已安装；重启 DSH 后生效",
    }))
}

/// 移除 DSH 注入：`dsh plugin remove`（官方命令）。
#[tauri::command]
pub fn remove_dsh() -> Result<serde_json::Value, String> {
    let out = std::process::Command::new("cmd")
        .args([
            "/c",
            "dsh",
            "plugin",
            "--profile",
            "desktop",
            "remove",
            "dsh-orbital-inject",
        ])
        .output()
        .map_err(|e| format!("调 dsh CLI 失败：{e}"))?;
    if !out.status.success() {
        return Err(format!(
            "dsh plugin remove 失败：{}{}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(serde_json::json!({
        "ok": true,
        "removed": true,
        "agent": "dsh",
        "note": "插件已移除；重启 DSH 后不再注入",
    }))
}

/// 清理孤儿边（T-M4-04）：只删指向不存在记忆的边；软删记忆的边保留。
#[tauri::command]
pub fn cleanup_orphan_edges() -> Result<serde_json::Value, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    let out = guard.cleanup_orphan_edges().map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "removed_edges": out.removed_edges,
        "kept_edges": out.kept_edges,
    }))
}

/// 补算缺失向量（T-M4-04）：无端点时回 degraded 原因，不假装成功。
#[tauri::command]
pub fn reembed_pending() -> Result<serde_json::Value, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    let out = guard.reembed_pending(50).map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "tried": out.tried,
        "ok": out.ok,
        "failed": out.failed,
        "degraded": out.degraded,
    }))
}

/// 重新扫描冲突（幂等；返回新增条数）。
///
/// 为什么是手动触发而不是写时自动：M2 还没有 `memory_facts` 的写入路径
/// （M3 才有），此时"每次写都扫全表"是纯浪费；等 M3 接上 facts 写入后
/// 再考虑在写入事务里做增量检测。
#[tauri::command]
pub fn detect_conflicts() -> Result<i64, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    guard
        .detect_conflicts()
        .map(|n| n as i64)
        .map_err(|e| e.to_string())
}

/// 裁决一条冲突：`keep_a` / `keep_b` / `keep_both` / `merged`。
#[tauri::command]
pub fn resolve_conflict(id: i64, resolution: String) -> Result<(), String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    guard
        .resolve_conflict(id, &resolution)
        .map_err(|e| e.to_string())
}

// ---------------- 撤销 / 回滚（T-M2-07）----------------

#[derive(serde::Serialize)]
pub struct UndoItem {
    pub event_id: i64,
    pub memory_id: String,
    pub note: String,
}

/// 撤销一条操作（编辑 / 内容变更 / 删除 / 失效 / 合并；**硬删会被明确拒绝**）。
#[tauri::command]
pub fn undo_event(event_id: i64) -> Result<UndoItem, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    let out = guard.undo_event(event_id).map_err(|e| e.to_string())?;
    Ok(UndoItem {
        event_id: out.event_id,
        memory_id: out.memory_id,
        note: out.note,
    })
}

/// 活动日志的一行。`undoable` / `undone` 由**服务端**判定，前端不许自己猜。
#[derive(serde::Serialize)]
pub struct ActivityItem {
    pub event_id: i64,
    pub operation: String,
    pub memory_id: String,
    pub summary: String,
    pub agent_id: String,
    pub created_at: f64,
    pub undoable: bool,
    pub undone: bool,
}

/// 最近操作（维护页「最近操作」区块；最近的在最前）。
#[tauri::command]
pub fn list_activity(limit: Option<i64>) -> Result<Vec<ActivityItem>, String> {
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    let rows = guard
        .list_activity(limit.unwrap_or(30).clamp(1, 200))
        .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|r| ActivityItem {
            event_id: r.event_id,
            operation: r.operation,
            memory_id: r.memory_id,
            summary: r.summary,
            agent_id: r.agent_id,
            created_at: r.created_at,
            undoable: r.undoable,
            undone: r.undone,
        })
        .collect())
}

// ---------------- 关系（T-M2-08 走查补 J4：把两条记忆连起来）----------------

/// 建立一条关系。`edge_type` 只接受五种合法值（与 schema CHECK 一致）。
#[tauri::command]
pub fn link_memory(
    source_id: String,
    target_id: String,
    edge_type: String,
    weight: Option<f64>,
) -> Result<(), String> {
    if !matches!(
        edge_type.as_str(),
        "temporal" | "semantic" | "causal" | "entity" | "supersedes"
    ) {
        return Err(format!("非法关系类型：{edge_type}"));
    }
    if source_id == target_id {
        return Err("不能把一条记忆连到自己（自环没有信息量）".into());
    }
    let svc = editor()?;
    let guard = svc.lock().map_err(|_| "记忆服务锁异常".to_string())?;
    guard
        .link_local_owner(&source_id, &target_id, &edge_type, weight.unwrap_or(1.0))
        .map_err(|e| e.to_string())
}

/// 一条关联（详情页「关联」区）。
#[derive(serde::Serialize)]
pub struct RelatedItem {
    pub id: String,
    pub summary: String,
    pub edge_type: String,
    pub weight: f64,
}

/// 某条记忆的关联列表（只读连接直接查；图遍历的完整门控在 core 的 `related`）。
#[tauri::command]
pub fn related_memories(id: String) -> Result<Vec<RelatedItem>, String> {
    let conn = open_ro()?;
    let mut stmt = conn
        .prepare(
            "SELECT m.id, m.content, e.edge_type, e.weight
               FROM edges e JOIN memories m ON m.id = e.target_id
              WHERE e.source_id = ?1 AND m.deleted_at IS NULL
             UNION
             SELECT m.id, m.content, e.edge_type, e.weight
               FROM edges e JOIN memories m ON m.id = e.source_id
              WHERE e.target_id = ?1 AND m.deleted_at IS NULL",
        )
        .map_err(|e| format!("准备关联查询失败：{e}"))?;
    let rows = stmt
        .query_map(params![id], |r| {
            let content: String = r.get(1)?;
            Ok(RelatedItem {
                id: r.get(0)?,
                summary: summarize(&content, 60),
                edge_type: r.get(2)?,
                weight: r.get(3)?,
            })
        })
        .map_err(|e| format!("查询关联失败：{e}"))?;
    let mut v = Vec::new();
    for x in rows {
        v.push(x.map_err(|e| format!("读关联失败：{e}"))?);
    }
    Ok(v)
}

/// 转义 LIKE 的通配符（`%`/`_`/`\`），配合 SQL 的 `ESCAPE '\'` 使用。
///
/// 审计修复：此前直接 `format!("%{}%", k)`，搜 `50%` 会把 `%` 当通配符 → 命中全部。
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}