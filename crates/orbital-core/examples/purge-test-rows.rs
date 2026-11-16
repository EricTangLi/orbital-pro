//! 清理测试痕迹（T-M9 之后的正式使用准备）：走 `forget_local_owner`（本地所有者语义），
//! **不裸写 SQL**——符合红线 5「不许绕过服务层直接 UPDATE」。
//!
//! 用法：`cargo run -p orbital-core --example purge-test-rows -- <db路径> <soft|invalidate> <id1> <id2> ...`

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("用法: purge-test-rows <db路径> <soft|invalidate> <id...>");
        std::process::exit(2);
    }
    let db = std::path::PathBuf::from(&args[1]);
    let mode = &args[2];
    let ids: Vec<&String> = args[3..].iter().collect();

    let svc = match orbital_core::service::MemoryService::open(db, std::path::PathBuf::from("./j")) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("打开库失败: {e}");
            std::process::exit(1);
        }
    };
    for id in ids {
        match svc.forget_local_owner(id, mode) {
            Ok(ev) => println!("ok  {id}  event={ev}"),
            Err(e) => println!("err {id}  {e}"),
        }
    }
}
