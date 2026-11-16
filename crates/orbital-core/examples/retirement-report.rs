//! 退役候选报告（T-M4-01）：只读跑一遍 ei 公式，打印候选与分母。
//!
//! 用法：`cargo run -p orbital-core --example retirement-report -- <db> [agent] [threshold]`

use orbital_core::identity::VerifiedAgent;
use orbital_core::service::MemoryService;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 2 {
        eprintln!("用法: retirement-report <db> [agent] [threshold]");
        std::process::exit(2);
    }
    let db = &a[1];
    let agent = a.get(2).cloned().unwrap_or_else(|| "workbuddy".to_string());
    let threshold: f64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(0.15);
    let journal = std::path::Path::new(db).with_file_name("retire-journal.ndjson");
    let svc = MemoryService::open(db, &journal).expect("打开记忆服务失败");
    let va = VerifiedAgent::for_local_owner(agent.clone());

    let rows = svc
        .retirement_candidates(&va, threshold, 100)
        .expect("退役候选计算失败");

    // 分母：直接用只读连接数（不走 service，避免影响任何计数）
    let conn = rusqlite::Connection::open(db).expect("打开只读连接失败");
    let total: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories WHERE deleted_at IS NULL AND valid_to IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(-1);

    println!(
        "RETIRE_REPORT total={total} candidates={} threshold={threshold} agent={agent}",
        rows.len()
    );
    for r in rows.iter().take(20) {
        println!(
            "- {} ei={:.3} imp={} acc={} days={:.0} edges={} origin={} reasons={:?} | {}",
            r.id,
            r.ei,
            r.importance,
            r.access_count,
            r.days_since_access,
            r.edge_count,
            r.origin,
            r.reasons,
            r.content_head
        );
    }
}
