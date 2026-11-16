//! 数据库：打开 / 版本自检 / v1 建表。
//!
//! 设计口径见 `docs/04 §2`；向量表按 `docs/07` ADR-019 用普通表 + BLOB。

use std::path::{Path, PathBuf};

use crate::error::{OrbitalError, Result};
use crate::schema::{
    SCHEMA_V1, SCHEMA_V2_ADDITIONS, SCHEMA_V2_DROP_LAYER, SCHEMA_V3_ADDITIONS, V1_TABLES,
    V1_VIRTUAL_TABLES,
};

/// 当前代码期望的 schema 版本。改 DDL 必须 +1 并写迁移。
/// - v1：五张表 + 向量 + FTS（M1）
/// - v2：`conversations`（ADR-031）+ 删 `memories.layer`
/// - v3：`memory_conflicts` 冲突收件箱（T-M2-06 / docs/11 P1-4）
pub const SCHEMA_VERSION: i64 = 3;

#[derive(Debug)]
pub struct Db {
    conn: rusqlite::Connection,
}

impl Db {
    /// 打开（或创建）数据库并保证 schema 就绪。
    ///
    /// - 新库（user_version=0）→ 执行 SCHEMA_V1 并置 1
    /// - 版本 = 1 → 直接服务
    /// - 其他版本 → `SchemaTooOld`（**拒绝服务、不动数据**，docs/04 §8 F4）
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| OrbitalError::Internal(format!("建数据目录失败: {e}")))?;
            }
        }
        let conn = rusqlite::Connection::open(path)
            .map_err(|e| OrbitalError::Internal(format!("打开数据库失败: {e}")))?;

        // 并发基线：docs/04 §3（5 条 PRAGMA，来自 TencentDB 实测）
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;\
             PRAGMA busy_timeout = 5000;\
             PRAGMA foreign_keys = ON;\
             PRAGMA cache_size = -65536;\
             PRAGMA mmap_size = 134217728;\
             PRAGMA wal_autocheckpoint = 1000;",
        )
        .map_err(|e| OrbitalError::Internal(format!("设置 PRAGMA 失败: {e}")))?;

        let current: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| OrbitalError::Internal(format!("读取 user_version 失败: {e}")))?;

        // ⚠️ 2026-11-12 修（schema v2 迁移）：
        // 原判断 `current != 0 && current != SCHEMA_VERSION` 会把 **v1 旧库直接拒绝**，
        // 导致 v1->v2 迁移逻辑永远执行不到。
        // 正确语义：**未来版本**（current > 本程序支持）才拒绝；
        // 旧版本（current < SCHEMA_VERSION）走迁移；0 = 新库。
        if current > SCHEMA_VERSION {
            return Err(OrbitalError::SchemaTooOld);
        }

        // ---------- schema 建立 / 迁移（ADR-031 v1→v2；T-M2-06 v2→v3）----------
        if current == 0 {
            // 新库：v1 基线 + v2/v3 增量
            conn.execute_batch(SCHEMA_V1)
                .map_err(|e| OrbitalError::Internal(format!("建 v1 表失败: {e}")))?;
            conn.execute_batch(SCHEMA_V2_ADDITIONS)
                .map_err(|e| OrbitalError::Internal(format!("建 v2 表失败: {e}")))?;
            // 新库的 memories 由 SCHEMA_V1 建出（含 layer）-> 同样删掉，保证与 v2 一致
            let _ = conn.execute_batch(SCHEMA_V2_DROP_LAYER);
            conn.execute_batch(SCHEMA_V3_ADDITIONS)
                .map_err(|e| OrbitalError::Internal(format!("建 v3 表失败: {e}")))?;
        } else if current < SCHEMA_VERSION {
            // 旧库**逐步**迁移：每一步都幂等，可单独重跑
            tracing::info!(from = current, to = SCHEMA_VERSION, "正在迁移 schema");
            if current < 2 {
                conn.execute_batch(SCHEMA_V2_ADDITIONS)
                    .map_err(|e| OrbitalError::Internal(format!("迁移加表(v2)失败: {e}")))?;
                match conn.execute_batch(SCHEMA_V2_DROP_LAYER) {
                    Ok(_) => {}
                    Err(e) => {
                        // 列可能已不存在（幂等重跑）；记录但不致命
                        tracing::warn!(%e, "删 layer 列跳过（可能已删除）");
                    }
                }
            }
            if current < 3 {
                conn.execute_batch(SCHEMA_V3_ADDITIONS)
                    .map_err(|e| OrbitalError::Internal(format!("迁移加表(v3)失败: {e}")))?;
            }
        }

        if current != SCHEMA_VERSION {
            conn.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION))
                .map_err(|e| OrbitalError::Internal(format!("写 user_version 失败: {e}")))?;
        }

        Ok(Self { conn })
    }

    pub fn sqlite_version(&self) -> Result<String> {
        self.conn
            .query_row("SELECT sqlite_version()", [], |r| r.get(0))
            .map_err(|e| OrbitalError::Internal(format!("查询 sqlite_version 失败: {e}")))
    }

    pub fn user_version(&self) -> Result<i64> {
        self.conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| OrbitalError::Internal(format!("读取 user_version 失败: {e}")))
    }

    /// 返回实际存在的表名（体检用：缺表要能被发现，而不是运行时才炸）。
    pub fn table_names(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type IN ('table','view') ORDER BY name")
            .map_err(|e| OrbitalError::Internal(format!("查询表名失败: {e}")))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| OrbitalError::Internal(format!("查询表名失败: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| OrbitalError::Internal(format!("读表名失败: {e}")))?);
        }
        Ok(out)
    }

    /// 自检：v1 要求的表是否齐全（缺一个就返回缺失列表）。
    pub fn missing_v1_tables(&self) -> Result<Vec<String>> {
        let names = self.table_names()?;
        let mut missing = Vec::new();
        for t in V1_TABLES.iter().chain(V1_VIRTUAL_TABLES.iter()) {
            if !names.iter().any(|n| n == t) {
                missing.push((*t).to_string());
            }
        }
        Ok(missing)
    }

    pub fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }

    /// 写路径需要 `&mut Connection`（开启 `BEGIN IMMEDIATE` 事务）。
    pub fn conn_mut(&mut self) -> &mut rusqlite::Connection {
        &mut self.conn
    }
}

