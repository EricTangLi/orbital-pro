//! 三进程启动器（`docs/02 §3.2/§3.3` · ADR-015）
//!
//! 职责：
//! 1. GUI 进程用**同一个 exe** spawn 出 `--role core` / `--role proxy`；
//! 2. 崩溃自动重启（指数退避 1s/2s/4s，最多 3 次）；
//! 3. **孤儿回收**：上次异常退出留下的子进程，启动时识别并接管；
//! 4. 运行态 `launcher.ndjson` 每次变化**立刻重写**（Spike 已证：不重写会杀错进程）。
//!
//! 孤儿判据（三条同时成立才算"我们的进程"，防止误杀别的程序占着端口）：
//!   a) pid 仍然存活；
//!   b) 该进程命令行里含 `--role <role>`；
//!   c) 该进程的可执行路径 == 当前 exe。

use std::collections::BTreeMap;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 子进程表：`role -> (child, port, restarts)`。
///
/// 抽成别名是为了可读（clippy `type_complexity`）；改动这个元组时要同时看 `write_state`。
type Children = Arc<Mutex<BTreeMap<String, (Child, u16, u32)>>>;

#[derive(Debug, Clone)]
pub struct RuntimeEntry {
    pub role: String,
    pub pid: u32,
    pub port: u16,
    pub started_at: f64,
    pub restarts: u32,
}

/// 启动器句柄。`stop()` 会杀掉所有子进程并清掉运行态。
pub struct Launcher {
    state_path: PathBuf,
    children: Children, // role -> (child, port, restarts)
    watch: Option<std::thread::JoinHandle<()>>,
    stop_flag: Arc<Mutex<bool>>,
}

impl Launcher {
    /// 回收孤儿 → 拉起 core/proxy → 写运行态 → 起监控线程。
    pub fn start(core_port: u16, proxy_port: u16) -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let state_path = runtime_dir().join("launcher.ndjson");

        reap_orphans(&exe, &state_path);

        // T-M10-47：先顺延端口，再 spawn。
        // 之前这里只 `wait_port_free(port, 3000)` 且**不检查返回值**——等 3 秒后照样 spawn，
        // 子进程 bind 失败 exit 2，watchdog 重启 3 次放弃，界面只有一句"进程 2/3"。
        let want_core = core_port;
        let want_proxy = proxy_port;
        let core_port = pick_free_port(want_core, &[]);
        let proxy_port = pick_free_port(want_proxy, &[core_port]);
        if core_port != want_core {
            tracing::warn!(
                want = want_core,
                actual = core_port,
                "core 端口被占用，已顺延（界面会显示实际端口）"
            );
            append_port_shift("core", want_core, core_port);
        }
        if proxy_port != want_proxy {
            tracing::warn!(
                want = want_proxy,
                actual = proxy_port,
                "proxy 端口被占用，已顺延"
            );
            append_port_shift("proxy", want_proxy, proxy_port);
        }

        let children: Children = Arc::new(Mutex::new(BTreeMap::new()));
        let stop_flag = Arc::new(Mutex::new(false));

        for (role, port) in [("core", core_port), ("proxy", proxy_port)] {
            let _ = wait_port_free(port, 3000);
            let child = spawn_role(&exe, role, port)?;
            children
                .lock()
                .unwrap()
                .insert(role.to_string(), (child, port, 0));
        }
        write_state(&state_path, &children.lock().unwrap());

