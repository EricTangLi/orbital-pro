//! Orbital Pro · 桌面壳（GUI + 启动器）
//!
//! 进程模型见 docs/02 §3.3 / ADR-015：
//!   同一个 exe 按 `--role core|proxy` spawn 自身；
//!   **role 判定必须在任何 Tauri 初始化之前**（T-M1-00 Spike 已实测：这样 core/proxy 不加载 WebView2）。

// T-M10-15（P0，用户正式使用时发现）：**这一行原先缺失**，导致 release 版按
// console 子系统编译 ⇒ 双击 `OrbitalPro.exe` 会附带一个 CMD 黑窗，关掉窗口 = 杀进程 = 程序退出。
// 我在终端里用 Start-Process 验证时看不到它（终端自己吞了控制台），所以前一轮误判为已修。
// debug 构建保留控制台（方便看日志），release 用 windows 子系统（无控制台窗口）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod data;
mod diag;
mod launcher;
mod status;
mod tray;

use std::io::{Read, Write};
use std::net::TcpListener;

use tauri::Manager;

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1).cloned())
}

/// 端口可配置（docs/02 §3.2）；与 orbital-core::config 用同一套环境变量名。
fn env_port(key: &str, fallback: u16) -> u16 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(fallback)
}

/// 把 tracing 输出同时写到 stderr 与当天日志文件（T-M5-04）。
struct TeeWriter {
    role: String,
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stderr().write_all(buf);
        crate::diag::append_log(&self.role, buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

/// 启动时 schema 迁移失败的原因（T-M6-05）。给界面显示用——用户得知道"库版本比程序新"。
struct SchemaIssue(std::sync::Mutex<Option<String>>);

struct LauncherState(std::sync::Mutex<Option<launcher::Launcher>>);

/// 单实例回调可能被连点多次并发触发：用锁串行化，避免窗口反复闪烁。
static SINGLE_INSTANCE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// T-M10-42：启动时预热的常见查询（填 embedding 缓存，四家 agent 共享）。
///
/// 选词依据：这台机器上实际高频出现的主题（见 Orbital 记忆库的分布）。
/// 预热失败的跳过，不影响启动；后台线程跑，不拖慢冷启动。
const WARM_QUERIES: &[&str] = &[
    "Orbital 项目",
    "端口约定",
    "记忆注入",
    "Hermes 配置",
    "pi-lens 配置",
    "文档管理",
    "工作计划",
    "Kindle 维护",
    "NAS 优化",
    "招聘产品",
    "经营分析",
    "法条核验",
    "微信推送",
    "文件分析系统",
    "备份恢复",
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let role = arg_value(&args, "--role").unwrap_or_else(|| "gui".into());

    // T-M5-04：日志同时落盘（按天分文件）——stderr 在 GUI 里会被窗口吞掉，出问题没法回溯
    diag::prune_old_logs();
    let log_role = role.clone();
    tracing_subscriber::fmt()
        .with_writer(move || TeeWriter {
            role: log_role.clone(),
        })
        .with_max_level(tracing::Level::INFO)
        .init();

    // ★ 必须在创建任何窗口/Builder 之前返回（否则会多加载 WebView2）
    match role.as_str() {
        "core" => {
            serve_http("core", env_port("ORBITAL_CORE_PORT", 8791));
            return;
        }
        "proxy" => {
            // T-M10-10（P1）：这里原先是 `serve_http("proxy", …)` ——
            // 那是个"任何请求都返回 /health JSON"的空壳，**不转发、不注入**。
            // 真正的注入逻辑在 `crates/orbital-proxy`，它的二进制从未被打包 ⇒
            // 发布版里"注入"这个核心功能根本没运行，而界面 3/3 进程健康是**假绿**
            // （空壳也回 health）。现在改为 spawn 真 proxy，并透传退出码。
            let proxy_exe = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("orbital-proxy.exe")))
                .filter(|p| p.exists());
            match proxy_exe {
                Some(exe) => {
                    eprintln!("T-M10-10: spawning real proxy {}", exe.display());
                    // T-M10-12（P2）：之前用 `.status()`，父被杀时子不会跟着退 ——
                    // 子继续占着 proxy 端口，watchdog 拉起的新子 bind 失败 ⇒ 配置改了不生效。
                    // 现在用 **stdin 管道做生命周期绑定**：
                    //   父持有子进程 stdin 的写端 → 父退出（含被 kill）时系统关闭写端 →
                    //   子读到 EOF → 子自己 exit。
                    // T-M10-14（用户实测：运行时有 CMD 窗口，关掉就"退出"）：
                    // `orbital-proxy.exe` 是**控制台子系统**程序，spawn 时若不指定
                    // CREATE_NO_WINDOW，Windows 会给它分配一个新控制台 ⇒ 用户看到黑窗口；
                    // 关掉该窗口 = 杀掉 orbital-proxy ⇒ 父进程 wait 返回并退出（watchdog 再拉起，窗口又出现）。
                    // 必须和 `launcher.rs::spawn_role` 一样加 CREATE_NO_WINDOW。
                    let mut cmd = std::process::Command::new(&exe);
                    cmd.env("ORBITAL_PARENT_PID", std::process::id().to_string())
                        .stdin(std::process::Stdio::piped());
                    #[cfg(windows)]
                    {
                        use std::os::windows::process::CommandExt;
                        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                        cmd.creation_flags(CREATE_NO_WINDOW);
                    }
                    let spawned = cmd.spawn();
                    match spawned {
                        Ok(mut child) => {
                            // ⚠️ 必须把 stdin 的写端**留在变量里**（不能 drop）：
                            // drop 掉就等于关闭管道，子进程会立刻 EOF 退出。
                            let _hold_stdin = child.stdin.take();
                            let code = match child.wait() {
                                Ok(s) => s.code().unwrap_or(1),
                                Err(e) => {
                                    eprintln!("T-M10-10: 等待 orbital-proxy 失败: {e}");
                                    1
                                }
                            };
                            std::process::exit(code);
                        }
                        Err(e) => {
                            eprintln!("T-M10-10: 启动 orbital-proxy 失败: {e}");
                            std::process::exit(1);
                        }
                    }
                }
                None => {
                    eprintln!(
                        "T-M10-10: 找不到 orbital-proxy.exe（安装包应自带；开发环境请先 \
                         cargo build -p orbital-proxy）。拒绝用空壳假装 proxy 已就绪。"
                    );
                    std::process::exit(1);
                }
            }
        }
        _ => {}
    }