// ---------------- 备份恢复（T-M5-02）----------------

/// 待恢复文件路径（与 db 同目录）。
///
/// 为什么用"待恢复 + 重启生效"而不是进程内改库：
/// 当前库被**多个连接**打开（GUI 读连接 + writer 写线程），在进程内整体替换内容会与外键/触发器/FTS 打架；
/// 而 Windows 下文件被占用时又无法直接覆盖。⇒ 先落一个 pending 文件，**下次启动早期**（还没有任何连接）替换，
/// 既简单又不会出现"一半新一半旧"。
pub fn pending_restore_path(db_path: &Path) -> PathBuf {
    db_path.with_file_name("restore-pending.db")
}

/// `stage_restore` 的结果（给 GUI 显示）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct StageReport {
    pub pending_path: String,
    pub schema_version: i64,
    pub memories: i64,
    /// 恢复必须重启应用才生效
    pub needs_restart: bool,
}

/// 第一步：校验备份 + 落成"待恢复"。**不改当前库**。
pub fn stage_restore(db_path: &Path, backup_path: &Path) -> Result<StageReport> {
    // 只读打开备份，校验完整性
    let conn = rusqlite::Connection::open_with_flags(
        backup_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| OrbitalError::InvalidArgument {
        reason: format!("打不开备份文件（可能损坏）: {e}"),
    })?;
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| OrbitalError::InvalidArgument {
            reason: format!("integrity_check 失败: {e}"),
        })?;
    if integrity != "ok" {
        return Err(OrbitalError::InvalidArgument {
            reason: format!("备份完整性检查未通过: {integrity}"),
        });
    }
    let ver: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(map_db_err)?;
    let memories: i64 = conn
        .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
        .map_err(map_db_err)?;
    drop(conn);

    // 备份 schema 不能比当前库新（否则迁移逻辑未知）
    let cur_ver: i64 = if db_path.exists() {
        let c = rusqlite::Connection::open(db_path).map_err(map_db_err)?;
        c.query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(map_db_err)?
    } else {
        0
    };
    if ver > cur_ver {
        return Err(OrbitalError::InvalidArgument {
            reason: format!("备份来自更高版本的 schema（v{ver} > 当前 v{cur_ver}），不能恢复"),
        });
    }

    let pending = pending_restore_path(db_path);
    std::fs::copy(backup_path, &pending)
        .map_err(|e| OrbitalError::Internal(format!("写入待恢复文件失败: {e}")))?;
    Ok(StageReport {
        pending_path: pending.display().to_string(),
        schema_version: ver,
        memories,
        needs_restart: true,
    })
}

