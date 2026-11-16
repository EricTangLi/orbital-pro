//! schema v1：五张表 + 索引 + 全文索引 + 触发器。
//!
//! 唯一来源：`docs/04-记忆系统设计.md §2`。
//! **不要在这里自创字段**；改字段必须先改文档 + 写 ADR。
//!
//! 向量表：按 `docs/07` ADR-019，用**普通表 + BLOB**而不是 sqlite-vec 的 `vec0` 虚表
//! （`vec0` 需要加载扩展，构建与分发成本高；先用 Rust 算余弦，M3 有基线再评估）。

/// v1 的完整 DDL。**幂等**：可重复执行。
pub const SCHEMA_V1: &str = r#"
-- ============ ① 自由文本通道 ============
CREATE TABLE IF NOT EXISTS memories (
  id                 TEXT PRIMARY KEY,
  content            TEXT NOT NULL,
  agent_id           TEXT NOT NULL,
  scope              TEXT NOT NULL DEFAULT 'private'
                     CHECK(scope IN ('private','shared','entity')),
  origin             TEXT NOT NULL DEFAULT 'agent_derived'
                     CHECK(origin IN ('user_endorsed','agent_derived')),
  category           TEXT NOT NULL DEFAULT 'fact',
  importance         INTEGER NOT NULL DEFAULT 3 CHECK(importance BETWEEN 1 AND 5),
  confidence         REAL NOT NULL DEFAULT 0.8,
  source_ref         TEXT,
  session_id         TEXT,
  tags               TEXT NOT NULL DEFAULT '[]',
  entities           TEXT NOT NULL DEFAULT '[]',
  created_at         REAL NOT NULL,
  updated_at         REAL NOT NULL,
  valid_from         REAL NOT NULL,
  valid_to           REAL,
  recorded_at        REAL NOT NULL,
  last_accessed_at   REAL,
  access_count       INTEGER NOT NULL DEFAULT 0,
  pinned             INTEGER NOT NULL DEFAULT 0 CHECK(pinned IN (0,1)),
  superseded_by      TEXT,
  -- ⚠️ M1–M3 该列**不参与任何决策**（计算在 M4，见 ADR-013）；禁止 ORDER BY 它
  effective_importance REAL NOT NULL DEFAULT 0.5,
  content_hash       TEXT NOT NULL,
  embedding_status   TEXT NOT NULL DEFAULT 'pending'
                     CHECK(embedding_status IN ('pending','ok','failed','skipped')),
  embedding_source   TEXT,
  deleted_at         REAL
);

-- 去重：同一 agent 同内容只存一条（软删的不算）
CREATE UNIQUE INDEX IF NOT EXISTS ux_mem_agent_content
  ON memories(agent_id, content_hash) WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS ix_mem_agent_active
  ON memories(agent_id, deleted_at, created_at DESC);
CREATE INDEX IF NOT EXISTS ix_mem_scope_valid
  ON memories(scope, valid_to) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS ix_mem_retention
  ON memories(deleted_at, pinned, effective_importance);
CREATE INDEX IF NOT EXISTS ix_mem_entity
  ON memories(entities) WHERE deleted_at IS NULL;

-- ============ ② 结构化属性通道 ============
CREATE TABLE IF NOT EXISTS memory_facts (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  memory_id    TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
  agent_id     TEXT NOT NULL,
  entity_type  TEXT NOT NULL,
  entity_id    TEXT NOT NULL,
  property     TEXT NOT NULL,
  value        TEXT NOT NULL,
  confidence   REAL NOT NULL DEFAULT 0.8,
  valid_from   REAL NOT NULL,
  valid_to     REAL,
  recorded_at  REAL NOT NULL
);

CREATE INDEX IF NOT EXISTS ix_facts_entity
  ON memory_facts(entity_type, entity_id, property, valid_to);
CREATE INDEX IF NOT EXISTS ix_facts_agent
  ON memory_facts(agent_id, valid_to);

