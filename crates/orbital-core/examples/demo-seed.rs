//! 生成**演示数据**：往一个临时库里写十几条虚构记忆，用于截图、演示与手动试用。
//!
//! 设计原则：
//! - **绝不写真实库**。只接受调用方显式给出的 db / journal 路径；
//! - 内容全部是**虚构的通用技术场景**（不是任何真实用户的数据）；
//! - 覆盖四种 agent（dsh / hermes / pi / workbuddy）、多 category、多 importance，
//!   这样界面上的分组、筛选、分层统计都有东西可看。
//!
//! 用法：
//! ```text
//! cargo run -p orbital-core --example demo-seed -- <db> <journal>
//! ```
//!
//! 典型用法（配合界面截图）：
//! ```powershell
//! $env:ORBITAL_DB_PATH="$env:TEMP\orbital-demo\orbital.db"
//! cargo run -p orbital-core --example demo-seed -- "$env:TEMP\orbital-demo\orbital.db" "$env:TEMP\orbital-demo\journal.ndjson"
//! # 再用同一个 ORBITAL_DB_PATH 启动 GUI 截图
//! ```

use orbital_core::db::Db;
use orbital_core::identity::{register_agent, verify_token};
use orbital_core::service::MemoryService;
use orbital_core::writer::WriteInput;

/// 演示数据：`(agent, category, importance, tags, content)`
const DEMO: &[(&str, &str, i32, &[&str], &str)] = &[
    (
        "dsh",
        "架构",
        5,
        &["架构", "进程"],
        "本机采用三进程架构：GUI 负责界面、core 负责记忆读写、proxy 负责注入代理。三者端口分离，任何一个挂掉不影响另外两个。",
    ),
    (
        "dsh",
        "配置",
        4,
        &["端口", "配置"],
        "端口约定：core 健康检查默认 8791、proxy 默认 8790、只读召回端点 8799。端口被占用时自动顺延，界面显示实际值。",
    ),
    (
        "dsh",
        "教训",
        5,
        &["调试", "判据"],
        "出故障先怀疑自己的判据，再怀疑被测对象。上次把 proxy 正常返回判成「被其他服务占用」，原因是检查器只认 role 字段，而 proxy 返回的是 service。",
    ),
    (
        "hermes",
        "设计",
        4,
        &["UI", "设计"],
        "界面字体统一用 Inter Variable，数字用 JetBrains Mono 等宽。圆角只用两档：控件 4px、卡片 6px。",
    ),
    (
        "hermes",
        "架构",
        4,
        &["数据库", "可靠性"],
        "SQLite 必须用 bundled 版本并把模式设为 WAL：系统自带的库版本存在 WAL 重置缺陷。",
    ),
    (
        "hermes",
        "调研",
        3,
        &["检索", "评测"],
        "召回基线：单路全文检索 hit@1 = 46/50、hit@5 = 47/50、MRR 0.93。任何检索改动都要先跑同一查询集做对照。",
    ),
    (
        "pi",
        "教训",
        5,
        &["Windows", "脚本"],
        "PowerShell 的 Get-Content 读只有一行的文件会返回字符串而不是数组，[0] 取到的是首字符。扫描器会因此假绿，必须强制 @(Get-Content ...)。",
    ),
    (
        "pi",
        "架构",
        4,
        &["注入", "超时"],
        "注入是热路径：超时默认 300ms，超时直接跳过注入，绝不阻塞对话。宁可少注入一次，也不能让用户等。",
    ),
    (
        "pi",
        "配置",
        3,
        &["备份", "SQLite"],
        "备份使用 SQLite 在线备份 API，备份完成后切换成 DELETE 日志模式并做整理，产出一个单独文件便于拷贝与校验。",
    ),
    (
        "workbuddy",
        "设计",
        3,
        &["协作", "规范"],
        "提交信息用中文写清楚「做了什么 + 为什么」，并在正文里贴关键验证输出，方便之后回溯。",
    ),
    (
        "workbuddy",
        "架构",
        4,
        &["删除", "可逆"],
        "删除一律先软删并保留恢复入口，硬删要用户二次确认且不可撤销。每个操作都必须有可回滚路径。",
    ),
    (
        "workbuddy",
        "调研",
        3,
        &["取证", "界面"],
        "界面改动必须真机截图取证，并且要看窗口实际矩形：高 DPI 下逻辑像素与物理像素混算，会让面板底部溢出屏幕。",
    ),
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("用法: demo-seed <db> <journal>");
        std::process::exit(2);
    }
    let db_path = &args[1];
    let journal = &args[2];

    // 1) 注册演示用的四个 agent（每个 agent 一个 token；演示库是临时的，token 用完即弃）
    let mut verified = Vec::new();
    for agent in ["dsh", "hermes", "pi", "workbuddy"] {
        let dbx = Db::open(db_path).expect("打开库失败");
        let tok = register_agent(dbx.conn(), agent, &format!("{agent} (demo)")).expect("注册 agent 失败");
        let va = verify_token(dbx.conn(), &tok).expect("验证 token 失败");
        drop(dbx);
        verified.push((agent.to_string(), va));
    }

    let svc = MemoryService::open(db_path, journal).expect("打开服务失败");

    let mut ok = 0usize;
    for (agent, category, importance, tags, content) in DEMO {
        let va = verified
            .iter()
            .find(|(a, _)| a == agent)
            .map(|(_, v)| v)
            .expect("agent 未注册");
        let mut wi = WriteInput::new(*agent, *content);
        wi.category = category.to_string();
        wi.importance = *importance;
        wi.tags = tags.iter().map(|t| t.to_string()).collect();
        match svc.write(va, wi) {
            Ok(m) => {
                ok += 1;
                println!("SEEDED {} [{}] {}", m.id, agent, category);
            }
            Err(e) => eprintln!("FAILED [{agent}] {category}: {e}"),
        }
    }
    println!("完成：写入 {ok}/{} 条演示记忆 → {db_path}", DEMO.len());
}
