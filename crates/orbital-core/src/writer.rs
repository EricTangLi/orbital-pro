//! 单写队列（docs/04 §3 / ADR-003）。
//!
//! 规则（**不许改**，改了就是丢数据）：
//! 1. 所有写请求进**同一个**队列，由**一个**线程串行处理；
//! 2. 批提交：攒到 `WRITER_BATCH_SIZE` 条或等满 `WRITER_FLUSH_MS` 毫秒就提交；
//! 3. 事务用 `BEGIN IMMEDIATE`（DEFERRED 在"先查重再插入"时会报 BUSY_SNAPSHOT）；
//! 4. **先 COMMIT 再回执**——回执说成功，数据必须已在盘上（防静默丢失 F1）；
//! 5. 去重：同 `agent_id` + 同 `content_hash` 只存一条，返回已有 id；
//! 6. journal 在 COMMIT 之后追加（只追加不改写，重建用）。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::config::{MAX_CONTENT_BYTES, WRITER_BATCH_SIZE, WRITER_FLUSH_MS};
use crate::db::Db;
use crate::error::{OrbitalError, Result};
use crate::identity::{assert_claimed_matches, VerifiedAgent};

/// 写入输入（调用方填）。
#[derive(Debug, Clone)]
pub struct WriteInput {
    pub agent_id: String,
    pub content: String,
    pub scope: String,
    pub category: String,
    pub importance: i32,
    pub tags: Vec<String>,
    pub entities: Vec<String>,
    pub source_ref: Option<String>,
    pub session_id: Option<String>,
    /// 来源：`agent_derived`（默认）或 `user_endorsed`。
    /// ADR-023 规则 1：只有 `user_endorsed` 才能写 `scope=shared`。
    pub origin: String,
}

impl WriteInput {
    pub fn new(agent_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            content: content.into(),
            scope: "private".to_string(),
            category: "fact".to_string(),
            importance: 3,
            tags: Vec::new(),
            entities: Vec::new(),
            source_ref: None,
            session_id: None,
            origin: "agent_derived".to_string(),
        }
    }
}

/// 写入回执（`docs/04 §10.1` 的承诺强度）。
#[derive(Debug, Clone)]
pub struct WriteReceipt {
    pub id: String,
    pub duplicate: bool,
    /// `exact` = 写进去就是最终形态（本实现同步落库）
    pub write_mode: &'static str,
    /// `soft` = 默认软删可恢复
    pub deletion_mode: &'static str,
}

/// 结构化事实写入（T-M3-03）。
///
/// 为什么必须走写队列（红线 5）：新增写路径不许直接 `conn.execute`。
/// 这是 ADR-034 记录的"Job 只支持单操作"债务的正式偿还——`JobKind` 现在有多个变体。
#[derive(Debug, Clone)]
pub struct FactWrite {
    pub memory_id: String,
    pub entity_type: String,
    pub entity_id: String,
    pub property: String,
    pub value: String,
    pub confidence: f64,
}

/// 队列里的一件活（T-M3-03 起支持多种操作）。
enum JobKind {
    Memory(WriteInput),
    Fact(FactWrite),
}

struct Job {
    kind: JobKind,
    reply: Sender<Result<WriteReceipt>>,
}

/// 写队列句柄。**Drop 时会关闭队列并 join 线程**（保证退出前 flush）。
pub struct Writer {
    tx: Sender<Job>,
    join: Option<thread::JoinHandle<()>>,
}

impl Writer {
    /// 启动写线程。`db_path` 与 `journal_path` 由调用方决定（config）。
    pub fn start(db_path: impl AsRef<Path>, journal_path: impl AsRef<Path>) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        let journal_path = journal_path.as_ref().to_path_buf();
        let (tx, rx) = mpsc::channel::<Job>();

        let join = thread::Builder::new()
            .name("orbital-writer".to_string())
            .spawn(move || writer_loop(rx, db_path, journal_path))
            .map_err(|e| OrbitalError::Internal(format!("启动写线程失败: {e}")))?;