-- ============ ③ 图关系通道（五类边，含 supersedes）============
CREATE TABLE IF NOT EXISTS edges (
  source_id  TEXT NOT NULL,
  target_id  TEXT NOT NULL,
  edge_type  TEXT NOT NULL CHECK(edge_type IN
             ('temporal','semantic','causal','entity','supersedes')),
  weight     REAL NOT NULL DEFAULT 1.0,
  metadata   TEXT NOT NULL DEFAULT '{}',
  created_at REAL NOT NULL,
  PRIMARY KEY (source_id, target_id, edge_type),
  FOREIGN KEY (source_id) REFERENCES memories(id) ON DELETE CASCADE,
  FOREIGN KEY (target_id) REFERENCES memories(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS ix_edges_type   ON edges(edge_type);
CREATE INDEX IF NOT EXISTS ix_edges_source ON edges(source_id, edge_type);
CREATE INDEX IF NOT EXISTS ix_edges_target ON edges(target_id, edge_type);

-- ============ ④ 审计事件（journal 回放也依赖它）============
CREATE TABLE IF NOT EXISTS memory_events (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  operation   TEXT NOT NULL,
  memory_id   TEXT,
  actor       TEXT NOT NULL,
  detail      TEXT NOT NULL DEFAULT '{}',
  created_at  REAL NOT NULL
);

CREATE INDEX IF NOT EXISTS ix_events_memory ON memory_events(memory_id, created_at);
CREATE INDEX IF NOT EXISTS ix_events_time   ON memory_events(created_at);

-- ============ ⑤ agent 接入状态 ============
CREATE TABLE IF NOT EXISTS agents (
  id               TEXT PRIMARY KEY,
  display_name     TEXT NOT NULL,
  mode             TEXT CHECK(mode IN ('mcp','proxy','off')),
  status           TEXT CHECK(status IN ('connected','disconnected','error')),
  config_path      TEXT,
  config_sha256    TEXT,
  -- 令牌：只存哈希（不存明文），接入时一次性返回明文给用户/agent
  token_hash       TEXT,
  token_issued_at  REAL,
  last_verified_at REAL,
  created_at       REAL NOT NULL
);

-- ============ ⑥ 向量（ADR-019：普通表 + BLOB，不是 vec0）============
-- embedding 用 f32 小端字节序列；dim 记录维度，改维度 = 换向量空间（必须重建）
CREATE TABLE IF NOT EXISTS memories_vec (
  memory_id  TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
  agent_id   TEXT NOT NULL,
  embedding  BLOB NOT NULL,
  dim        INTEGER NOT NULL,
  updated_at REAL NOT NULL
);

CREATE INDEX IF NOT EXISTS ix_vec_agent ON memories_vec(agent_id);

-- ============ ⑦ 全文索引（FTS5，外部内容表）============
-- tokenize='trigram' 对中文更友好；2 字查询另走 LIKE 兜底（docs/04 §4）
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
  content,
  tags_text,
  entities_text,
  content='memories',
  content_rowid='rowid',
  tokenize='trigram'
);

-- 三个同步触发器
DROP TRIGGER IF EXISTS trg_mem_ai;
CREATE TRIGGER trg_mem_ai AFTER INSERT ON memories BEGIN
  INSERT INTO memories_fts(rowid, content, tags_text, entities_text)
  VALUES (new.rowid, new.content, new.tags, new.entities);
END;

DROP TRIGGER IF EXISTS trg_mem_ad;
CREATE TRIGGER trg_mem_ad AFTER DELETE ON memories BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, content, tags_text, entities_text)
  VALUES ('delete', old.rowid, old.content, old.tags, old.entities);
END;

DROP TRIGGER IF EXISTS trg_mem_au;
CREATE TRIGGER trg_mem_au AFTER UPDATE ON memories BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, content, tags_text, entities_text)
  VALUES ('delete', old.rowid, old.content, old.tags, old.entities);
  INSERT INTO memories_fts(rowid, content, tags_text, entities_text)
  VALUES (new.rowid, new.content, new.tags, new.entities);
END;
"#;

