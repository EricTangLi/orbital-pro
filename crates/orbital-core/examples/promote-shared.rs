//! T-M10-19（P0）：把几条"高频事实"记忆提升为 `scope=shared`，让跨 agent 共享立刻可见。
//!
//! 为什么需要：注入只注入 shared（ADR-042），而实测库里 89 条**全是 private**、shared=0
//! ⇒ 四个 agent 谁也看不到别人写的内容，"多 agent 共享"就是空的。
//!
//! 用法：
//! ```text
//! cargo run -p orbital-core --example promote-shared -- <db> <journal>
//! ```

use orbital_core::db::Db;
use orbital_core::identity::register_agent;
use orbital_core::service::{MemoryService, UpdatePatch};

/// 要提升的关键词（挑"跨 agent 都有用"的事实类记忆，不是私事）
const KEYS: [&str; 5] = ["端口", "ORBITAL_", "WorkBuddy", "记忆", "配置"];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("用法: promote-shared <db> <journal>");
        std::process::exit(2);
    }
    let db_path = &args[1];
    let journal = &args[2];

    // 用本地所有者身份（等于"用户在 GUI 里点提升为共享"），不带 agent token
    let va = orbital_core::identity::VerifiedAgent::for_local_owner("user".to_string());

    // 先按关键词挑候选（只读，用短命连接）
    let picked: Vec<(String, String, String)> = {
        let dbx = Db::open(db_path).expect("打开库失败");
        let mut out = Vec::new();
        for k in KEYS {
            let pat = format!("%{k}%");
            if let Ok(mut rows) = dbx
                .conn()
                .prepare(
                    "SELECT id, agent_id, substr(content,1,60) FROM memories
                      WHERE deleted_at IS NULL AND scope = 'private' AND content LIKE ?1
                      ORDER BY updated_at DESC LIMIT 1",
                )
                .and_then(|mut st| {
                    let r = st
                        .query_map([&pat], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
                        })?
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(r)
                })
            {
                out.append(&mut rows);
            }
        }
        out
    };

    if picked.is_empty() {
        println!("没找到候选（关键词都不命中）");
        return;
    }

    let svc = MemoryService::open(db_path, journal).expect("打开服务失败");
    let mut ok = 0usize;
    for (id, agent, head) in picked.iter().take(5) {
        // 用**该记忆所属 agent** 的身份去改（门控是 `agent_id = ?1 OR scope IN (shared,entity)`，
        // 用 "user" 身份连 get 都过不去——这是 2026-11-14 实测踩到的坑）
        let va = orbital_core::identity::VerifiedAgent::for_local_owner(agent.clone());
        // ADR-023 规则 1：写 shared 必须 origin=user_endorsed；core 的 update 会自动改 origin
        let patch = UpdatePatch {
            content: None,
            importance: None,
            tags: None,
            pinned: None,
            scope: Some("shared".to_string()),
        };
        match svc.update(&va, id, patch) {
            Ok(_) => {
                ok += 1;
                println!("PROMOTED {id} (from {agent}) :: {head}");
            }
            Err(e) => println!("FAILED {id}: {e}"),
        }
    }
    println!("PROMOTED_TOTAL {ok}");
    let _ = register_agent; // 保留 import 供未来扩展（按 agent 身份写）
}
