//! 验证 verify_inject 的后端逻辑（不经过 IPC）：四个身份各自能否召回。
use orbital_core::identity::VerifiedAgent;
use orbital_core::service::{MemoryService, RecallOptions};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let db = &args[1];
    let journal = &args[2];
    let svc = MemoryService::open(db, journal).expect("打开服务失败");
    for id in ["dsh", "hermes", "pi", "workbuddy"] {
        let va = VerifiedAgent::for_local_owner(id.to_string());
        let opts = RecallOptions::default();
        match svc.recall_fused(&va, "Orbital 项目", 3, &opts) {
            Ok(f) => println!("{id:10} hits={} degraded={:?}", f.hits.len(), f.degraded),
            Err(e) => println!("{id:10} ERROR: {e}"),
        }
    }
}