    run_gui(&args);
}

/// headless 子服务：最小 HTTP，用于健康检查与"端口属于谁"的行为证据。
///
/// T-M5-04：三条输出都要**同时落盘**（`logs/{role}-YYYYMMDD.log`），
/// 因为子进程的 stdout/stderr 在 GUI 里被吞掉，出问题没法回溯。
fn serve_http(role: &str, port: u16) {
    match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => {
            println!(
                "HEADLESS_READY role={} pid={} port={}",
                role,
                std::process::id(),
                port
            );
            crate::diag::append_log(
                role,
                format!(
                    "{} HEADLESS_READY pid={} port={}\n",
                    role,
                    std::process::id(),
                    port
                )
                .as_bytes(),
            );
            let _ = std::io::stdout().flush();
            for mut s in listener.incoming().flatten() {
                {
                    let mut buf = [0u8; 1024];
                    let n = s.read(&mut buf).unwrap_or(0);
                    let req_line = String::from_utf8_lossy(&buf[..n])
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_string();
                    crate::diag::append_log(
                        role,
                        format!("{role} {req_line} -> 200\n").as_bytes(),
                    );
                    let body = format!(
                        "{{\"status\":\"ok\",\"role\":\"{}\",\"pid\":{},\"version\":\"{}\"}}",
                        role,
                        std::process::id(),
                        env!("CARGO_PKG_VERSION")
                    );
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = s.write_all(resp.as_bytes());
                }
            }
        }
        Err(e) => {
            // 端口被占：由启动器负责顺延/接管，这里如实报错退出（不静默）
            let line = format!("BIND_FAIL role={role} port={port} err={e}");
            eprintln!("{line}");
            crate::diag::append_log(role, format!("{line}\n").as_bytes());
            std::process::exit(2);
        }
    }
}