        Ok(Self {
            tx,
            join: Some(join),
        })
    }

    /// 提交一条写入并**等待落库结果**（同步返回，保证"回执即落盘"）。
    ///
    /// 必须传入 [`VerifiedAgent`]：请求里声明的 agent 与令牌不一致时直接 403（ADR-005）。
    pub fn write(&self, verified: &VerifiedAgent, input: WriteInput) -> Result<WriteReceipt> {
        assert_claimed_matches(verified, Some(&input.agent_id))?;
        if input.content.trim().is_empty() {
            return Err(OrbitalError::InvalidArgument {
                reason: "content 不能为空".into(),
            });
        }
        // ADR-023 规则 1：shared 必须 user_endorsed
        if input.scope == "shared" && input.origin != "user_endorsed" {
            return Err(OrbitalError::ScopeForbidden);
        }
        if input.content.len() > MAX_CONTENT_BYTES {
            return Err(OrbitalError::InvalidArgument {
                reason: format!("content 超过 {} 字节上限", MAX_CONTENT_BYTES),
            });
        }

        let (rtx, rrx) = mpsc::channel();
        self.tx
            .send(Job {
                kind: JobKind::Memory(input),
                reply: rtx,
            })
            .map_err(|_| OrbitalError::Internal("写队列已关闭".into()))?;
        rrx.recv()
            .map_err(|_| OrbitalError::Internal("写线程未回执即退出".into()))?
    }

    /// 提交一条结构化事实（T-M3-03）。返回回执：`id` = 所属记忆 id，`duplicate` = 已存在同值。
    pub fn write_fact(&self, fw: FactWrite) -> Result<WriteReceipt> {
        if fw.memory_id.trim().is_empty() {
            return Err(OrbitalError::InvalidArgument {
                reason: "memory_id 不能为空".into(),
            });
        }
        if fw.entity_type.trim().is_empty()
            || fw.entity_id.trim().is_empty()
            || fw.property.trim().is_empty()
        {
            return Err(OrbitalError::InvalidArgument {
                reason: "entity_type / entity_id / property 都不能为空".into(),
            });
        }
        let (rtx, rrx) = mpsc::channel();
        self.tx
            .send(Job {
                kind: JobKind::Fact(fw),
                reply: rtx,
            })
            .map_err(|_| OrbitalError::Internal("写队列已关闭".into()))?;
        rrx.recv()
            .map_err(|_| OrbitalError::Internal("写线程未回执即退出".into()))?
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // 关闭发送端 → writer_loop 的 recv 返回 None → 线程退出
        // 这里通过替换 sender 让原 tx drop：用一个不可能成功的 channel 替换
        let (tx, _rx) = mpsc::channel();
        drop(std::mem::replace(&mut self.tx, tx));
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn writer_loop(rx: mpsc::Receiver<Job>, db_path: PathBuf, journal_path: PathBuf) {
    let mut db = match Db::open(&db_path) {
        Ok(d) => d,
        Err(e) => {
            // 启动失败要把排队中的请求全部回错，不能静默丢弃
            while let Ok(job) = rx.recv() {
                let _ = job
                    .reply
                    .send(Err(OrbitalError::Internal(format!("writer 启动失败: {e}"))));
            }
            return;
        }
    };

    loop {
        let first = match rx.recv() {
            Ok(j) => j,
            Err(_) => break,
        };
        let mut batch = vec![first];
        let deadline = Instant::now() + Duration::from_millis(WRITER_FLUSH_MS);
        while batch.len() < WRITER_BATCH_SIZE {
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
                break;
            }
            match rx.recv_timeout(remain) {
                Ok(j) => batch.push(j),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        process_batch(db.conn_mut(), &journal_path, batch);
    }
}

/// 处理一批：事务内逐条落库，**COMMIT 后才回执**。
fn process_batch(conn: &mut Connection, journal_path: &Path, batch: Vec<Job>) {
    // BUSY 退避重试（50/100/200ms × 3），仍失败则整批回 WRITE_BUSY
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match try_process_batch(conn, journal_path, &batch) {
            Ok(receipts) => {
                for (job, receipt) in batch.into_iter().zip(receipts) {
                    let _ = job.reply.send(receipt);
                }
                return;
            }
            Err(OrbitalError::WriteBusy) if attempt < 3 => {
                thread::sleep(Duration::from_millis(50 * (1u64 << (attempt - 1))));
            }
            Err(e) => {
                let msg = format!("{}", e);
                for job in batch {
                    let _ = job
                        .reply
                        .send(Err(OrbitalError::Internal(msg.clone())));
                }
                return;
            }
        }
    }
}

fn try_process_batch(
    conn: &mut Connection,
    journal_path: &Path,
    batch: &[Job],
) -> Result<Vec<Result<WriteReceipt>>> {
    let now = now_secs();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_db_err)?;

    let mut out: Vec<Result<WriteReceipt>> = Vec::with_capacity(batch.len());
    // T-M3-04：本批的自动建边预算（时间边 / 实体边共用）
    let mut auto_edges = 0usize;
    for job in batch {
        // T-M3-03：事实写入与记忆写入走**同一个队列**（ADR-034 债务：Job 已支持多操作）
        if let JobKind::Fact(fw) = &job.kind {
            out.push(process_fact(&tx, fw, now, &mut auto_edges));
            continue;
        }
        let input = match &job.kind {
            JobKind::Memory(i) => i,
            JobKind::Fact(_) => unreachable!("Fact 分支已在上面 continue"),
        };
        let hash = content_hash(&input.content);

        // 去重：同 agent + 同 hash 且未软删
        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM memories WHERE agent_id = ?1 AND content_hash = ?2 AND deleted_at IS NULL LIMIT 1",
                params![input.agent_id, hash],
                |r| r.get(0),
            )
            .optional_row()
            .map_err(map_db_err)?;

        if let Some(id) = existing {
            out.push(Ok(WriteReceipt {
                id,
                duplicate: true,
                write_mode: "exact",
                deletion_mode: "soft",
            }));
            continue;
        }

        let id = format!("mem_{}", uuid::Uuid::new_v4().simple());
        let tags_json = serde_json::to_string(&input.tags).unwrap_or_else(|_| "[]".into());
        let ents_json = serde_json::to_string(&input.entities).unwrap_or_else(|_| "[]".into());

        tx.execute(
            "INSERT INTO memories
              (id, content, agent_id, scope, origin, category, importance, tags, entities,
               source_ref, session_id, created_at, updated_at, valid_from, recorded_at,
               content_hash)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12,?12,?12,?13)",
            params![
                id,
                input.content,
                input.agent_id,
                input.scope,
                input.origin,
                input.category,
                input.importance,
                tags_json,
                ents_json,
                input.source_ref,
                input.session_id,
                now,
                hash,
            ],
        )
        .map_err(map_db_err)?;

        tx.execute(
            "INSERT INTO memory_events (operation, memory_id, actor, detail, created_at)
             VALUES ('write', ?1, ?2, ?3, ?4)",
            params![id, input.agent_id, format!("{{\"scope\":\"{}\"}}", input.scope), now],
        )
        .map_err(map_db_err)?;

        // ---- T-M3-04：时间边（同 agent + 同 session 的相邻两条）----
        if auto_edges < MAX_AUTO_EDGES_PER_BATCH && auto_temporal_edge(&tx, input, &id, now)? {
            auto_edges += 1;
        }

        out.push(Ok(WriteReceipt {
            id,
            duplicate: false,
            write_mode: "exact",
            deletion_mode: "soft",
        }));
    }

    // ★ 先 COMMIT（落盘），之后才回执 / 写 journal
    tx.commit().map_err(map_db_err)?;

    // journal：事务成功后追加（失败只记日志，不影响已成功的写入）
    if let Err(e) = append_journal(journal_path, batch, &out, now) {
        tracing::warn!("journal 追加失败（数据已入库）: {e}");
    }

    Ok(out)
}

