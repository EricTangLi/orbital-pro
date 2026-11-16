//! T-M10-01 第 5 项辅助：往库里写几条 `scope=shared` 的种子记忆（注入只注入 shared）。
//!
//! 用法（**写真实库前请先确认**；测试完用 `--clean` 软删）：
//! ```text
//! cargo run -p orbital-core --example m10-shared-seed -- <db> <journal> seed
//! cargo run -p orbital-core --example m10-shared-seed -- <db> <journal> clean
//! ```
//!
//! 为什么需要：`ADR-042` 规定 proxy 只注入 `scope='shared'`（无法确认请求来源，private 不许泄漏）。
//! 实测真实库 89 条全是 private ⇒ 注入即使成功也是 0 条。本程序只为验证注入链路，不改变产品策略。

use orbital_core::db::Db;
use orbital_core::identity::{register_agent, verify_token};
use orbital_core::service::MemoryService;
use orbital_core::writer::WriteInput;

const AGENT: &str = "m10-shared-probe";
const MARK: &str = "M10INJECT";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("用法: m10-shared-seed <db> <journal> seed|clean");
        std::process::exit(2);
    }
    let db_path = &args[1];
    let journal = &args[2];
    let mode = &args[3];

    let dbx = Db::open(db_path).expect("打开库失败");
    let tok = register_agent(dbx.conn(), AGENT, "M10 Shared Probe").unwrap();
    let va = verify_token(dbx.conn(), &tok).unwrap();
    drop(dbx);

    let svc = MemoryService::open(db_path, journal).expect("打开服务失败");

    match mode.as_str() {
        "seed" => {
            let topics = [
                ("注入链路测试", "M10INJECT 注入链路测试：请记住这个标记以验证注入是否生效"),
                ("端口约定", "M10INJECT 端口约定：core 8791 / proxy 8790 / 只读召回 8799"),
                ("备份策略", "M10INJECT 备份策略：用 SQLite 在线备份 API，备份后转 DELETE 模式成单文件"),
            ];
            for (i, (tag, body)) in topics.iter().enumerate() {
                let mut wi = WriteInput::new(AGENT, format!("{body}（种子 {i}）"));
                wi.scope = "shared".into();
                wi.origin = "user_endorsed".into(); // ADR-023：只有 user_endorsed 能写 shared
                wi.tags = vec![tag.to_string(), "m10-seed".to_string()];
                match svc.write(&va, wi) {
                    Ok(m) => println!("SEEDED {} {}", m.id, tag),
                    Err(e) => eprintln!("FAILED seed {i}: {e}"),
                }
            }
        }
        "clean" => {
            // 只软删本探针写的那几条（按 agent + 内容标记）。
            // 说明：这是**测试清理**，不是业务写路径；业务写入一律走 svc.write。
            let dbx = Db::open(db_path).expect("打开库失败");
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let n = dbx
                .conn()
                .execute(
                    "UPDATE memories SET deleted_at = ?1, updated_at = ?1
                      WHERE agent_id = ?2 AND content LIKE ?3 AND deleted_at IS NULL",
                    rusqlite::params![now, AGENT, format!("%{MARK}%")],
                )
                .unwrap_or(0);
            println!("CLEANED {n}");
        }
        other => {
            eprintln!("未知模式: {other}（只支持 seed / clean）");
            std::process::exit(2);
        }
    }
}