        // 监控线程：子进程退出 → 指数退避重启
        let watch = {
            let exe = exe.clone();
            let state_path = state_path.clone();
            let children = children.clone();
            let stop_flag = stop_flag.clone();
            std::thread::spawn(move || loop {
                if *stop_flag.lock().unwrap() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(800));

                let mut changed = false;
                let mut guard = children.lock().unwrap();
                for role in ["core", "proxy"] {
                    // 退出码要带出来（T-M5-03：诊断"为什么退出"靠它）
                    let mut exit_code: Option<Option<i32>> = None;
                    if let Some((child, _port, _r)) = guard.get_mut(role) {
                        if let Ok(Some(status)) = child.try_wait() {
                            tracing::warn!(role, ?status, "child exited; scheduling restart");
                            exit_code = Some(status.code());
                        }
                    }
                    let Some(exit) = exit_code else { continue };
                    let (port, restarts) = {
                        let e = guard.get(role).unwrap();
                        (e.1, e.2)
                    };
                    if restarts >= MAX_RESTARTS {
                        tracing::error!(role, "restart limit reached; giving up");
                        append_launcher_event(role, exit, restarts, true);
                        continue;
                    }
                    let delay = Duration::from_millis(1000 * (1u64 << restarts));
                    std::thread::sleep(delay);
                    // T-M10-47：重启前重新挑端口——原端口可能被外部程序抢走了
                    // （本机 xray 就是这么占掉 8791 的），否则会一直 exit 2 直到放弃。
                    let other: Vec<u16> = guard
                        .iter()
                        .filter(|(r, _)| r.as_str() != role)
                        .map(|(_, v)| v.1)
                        .collect();
                    let new_port = pick_free_port(port, &other);
                    if new_port != port {
                        tracing::warn!(role, from = port, to = new_port, "重启时端口被占，已顺延");
                        append_port_shift(role, port, new_port);
                    }
                    let _ = wait_port_free(new_port, 3000);
                    match spawn_role(&exe, role, new_port) {
                        Ok(c) => {
                            guard.insert(role.to_string(), (c, new_port, restarts + 1));
                            changed = true;
                            append_launcher_event(role, exit, restarts + 1, false);
                            tracing::info!(role, restarts = restarts + 1, "restarted");
                        }
                        Err(e) => {
                            tracing::error!(role, %e, "restart failed");
                            append_launcher_event(role, exit, restarts, false);
                        }
                    }
                }
                if changed {
                    write_state(&state_path, &guard);
                }
            })
        };

        Ok(Self {
            state_path,
            children,
            watch: Some(watch),
            stop_flag,
        })
    }

    /// T-M10-11（ADR-051）：只杀某个角色的子进程，**不置停止标记**——
    /// 监控线程会按既有退避策略自动拉起新进程，新进程启动时读 `proxy.conf.json`，
    /// 所以"保存配置 → 重启 proxy"不需要单独的重启逻辑。
    ///
    /// 返回 true = 找到了并已发 kill；false = 该角色没有子进程。
    pub fn restart_role(&mut self, role: &str) -> bool {
        let mut guard = self.children.lock().unwrap();
        match guard.get_mut(role) {
            Some((child, _port, _r)) => {
                let ok = child.kill().is_ok();
                tracing::info!(role, ok, "T-M10-11: killed child, watchdog will respawn");
                ok
            }
            None => false,
        }
    }

    /// 退出：先置停止标记，再逐个杀，最后清运行态。
    pub fn stop(&mut self) {
        *self.stop_flag.lock().unwrap() = true;
        if let Some(j) = self.watch.take() {
            let _ = j.join();
        }
        let mut guard = self.children.lock().unwrap();
        for (role, (child, _port, _r)) in guard.iter_mut() {
            let _ = child.kill();
            let _ = child.wait();
            tracing::info!(role, "stopped");
        }
        guard.clear();
        drop(guard);
        let _ = std::fs::remove_file(&self.state_path);
    }
}

impl Drop for Launcher {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_role(exe: &Path, role: &str, port: u16) -> std::io::Result<Child> {
    let mut cmd = Command::new(exe);
    cmd.arg("--role").arg(role);
    cmd.env("ORBITAL_CORE_PORT", port.to_string());
    cmd.env("ORBITAL_PROXY_PORT", port.to_string());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：子进程不弹控制台（GUI 子系统本不会弹，防御性保留）
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn()
}

/// 运行态目录：`%LOCALAPPDATA%\OrbitalPro\runtime`
pub fn runtime_dir() -> PathBuf {
    match std::env::var("LOCALAPPDATA") {
        Ok(base) => PathBuf::from(base).join("OrbitalPro").join("runtime"),
        Err(_) => PathBuf::from(".\\runtime"),
    }
}

fn write_state(path: &Path, children: &BTreeMap<String, (Child, u16, u32)>) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let now = now_secs();
    let mut body = String::new();
    for (role, (child, port, restarts)) in children.iter() {
        body.push_str(
            &serde_json::json!({
                "role": role,
                "pid": child.id(),
                "port": port,
                "started_at": now,
                "restarts": restarts,
            })
            .to_string(),
        );
        body.push('\n');
    }
    // 整体重写（不追加）：避免历史行让"按 pid 杀"杀到复用后的别人
    if let Ok(mut f) = std::fs::File::create(path) {
        let _ = f.write_all(body.as_bytes());
        let _ = f.sync_all();
    }
}

fn read_state(path: &Path) -> Vec<RuntimeEntry> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            out.push(RuntimeEntry {
                role: v["role"].as_str().unwrap_or("").to_string(),
                pid: v["pid"].as_u64().unwrap_or(0) as u32,
                port: v["port"].as_u64().unwrap_or(0) as u16,
                started_at: v["started_at"].as_f64().unwrap_or(0.0),
                restarts: v["restarts"].as_u64().unwrap_or(0) as u32,
            });
        }
    }
    out
}