/// 自动建边上限（每批最多多少条）：防边数爆炸（T-M3-04 风险项）。
const MAX_AUTO_EDGES_PER_BATCH: usize = 50;

/// 时间边：同 agent + 同 `session_id` 的**上一条**（`docs/04 §2.3`）。
///
/// 权重 `1/(1+小时差)`；`metadata.derived=true` 标记"自动边"——
/// journal 回放时按规则重建、不直接回填（`docs/04 §2.3` 的 derived 约定）。
fn auto_temporal_edge(
    tx: &rusqlite::Transaction<'_>,
    input: &WriteInput,
    new_id: &str,
    now: f64,
) -> Result<bool> {
    let Some(sess) = input.session_id.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(false);
    };
    let prev: Option<(String, f64)> = tx
        .query_row(
            "SELECT id, created_at FROM memories
              WHERE agent_id = ?1 AND session_id = ?2 AND id <> ?3 AND deleted_at IS NULL
              ORDER BY created_at DESC, id DESC LIMIT 1",
            params![input.agent_id, sess, new_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional_row()
        .map_err(map_db_err)?;
    let Some((prev_id, prev_at)) = prev else {
        return Ok(false);
    };
    let hours = (now - prev_at).max(0.0) / 3600.0;
    let weight = 1.0 / (1.0 + hours);
    tx.execute(
        "INSERT OR IGNORE INTO edges (source_id, target_id, edge_type, weight, metadata, created_at)
         VALUES (?1, ?2, 'temporal', ?3, '{\"derived\":true}', ?4)",
        params![prev_id, new_id, weight, now],
    )
    .map_err(map_db_err)?;
    Ok(true)
}

/// 实体边：写入事实后，把"共享同一实体的**同 agent** 记忆"连起来（`docs/04 §2.3`）。
///
/// 为什么限同 agent：跨 agent 自动边要靠 shared/entity scope 的门控（`docs/04 §4.1` 门控 2），
/// 这里保守只连同 agent，避免自动边制造跨 agent 泄漏。
fn auto_entity_edge(
    tx: &rusqlite::Transaction<'_>,
    memory_id: &str,
    entity_type: &str,
    entity_id: &str,
    now: f64,
) -> Result<usize> {
    let others: Vec<String> = {
        let mut st = tx
            .prepare(
                "SELECT DISTINCT f.memory_id FROM memory_facts f
                   JOIN memories m ON m.id = f.memory_id
                  WHERE f.entity_type = ?1 AND f.entity_id = ?2 AND f.valid_to IS NULL
                    AND f.memory_id <> ?3 AND m.deleted_at IS NULL
                    AND m.agent_id = (SELECT agent_id FROM memories WHERE id = ?3)
                  LIMIT 20",
            )
            .map_err(map_db_err)?;
        let rows = st
            .query_map(params![entity_type, entity_id, memory_id], |r| {
                r.get::<_, String>(0)
            })
            .map_err(map_db_err)?;
        let mut v = Vec::new();
        for x in rows {
            v.push(x.map_err(map_db_err)?);
        }
        v
    };
    let mut n = 0usize;
    for other in others {
        tx.execute(
            "INSERT OR IGNORE INTO edges (source_id, target_id, edge_type, weight, metadata, created_at)
             VALUES (?1, ?2, 'entity', 0.9, '{\"derived\":true}', ?3)",
            params![memory_id, other, now],
        )
        .map_err(map_db_err)?;
        n += 1;
    }
    Ok(n)
}

/// 处理一条事实写入（T-M3-03）：去重 → 插入 → **增量冲突检测**（同一事务内）。
///
/// 为什么冲突检测放在写侧：M2 的 `detect_conflicts` 是"手动全表扫"，
/// 有了写入路径之后，同实体同属性的第二个值在写入那一刻就能被发现（避免等到用户点扫描）。
fn process_fact(
    tx: &rusqlite::Transaction<'_>,
    fw: &FactWrite,
    now: f64,
    auto_edges: &mut usize,
) -> Result<WriteReceipt> {
    // 记忆必须存在且未删（MemoryService::add_fact 已做可见性校验，这里兜底）
    let exists: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM memories WHERE id = ?1 AND deleted_at IS NULL",
            params![fw.memory_id],
            |r| r.get(0),
        )
        .optional_row()
        .map_err(map_db_err)?;
    if exists.is_none() {
        return Err(OrbitalError::MemoryNotFound {
            id: fw.memory_id.clone(),
        });
    }

    // 完全同值（同记忆 + 同实体 + 同属性 + 同值 + 有效）→ 不重复插入
    let dup: Option<i64> = tx
        .query_row(
            "SELECT id FROM memory_facts
              WHERE memory_id = ?1 AND entity_type = ?2 AND entity_id = ?3
                AND property = ?4 AND value = ?5 AND valid_to IS NULL LIMIT 1",
            params![
                fw.memory_id,
                fw.entity_type,
                fw.entity_id,
                fw.property,
                fw.value
            ],
            |r| r.get(0),
        )
        .optional_row()
        .map_err(map_db_err)?;
    if dup.is_some() {
        return Ok(WriteReceipt {
            id: fw.memory_id.clone(),
            duplicate: true,
            write_mode: "fact",
            deletion_mode: "soft",
        });
    }

    tx.execute(
        "INSERT INTO memory_facts
           (memory_id, agent_id, entity_type, entity_id, property, value,
            confidence, valid_from, valid_to, recorded_at)
         VALUES (?1, (SELECT agent_id FROM memories WHERE id = ?1), ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?7)",
        params![
            fw.memory_id,
            fw.entity_type,
            fw.entity_id,
            fw.property,
            fw.value,
            fw.confidence,
            now
        ],
    )
    .map_err(map_db_err)?;
    let new_fact_id = tx.last_insert_rowid();

    // ---- 增量冲突检测：同实体同属性已有**不同值**的有效事实 ----
    let other: Option<(String, String)> = tx
        .query_row(
            "SELECT f.memory_id, f.value FROM memory_facts f
              WHERE f.entity_type = ?1 AND f.entity_id = ?2 AND f.property = ?3
                AND f.valid_to IS NULL AND f.value <> ?4 AND f.memory_id <> ?5
              LIMIT 1",
            params![
                fw.entity_type,
                fw.entity_id,
                fw.property,
                fw.value,
                fw.memory_id
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional_row()
        .map_err(map_db_err)?;
    if let Some((other_memory, other_value)) = other {
        // a/b 按 memory_id 字典序，与 core 的 detect_conflicts 同一口径（否则会重复登记）
        let (ma, mb, va, vb) = if other_memory < fw.memory_id {
            (other_memory, fw.memory_id.clone(), other_value, fw.value.clone())
        } else {
            (fw.memory_id.clone(), other_memory, fw.value.clone(), other_value)
        };
        tx.execute(
            "INSERT OR IGNORE INTO memory_conflicts
               (entity_type, entity_id, property, memory_a, memory_b,
                value_a, value_b, fact_ids, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'open', ?9)",
            params![
                fw.entity_type,
                fw.entity_id,
                fw.property,
                ma,
                mb,
                va,
                vb,
                serde_json::json!([new_fact_id]).to_string(),
                now
            ],
        )
        .map_err(map_db_err)?;
    }

    // ---- T-M3-04：实体边（与共享同一实体的同 agent 记忆相连）----
    if *auto_edges < MAX_AUTO_EDGES_PER_BATCH {
        let n = auto_entity_edge(tx, &fw.memory_id, &fw.entity_type, &fw.entity_id, now)?;
        *auto_edges += n.min(MAX_AUTO_EDGES_PER_BATCH - *auto_edges);
    }

    Ok(WriteReceipt {
        id: fw.memory_id.clone(),
        duplicate: false,
        write_mode: "fact",
        deletion_mode: "soft",
    })
}

fn append_journal(
    journal_path: &Path,
    batch: &[Job],
    receipts: &[Result<WriteReceipt>],
    ts: f64,
) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = journal_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal_path)?;
    for (job, r) in batch.iter().zip(receipts.iter()) {
        if let Ok(rec) = r {
            match &job.kind {
                JobKind::Memory(input) => {
                    let line = serde_json::json!({
                        "op": "write",
                        "id": rec.id,
                        "agent_id": input.agent_id,
                        "scope": input.scope,
                        "write_mode": rec.write_mode,
                        "ts": ts,
                    });
                    writeln!(f, "{}", line)?;
                }
                JobKind::Fact(fw) => {
                    let line = serde_json::json!({
                        "op": "fact",
                        "id": rec.id,
                        "memory_id": fw.memory_id,
                        "entity": fw.entity_id,
                        "property": fw.property,
                        "value": fw.value,
                        "ts": ts,
                    });
                    writeln!(f, "{}", line)?;
                }
            }
        }
    }
    Ok(())
}

