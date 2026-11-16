//! 修正一条过时记忆：把「Orbital / 忆核」的旧项目路径（agent-memory-hub）更新为当前项目（orbital-pro）。
//!
//! 用法：cargo run -p orbital-core --example fix-stale-memory -- <db> <journal>

use orbital_core::identity::VerifiedAgent;
use orbital_core::service::{MemoryService, UpdatePatch};

const ID: &str = "mem_6d30eaadf8e5465c9e8a129194cae101";
const NEW_CONTENT: &str = "\
「Orbital / 忆核」= C:\\Users\\<username>\\Documents\\Development project\\20_项目_Projects\\orbital-pro（当前正式项目；\
旧项目 agent-memory-hub 已归档为测试项目，不要再引用它的路径与 schema v2 数字）。\
技术栈：Tauri v2 + Rust 多 agent 记忆中枢（crates/orbital-core + orbital-mcp + orbital-proxy + src/ React + src-tauri）；\
生产库 %LOCALAPPDATA%\\OrbitalPro\\data\\orbital.db（schema v3；版本 0.92.1；根 workspace 141 测试 + src-tauri 12 测试）。\
四 agent 注入通道（2026-11-14 全部真机确认）：WorkBuddy=MEMORY.md 受控区块 / Hermes=pre_llm_call shell hook / \
pi=before_agent_start 扩展 / DSH=agent/pre-step 插件（写 decision.messages，不能写 additionalContexts）。\
设计变更三件套：①docs/07 写 ADR（含被否方案与不许回退的理由）②「已废止设计清单」追加一行 ③check-docs.ps1 术语表加一条并做红绿对照。\
坑：Get-Content 读单行文件返回 String 不是数组（[0] 取到首字符 → 扫描器假绿）。";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("用法: fix-stale-memory <db> <journal>");
        std::process::exit(2);
    }
    let svc = MemoryService::open(&args[1], &args[2]).expect("打开服务失败");
    // 用该记忆所属 agent 的身份（门控要求）
    let va = VerifiedAgent::for_local_owner("workbuddy".to_string());
    let patch = UpdatePatch {
        content: Some(NEW_CONTENT.to_string()),
        importance: None,
        tags: None,
        pinned: None,
        scope: None, // 保持 shared
    };
    match svc.update(&va, ID, patch) {
        Ok(out) => println!(
            "UPDATED action={} id={} superseded={:?} note={}",
            out.action, out.id, out.superseded_id, out.note
        ),
        Err(e) => {
            eprintln!("FAILED: {e}");
            std::process::exit(1);
        }
    }
}
