//! 退役候选对照（T-M9-01）：把半衰期参数改一改，看候选列表怎么变、变化能不能解释。
//!
//! 用法：`cargo run -p orbital-core --example retirement-diff -- <db路径> [hlA] [hlB]`
//!
//! 判据（docs/05 M4「公式参数改动后，退役候选列表变化可解释」）：
//!   - 半衰期**变短** → 老记忆衰减更快 → 候选变多；且新增的必须是"更老/更少访问"的那批；
//!   - 输出抽样 10 条（进入/退出各 5 条），附 age(天) / access / importance，能逐条解释。

use std::collections::BTreeMap;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("用法: retirement-diff <db路径> [hlA=30] [hlB=15]");
        std::process::exit(2);
    }
    let db = &args[1];
    let hl_a: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(30.0);
    let hl_b: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(15.0);

    let svc = match orbital_core::service::MemoryService::open(
        std::path::PathBuf::from(db),
        std::path::PathBuf::from("./j"),
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("打开库失败: {e}");
            std::process::exit(1);
        }
    };

    let scan = |hl: f64, th: f64| -> Vec<orbital_core::service::RetirementRow> {
        std::env::set_var("ORBITAL_HALF_LIFE_DAYS", hl.to_string());
        svc.retirement_candidates_all(th, 500).unwrap_or_default()
    };

    // 阈值扫描：默认 0.35 时常常"全入选"或"全不入选"，看不出差异。
    // 自动找第一个能让两档**结果不同**的阈值，再在那个阈值下做抽样——
    // 因为"参数影响可解释"只有在候选跨越阈值时才能被观察到。
    let mut th_used = 0.35f64;
    for th in [0.35f64, 0.30, 0.25, 0.20, 0.15, 0.10, 0.07, 0.05, 0.03, 0.02] {
        let na = scan(hl_a, th).len();
        let nb = scan(hl_b, th).len();
        if na != nb {
            th_used = th;
            println!("== 阈值扫描：{th} 时两档不同（{na} vs {nb}），用它做抽样");
            break;
        }
    }

    let a = scan(hl_a, th_used);
    let b = scan(hl_b, th_used);
    let set_a: BTreeMap<&str, &orbital_core::service::RetirementRow> =
        a.iter().map(|r| (r.id.as_str(), r)).collect();
    let set_b: BTreeMap<&str, &orbital_core::service::RetirementRow> =
        b.iter().map(|r| (r.id.as_str(), r)).collect();

    println!("== 半衰期 {hl_a} 天（阈值 {th_used}）：{} 条候选", a.len());
    println!("== 半衰期 {hl_b} 天（阈值 {th_used}）：{} 条候选", b.len());

    let mut entered: Vec<&orbital_core::service::RetirementRow> = Vec::new();
    let mut left: Vec<&orbital_core::service::RetirementRow> = Vec::new();
    for r in b.iter() {
        if !set_a.contains_key(r.id.as_str()) {
            entered.push(r);
        }
    }
    for r in a.iter() {
        if !set_b.contains_key(r.id.as_str()) {
            left.push(r);
        }
    }

    println!("\n== 进入候选（半衰期 {hl_b} 才有）：{} 条", entered.len());
    for r in entered.iter().take(10) {
        println!(
            "  {} | 未访问(天) {:.0} | access {} | importance {} | ei {:.3}",
            short(&r.content_head),
            r.days_since_access,
            r.access_count,
            r.importance,
            r.ei
        );
    }
    println!("\n== 退出候选（半衰期 {hl_b} 就没有了）：{} 条", left.len());
    for r in left.iter().take(10) {
        println!(
            "  {} | 未访问(天) {:.0} | access {} | importance {} | ei {:.3}",
            short(&r.content_head),
            r.days_since_access,
            r.access_count,
            r.importance,
            r.ei
        );
    }

    // 可解释性自检：半衰期变短后，进入候选的应该**更老**（或访问更少），退出的应该**更新**
    let avg_age = |v: &[&orbital_core::service::RetirementRow]| -> f64 {
        if v.is_empty() {
            0.0
        } else {
            v.iter().map(|r| r.days_since_access).sum::<f64>() / v.len() as f64
        }
    };
    println!("\n== 可解释性 ==");
    println!(
        "进入候选的平均年龄 {:.0} 天 / 退出候选的平均年龄 {:.0} 天",
        avg_age(&entered),
        avg_age(&left)
    );
    let ok = entered.is_empty() || avg_age(&entered) >= avg_age(&left);
    println!(
        "判据（半衰期变短 → 新增候选应更老）：{}",
        if ok { "PASS" } else { "FAIL（需人工看抽样）" }
    );
}

fn short(s: &str) -> String {
    let t: String = s.chars().take(24).collect();
    if s.chars().count() > 24 {
        format!("{t}…")
    } else {
        t
    }
}