/// 第二步：**启动早期**（任何连接打开之前）替换数据库。返回自动备份路径。
///
/// 顺序很重要：
/// 1. 先 checkpoint 当前库的 WAL（否则替换主文件会丢 WAL 里的页）
/// 2. 把当前库复制成 `pre-restore-*.db`（保底）
/// 3. 用 pending 覆盖当前库，删掉 pending
pub fn apply_pending_restore(db_path: &Path) -> Result<Option<PathBuf>> {
    let pending = pending_restore_path(db_path);
    if !pending.exists() {
        return Ok(None);
    }
    // 1) 当前库 checkpoint 成单文件（WAL 合并回主库）
    if db_path.exists() {
        let c = rusqlite::Connection::open(db_path).map_err(map_db_err)?;
        let _ = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        let _ = c.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get::<_, String>(0));
        drop(c);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
    }
    // 2) 保底备份当前库
    let pre = db_path.with_file_name(format!("pre-restore-{}.db", now_stamp()));
    if db_path.exists() {
        std::fs::copy(db_path, &pre)
            .map_err(|e| OrbitalError::Internal(format!("保底备份失败: {e}")))?;
    }
    // 3) 替换
    std::fs::copy(&pending, db_path)
        .map_err(|e| OrbitalError::Internal(format!("应用恢复失败: {e}")))?;
    std::fs::remove_file(&pending)
        .map_err(|e| OrbitalError::Internal(format!("清理待恢复文件失败: {e}")))?;
    Ok(Some(pre))
}

