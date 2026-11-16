//! 召回基线 / 三路对照评分（T-M3-01 + T-M3-02）。
//!
//! 读 `queries.jsonl`（每条带 `expected_id`），逐条调 **core 的召回路径**
//! （单路时 `recall()`；多路时 `recall_fused()`），输出 hit@1 / hit@5 / MRR。
//!
//! 用法：
//! ```text
//! cargo run -p orbital-core --example recall-baseline -- <db> <queries.jsonl> [agent_id] [channels]
//! ```
//! - `channels`：逗号分隔，`fts` / `vec` / `graph`，默认 `fts`（= T-M3-01 基线的单路口径）
//! - 四组对照：`fts` / `fts,vec` / `fts,graph` / `fts,vec,graph`
//! - `db` 应是**副本**（`MemoryService::open` 会建表/迁移并启动写线程，不写业务数据）
//!
//! 输出是**确定性的**：同一条 db + 同一份 queries + 同一组 channels 跑两次，数字必须一致。

use std::io::{BufRead, BufReader};

use orbital_core::identity::VerifiedAgent;
use orbital_core::service::{MemoryService, RecallChannel, RecallOptions};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("用法: recall-baseline <db> <queries.jsonl> [agent_id] [channels]");
        std::process::exit(2);
    }
    let db = &args[1];
    let qpath = &args[2];
    let agent = args.get(3).cloned().unwrap_or_else(|| "workbuddy".to_string());
    let channels_arg = args.get(4).cloned().unwrap_or_else(|| "fts".to_string());
    let channels: Vec<RecallChannel> = channels_arg
        .split(',')
        .filter_map(RecallChannel::parse)
        .collect();
    if channels.is_empty() {
        eprintln!("无法解析 channels: {channels_arg}（只支持 fts/vec/graph）");
        std::process::exit(2);
    }
    // 第 5 个参数：逗号分隔的权重（与 channels 对齐），可省略 = 全 1.0
    let weights: Vec<f64> = match args.get(5) {
        Some(s) => s
            .split(',')
            .filter_map(|x| x.trim().parse::<f64>().ok())
            .collect(),
        None => vec![1.0; channels.len()],
    };
    let weights = if weights.len() == channels.len() {
        weights
    } else {
        eprintln!("权重个数与通道数不一致（{} vs {}），回退全 1.0", weights.len(), channels.len());
        vec![1.0; channels.len()]
    };
    // 第 6 个参数：图加成衰减系数（ADR-040，默认 0.5）
    let graph_decay = args
        .get(6)
        .and_then(|x| x.parse::<f64>().ok())
        .unwrap_or(0.5);

    let opts = RecallOptions {
        weights: weights.clone(),
        channels: channels.clone(),
        k: 60,
        // 基线对照不启用结构化过滤（T-M3-03 的过滤能力由单测覆盖）
        fact_filter: None,
        graph_decay,
    };

    let journal = std::path::Path::new(db).with_file_name("recall-baseline-journal.ndjson");
    let svc = MemoryService::open(db, &journal).expect("打开记忆服务失败");
    let va = VerifiedAgent::for_local_owner(agent.clone());

    let file = std::fs::File::open(qpath).expect("打开 queries.jsonl 失败");
    let reader = BufReader::new(file);

    let mut total = 0usize;
    let mut hit1 = 0usize;
    let mut hit5 = 0usize;
    let mut rr_sum = 0.0f64;
    let mut misses: Vec<(String, String, Vec<String>)> = Vec::new();
    let mut degraded: Vec<String> = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        let q: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("跳过无法解析的行: {e}");
                continue;
            }
        };
        let query = q["query"].as_str().unwrap_or("").to_string();
        let expected = q["expected_id"].as_str().unwrap_or("").to_string();
        let qid = q["id"].as_str().unwrap_or("?").to_string();
        if query.is_empty() || expected.is_empty() {
            continue;
        }
        total += 1;

        let ids: Vec<String> = if channels.len() == 1 && channels[0] == RecallChannel::Fts {
            // 单路：走 `recall()`，与 T-M3-01 基线同一路径
            svc.recall(&va, &query, 5)
                .expect("recall 失败")
                .into_iter()
                .map(|h| h.id)
                .collect()
        } else {
            let r = svc
                .recall_fused(&va, &query, 5, &opts)
                .expect("recall_fused 失败");
            for d in r.degraded {
                if !degraded.contains(&d) {
                    degraded.push(d);
                }
            }
            r.hits.into_iter().map(|h| h.id).collect()
        };

        let rank = ids.iter().position(|id| id == &expected);
        match rank {
            Some(0) => {
                hit1 += 1;
                hit5 += 1;
                rr_sum += 1.0;
            }
            Some(r) => {
                hit5 += 1;
                rr_sum += 1.0 / ((r + 1) as f64);
            }
            None => misses.push((qid, query, ids)),
        }
    }

    let pct = |n: usize| {
        if total == 0 {
            0.0
        } else {
            n as f64 * 100.0 / total as f64
        }
    };
    let mrr = if total == 0 { 0.0 } else { rr_sum / total as f64 };

    println!("RECALL_BASELINE_RESULT");
    println!("agent={agent}");
    println!("channels={channels_arg}");
    println!("weights={:?}", weights);
    println!("queries={total}");
    println!("hit_at_1={hit1} ({:.1}%)", pct(hit1));
    println!("hit_at_5={hit5} ({:.1}%)", pct(hit5));
    println!("mrr={mrr:.4}");
    println!("misses={}", misses.len());
    for d in &degraded {
        println!("degraded={d}");
    }
    for (qid, query, got) in misses.iter().take(20) {
        println!("MISS {qid} query={query:?} got={got:?}");
    }
}
