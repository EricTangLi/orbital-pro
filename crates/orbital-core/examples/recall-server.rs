//! 起 core 的本地只读召回端点（ADR-041），用于 T-M3-06 的注入冒烟。
//!
//! 用法：`cargo run -p orbital-core --example recall-server -- <db> [bind]`
//! 输出 `RECALL_SERVER_READY port=NNNN` 后常驻。

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("用法: recall-server <db> [bind]");
        std::process::exit(2);
    }
    let db = &args[1];
    let bind = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:8791".to_string());
    let journal = std::path::Path::new(db).with_file_name("recall-server-journal.ndjson");
    match orbital_core::local_api::spawn_recall_server(db, &journal, &bind) {
        Ok(port) => println!("RECALL_SERVER_READY port={port}"),
        Err(e) => {
            eprintln!("起端点失败: {e}");
            std::process::exit(1);
        }
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}