/// v1 期望存在的表（体检与测试用）。
/// schema v2 新增：L0 原始对话表（ADR-031）。
///
/// **为什么独立成表**（不是给 `memories` 加字段）：
/// 1. 体量差 1000 倍（对话原文 vs 结论）；
/// 2. 字段零重叠（`role/content/round` vs `importance/scope`）；
/// 3. 索引策略不同（L0 只需 FTS）。
pub const SCHEMA_V2_ADDITIONS: &str = r#"
CREATE TABLE IF NOT EXISTS conversations (
  id            TEXT PRIMARY KEY,              -- conv_<uuid>
  agent_id      TEXT NOT NULL,
  session_id    TEXT NOT NULL,
  round_index   INTEGER NOT NULL,
  role          TEXT NOT NULL CHECK(role IN ('user','assistant','tool','system')),
  content       TEXT NOT NULL,
  raw_json      TEXT,                          -- 原始事件（含工具调用）
  created_at    REAL NOT NULL,
  -- 抽取状态：**失败不推进** -> 可重放（docs/04 §3.5 的硬要求）
  extracted_at  REAL,
  extract_error TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_conv_round
  ON conversations(agent_id, session_id, round_index);
CREATE INDEX IF NOT EXISTS ix_conv_pending
  ON conversations(extracted_at) WHERE extracted_at IS NULL;
CREATE INDEX IF NOT EXISTS ix_conv_session
  ON conversations(agent_id, session_id, round_index);

-- L0 全文索引（只有 FTS，不需要向量）
CREATE VIRTUAL TABLE IF NOT EXISTS conversations_fts USING fts5(
  content,
  content='conversations',
  content_rowid='rowid',
  tokenize='trigram'
);
CREATE TRIGGER IF NOT EXISTS trg_conv_ai AFTER INSERT ON conversations BEGIN
  INSERT INTO conversations_fts(rowid, content) VALUES (new.rowid, new.content);
END;
CREATE TRIGGER IF NOT EXISTS trg_conv_ad AFTER DELETE ON conversations BEGIN
  INSERT INTO conversations_fts(conversations_fts, rowid, content) VALUES('delete', old.rowid, old.content);
END;
CREATE TRIGGER IF NOT EXISTS trg_conv_au AFTER UPDATE ON conversations BEGIN
  INSERT INTO conversations_fts(conversations_fts, rowid, content) VALUES('delete', old.rowid, old.content);
  INSERT INTO conversations_fts(rowid, content) VALUES (new.rowid, new.content);
END;
"#;

/// v2 迁移：删除 `memories.layer`（ADR-031 决策二）。
/// 先检测列是否存在，避免在已是 v2 的库上重复执行报错。
pub const SCHEMA_V2_DROP_LAYER: &str = r#"
ALTER TABLE memories DROP COLUMN layer;
"#;

/// v3 增量：冲突收件箱（`docs/04 §7` 规则 + `docs/11` P1-4 的修复）。
///
/// **为什么是表不是视图**（P1-4 要求把这件事定下来）：
/// 1. 冲突要能记**裁决结果**（keep_a / keep_b / keep_both / merged）——视图存不下决定；
/// 2. "都保留"意味着这批事实不再算冲突，必须有地方记 `ignored`，
///    否则每次扫描都会把同一组重新报出来（用户会以为处理没生效）。
///
/// 字段与检测规则（`docs/04 §7`）：
/// - 同一 `(entity_type, entity_id, property)` 存在多个 `valid_to IS NULL` 且 value 不同 → 冲突
/// - `memory_a` / `memory_b` 是两条**承载冲突值**的记忆（注入排除要靠它们，不能靠 LIKE 匹配 JSON）
/// - `fact_ids` 存两侧的 `memory_facts.id`（JSON 数组），裁决"留 A"时据此把 B 侧的 fact 标失效
pub const SCHEMA_V3_ADDITIONS: &str = r#"
CREATE TABLE IF NOT EXISTS memory_conflicts (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  entity_type TEXT NOT NULL,
  entity_id   TEXT NOT NULL,
  property    TEXT NOT NULL,
  memory_a    TEXT NOT NULL,
  memory_b    TEXT NOT NULL,
  value_a     TEXT NOT NULL,
  value_b     TEXT NOT NULL,
  fact_ids    TEXT NOT NULL DEFAULT '[]',
  status      TEXT NOT NULL DEFAULT 'open'
              CHECK(status IN ('open','resolved','ignored')),
  resolution  TEXT,
  created_at  REAL NOT NULL,
  resolved_at REAL,
  UNIQUE(entity_type, entity_id, property, memory_a, memory_b)
);
CREATE INDEX IF NOT EXISTS ix_conflicts_open
  ON memory_conflicts(status, created_at DESC);
"#;

pub const V2_TABLES: &[&str] = &["conversations"];
pub const V2_VIRTUAL_TABLES: &[&str] = &["conversations_fts"];
pub const V3_TABLES: &[&str] = &["memory_conflicts"];

pub const V1_TABLES: &[&str] = &[
    "memories",
    "memory_facts",
    "edges",
    "memory_events",
    "agents",
    "memories_vec",
];

/// v1 期望存在的虚拟表（FTS5）。
pub const V1_VIRTUAL_TABLES: &[&str] = &["memories_fts"];
