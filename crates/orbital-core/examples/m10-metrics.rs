//! T-M10-01 第 3/4 项：**静默失败** 与 **跨 agent 泄漏** 的实测程序。
//!
//! 用法（**必须指向副本库**，不要对真实库跑）：
//! ```text
//! cargo run -p orbital-core --example m10-metrics -- <db副本> <journal副本路径>
//! ```
//!
//! 口径：
//! - **静默失败** = `write()` 返回 Ok，但按 id 回查不到 / 内容不一致。目标 **0**。
//! - **跨 agent 泄漏** = B 能 `get` 到 A 的 private 记忆，或 `recall` 结果里出现 A 的 private 内容。
//!   目标 **0**（160 次探测）。
//! - **正对照**：A 写一条 `scope=shared, origin=user_endorsed`，B 必须能召回——
//!   若正对照也失败，说明"查不到"是门控太严或召回坏了，不能算"无泄漏"。

use orbital_core::db::Db;
use orbital_core::identity::{register_agent, verify_token};
use orbital_core::service::MemoryService;
use orbital_core::writer::WriteInput;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("用法: m10-metrics <db副本> <journal副本路径>");
        std::process::exit(2);
    }
    let db = &args[1];
    let journal = &args[2];

    // 先用一个短命连接注册两个探针 agent（拿 token 并验出身份），随后释放连接，
    // 再 open MemoryService（只保留一个写连接，避免锁竞争）。
    let (va, vb) = {
        let dbx = Db::open(db).expect("打开库失败");
        let tok_a = register_agent(dbx.conn(), "m10-probe-a", "M10 Probe A").unwrap();
        let tok_b = register_agent(dbx.conn(), "m10-probe-b", "M10 Probe B").unwrap();
        (
            verify_token(dbx.conn(), &tok_a).unwrap(),
            verify_token(dbx.conn(), &tok_b).unwrap(),
        )
    };

    let svc = MemoryService::open(db, journal).expect("打开记忆服务失败");

    println!("## 静默失败（写 100 条 → 逐条回查）\n");

    let mut written: Vec<(usize, String, String)> = Vec::new();
    let mut write_err = 0usize;
    for i in 0..100usize {
        // 每条内容唯一，避免 UNIQUE(agent,session,hash) 去重把重复写入吞掉
        let content = format!("M10 silent-fail probe #{i:03} 唯一标记 zq{i:03}wk");
        match svc.write(&va, WriteInput::new("m10-probe-a", content.clone())) {
            Ok(m) => written.push((i, m.id, content)),
            Err(_) => write_err += 1,
        }
    }

    let mut readback_fail = 0usize;
    let mut content_mismatch = 0usize;
    let mut fail_ids: Vec<usize> = Vec::new();
    for (i, id, content) in &written {
        match svc.get(&va, id) {
            Ok(m) => {
                if &m.content != content {
                    content_mismatch += 1;
                    fail_ids.push(*i);
                }
            }
            Err(_) => {
                readback_fail += 1;
                fail_ids.push(*i);
            }
        }
    }
    let silent_fail = readback_fail + content_mismatch;
    println!("| 指标 | 值 |");
    println!("|---|---|");
    println!("| 写入成功 | {} / 100 |", written.len());
    println!("| 写入报错 | {} |", write_err);
    println!("| 回查失败（查不到） | {} |", readback_fail);
    println!("| 内容不一致 | {} |", content_mismatch);
    println!("| **静默失败合计** | **{}**（目标 0） |", silent_fail);
    if !fail_ids.is_empty() {
        println!("| 失败样本序号 | {:?} |", &fail_ids[..fail_ids.len().min(10)]);
    }

    // ===== 跨 agent 泄漏 =====
    println!("\n## 跨 agent 泄漏（B 用 160 次探测读 A 的 private）\n");

    // A 写 16 条 private（不同主题词）
    let topics = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
        "juliet", "kilo", "lima", "mike", "november", "oscar", "papa",
    ];
    let mut a_private: Vec<(String, String)> = Vec::new(); // (id, 主题词)
    for t in topics {
        let content = format!("M10 私有记忆主题 {t} 机密 m10secret{t} 仅 A 可见");
        let m = svc
            .write(&va, WriteInput::new("m10-probe-a", content))
            .expect("写私有记忆失败");
        a_private.push((m.id, t.to_string()));
    }

    // ① get 探测：B 按 id 直接取 A 的私有（16 次 × 10 轮 = 160 次）
    let mut leak_get = 0usize;
    let mut probes = 0usize;
    for _round in 0..10 {
        for (id, _) in &a_private {
            probes += 1;
            if svc.get(&vb, id).is_ok() {
                leak_get += 1;
            }
        }
    }

    // ② recall 探测：B 用 160 次不同查询词召回（含 A 的机密串、主题词）
    let mut leak_recall = 0usize;
    let mut recall_probes = 0usize;
    let mut queries: Vec<String> = Vec::new();
    for (_, t) in &a_private {
        queries.push(format!("m10secret{t}"));
        queries.push(format!("私有记忆主题 {t}"));
    }
    // 16 主题 × 2 = 32 条查询；每条做 5 轮 = 160 次
    for _round in 0..5 {
        for q in &queries {
            recall_probes += 1;
            if let Ok(hits) = svc.recall(&vb, q, 20) {
                if hits.iter().any(|h| h.content.contains("m10secret")) {
                    leak_recall += 1;
                }
            }
        }
    }

    // ③ 正对照：A 写 shared（user_endorsed），B 必须能召回
    let mut shared_in = WriteInput::new("m10-probe-a", "M10 shared 正对照 sharedcontrol-visible");
    shared_in.scope = "shared".into();
    shared_in.origin = "user_endorsed".into();
    let shared_ok = svc.write(&va, shared_in).is_ok();
    let control_hits = svc.recall(&vb, "sharedcontrol-visible", 10).unwrap_or_default();
    let control_ok = control_hits.iter().any(|h| h.content.contains("sharedcontrol-visible"));

    println!("| 指标 | 值 |");
    println!("|---|---|");
    println!("| get 探测（B 按 id 取 A 私有） | {} 次，命中 **{}** |", probes, leak_get);
    println!("| recall 探测（B 查 A 私有内容） | {} 次，命中 **{}** |", recall_probes, leak_recall);
    println!("| **泄漏合计** | **{}**（目标 0） |", leak_get + leak_recall);
    println!("| 正对照：shared 写入 | {} |", if shared_ok { "Ok" } else { "失败" });
    println!(
        "| 正对照：B 能召回 A 的 shared | {} |",
        if control_ok { "✅ 通过（说明门控不是一味拒绝）" } else { "❌ 失败（此时上面的 0 不能算无泄漏）" }
    );

    println!("\nM10_METRICS_DONE silent_fail={} leak_get={} leak_recall={} control_ok={}",
        silent_fail, leak_get, leak_recall, control_ok as i32);
}