fn run_gui(args: &[String]) {
    let run_ms: u64 = arg_value(args, "--run-ms")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let core_port = env_port("ORBITAL_CORE_PORT", 8791);
    let proxy_port = env_port("ORBITAL_PROXY_PORT", 8790);
    // T-M10-46：调试/快捷入口 —— 只弹托盘面板、不显示主窗
    let panel_only_mode = args.iter().any(|a| a == "--open-panel");

    // T-M10-46（2026-11-16 真机实测）：**托盘面板必须从窗口状态持久化里排除**。
    // 插件会把 `tray-panel` 的尺寸/位置写进 `%APPDATA%\com.orbital.pro\.window-state.json`
    // 并在下次启动时恢复 —— 实测该文件里存着 `tray-panel: 560×735 @ (44,44)`，
    // 于是 `inner_size(320,520)` 与 `set_position(...)` 全部被覆盖：
    //   · 高度被还原成旧的 420 逻辑像素（代码改了 520 也没用）⇒ 内容放不下"只显示一半"
    //   · 位置被还原成 (44,44) ⇒ 面板跑到屏幕左上角
    // 面板是**每次按托盘图标矩形重新定位**的临时窗口，不该有"上次位置"。
    let mut ws = tauri_plugin_window_state::Builder::default().with_denylist(&["tray-panel"]);
    if panel_only_mode {
        // 同一份状态文件里 `main.visible = true`，`--open-panel` 启动时插件会把主窗恢复成
        // 可见并抢走焦点 ⇒ 面板刚弹出就失焦隐藏（T-M10-44/45 反复踩到的"面板闪没"）。
        // 调试模式不恢复主窗的初始状态，面板才是屏幕上唯一的窗口。
        ws = ws.skip_initial_state(tray::MAIN);
    }

    let app = tauri::Builder::default()
        // 单实例：第二次启动唤醒已有窗口（ADR-016）
        .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            // 第二个实例启动 -> 唤醒已有窗口（ADR-016 行为）
            let app = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                let _guard = SINGLE_INSTANCE_LOCK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                tracing::info!(?argv, ?cwd, "second instance -> wake main window");
                // unminimize + show + set_focus（卡要求的三步）
                tray::show_main(&app);
                tray::mark_visible(true);
            });
        }))
        .plugin(ws.build())
        .plugin(tauri_plugin_store::Builder::default().build())
        // T-M1-13：三进程状态探测（真实数据，不是 mock）
        .invoke_handler(tauri::generate_handler![
        status::get_process_status,
        data::list_memories,
        data::get_memory,
        data::filter_options,
        data::overview,
        data::update_memory,
        data::delete_impact,
        data::forget_memory,
        data::restore_memory,
        data::toggle_pin,
        data::invalidate_memory,
        data::restore_validity,
        data::effective_importance,
        data::merge_preview,
        data::merge_memories,
        data::undo_merge,
        data::maintenance_todos,
        data::detect_conflicts,
        data::retirement_candidates,
        data::retire_memory,
        data::restore_retire_memory,
        data::recovery_list,
        data::restore_to_baseline_memory,
        data::cleanup_orphan_edges,
        data::reembed_pending,
        data::create_backup,
        data::stage_restore,
        status::launcher_state,
        status::launcher_events,
        status::export_diagnostics,
        status::port_probes,
        tray::show_main_window,
        tray::quit_app,
        tray::hide_panel,
        status::onboarding_info,
        status::schema_issue,
        status::health_checkup,
        status::health_summary,
        status::injection_stats,
        data::agent_connections,
        data::attach_agent,
        data::proxy_config,
        data::save_proxy_config,
        data::inject_status,
        data::deploy_inject,
        data::remove_inject,
        data::verify_inject,
        data::list_backups,
        data::verify_backup,
        data::resolve_conflict,
        data::undo_event,
        data::list_activity,
        data::link_memory,
        data::related_memories,
    ])
        // 静默启动：窗口 visible:false，页面加载完成才显示（消除白屏）
        .on_page_load(|window, _payload| {
            // ★ 只在**首次**加载时显示：否则每次 hide 后页面重载都会把它又弹出来，
            //   把"左键托盘隐藏主窗"直接覆盖掉（这正是 toggle 看起来失效的根因）。
            static FIRST_SHOW: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(true);
            if window.label() == tray::MAIN
                && FIRST_SHOW.swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                // T-M10-38：`--open-panel` 是"只弹托盘面板"的调试/快捷入口，
                // 若同时显示主窗，主窗会抢焦点 → 面板刚弹出就**失焦隐藏**（实测踩到）。
                // ⇒ 该参数下跳过主窗显示。
                let panel_only = std::env::args().any(|a| a == "--open-panel");
                if panel_only {
                    tracing::info!("--open-panel: 跳过主窗显示（只弹面板）");
                    // T-M10-43b：`tauri_plugin_window_state` 会恢复上次的可见状态
                    // （即使这里不 show，它也会在之后把主窗恢复成"可见"）。
                    // ⇒ 延迟再 hide 一次，确保主窗不挡面板（实测踩到）。
                    let w = window.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(600));
                        let _ = w.hide();
                        tracing::info!("--open-panel: main window force-hidden");
                    });
                } else {
                    let _ = window.show();
                    tray::mark_visible(true);
                    tracing::info!("main window shown on first page load");
                }
            }
        })
        .on_window_event(|window, event| {
            if window.label() == tray::MAIN {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    // 关闭 = 隐藏到托盘（ADR-016），真退出只走托盘菜单
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .manage(LauncherState(std::sync::Mutex::new(None)))
        .manage(SchemaIssue(std::sync::Mutex::new(None)))
        .setup(move |app| {
            // 三进程启动器：GUI 作为父进程拉起 core/proxy（T-M1-06）
            match launcher::Launcher::start(core_port, proxy_port) {
                Ok(l) => {
                    if let Some(state) = app.try_state::<LauncherState>() {
                        *state.0.lock().unwrap() = Some(l);
                    }
                    tracing::info!(core_port, proxy_port, "three processes up");
                }
                Err(e) => tracing::error!(%e, "launcher start failed"),
            }

            // M5-02 从备份恢复：**必须在任何连接打开之前**应用"待恢复"文件。
            // 放在 setup 最前面，schema 迁移与本地端点都会用到恢复后的库。
            match orbital_core::db::apply_pending_restore(&data::db_path()) {
                Ok(Some(pre)) => {
                    tracing::info!(pre = %pre.display(), "已应用待恢复备份（原库已保底备份）");
                }
                Ok(None) => {}
                Err(e) => tracing::error!(%e, "应用待恢复备份失败（原库未改动）"),
            }

            // schema 迁移：只读页面不会触发懒初始化的 MemoryService，这里主动迁一次
            // （否则升级后维护页的「冲突」一类永远显示"表尚未迁移"）
            // T-M6-05：迁移失败要能被界面看见（尤其是"库版本比程序新"）
            match data::ensure_migrated() {
                Ok(()) => {}
                Err(e) => {
                    tracing::error!(%e, "schema 迁移失败");
                    if let Some(state) = app.try_state::<SchemaIssue>() {
                        *state.0.lock().unwrap() = Some(e);
                    }
                }
            }

            // 本地只读召回端点（ADR-041）：给 proxy 注入用，只绑 127.0.0.1
            {
                let db = data::db_path();
                let journal = db.with_file_name("journal.ndjson");
                // 注意：core 角色的 health 端点也占用 ORBITAL_CORE_PORT（默认 8791），两者不能同端口（P0 审计发现）
                match orbital_core::local_api::spawn_recall_server(&db, &journal, "127.0.0.1:8799") {
                    Ok(port) => tracing::info!(port, "local recall api listening (ADR-041)"),
                    Err(e) => tracing::warn!(%e, "local recall api 未启动（注入会降级为 skipped）"),
                }
            }

            // 托盘与生命周期（T-M1-07）：关闭主窗只隐藏，不退出
            if let Err(e) = tray::build(app.handle()) {
                tracing::error!(%e, "tray build failed");
            }

            // T-M10-42：**后台预热 query embedding 缓存**（不阻塞启动）。
            // 冷查询 P95 ≈ 240ms 离注入的 300ms 预算只剩 60ms（T-M10-41）；
            // 启动后先算一遍常见查询，用户真正提问时就命中缓存（≈16ms）。
            // 注意：缓存 key 只有 query 文本，预热一次四家 agent 共享。
            {
                let db = data::db_path();
                std::thread::spawn(move || {
                    let journal = db.with_file_name("journal.ndjson");
                    match orbital_core::service::MemoryService::open(&db, &journal) {
                        Ok(svc) => {
                            let va = orbital_core::identity::VerifiedAgent::for_local_owner(
                                "dsh".to_string(),
                            );
                            let n = svc.warm_query_cache(&va, WARM_QUERIES);
                            tracing::info!(n, "query embedding cache warmed");
                        }
                        Err(e) => tracing::warn!(%e, "warm query cache skipped"),
                    }
                });
            }
            // T-M8-01：--open-panel 时把面板直接弹出来（与右键托盘同一条路径）
            if std::env::args().any(|a| a == "--open-panel") {
                // T-M10-46：此前传 `Rect::default()`（原点）⇒ 调试面板永远钉在屏幕左上角，
                // 与右键路径不是同一种定位，没法用它验证"贴托盘"。改用主显示器右下角伪图标。
                let anchor = tray::debug_panel_anchor(app.handle());
                tray::show_panel(app.handle(), anchor);
            }
            if run_ms > 0 {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(run_ms));
                    handle.exit(0);
                });
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("tauri app failed to build");

    app.run(|handle, event| {
        if let tauri::RunEvent::Exit = event {
            if let Some(st) = handle.try_state::<LauncherState>() {
                if let Ok(mut g) = st.0.lock() {
                    if let Some(mut l) = g.take() {
                        l.stop();
                        tracing::info!("launcher stopped; runtime state removed");
                    }
                }
            }
            // 移除托盘图标：防止更新重启后任务栏残留一个"幽灵图标"
            if let Some(tray_icon) = handle.tray_by_id("orbital-tray") {
                let _ = tray_icon.set_visible(false);
                tracing::info!("tray icon hidden on exit");
            }
        }
    });
}