fn content_hash(content: &str) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    let d = h.finalize();
    let mut s = String::with_capacity(d.len() * 2);
    for b in d.iter() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

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

/// 小工具：`query_row` 找不到时不报错，返回 None。
trait OptionalRow<T> {
    fn optional_row(self) -> std::result::Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalRow<T> for std::result::Result<T, rusqlite::Error> {
    fn optional_row(self) -> std::result::Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Env {
        dir: PathBuf,
        writer: Writer,
    }

    fn setup(tag: &str) -> (Env, VerifiedAgent) {
        let dir = std::env::temp_dir().join(format!("orbital-writer-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("t.db");
        let journal = dir.join("journal").join("t.ndjson");
        // 先建库并注册 dsh 的令牌（T-M1-04：写必须带已验证身份）
        let token = {
            let d = Db::open(&db).unwrap();
            crate::identity::register_agent(d.conn(), "dsh", "DSH").unwrap()
        };
        let writer = Writer::start(&db, &journal).expect("启动 writer");
        let va = {
            let d = Db::open(&db).unwrap();
            crate::identity::verify_token(d.conn(), &token).unwrap()
        };
        (Env { dir, writer }, va)
    }

    #[test]
    fn writes_and_reads_back() {
        let (env, va) = setup("basic");
        let r = env
            .writer
            .write(&va, WriteInput::new("dsh", "orbital core listens on 8791"))
            .expect("写入成功");
        assert!(!r.duplicate);
        assert_eq!(r.write_mode, "exact");

        let db = Db::open(env.dir.join("t.db")).unwrap();
        let got: String = db
            .conn()
            .query_row("SELECT content FROM memories WHERE id=?1", params![r.id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(got, "orbital core listens on 8791");
        drop(env);
    }

    #[test]
    fn dedup_same_agent_same_content() {
        let (env, va) = setup("dedup");
        let a = env.writer.write(&va, WriteInput::new("dsh", "same text")).unwrap();
        let b = env.writer.write(&va, WriteInput::new("dsh", "same text")).unwrap();
        assert!(!a.duplicate);
        assert!(b.duplicate, "同 agent 同内容应判重复");
        assert_eq!(a.id, b.id);
        drop(env);
    }

    #[test]
    fn different_agents_same_content_not_deduped() {
        // docs/04 §3：跨 agent 相同内容**不合并**（是"共识信号"），各自留一条
        let (env, va) = setup("crossagent");
        // hermes 需要自己的令牌（T-M1-04 门控：不能拿 dsh 的身份写 hermes 的内容）
        let hermes_token = {
            let d = Db::open(env.dir.join("t.db")).unwrap();
            crate::identity::register_agent(d.conn(), "hermes", "Hermes").unwrap()
        };
        let vb = {
            let d = Db::open(env.dir.join("t.db")).unwrap();
            crate::identity::verify_token(d.conn(), &hermes_token).unwrap()
        };
        let a = env
            .writer
            .write(&va, WriteInput::new("dsh", "shared fact"))
            .unwrap();
        let b = env
            .writer
            .write(&vb, WriteInput::new("hermes", "shared fact"))
            .unwrap();
        assert_ne!(a.id, b.id);
        assert!(!b.duplicate);
        let db = Db::open(env.dir.join("t.db")).unwrap();
        let n: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM memories WHERE content='shared fact'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 2);
        drop(env);
    }

    #[test]
    fn writes_audit_event_and_journal_line() {
        let (env, va) = setup("event");
        let r = env.writer.write(&va, WriteInput::new("dsh", "audit me")).unwrap();
        let db = Db::open(env.dir.join("t.db")).unwrap();
        let n: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM memory_events WHERE memory_id=?1 AND operation='write'",
                params![r.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "缺审计事件");

        let jp = env.dir.join("journal").join("t.ndjson");
        let text = std::fs::read_to_string(&jp).expect("journal 应存在");
        assert!(text.contains(&r.id), "journal 缺这次写入");
        drop(env);
    }

    #[test]
    fn concurrent_writes_are_not_lost() {
        let (env, va) = setup("concurrent");
        let dir = env.dir.clone();
        drop(env); // 先释放，重建 writer 并跨线程写
        let db = dir.join("t.db");
        let journal = dir.join("journal").join("c.ndjson");
        let writer = std::sync::Arc::new(Writer::start(&db, &journal).unwrap());

        let mut handles = Vec::new();
        for i in 0..20 {
            let w = writer.clone();
            let va = va.clone();
            handles.push(std::thread::spawn(move || {
                for j in 0..5 {
                    w.write(&va, WriteInput::new("dsh", format!("content {}-{}", i, j)))
                        .expect("写入成功");
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let check = Db::open(&db).unwrap();
        let n: i64 = check
            .conn()
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 100, "100 条并发写有丢失");

        // 回查 100/100
        let ids: Vec<String> = {
            let mut stmt = check.conn().prepare("SELECT id FROM memories").unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(ids.len(), 100);
        drop(writer);
    }

    #[test]
    fn impersonation_is_refused_and_writes_nothing() {
        // T-M1-04 核心验收：拿 dsh 的令牌去写 hermes 的内容 → 403 且库内 0 行
        let (env, va) = setup("impersonate");
        let e = env
            .writer
            .write(&va, WriteInput::new("hermes", "forged write"))
            .unwrap_err();
        assert_eq!(e.code(), "AGENT_IDENTITY_MISMATCH");
        assert_eq!(e.http_status(), 403);

        let d = Db::open(env.dir.join("t.db")).unwrap();
        let n: i64 = d
            .conn()
            .query_row(
                "SELECT count(*) FROM memories WHERE content='forged write'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "伪造身份居然写进去了");
    }


    /// ADR-023 规则 1：shared 必须 user_endorsed（agent 推断的只能进 private）。
    #[test]
    fn shared_scope_requires_user_endorsement() {
        let (env, va) = setup("sharedscope");

        // agent_derived + shared -> 拒绝
        let mut bad = WriteInput::new("dsh", "shared without endorsement");
        bad.scope = "shared".to_string();
        let e = env.writer.write(&va, bad).unwrap_err();
        assert_eq!(e.code(), "SCOPE_FORBIDDEN");

        // user_endorsed + shared -> 通过
        let mut good = WriteInput::new("dsh", "shared with endorsement");
        good.scope = "shared".to_string();
        good.origin = "user_endorsed".to_string();
        assert!(env.writer.write(&va, good).is_ok(), "user_endorsed 应能写 shared");

        // agent_derived + private -> 通过（默认路径不受影响）
        assert!(env.writer.write(&va, WriteInput::new("dsh", "private ok")).is_ok());
        drop(env);
    }

    #[test]
    fn rejects_empty_and_oversize_content() {
        let (env, va) = setup("limits");        let e = env.writer.write(&va, WriteInput::new("dsh", "   ")).unwrap_err();
        assert_eq!(e.code(), "INVALID_ARGUMENT");
        let big = "x".repeat(MAX_CONTENT_BYTES + 1);
        let e = env.writer.write(&va, WriteInput::new("dsh", big)).unwrap_err();
        assert_eq!(e.code(), "INVALID_ARGUMENT");
        drop(env);
    }
}