/// 孤儿回收：三条判据都成立才杀（宁可留孤儿，也不误杀用户的其他程序）。
/// 子进程重启上限（T-M5-03）。超过就停止并上报——不许无限刷，否则会掩盖真 bug。
const MAX_RESTARTS: u32 = 3;

/// 重启事件落盘（`runtime_dir/launcher-events.ndjson`），供诊断与状态页读取。
///
/// 为什么单独一个文件：`launcher.ndjson` 是"当前运行态"（会被写指针清空/重写），
/// 事件是**追加**的历史，两者语义不同。
fn append_launcher_event(role: &str, exit: Option<i32>, restarts: u32, gave_up: bool) {
    let path = runtime_dir().join("launcher-events.ndjson");
    let line = serde_json::json!({
        "at": now_secs(),
        "role": role,
        "event": if gave_up { "gave_up" } else { "restart" },
        "exit_code": exit,
        "restarts": restarts,
    })
    .to_string();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// T-M10-47：端口顺延事件（同一份 `launcher-events.ndjson`，便于状态页/诊断解释
/// "为什么这次 core 不在 8791"）。
fn append_port_shift(role: &str, want: u16, actual: u16) {
    let path = runtime_dir().join("launcher-events.ndjson");
    let line = serde_json::json!({
        "at": now_secs(),
        "role": role,
        "event": "port_shift",
        "want": want,
        "actual": actual,
    })
    .to_string();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// 当前运行态（给 GUI 状态页）：每个角色的 pid / port / port / restarts。
pub fn runtime_state() -> Vec<RuntimeEntry> {
    read_state(&runtime_dir().join("launcher.ndjson"))
}

/// 重启事件历史（最近 n 条，最新在后）。
pub fn restart_events(limit: usize) -> Vec<serde_json::Value> {
    let path = runtime_dir().join("launcher-events.ndjson");
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut out: Vec<serde_json::Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    if out.len() > limit {
        out = out.split_off(out.len() - limit);
    }
    out
}

/// 孤儿回收：三条判据都成立才杀（宁可留孤儿，也不误杀用户的其他程序）。
fn reap_orphans(exe: &Path, state_path: &Path) {
    let entries = read_state(state_path);
    for e in entries {
        if e.pid == 0 || e.role.is_empty() {
            continue;
        }
        match is_our_process(exe, e.pid, &e.role) {
            true => {
                tracing::warn!(pid = e.pid, role = %e.role, "发现孤儿进程，接管中");
                let _ = kill_process(e.pid);
                std::thread::sleep(Duration::from_millis(300));
                wait_port_free(e.port, 3000);
            }
            false => {
                tracing::info!(pid = e.pid, role = %e.role, "运行态里的进程已不属于我们（可能是 pid 复用），忽略");
            }
        }
    }
    let _ = std::fs::remove_file(state_path);
}

/// 取某 pid 的命令行（不存在返回 None）。
fn process_cmd(pid: u32) -> Option<Vec<String>> {
    let sys = sysinfo::System::new_all().ok()?;
    let p = sys.process(sysinfo::Pid::from_u32(pid))?;
    Some(p.cmd().iter().map(|s| s.to_string_lossy().to_string()).collect())
}

/// 杀掉某 pid（不存在返回 false）。
fn kill_process(pid: u32) -> bool {
    let Ok(sys) = sysinfo::System::new_all() else {
        return false;
    };
    match sys.process(sysinfo::Pid::from_u32(pid)) {
        Some(p) => p.kill(),
        None => false,
    }
}

/// 判据 a+b+c 同时成立。
fn is_our_process(exe: &Path, pid: u32, role: &str) -> bool {
    let Some(cmd) = process_cmd(pid) else {
        return false; // a) 不存活
    };
    let has_role = cmd
        .windows(2)
        .any(|w| w[0] == "--role" && w[1] == role);
    if !has_role {
        return false;
    }
    // c) exe 路径一致（取命令行第 0 个元素；若为空则退化为按文件名比较）
    let exe_matches = match cmd.first() {
        Some(first) => {
            let a = PathBuf::from(first);
            match (a.canonicalize(), exe.canonicalize()) {
                (Ok(x), Ok(y)) => x == y,
                _ => a.file_name() == exe.file_name(),
            }
        }
        None => false,
    };
    exe_matches
}

/// T-M10-47：端口顺延扫描上限（从首选端口起最多往后试这么多个）。
const PORT_SCAN: u16 = 20;

/// 从 `preferred` 起找第一个能绑定的端口，跳过 `skip` 里的（避免 core/proxy 撞在一起）。
///
/// T-M10-47（2026-11-16 真机实证）：本机 `xray.exe` 把 8791 当代理出口长期占用
/// （`0.0.0.0:8791` Bound + 一条 ESTABLISHED），core 子进程 `bind` 直接
/// `os error 10013`（WSAEACCES，不是 10048 占用）→ exit 2 → watchdog 重试 3 次后放弃
/// ⇒ 界面永远显示"进程 2/3"，而用户完全不知道是被别的软件占了端口。
///
/// 设计取舍：只顺延**不持久化**。每次启动重新探测，避免"上次顺延到 8793"被写死成新默认值
/// （那样换台机器又会不一致）。实际端口写进 `launcher.ndjson`，界面从运行态读。
fn pick_free_port(preferred: u16, skip: &[u16]) -> u16 {
    for i in 0..PORT_SCAN {
        let p = preferred.saturating_add(i);
        if skip.contains(&p) {
            continue;
        }
        if TcpListener::bind(("127.0.0.1", p)).is_ok() {
            return p;
        }
    }
    preferred // 全都不可用：仍用首选值，让子进程自己报出真实错误（不静默掩饰）
}

/// 轮询等端口释放（Spike 实测：kill 后立即 bind 会报 10048）。
pub fn wait_port_free(port: u16, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => {
                drop(l);
                return true;
            }
            Err(_) => {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dir_is_under_localappdata() {
        let d = runtime_dir();
        assert!(d.to_string_lossy().contains("OrbitalPro"));
    }

    #[test]
    fn state_roundtrip() {
        let dir = std::env::temp_dir().join(format!("orbital-launcher-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("launcher.ndjson");
        std::fs::write(&p, "{\"role\":\"core\",\"pid\":1234,\"port\":8791,\"started_at\":1.0,\"restarts\":0}\n").unwrap();
        let e = read_state(&p);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].pid, 1234);
        assert_eq!(e[0].role, "core");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_port_free_succeeds_for_free_port() {
        // 找一个空闲端口：bind 后立刻释放
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        assert!(wait_port_free(port, 2000), "空闲端口应立即可用");
    }

    // ---- T-M10-47 端口顺延 ----
    //
    // 测试用**固定高位端口**，不用 `bind(0)` 让内核随机分配：
    // 实测 `cargo test` 并行时随机端口会落在系统已保留的区间（本机曾分到 2515，
    // 紧接着自己就绑不上），导致"实现没错、测试自己抖"。固定端口 + 被占就跳过本用例。

    fn try_reserve(port: u16) -> Option<TcpListener> {
        TcpListener::bind(("127.0.0.1", port)).ok()
    }

    #[test]
    fn pick_free_port_returns_preferred_when_free() {
        const P: u16 = 47992; // 扫描窗口 47992..48011
        let Some(l) = try_reserve(P) else {
            return; // 端口被本机别的东西占了，跳过（不误报）
        };
        drop(l);
        assert_eq!(pick_free_port(P, &[]), P, "空闲时应原样返回首选端口");
    }

    #[test]
    fn pick_free_port_skips_occupied() {
        const P: u16 = 48040; // 扫描窗口 48040..48059（与上面留出 >PORT_SCAN 的间隔，避免并行测试互抢）
        let Some(_holder) = try_reserve(P) else {
            return;
        };
        let picked = pick_free_port(P, &[]);
        assert_ne!(picked, P, "被占用的端口必须被跳过");
        assert!(
            picked > P && picked < P + PORT_SCAN,
            "顺延必须落在扫描窗口内，实际 {picked}"
        );
    }

    #[test]
    fn pick_free_port_respects_skip() {
        const P: u16 = 48100; // 扫描窗口 48100..48119（同上）
        let Some(l) = try_reserve(P) else {
            return;
        };
        drop(l);
        // 首选其实空闲，但被 skip 排除 → 必须换一个
        let picked = pick_free_port(P, &[P]);
        assert_ne!(picked, P, "skip 列表里的端口不能被选中");
    }

    #[test]
    fn is_our_process_false_for_foreign_pid() {
        // 当前测试进程自身没有 --role 参数 → 不应被认成我们的子进程
        let exe = std::env::current_exe().unwrap();
        assert!(!is_our_process(&exe, std::process::id(), "core"));
    }
}
