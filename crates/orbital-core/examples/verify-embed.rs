//! 验证嵌入端点一致性（真实调用，不是 mock）。
//!
//! 判据：同模型不同端点对同一组探测句算出的向量，逐条余弦 ≥ 0.99。
//! 运行：cargo run -p orbital-core --example verify-embed
//!
//! 密钥从 `~/.orbital/secrets.json` 读，**不打印**。

use orbital_core::embed::{load_config, verify_endpoints, EndpointKind};

fn main() {
    let cfg = load_config();
    println!("模型规格: {} / {} 维", cfg.model.id, cfg.model.dimensions);
    println!("端点总数: {}", cfg.endpoints.len());
    for ep in &cfg.endpoints {
        println!(
            "  - {} [{}] kind={} base={} model={} key={}",
            ep.id,
            if ep.enabled { "enabled" } else { "disabled" },
            ep.kind.as_str(),
            ep.base_url,
            ep.model_name,
            if ep.kind == EndpointKind::Cloud {
                if ep.api_key.is_empty() {
                    "(none)"
                } else {
                    "(set)"
                }
            } else {
                "(n/a)"
            }
        );
    }
    let active = cfg.active_endpoints();
    println!("\n可用端点: {}", active.len());
    if active.is_empty() {
        println!("没有可用端点（检查 secrets.json / allowRemoteContent）");
        return;
    }

    println!("\n开始一致性验证（5 条探测句）…");
    let checks = verify_endpoints(&cfg);
    let mut fails = 0;
    for c in &checks {
        println!(
            "{} {:<12} dim={:<5} {}ms  {}",
            if c.ok { "PASS" } else { "FAIL" },
            c.endpoint_id,
            c.dimensions,
            c.elapsed_ms,
            c.reason
        );
        if !c.ok {
            fails += 1;
        }
    }
    println!();
    if fails == 0 {
        println!("EMBED-VERIFY: PASS（所有端点同一向量空间）");
    } else {
        println!("EMBED-VERIFY: FAIL ({fails} 个端点不一致或不可用)");
        std::process::exit(1);
    }
}
