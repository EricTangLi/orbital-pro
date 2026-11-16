//! 身份与令牌（ADR-005 + 审核 P1-12）。
//!
//! 为什么需要令牌：`docs/04 §3` 原本写"agent 从进程启动参数推导"，
//! 但任何进程都能 `OrbitalPro.exe --role mcp --agent hermes` 冒充（审核 P1-12 抓到的篡改面）。
//!
//! 设计：
//! - 每个 agent 一个 64 位十六进制令牌（32 字节随机），**库里只存 sha256 哈希**；
//! - 接入时把明文令牌一次性交给用户/agent（写进它的 MCP 配置）；
//! - 每次请求带令牌 → 查 `agents.token_hash` → 得到真实身份；
//! - 请求体里若也声明 `agent`，**只做比对**，不一致直接 403（绝不降级）。

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

use crate::error::{OrbitalError, Result};

/// 已被验证的调用方身份。
///
/// **只能由 [`verify_token`] 或 [`register_agent`] 的内部路径构造**——
/// 写队列只接受这个类型，从类型上杜绝"随手传个字符串就算身份"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedAgent {
    id: String,
}

impl VerifiedAgent {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// **本地特权构造**（ADR-034）：只给本机 GUI 用，**绝不许**出现在 MCP / HTTP / proxy 路径上。
    ///
    /// 为什么需要：GUI 是本地单用户应用，用户本来就能直接改库文件；但 GUI 拿不到某个 agent 的
    /// token（库里只存 hash），所以无法走 `verify_token`。编辑某条记忆时，GUI 以**那条记忆自己的
    /// agent_id** 执行，等价于"主人替该 agent 改它自己的记录"。
    ///
    /// 安全边界：调用方必须已经确认 `agent_id` 来自库里那条记录（不是请求参数）。
    pub fn for_local_owner(agent_id: impl Into<String>) -> Self {
        Self {
            id: agent_id.into(),
        }
    }
}

/// 生成新令牌（64 位十六进制 = 32 字节熵）。
pub fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

pub fn hash_token(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    let d = h.finalize();
    let mut s = String::with_capacity(d.len() * 2);
    for b in d.iter() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// 注册（或重置）一个 agent 的令牌，返回**明文令牌**（只此一次，请立即写入对方配置）。
pub fn register_agent(conn: &Connection, id: &str, display_name: &str) -> Result<String> {
    if id.trim().is_empty() {
        return Err(OrbitalError::InvalidArgument {
            reason: "agent id 不能为空".into(),
        });
    }
    let token = new_token();
    let hash = hash_token(&token);
    let now = now_secs();
    conn.execute(
        "INSERT INTO agents (id, display_name, token_hash, token_issued_at, status, created_at)
         VALUES (?1, ?2, ?3, ?4, 'connected', ?4)
         ON CONFLICT(id) DO UPDATE SET
           display_name = excluded.display_name,
           token_hash = excluded.token_hash,
           token_issued_at = excluded.token_issued_at,
           status = 'connected'",
        params![id, display_name, hash, now],
    )
    .map_err(|e| OrbitalError::Internal(format!("注册 agent 失败: {e}")))?;
    Ok(token)
}

/// 校验令牌 → 得到身份。缺令牌返回 `AGENT_TOKEN_MISSING`，不匹配返回 `AGENT_TOKEN_INVALID`。
pub fn verify_token(conn: &Connection, token: &str) -> Result<VerifiedAgent> {
    if token.trim().is_empty() {
        return Err(OrbitalError::AgentTokenMissing);
    }
    let hash = hash_token(token);
    let r: std::result::Result<String, rusqlite::Error> = conn.query_row(
        "SELECT id FROM agents WHERE token_hash = ?1",
        params![hash],
        |r| r.get(0),
    );
    match r {
        Ok(id) => {
            let now = now_secs();
            let _ = conn.execute(
                "UPDATE agents SET last_verified_at = ?2 WHERE id = ?1",
                params![id, now],
            );
            Ok(VerifiedAgent { id })
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(OrbitalError::AgentTokenInvalid),
        Err(e) => Err(OrbitalError::Internal(format!("校验令牌失败: {e}"))),
    }
}

/// 比对"请求里声明的 agent"与"令牌对应的 agent"。
///
/// 声明可以不传（不影响）；传了就必须一致，否则 403。
pub fn assert_claimed_matches(verified: &VerifiedAgent, claimed: Option<&str>) -> Result<()> {
    if let Some(c) = claimed {
        if !c.trim().is_empty() && c != verified.id() {
            return Err(OrbitalError::AgentIdentityMismatch {
                configured: verified.id().to_string(),
                claimed: c.to_string(),
            });
        }
    }
    Ok(())
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn tmp_db(tag: &str) -> (std::path::PathBuf, Db) {
        let dir =
            std::env::temp_dir().join(format!("orbital-identity-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(dir.join("t.db")).unwrap();
        (dir, db)
    }

    #[test]
    fn token_roundtrip_and_hash_only_stored() {
        let (dir, db) = tmp_db("round");
        let token = register_agent(db.conn(), "dsh", "DSH").unwrap();
        assert_eq!(token.len(), 64);

        // 库里不能有明文
        let plain: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM agents WHERE token_hash = ?1",
                params![token],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(plain, 0, "明文令牌被直接存进库了");

        let v = verify_token(db.conn(), &token).unwrap();
        assert_eq!(v.id(), "dsh");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_and_invalid_tokens_are_distinguished() {
        let (dir, db) = tmp_db("bad");
        register_agent(db.conn(), "dsh", "DSH").unwrap();
        assert_eq!(
            verify_token(db.conn(), "").unwrap_err().code(),
            "AGENT_TOKEN_MISSING"
        );
        assert_eq!(
            verify_token(db.conn(), "not-a-real-token").unwrap_err().code(),
            "AGENT_TOKEN_INVALID"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn claimed_agent_must_match_token() {
        let (dir, db) = tmp_db("claim");
        let t = register_agent(db.conn(), "dsh", "DSH").unwrap();
        let v = verify_token(db.conn(), &t).unwrap();
        // 一致 → 通过
        assert!(assert_claimed_matches(&v, Some("dsh")).is_ok());
        // 不声明 → 通过
        assert!(assert_claimed_matches(&v, None).is_ok());
        // 冒充 → AGENT_IDENTITY_MISMATCH
        let e = assert_claimed_matches(&v, Some("hermes")).unwrap_err();
        assert_eq!(e.code(), "AGENT_IDENTITY_MISMATCH");
        assert_eq!(e.http_status(), 403);
        let _ = std::fs::remove_dir_all(dir);
    }
}