/// `YYYYMMDD-HHMMSS`（UTC），仅用于备份文件名。
/// 本模块的数据库错误映射（`service.rs` 有一份同名私有实现，不跨模块共享）。
fn map_db_err(e: rusqlite::Error) -> OrbitalError {
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

fn now_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
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
    format!("{y:04}{:02}{:02}-{:02}{:02}{:02}", m + 1, d + 1, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("orbital-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("t.db")
    }

    #[test]
    fn opens_new_db_and_creates_all_v1_tables() {
        let path = tmp_path("v1");
        let db = Db::open(&path).expect("应能建库");
        assert_eq!(db.user_version().unwrap(), SCHEMA_VERSION);
        let missing = db.missing_v1_tables().unwrap();
        assert!(missing.is_empty(), "缺表: {:?}", missing);

        // bundled 版本必须 >= 3.54（ADR-002）
        let v = db.sqlite_version().unwrap();
        let mm: f64 = v
            .rsplit_once('.')
            .map(|(maj, _)| maj)
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        assert!(mm >= 3.54, "bundled SQLite 版本过低: {v}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }


    /// ADR-031 决策一：新库必须有 conversations 表（L0 原始对话）。
    #[test]
    fn v2_adds_conversations_table() {
        let path = tmp_path("v2-conv");
        let db = Db::open(&path).expect("建库");
        let names = db.table_names().unwrap();
        assert!(names.contains(&"conversations".to_string()), "应有 conversations 表");
        assert!(
            names.contains(&"conversations_fts".to_string()),
            "应有 conversations_fts"
        );
        assert_eq!(db.user_version().unwrap(), 3, "新库应是 v3");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// ADR-031 决策二：新库的 memories **不得**再有 layer 列。
    #[test]
    fn v2_drops_layer_column() {
        let path = tmp_path("v2-nolayer");
        let db = Db::open(&path).expect("建库");
        let cols: Vec<String> = {
            let c = db.conn();
            let mut stmt = c.prepare("PRAGMA table_info(memories)").unwrap();
            let it = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
            it.map(|x| x.unwrap()).collect()
        };
        assert!(
            !cols.contains(&"layer".to_string()),
            "layer 字段必须已删除，实际列: {cols:?}"
        );
        // 关键字段仍在
        for must in ["id", "content", "agent_id", "superseded_by", "deleted_at"] {
            assert!(cols.contains(&must.to_string()), "不应误删 {must}");
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// v1 库（含 layer）能迁移到 v2：加表 + 删列 + 数据不丢。
    #[test]
    fn v1_db_migrates_to_v2_keeping_data() {
        let path = tmp_path("v1-to-v2");
        // 手工造一个 v1 库：建 memories（含 layer）+ 写一条 + user_version=1
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE memories (
                   id TEXT PRIMARY KEY, content TEXT NOT NULL, agent_id TEXT NOT NULL,
                   scope TEXT NOT NULL DEFAULT 'private',
                   origin TEXT NOT NULL DEFAULT 'agent_derived',
                   category TEXT DEFAULT 'fact',
                   layer INTEGER NOT NULL DEFAULT 1,
                   importance INTEGER NOT NULL DEFAULT 3,
                   confidence REAL NOT NULL DEFAULT 0.8,
                   source_ref TEXT, session_id TEXT,
                   tags TEXT NOT NULL DEFAULT '[]', entities TEXT NOT NULL DEFAULT '[]',
                   created_at REAL NOT NULL, updated_at REAL NOT NULL,
                   valid_from REAL NOT NULL, valid_to REAL,
                   recorded_at REAL NOT NULL, last_accessed_at REAL,
                   access_count INTEGER NOT NULL DEFAULT 0,
                   pinned INTEGER NOT NULL DEFAULT 0,
                   superseded_by TEXT,
                   effective_importance REAL NOT NULL DEFAULT 0.5,
                   content_hash TEXT NOT NULL,
                   embedding_status TEXT NOT NULL DEFAULT 'pending',
                   embedding_source TEXT, deleted_at REAL
                 );
                 INSERT INTO memories (id, content, agent_id, created_at, updated_at, valid_from, recorded_at, content_hash)
                 VALUES ('m1', '迁移前的记忆', 'dsh', 1, 1, 1, 1, 'h1');
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }

        // 用 Db::open 触发迁移（v1 会一路迁到当前版本 v3）
        let db = Db::open(&path).expect("v1 库应能迁移到 v3");
        assert_eq!(db.user_version().unwrap(), 3);

        // 数据还在
        let n: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM memories WHERE id='m1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "迁移不能丢数据");

        // layer 已删、conversations 已加
        let cols: Vec<String> = {
            let c = db.conn();
            let mut stmt = c.prepare("PRAGMA table_info(memories)").unwrap();
            let it = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
            it.map(|x| x.unwrap()).collect()
        };
        assert!(!cols.contains(&"layer".to_string()), "迁移后 layer 必须删除");
        assert!(db.table_names().unwrap().contains(&"conversations".to_string()));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// T-M2-06 / docs/11 P1-4：冲突收件箱必须是一张**真表**（能存裁决结果）。
    #[test]
    fn v3_adds_conflicts_table() {
        let path = tmp_path("v3-conflicts");
        let db = Db::open(&path).expect("建库");
        assert!(
            db.table_names().unwrap().contains(&"memory_conflicts".to_string()),
            "应有 memory_conflicts 表"
        );
        assert_eq!(db.user_version().unwrap(), 3, "新库应是 v3");
        // 关键列必须齐（缺一个就说明 DDL 与 docs/04 §7 的裁决需求不符）
        let cols: Vec<String> = {
            let c = db.conn();
            let mut stmt = c.prepare("PRAGMA table_info(memory_conflicts)").unwrap();
            let it = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
            it.map(|x| x.unwrap()).collect()
        };
        for must in [
            "entity_type",
            "entity_id",
            "property",
            "memory_a",
            "memory_b",
            "value_a",
            "value_b",
            "fact_ids",
            "status",
            "resolution",
        ] {
            assert!(cols.contains(&must.to_string()), "缺列 {must}，实际: {cols:?}");
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// v2 库（已删 layer、有 conversations、没有 conflicts）能迁到 v3 且数据不丢。
    ///
    /// 为什么单独测这一跳：v1→v3 的路径已被 `v1_db_migrates_to_v2_keeping_data` 覆盖，
    /// 但**线上真实库就是 v2**（上一轮部署的版本），它的迁移路径必须单独证明。
    #[test]
    fn v2_db_migrates_to_v3_keeping_data() {
        let path = tmp_path("v2-to-v3");
        {
            // 先用当前代码建一个 v3 库，再"退化成 v2"：删表 + 改 user_version
            let db = Db::open(&path).expect("建库");
            db.conn()
                .execute_batch(
                    "INSERT INTO memories (id, content, agent_id, created_at, updated_at,
                                           valid_from, recorded_at, content_hash)
                     VALUES ('m2', 'v2 时代的记忆', 'dsh', 1, 1, 1, 1, 'h2');
                     DROP TABLE memory_conflicts;
                     PRAGMA user_version = 2;",
                )
                .unwrap();
        }

        let db = Db::open(&path).expect("v2 库应能迁移到 v3");
        assert_eq!(db.user_version().unwrap(), 3);
        assert!(
            db.table_names().unwrap().contains(&"memory_conflicts".to_string()),
            "迁移必须补建 memory_conflicts"
        );
        let n: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM memories WHERE id='m2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "迁移不能丢数据");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn refuses_unknown_schema_version_and_keeps_data() {
        let path = tmp_path("bad");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch("CREATE TABLE keep_me(x); PRAGMA user_version = 99;")
                .unwrap();
        }
        let err = Db::open(&path).expect_err("版本不符必须拒绝");
        assert_eq!(err.code(), "SCHEMA_TOO_OLD");

        // 数据必须保留
        let c = rusqlite::Connection::open(&path).unwrap();
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='keep_me'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "旧库数据被破坏");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn fts_trigger_indexes_inserted_content() {
        let path = tmp_path("fts");
        let db = Db::open(&path).expect("建库");
        let c = db.conn();
        c.execute(
            "INSERT INTO memories (id, content, agent_id, created_at, updated_at, valid_from, recorded_at, content_hash)
             VALUES ('m1','orbital proxy listens on port 8790','dsh',1,1,1,1,'h1')",
            [],
        )
        .unwrap();

        let hit: i64 = c
            .query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'port'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hit, 1, "FTS 触发器未生效（插入了但搜不到）");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn fts_update_trigger_reindexes() {
        let path = tmp_path("fts-upd");
        let db = Db::open(&path).expect("建库");
        let c = db.conn();
        c.execute(
            "INSERT INTO memories (id, content, agent_id, created_at, updated_at, valid_from, recorded_at, content_hash)
             VALUES ('m1','alpha text','dsh',1,1,1,1,'h1')",
            [],
        )
        .unwrap();
        // 改内容后：旧的搜不到，新的搜得到
        c.execute("UPDATE memories SET content='beta text' WHERE id='m1'", []).unwrap();
        let old: i64 = c
            .query_row("SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'alpha'", [], |r| r.get(0))
            .unwrap();
        let new: i64 = c
            .query_row("SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'beta'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(old, 0, "更新后旧内容不应再命中");
        assert_eq!(new, 1, "更新后新内容应可命中");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn vec_table_partitions_by_agent() {
        let path = tmp_path("vec");
        let db = Db::open(&path).expect("建库");
        let c = db.conn();
        for id in ["m1", "m2"] {
            c.execute(
                "INSERT INTO memories (id, content, agent_id, created_at, updated_at, valid_from, recorded_at, content_hash)
                 VALUES (?1, ?1, ?2, 1,1,1,1,?1)",
                rusqlite::params![id, if id == "m1" { "dsh" } else { "hermes" }],
            )
            .unwrap();
        }
        // 两个 3 维向量（f32 小端）
        fn blob(v: &[f32]) -> Vec<u8> {
            v.iter().flat_map(|f| f.to_le_bytes()).collect()
        }
        c.execute(
            "INSERT INTO memories_vec (memory_id, agent_id, embedding, dim, updated_at) VALUES ('m1','dsh',?1,3,1)",
            rusqlite::params![blob(&[1.0, 0.0, 0.0])],
        )
        .unwrap();
        c.execute(
            "INSERT INTO memories_vec (memory_id, agent_id, embedding, dim, updated_at) VALUES ('m2','hermes',?1,3,1)",
            rusqlite::params![blob(&[0.0, 1.0, 0.0])],
        )
        .unwrap();

        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM memories_vec WHERE agent_id='dsh'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "向量表未按 agent 分区");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}

/// T-M5-02：从备份恢复（"待恢复 + 重启生效"）。
#[cfg(test)]
mod restore_tests {
    use super::*;
    use crate::identity::{register_agent, verify_token, VerifiedAgent};
    use crate::service::MemoryService;
    use crate::writer::WriteInput;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("orbital-restore-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn memory_count(path: &Path) -> i64 {
        let c = rusqlite::Connection::open(path).unwrap();
        c.query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .unwrap()
    }

    /// 用本地特权身份写 n 条（agent 名与 WriteInput 一致）。
    ///
    /// 顺序很重要：`MemoryService::open` 才会建 schema，所以必须先 open 再注册 agent。
    fn seed(db_path: &Path, journal: &Path, agent: &str, tag: &str, n: usize) {
        let svc = MemoryService::open(db_path, journal).unwrap();
        {
            let c = rusqlite::Connection::open(db_path).unwrap();
            let tok = register_agent(&c, agent, agent).unwrap();
            let va = verify_token(&c, &tok).unwrap();
            assert_eq!(va.id(), agent);
        }
        let va = VerifiedAgent::for_local_owner(agent.to_string());
        for i in 0..n {
            svc.write(&va, WriteInput::new(agent, format!("{tag} {i}")))
                .unwrap();
        }
    }

    /// 验收 1+2：恢复真的生效，且恢复前自动保底。
    #[test]
    fn stage_and_apply_restore_roundtrip() {
        let dir = tmp_dir("rt");
        let db_path = dir.join("orbital.db");
        let journal = dir.join("journal.ndjson");

        seed(&db_path, &journal, "rt", "恢复测例", 3);
        assert_eq!(memory_count(&db_path), 3);

        // 备份（3 条）
        let backup = dir.join("backup.db");
        {
            let svc = MemoryService::open(&db_path, &journal).unwrap();
            svc.backup_to(&backup).unwrap();
        }
        // 备份后再写 1 条（当前 4 条）
        seed(&db_path, &journal, "rt", "备份之后写的", 1);
        assert_eq!(memory_count(&db_path), 4);

        // stage：不改当前库
        let rep = stage_restore(&db_path, &backup).unwrap();
        assert!(rep.needs_restart);
        assert_eq!(rep.memories, 3);
        assert!(pending_restore_path(&db_path).exists());
        assert_eq!(memory_count(&db_path), 4, "stage 阶段绝不能改当前库");

        // apply：回到 3 条 + 留下保底
        let pre = apply_pending_restore(&db_path)
            .unwrap()
            .expect("应留下保底备份");
        assert!(!pending_restore_path(&db_path).exists(), "pending 应被清掉");
        assert_eq!(memory_count(&db_path), 3, "应回到备份时的 3 条");
        assert_eq!(memory_count(&pre), 4, "保底备份应保存恢复前的 4 条");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 3：损坏的备份必须被拒绝，且不写 pending。
    #[test]
    fn stage_restore_rejects_corrupt_backup() {
        let dir = tmp_dir("bad");
        let db_path = dir.join("orbital.db");
        let journal = dir.join("journal.ndjson");

        seed(&db_path, &journal, "rb", "会被截断", 1);
        let backup = dir.join("bad-backup.db");
        {
            let svc = MemoryService::open(&db_path, &journal).unwrap();
            svc.backup_to(&backup).unwrap();
        }
        let bytes = std::fs::read(&backup).unwrap();
        std::fs::write(&backup, &bytes[..1024]).unwrap();

        let err = stage_restore(&db_path, &backup).unwrap_err();
        assert!(
            err.to_string().contains("备份") || err.to_string().contains("integrity"),
            "应明确拒绝: {err}"
        );
        assert!(
            !pending_restore_path(&db_path).exists(),
            "拒绝时不能留下 pending"
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}

/// T-M6-03：schema 升级演练（旧库 → 当前版本，不丢数据）。
///
/// 这是 M6 门禁"升级不丢数据"的证据：造 v1/v2 真实形态的旧库（**带数据**），
/// 用当前代码打开，断言迁移后行数不变、新表就位、高版本被拒绝。
#[cfg(test)]
mod migration_tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "orbital-mig-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造 v1 库：只跑 `SCHEMA_V1`（含 `memories.layer`），插一条记忆与一条事件。
    fn make_v1(path: &Path) {
        let c = rusqlite::Connection::open(path).unwrap();
        c.execute_batch(SCHEMA_V1).unwrap();
        c.execute_batch(
            "INSERT INTO memories
               (id, content, agent_id, created_at, updated_at, valid_from, recorded_at, content_hash)
             VALUES ('m1', 'v1 时代的数据', 'legacy', 1000, 1000, 1000, 1000, 'h1');
             INSERT INTO memory_events (operation, memory_id, actor, detail, created_at)
             VALUES ('write', 'm1', 'legacy', '{}', 1000);",
        )
        .unwrap();
        c.execute_batch("PRAGMA user_version = 1").unwrap();
    }

    /// 造 v2 库：v1 + conversations，`memories.layer` 已删，并带上 v2 的数据。
    fn make_v2(path: &Path) {
        make_v1(path);
        let c = rusqlite::Connection::open(path).unwrap();
        c.execute_batch(SCHEMA_V2_ADDITIONS).unwrap();
        let _ = c.execute_batch(SCHEMA_V2_DROP_LAYER);
        c.execute_batch(
            "INSERT INTO conversations
               (id, agent_id, session_id, round_index, role, content, created_at)
             VALUES ('c1', 'legacy', 's1', 0, 'user', 'v2 时的对话', 2000);",
        )
        .unwrap();
        c.execute_batch("PRAGMA user_version = 2").unwrap();
    }

    fn count(path: &Path, table: &str) -> i64 {
        let c = rusqlite::Connection::open(path).unwrap();
        c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap_or(-1)
    }

    fn has_table(path: &Path, name: &str) -> bool {
        let c = rusqlite::Connection::open(path).unwrap();
        c.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name = ?1",
            [name],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    }

    fn has_column(path: &Path, table: &str, col: &str) -> bool {
        let c = rusqlite::Connection::open(path).unwrap();
        let mut st = c
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let names: Vec<String> = st
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .flatten()
            .collect();
        names.iter().any(|n| n == col)
    }

    fn version(path: &Path) -> i64 {
        let c = rusqlite::Connection::open(path).unwrap();
        c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
    }

    /// 验收 1：v1 → v3，记忆与事件一行不少，新表就位，layer 列被删。
    #[test]
    fn v1_migrates_to_v3_keeping_data() {
        let dir = tmp_dir("v1");
        let db_path = dir.join("v1.db");
        make_v1(&db_path);

        let mem_before = count(&db_path, "memories");
        let ev_before = count(&db_path, "memory_events");
        assert_eq!(mem_before, 1);

        let db = Db::open(&db_path).unwrap();
        drop(db);

        assert_eq!(version(&db_path), 3, "应升到 v3");
        assert_eq!(count(&db_path, "memories"), mem_before, "迁移不能丢记忆");
        assert_eq!(count(&db_path, "memory_events"), ev_before, "迁移不能丢事件");
        assert!(has_table(&db_path, "conversations"), "v2 新表应存在");
        assert!(has_table(&db_path, "memory_conflicts"), "v3 新表应存在");
        assert!(
            !has_column(&db_path, "memories", "layer"),
            "layer 列应被删除（ADR-031）"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 2：v2 → v3，conversations 数据保留。
    #[test]
    fn v2_migrates_to_v3_keeping_data() {
        let dir = tmp_dir("v2");
        let db_path = dir.join("v2.db");
        make_v2(&db_path);
        assert_eq!(count(&db_path, "conversations"), 1);

        let db = Db::open(&db_path).unwrap();
        drop(db);

        assert_eq!(version(&db_path), 3);
        assert_eq!(count(&db_path, "conversations"), 1, "对话不能丢");
        assert_eq!(count(&db_path, "memories"), 1, "记忆不能丢");
        assert!(has_table(&db_path, "memory_conflicts"));

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 3：未来版本（v4）必须**明确拒绝**，不能硬跑。
    #[test]
    fn future_version_is_rejected() {
        let dir = tmp_dir("v4");
        let db_path = dir.join("v4.db");
        {
            let c = rusqlite::Connection::open(&db_path).unwrap();
            c.execute_batch("CREATE TABLE t(x); PRAGMA user_version = 4;")
                .unwrap();
        }
        let err = Db::open(&db_path).unwrap_err();
        assert_eq!(err.code(), "SCHEMA_TOO_OLD", "应报版本过高: {err}");
        assert_eq!(version(&db_path), 4, "被拒绝时不能改库");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 4：迁移后的 v1 库能被服务层真正读出来（不是"表在但读不到"）。
    #[test]
    fn migrated_v1_db_is_readable_by_service() {
        use crate::identity::{register_agent, verify_token};
        use crate::service::MemoryService;

        let dir = tmp_dir("v1-read");
        let db_path = dir.join("v1.db");
        let journal = dir.join("journal.ndjson");
        make_v1(&db_path);

        let svc = MemoryService::open(&db_path, &journal).unwrap();
        // 注册新 agent：用独立连接（MemoryService 的 reader 字段是私有的）
        let tok = {
            let c = rusqlite::Connection::open(&db_path).unwrap();
            register_agent(&c, "reader", "Reader").unwrap()
        };
        let va = {
            let c = rusqlite::Connection::open(&db_path).unwrap();
            verify_token(&c, &tok).unwrap()
        };
        // 迁移前的旧数据属于 agent `legacy`；用 legacy 身份能读到
        let legacy = crate::identity::VerifiedAgent::for_local_owner("legacy".to_string());
        let got = svc.get(&legacy, "m1").unwrap();
        assert_eq!(got.content, "v1 时代的数据");
        // 新身份也能正常写入（迁移后 schema 完整）
        svc.write(&va, crate::writer::WriteInput::new("reader", "迁移后新写入"))
            .unwrap();

        let _ = std::fs::remove_dir_all(dir);
    }
}
