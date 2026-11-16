//! 三进程状态探测（T-M1-13）。
//!
//! **真实数据，不是 mock**（docs/05 验收第 11 条）：
//! - GUI：本进程 pid
//! - core / proxy：向它们的 `/health` 发真实 HTTP 请求，取 pid + version
//!
//! 探测失败必须**如实返回原因**（端口不可达 / 超时 / 响应非法），
//! 绝不能把"探测失败"显示成"正常"。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 一个角色的运行状态。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProcStatus {
    /// `gui` / `core` / `proxy`
    pub role: String,
    pub alive: bool,
    /// 探测到的 pid（GUI 是本进程；core/proxy 从 /health 拿）
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub version: Option<String>,
    /// 探测失败的真实原因（alive=false 时必有）
    pub error: Option<String>,
    /// 本次探测的 Unix 秒
    pub checked_at: u64,
}

/// 与 `crates/orbital-core/src/config.rs` 的 `DEFAULT_CORE_PORT` / `DEFAULT_PROXY_PORT` 一致。
/// （两边必须同步改；`docs/02 §3.2` 规定端口可配）
const DEFAULT_CORE_PORT: u16 = 8791;
const DEFAULT_PROXY_PORT: u16 = 8790;

fn env_port(key: &str, fallback: u16) -> u16 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(fallback)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 向 `127.0.0.1:<port>/health` 发一个最小 HTTP/1.0 请求并解析 JSON 里的 pid/version。
///
/// 只用标准库（不引 reqwest）：状态探测是最小依赖路径，越少越好。
fn probe_http(port: u16) -> (bool, Option<u32>, Option<String>, Option<String>) {
    let addr: SocketAddr = match format!("127.0.0.1:{port}").parse() {
        Ok(a) => a,
        Err(e) => return (false, None, None, Some(format!("地址解析失败: {e}"))),
    };

    // 连接超时 800ms：状态页要"快"，宁可报不可达也不要让界面卡住
    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_millis(800)) {
        Ok(s) => s,
        Err(e) => {
            return (
                false,
                None,
                None,
                Some(format!("端口 {port} 不可达: {}", short_err(&e))),
            )
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(800)));

    let req = format!("GET /health HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if let Err(e) = stream.write_all(req.as_bytes()) {
        return (false, None, None, Some(format!("写请求失败: {}", short_err(&e))));
    }

    let mut buf = Vec::with_capacity(1024);
    if let Err(e) = stream.read_to_end(&mut buf) {
        // 读超时/中断：进程可能活着但没回包
        return (false, None, None, Some(format!("读响应失败: {}", short_err(&e))));
    }
    let text = String::from_utf8_lossy(&buf);

    // 极简解析：找 "pid":N 与 "version":"x"
    let pid = extract_num(&text, "\"pid\":");
    let version = extract_str(&text, "\"version\":\"");
    if pid.is_none() && version.is_none() {
        return (
            false,
            None,
            None,
            Some(format!("响应里没有 pid/version（前 80 字: {}）", truncate(&text, 80))),
        );
    }
    (true, pid, version, None)
}

fn short_err(e: &std::io::Error) -> String {
    // Windows 的 "由于目标计算机积极拒绝..." 太长，只留错误码
    match e.kind() {
        std::io::ErrorKind::ConnectionRefused => "连接被拒绝".into(),
        std::io::ErrorKind::TimedOut => "超时".into(),
        std::io::ErrorKind::PermissionDenied => "权限拒绝".into(),
        _ => format!("{:?}", e.kind()),
    }
}

fn extract_num(text: &str, key: &str) -> Option<u32> {
    let i = text.find(key)? + key.len();
    let rest = &text[i..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn extract_str(text: &str, key: &str) -> Option<String> {
    let i = text.find(key)? + key.len();
    let rest = &text[i..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// 从 `/health` 响应里认出**谁在提供服务**。
///
/// T-M10-47：两个角色的字段名不一样——
/// - core：`{"status":"ok","role":"core",...}`（`main.rs::serve_http`）
/// - proxy：`{"service":"orbital-proxy",...}`（`crates/orbital-proxy/src/main.rs:296`）
///
/// 旧实现只认 `role`，于是 proxy 明明在监听也被判成"被其他服务占用"，
/// 状态页端口一览长期挂一条假异常。这里统一收敛成 `core` / `proxy`。
fn served_role(body: &str) -> Option<String> {
    if let Some(s) = extract_str(body, "\"role\":\"") {
        return Some(s);
    }
    let s = extract_str(body, "\"service\":\"")?;
    if s.contains("proxy") {
        Some("proxy".to_string())
    } else {
        Some(s)
    }
}

fn truncate(s: &str, n: usize) -> String {
    let one: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{one}…")
    } else {
        one
    }
}

/// T-M10-47：**实际端口**——优先取 launcher 运行态里真正在用的端口（可能已顺延），
/// 没有再退回环境变量、最后退回默认值。
///
/// 为什么必须有这个函数：端口顺延后，环境变量仍写着 8791，而 core 实际在 8792。
/// 若界面继续探测 8791，用户看到的是"配置没错但进程 2/3"——比原来更难排查。
fn actual_port(role: &str, fallback: u16) -> u16 {
    crate::launcher::runtime_state()
        .into_iter()
        .find(|e| e.role == role)
        .map(|e| e.port)
        .unwrap_or(fallback)
}

/// 探测三个角色。**GUI 恒为存活**（能执行到这个函数说明它活着）。
pub fn collect() -> Vec<ProcStatus> {
    // 端口来源与 core 的 config.rs 保持一致（同一套环境变量名）；
    // src-tauri 是独立 workspace，不引 orbital-core 以免拖慢编译，
    // 故这里只重复**两个默认端口**，且注明来源（不是魔法数字）。
    // T-M10-47：再叠一层"运行态实际端口"（顺延后以它为准）。
    let core_port = actual_port("core", env_port("ORBITAL_CORE_PORT", DEFAULT_CORE_PORT));
    let proxy_port = actual_port("proxy", env_port("ORBITAL_PROXY_PORT", DEFAULT_PROXY_PORT));
    let now = now_secs();

    let mut out = Vec::with_capacity(3);

    // 1) GUI（本进程）
    out.push(ProcStatus {
        role: "gui".into(),
        alive: true,
        pid: Some(std::process::id()),
        port: None,
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        error: None,
        checked_at: now,
    });

    // 2) core / proxy
    for (role, port) in [("core", core_port), ("proxy", proxy_port)] {
        let (alive, pid, version, error) = probe_http(port);
        out.push(ProcStatus {
            role: role.into(),
            alive,
            pid,
            port: Some(port),
            version,
            error,
            checked_at: now,
        });
    }
    out
}

/// Tauri command：前端 `invoke('get_process_status')`。
#[tauri::command]
pub fn get_process_status() -> Vec<ProcStatus> {
    collect()
}

/// 一个端口的探测结论（T-M5-05）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct PortProbe {
    pub port: u16,
    /// 端口表里这个端口**应该**由谁提供
    pub expected: String,
    pub listening: bool,
    /// 实际探测到的角色（`core` / `proxy` / `recall`；读不到为 None）
    pub served_by: Option<String>,
    /// 与端口表一致
    pub ok: bool,
    pub note: String,
}

/// 发一个最小 HTTP GET，返回响应全文（连不上返回 None）。
///
/// 为什么不用 reqwest：状态页每 3 秒探测一次，这里只要"连得上/读得到"，
/// 引一个异步 HTTP 栈不值当（而且 src-tauri 是独立 workspace，尽量不加重）。
fn get_http(port: u16, path: &str, timeout_ms: u64) -> Option<String> {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().ok()?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(timeout_ms)).ok()?;
    let _ = s.set_read_timeout(Some(Duration::from_millis(timeout_ms)));
    let req = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).ok()?;
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    Some(buf)
}

/// 端口占用一览（T-M5-05）：对照 M5-03 的端口表，判断"谁在听、是否符合预期"。
///
/// 端口表（`HANDOFF.md` 有同一张表，改端口必须两处同步）：
/// - core health：`ORBITAL_CORE_PORT`（默认 8791）
/// - proxy health：`ORBITAL_PROXY_PORT`（默认 8790）
/// - 只读召回端点：**8799**（GUI 提供，ADR-041）
/// T-M6-05：启动时的 schema 迁移失败原因（None = 正常）。
///
/// 为什么要暴露给界面：库版本比程序新时，**用户看到的是"记忆 0 条"而不是错误**，
/// 会以为数据丢了。必须明确告诉他"库来自更新版本，请升级程序"。
#[tauri::command]
pub fn schema_issue(state: tauri::State<'_, crate::SchemaIssue>) -> Option<String> {
    state.0.lock().ok().and_then(|g| g.clone())
}

/// 首启引导需要的信息（T-M6-02）。
///
/// 判断"首启"由前端做（`ov.total === 0`）——新库必然 0 条，
/// 既不引入"已看过"持久化（红线：不用 localStorage 存业务状态），也不会误骚扰老用户。
#[tauri::command]
pub fn onboarding_info() -> serde_json::Value {
    let db = crate::data::db_path();
    let data_dir = db
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    // 安装版里 mcp 二进制与主程序同目录（T-M6-02 的 bundle.resources 保证）
    let mcp = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("orbital-mcp.exe")))
        .filter(|p| p.exists());
    let secrets = std::env::var("USERPROFILE")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".orbital").join("secrets.json"))
        .filter(|p| p.exists());
    serde_json::json!({
        "data_dir": data_dir,
        "db_path": db.display().to_string(),
        "mcp_path": mcp.as_ref().map(|p| p.display().to_string()),
        "mcp_available": mcp.is_some(),
        "embedding_configured": secrets.is_some(),
        // T-M10-47：首启引导里的端口也用实际值（顺延后 8791 可能不是它在听）
        "ports": {
            "core": actual_port("core", DEFAULT_CORE_PORT),
            "proxy": actual_port("proxy", DEFAULT_PROXY_PORT),
            "recall": 8799
        },
    })
}

#[tauri::command]
pub fn port_probes() -> Vec<PortProbe> {
    // T-M10-47：用实际端口（顺延后端口表也要跟着变，否则会误报"无人监听"）
    let core_port = actual_port("core", env_port("ORBITAL_CORE_PORT", DEFAULT_CORE_PORT));
    let proxy_port = actual_port("proxy", env_port("ORBITAL_PROXY_PORT", DEFAULT_PROXY_PORT));
    const RECALL_PORT: u16 = 8799;

    let mut out = Vec::with_capacity(3);

    for (port, expected, want) in [
        (core_port, "core health", "core"),
        (proxy_port, "proxy health", "proxy"),
    ] {
        let body = get_http(port, "/health", 600);
        // T-M10-47：core 返回 role、proxy 返回 service，统一由 served_role 判定
        let served_by = body.as_deref().and_then(served_role);
        let listening = body.is_some();
        let ok = listening && served_by.as_deref() == Some(want);
        let note = if ok {
            "符合端口表".to_string()
        } else if !listening {
            "无人监听".to_string()
        } else {
            format!("被其他服务占用（应为 {want}）")
        };
        out.push(PortProbe {
            port,
            expected: expected.to_string(),
            listening,
            served_by,
            ok,
            note,
        });
    }

    // 召回端点没有 /health，用 /recall 探测
    let body = get_http(RECALL_PORT, "/recall?q=probe&limit=1", 600);
    let listening = body.is_some();
    let looks_like_recall = body.as_deref().map(|b| b.contains("\"count\"")).unwrap_or(false);
    out.push(PortProbe {
        port: RECALL_PORT,
        expected: "recall (GUI)".to_string(),
        listening,
        served_by: if looks_like_recall {
            Some("recall".to_string())
        } else {
            None
        },
        ok: looks_like_recall,
        note: if looks_like_recall {
            "符合端口表".to_string()
        } else if listening {
            "端口有服务但不是召回端点".to_string()
        } else {
            "无人监听（注入会降级为 skipped）".to_string()
        },
    });

    out
}

/// Tauri command（T-M5-03）：launcher 运行态（pid / port / 重启次数）。
#[tauri::command]
pub fn launcher_state() -> Vec<serde_json::Value> {
    crate::launcher::runtime_state()
        .into_iter()
        .map(|e| {
            serde_json::json!({
                "role": e.role,
                "pid": e.pid,
                "port": e.port,
                "started_at": e.started_at,
                "restarts": e.restarts,
            })
        })
        .collect()
}

/// Tauri command（T-M5-04）：导出诊断包（日志 + launcher + db-info + env，已脱敏）。
#[tauri::command]
pub fn export_diagnostics() -> Result<serde_json::Value, String> {
    let r = crate::diag::export()?;
    serde_json::to_value(r).map_err(|e| e.to_string())
}

/// Tauri command（T-M5-03）：最近的重启事件（退出码 / 是否放弃）。
#[tauri::command]
pub fn launcher_events(limit: Option<usize>) -> Vec<serde_json::Value> {
    crate::launcher::restart_events(limit.unwrap_or(10).clamp(1, 100))
}

#[cfg(test)]
mod tests {
    use super::{extract_num, extract_str, served_role};

    #[test]
    fn extracts_pid_and_version() {
        let body = "{\"status\":\"ok\",\"role\":\"core\",\"pid\":1234,\"version\":\"0.1.0\"}";
        assert_eq!(extract_num(body, "\"pid\":"), Some(1234));
        assert_eq!(extract_str(body, "\"version\":\""), Some("0.1.0".to_string()));
    }

    #[test]
    fn returns_none_when_absent() {
        assert_eq!(extract_num("no pid here", "\"pid\":"), None);
        assert_eq!(extract_str("no version", "\"version\":\""), None);
    }

    // ---- T-M10-47：/health 的"谁在服务"判定 ----

    #[test]
    fn served_role_reads_core_role_field() {
        let body = r#"{"status":"ok","role":"core","pid":41356,"version":"0.94.8"}"#;
        assert_eq!(served_role(body).as_deref(), Some("core"));
    }

    #[test]
    fn served_role_reads_proxy_service_field() {
        // proxy 的 /health 用 service 而不是 role（crates/orbital-proxy/src/main.rs:296）。
        // 旧判据只认 role ⇒ proxy 明明在听也被判成"被其他服务占用"（状态页假异常）。
        let body = r#"{"status":"ok","service":"orbital-proxy","version":"0.94.8"}"#;
        assert_eq!(served_role(body).as_deref(), Some("proxy"));
    }

    #[test]
    fn served_role_none_for_unrelated_service() {
        let body = r#"{"hello":"world"}"#;
        assert_eq!(served_role(body), None);
    }
}

#[cfg(test)]
mod injection_tests {
    use super::stats_from_dir;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("orbital-inj-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// T-M10-06 验收 2/3：fixture 聚合 + 坏行跳过并计数。
    #[test]
    fn aggregates_fixture_and_counts_bad_lines() {
        let d = tmp("agg");
        std::fs::write(
            d.join("injection-2026-01-01.jsonl"),
            concat!(
                r#"{"ts":1,"protocol":"openai","hits":3,"tokens":100,"elapsed_ms":10,"status":"on","cache":"hit"}"#,
                "\n",
                r#"{"ts":2,"protocol":"openai","hits":0,"tokens":0,"elapsed_ms":300,"status":"timeout","cache":"miss"}"#,
                "\n",
                "{not json}\n",
            ),
        )
        .unwrap();
        std::fs::write(
            d.join("injection-2026-01-02.jsonl"),
            concat!(
                r#"{"ts":3,"protocol":"openai","hits":2,"tokens":50,"elapsed_ms":20,"status":"on","cache":"miss"}"#,
                "\n",
            ),
        )
        .unwrap();

        let s = stats_from_dir(&d, 7).unwrap();
        assert!(!s.empty, "有数据时不能标空");
        assert_eq!(s.total_rows, 3, "有效行");
        assert_eq!(s.injected_rows, 2, "status=on");
        assert_eq!(s.hits_total, 5);
        assert_eq!(s.tokens_total, 150);
        assert_eq!(s.cache_hit_rows, 1);
        assert_eq!(s.skipped_rows, 1);
        assert_eq!(s.bad_lines, 1, "非法 JSON 行必须计数而不是静默吞");
        assert_eq!(s.daily.len(), 2);
        // elapsed 排序 [10,20,300]：p50 -> idx ceil(1.5)-1 = 1 → 20；p95 -> idx 2 → 300
        assert_eq!(s.p50_ms, 20);
        assert_eq!(s.p95_ms, 300);
    }

    /// T-M10-06 验收 1：四种空态各自如实返回原因（不假绿）。
    #[test]
    fn reports_four_empty_reasons() {
        // ① 目录不存在
        let base = tmp("empty");
        let s = stats_from_dir(&base.join("nope"), 7).unwrap();
        assert!(s.empty);
        assert_eq!(s.reason, "no-dir");

        // ② 有目录无文件
        let s2 = stats_from_dir(&base, 7).unwrap();
        assert!(s2.empty);
        assert_eq!(s2.reason, "no-file");

        // ③ 有文件但 0 行
        std::fs::write(base.join("injection-2026-01-01.jsonl"), "\n\n").unwrap();
        let s3 = stats_from_dir(&base, 7).unwrap();
        assert!(s3.empty);
        assert_eq!(s3.reason, "no-rows");

        // ④ 有行但全是 off（开关没开 / agent 没走 proxy）
        std::fs::write(
            base.join("injection-2026-01-02.jsonl"),
            concat!(
                r#"{"ts":1,"hits":0,"tokens":0,"elapsed_ms":1,"status":"off","cache":"miss"}"#,
                "\n",
            ),
        )
        .unwrap();
        let s4 = stats_from_dir(&base, 7).unwrap();
        assert!(s4.empty);
        assert_eq!(s4.reason, "all-off");

        // ⑤ 有行但全超时（真实数据里出现过 status="error:timeout"）——不能算"有数据",
        //    要单独说明"一次都没注入成功"
        std::fs::write(
            base.join("injection-2026-01-03.jsonl"),
            concat!(
                r#"{"ts":1,"hits":0,"tokens":0,"elapsed_ms":0,"status":"error:timeout"}"#,
                "\n",
            ),
        )
        .unwrap();
        let s5 = stats_from_dir(&base, 1).unwrap();
        assert!(s5.empty);
        assert_eq!(s5.reason, "all-skipped");
    }

    /// 窗口截断：days=1 只算最后一天。
    #[test]
    fn window_keeps_last_n_days() {
        let d = tmp("window");
        for (day, hits) in [("2026-01-01", 1), ("2026-01-02", 2), ("2026-01-03", 3)] {
            std::fs::write(
                d.join(format!("injection-{day}.jsonl")),
                format!(
                    "{{\"ts\":1,\"hits\":{hits},\"tokens\":{hits},\"elapsed_ms\":1,\"status\":\"on\",\"cache\":\"miss\"}}\n"
                ),
            )
            .unwrap();
        }
        let s = stats_from_dir(&d, 1).unwrap();
        assert_eq!(s.daily.len(), 1, "只保留最后 1 天");
        assert_eq!(s.hits_total, 3, "只算 01-03");
    }
}

// ===== T-M9-01：一键体检（PRD J8「一键体检」/ docs/05 M5 5.4）=====
//
// 设计要点（对应 docs/05 两条验收）：
//   ① **每项都有判据**（`criterion`）与明细（`detail`）→ 界面可点开看"凭什么这么说"；
//   ② **空库不出现"满分"假绿**：没有样本可判的项标 `waived`（不计入 pass 数），
//      而不是让它"顺利通过"——3 条记忆的库"重复组≤5"当然通过，那说明不了健康。

/// 单项体检结果。
#[derive(serde::Serialize)]
pub struct CheckItem {
    pub id: String,
    pub title: String,
    /// 判据（人话）：这一项到底在验什么
    pub criterion: String,
    /// pass | fail | waived
    pub status: String,
    /// 明细：实测值 / 为什么 waived
    pub detail: String,
}

fn item(
    id: &str,
    title: &str,
    criterion: &str,
    status: &str,
    detail: impl Into<String>,
) -> CheckItem {
    CheckItem {
        id: id.to_string(),
        title: title.to_string(),
        criterion: criterion.to_string(),
        status: status.to_string(),
        detail: detail.into(),
    }
}

/// 一键体检：返回逐项结果（每项自带判据与明细）。
#[tauri::command]
pub fn health_checkup() -> Vec<CheckItem> {
    let mut out: Vec<CheckItem> = Vec::new();
    let db = crate::data::db_path();

    // 1. 库文件存在
    if !db.exists() {
        out.push(item(
            "db_exists",
            "记忆库文件存在",
            "数据库文件能被找到",
            "waived",
            format!("尚未创建（首启还没有写入过）：{}", db.display()),
        ));
        // 库都没有，后面那些"查表"的项全部 waived，不能给假绿
        for (id, title, crit) in [
            ("integrity", "数据库完整性", "PRAGMA integrity_check 返回 ok"),
            ("schema_version", "schema 版本", "user_version 等于代码支持的版本"),
            ("wal_mode", "WAL 模式", "journal_mode 为 wal（并发读写更安全）"),
            ("memories", "记忆条数", "库里至少有一条记忆"),
            ("embeddings", "向量覆盖率", "已算向量的记忆占比"),
            ("orphan_facts", "孤儿 fact", "memory_facts 都指向存在的记忆"),
            ("orphan_edges", "孤儿边", "edges 两端都指向存在的记忆"),
            ("open_conflicts", "未裁决冲突", "open 状态冲突数为 0"),
        ] {
            out.push(item(id, title, crit, "waived", "无库可查（先写一条记忆）"));
        }
        out.push(item(
            "processes",
            "三进程健康",
            "GUI / core / proxy 都在跑",
            if crate::status::collect().len() >= 3 { "pass" } else { "fail" },
            format!("当前 {} 个进程", crate::status::collect().len()),
        ));
        return out;
    }

    out.push(item("db_exists", "记忆库文件存在", "数据库文件能被找到", "pass", db.display().to_string()));

    // 2..N：借读连接查（只读，不写）
    match rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) {
        Err(e) => {
            out.push(item("integrity", "数据库完整性", "PRAGMA integrity_check 返回 ok", "fail", format!("打不开库：{e}")));
            return out;
        }
        Ok(c) => {
            // 完整性
            let integ: String = c
                .query_row("PRAGMA integrity_check", [], |r| r.get(0))
                .unwrap_or_else(|e| format!("查询失败：{e}"));
            out.push(item(
                "integrity",
                "数据库完整性",
                "PRAGMA integrity_check 返回 ok",
                if integ == "ok" { "pass" } else { "fail" },
                integ,
            ));

            // schema 版本
            let ver: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(-1);
            let expect = orbital_core::db::SCHEMA_VERSION as i64;
            out.push(item(
                "schema_version",
                "schema 版本",
                "user_version 等于代码支持的版本",
                if ver == expect { "pass" } else { "fail" },
                format!("实际 {ver} / 期望 {expect}"),
            ));

            // WAL
            let jm: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap_or_default();
            out.push(item(
                "wal_mode",
                "WAL 模式",
                "journal_mode 为 wal（并发读写更安全）",
                if jm.eq_ignore_ascii_case("wal") { "pass" } else { "fail" },
                jm,
            ));

            // 记忆条数：0 条 → waived（不是 pass）
            let total: i64 = c.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0)).unwrap_or(0);
            out.push(item(
                "memories",
                "记忆条数",
                "库里至少有一条记忆",
                if total > 0 { "pass" } else { "waived" },
                format!("{total} 条"),
            ));

            // 向量覆盖率：0 条 → waived（表名是 memories_vec，不是 memory_embeddings——
            // 上一版写错表名导致"0/50 已算"的误报，真机截图抓到）
            let emb: i64 = c
                .query_row("SELECT COUNT(*) FROM memories_vec", [], |r| r.get(0))
                .unwrap_or(0);
            let (st, dt) = if total == 0 {
                ("waived", "无记忆，覆盖率无意义".to_string())
            } else if emb >= total {
                ("pass", format!("{emb}/{total} 已算"))
            } else {
                ("fail", format!("{emb}/{total} 已算，有 {}({}) 缺口", total - emb, "待补算".to_string()))
            };
            out.push(item("embeddings", "向量覆盖率", "已算向量的记忆占比（全域）", st, dt));

            // 孤儿 fact
            let of: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM memory_facts f WHERE NOT EXISTS (SELECT 1 FROM memories m WHERE m.id = f.memory_id)",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            out.push(item(
                "orphan_facts",
                "孤儿 fact",
                "memory_facts 都指向存在的记忆",
                if of == 0 { "pass" } else { "fail" },
                format!("{of} 条孤儿"),
            ));

            // 孤儿边
            let oe: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM edges e WHERE NOT EXISTS (SELECT 1 FROM memories m WHERE m.id = e.source_id) OR NOT EXISTS (SELECT 1 FROM memories m WHERE m.id = e.target_id)",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            out.push(item(
                "orphan_edges",
                "孤儿边",
                "edges 两端都指向存在的记忆",
                if oe == 0 { "pass" } else { "fail" },
                format!("{oe} 条孤儿"),
            ));

            // open 冲突：0 条记忆 → waived
            let oc: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM memory_conflicts WHERE status = 'open'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let st = if total == 0 { "waived" } else if oc == 0 { "pass" } else { "fail" };
            out.push(item(
                "open_conflicts",
                "未裁决冲突",
                "open 状态冲突数为 0",
                st,
                if total == 0 { "无记忆可冲突".to_string() } else { format!("{oc} 条 open") },
            ));
        }
    }

    // 进程
    let n = crate::status::collect().len();
    out.push(item(
        "processes",
        "三进程健康",
        "GUI / core / proxy 都在跑",
        if n >= 3 { "pass" } else { "fail" },
        format!("{n} 个进程"),
    ));

    // 备份新鲜度（目录是 %LOCALAPPDATA%\OrbitalPro\backups —— **不是** db 同级的 data/backups；
    // 上一版用 db.parent() 导致"有备份却说没有"的误报，真机截图抓到）
    let bdir = std::env::var("LOCALAPPDATA").ok().map(|b| {
        std::path::PathBuf::from(b)
            .join("OrbitalPro")
            .join("backups")
    });
    match bdir {
        Some(d) if d.exists() => {
            let cnt = std::fs::read_dir(&d).map(|it| it.count()).unwrap_or(0);
            out.push(item(
                "backups",
                "有备份",
                "备份目录里至少有一份备份",
                if cnt > 0 { "pass" } else { "waived" },
                format!("{cnt} 份"),
            ));
        }
        _ => out.push(item("backups", "有备份", "备份目录里至少有一份备份", "waived", "还没有备份目录")),
    }

    out
}

/// 体检小结：pass / fail / waived 各多少（界面标题用）。
#[tauri::command]
pub fn health_summary() -> serde_json::Value {
    let items = health_checkup();
    let pass = items.iter().filter(|i| i.status == "pass").count();
    let fail = items.iter().filter(|i| i.status == "fail").count();
    let waived = items.iter().filter(|i| i.status == "waived").count();
    serde_json::json!({ "total": items.len(), "pass": pass, "fail": fail, "waived": waived })
}

// ============================================================================
// T-M10-06（J6）注入统计
//
// 数据源：proxy 的 `record_metric` 写的按日 JSONL
// `%LOCALAPPDATA%\OrbitalPro\metrics\injection-YYYY-MM-DD.jsonl`
// 每行 `{ts, protocol, hits, tokens, elapsed_ms, status, cache}`。
//
// 两条硬约束：
// 1. **只读文件，不写库**（ADR-008：proxy 从不写库；本命令也不许写）——不新建表、不动 SQLite；
// 2. 空态不假绿：目录不存在 / 有目录无文件 / 有文件 0 行 / 有行但全 `off`，
//    四种空态各自返回原因，前端分别显示（对齐体检的 waived 口径，M9-01）。
// ============================================================================

/// 一天的注入汇总（趋势柱用）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DayInjection {
    pub day: String,
    pub rows: i64,
    pub hits: i64,
    pub tokens: i64,
}

/// 注入统计。
///
/// **MVP 不按 agent 分组**：proxy 的 JSONL 没有 agent 字段（它只看到 HTTP 请求），
/// 要分组得让 agent 侧传头（改动 4 个 agent 配置），属增强项（`docs/30 §2.3`）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct InjectionStats {
    /// true = 窗口内没有任何可用数据（`reason` 说明是哪一种空）
    pub empty: bool,
    /// 空的原因：`no-dir` / `no-file` / `no-rows` / `all-off` / `""`
    pub reason: String,
    /// 统计窗口（天）
    pub days: u32,
    /// 总行数 = 处理过多少次请求
    pub total_rows: i64,
    /// `status == "on"` 的行数（真的注入了）
    pub injected_rows: i64,
    /// 注入条数之和
    pub hits_total: i64,
    /// 注入 token 之和
    pub tokens_total: i64,
    /// `cache == "hit"` 的行数
    pub cache_hit_rows: i64,
    /// `status` 为 off/timeout/skipped 的行数（跳过的原因分布）
    pub skipped_rows: i64,
    /// T-M10-40：其中 `status == "off"`（注入被关/无命中）
    pub off_rows: i64,
    /// T-M10-40：其中 `status` 含 timeout（**真的超时**，与"没命中"完全不同）
    pub timeout_rows: i64,
    /// 延迟 P50 / P95（ms）
    pub p50_ms: i64,
    pub p95_ms: i64,
    /// 无法解析的行数（坏行不静默吞掉）
    pub bad_lines: i64,
    /// 每日序列（按文件名日期，升序，只含窗口内）
    pub daily: Vec<DayInjection>,
    /// metrics 目录的绝对路径（界面上让用户知道去哪看原始文件）
    pub dir: String,
}

/// `%LOCALAPPDATA%\OrbitalPro\metrics`（与 `crates/orbital-proxy/src/main.rs` 的 `metrics_dir` 同源）。
fn metrics_dir() -> std::path::PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(base).join("OrbitalPro").join("metrics")
}

/// 从文件名 `injection-YYYY-MM-DD.jsonl` 取日期串；不匹配返回 None。
fn day_from_name(name: &str) -> Option<String> {
    let stem = name.strip_prefix("injection-")?.strip_suffix(".jsonl")?;
    // 必须是 `YYYY-MM-DD`（10 字符 + 两个连字符）
    if stem.len() != 10 || stem.as_bytes().get(4) != Some(&b'-') || stem.as_bytes().get(7) != Some(&b'-')
    {
        return None;
    }
    Some(stem.to_string())
}

/// 分位（nearest-rank；样本为空返回 0）。
fn percentile(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[idx]
}

#[tauri::command]
pub fn injection_stats(days: u32) -> Result<InjectionStats, String> {
    stats_from_dir(&metrics_dir(), days)
}

/// 从指定目录聚合（测试可注入临时目录；生产走 `metrics_dir()`）。
fn stats_from_dir(dir: &std::path::Path, days: u32) -> Result<InjectionStats, String> {
    let days = days.clamp(1, 30);
    let dir_s = dir.display().to_string();

    let mut empty_base = InjectionStats {
        empty: true,
        reason: String::new(),
        days,
        total_rows: 0,
        injected_rows: 0,
        hits_total: 0,
        tokens_total: 0,
        cache_hit_rows: 0,
        skipped_rows: 0,
        off_rows: 0,
        timeout_rows: 0,
        p50_ms: 0,
        p95_ms: 0,
        bad_lines: 0,
        daily: Vec::new(),
        dir: dir_s.clone(),
    };

    if !dir.is_dir() {
        empty_base.reason = "no-dir".into();
        return Ok(empty_base);
    }

    // ① 收集所有按日文件（目录很小：一天一个文件）
    let mut files: Vec<(String, std::path::PathBuf)> = Vec::new();
    let rd = std::fs::read_dir(&dir).map_err(|e| format!("读 metrics 目录失败：{e}"))?;
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if let Some(day) = day_from_name(&name) {
            files.push((day, ent.path()));
        }
    }
    if files.is_empty() {
        empty_base.reason = "no-file".into();
        return Ok(empty_base);
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    // ② 只看窗口内最后 N 天
    let take_from = files.len().saturating_sub(days as usize);
    let window = &files[take_from..];

    let mut elapsed: Vec<i64> = Vec::new();
    let mut daily: Vec<DayInjection> = Vec::new();
    let mut bad_lines: i64 = 0;
    let mut total_rows: i64 = 0;
    let mut injected_rows: i64 = 0;
    let mut hits_total: i64 = 0;
    let mut tokens_total: i64 = 0;
    let mut cache_hit_rows: i64 = 0;
    let mut skipped_rows: i64 = 0;
    let mut off_rows: i64 = 0;
    // T-M10-40：把"真的超时"单独计数——此前与"没命中"合并成"跳过/超时 N 次"，
    // 用户无法判断注入是坏了还是只是没命中（2026-11-16 用户报"400 次跳过/超时"）。
    let mut timeout_rows: i64 = 0;

    for (day, path) in window {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => continue, // 单文件读失败不拖垮整体
        };
        let mut d = DayInjection {
            day: day.clone(),
            rows: 0,
            hits: 0,
            tokens: 0,
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => {
                    bad_lines += 1;
                    continue;
                }
            };
            let hits = v.get("hits").and_then(|x| x.as_i64()).unwrap_or(0);
            let tokens = v.get("tokens").and_then(|x| x.as_i64()).unwrap_or(0);
            let status = v.get("status").and_then(|x| x.as_str()).unwrap_or("");
            let cache = v.get("cache").and_then(|x| x.as_str()).unwrap_or("");
            let ms = v
                .get("elapsed_ms")
                .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))
                .unwrap_or(0);

            total_rows += 1;
            d.rows += 1;
            d.hits += hits;
            d.tokens += tokens;
            hits_total += hits;
            tokens_total += tokens;
            if status == "on" {
                injected_rows += 1;
            } else {
                skipped_rows += 1;
                if status == "off" {
                    off_rows += 1;
                }
                // T-M10-40：真实 metrics 里超时会出现 `timeout` 与 `error:timeout` 两种写法
                if status.contains("timeout") {
                    timeout_rows += 1;
                }
            }
            if cache == "hit" {
                cache_hit_rows += 1;
            }
            elapsed.push(ms);
        }
        daily.push(d);
    }

    elapsed.sort_unstable();
    let p50 = percentile(&elapsed, 0.50);
    let p95 = percentile(&elapsed, 0.95);

    // ③ 空态判定（不假绿）
    //
    // 真实数据（2026-11-13 的 metrics）里 status 出现过 `error:timeout`：
    // 既不是 `on` 也不是 `off`。所以判据必须是"**一次都没真正注入**"，
    // 而不是"全是 off"——否则 400 行全超时会显示成"有数据但 0 注入"，用户不知道原因。
    let reason = if total_rows == 0 {
        "no-rows"
    } else if injected_rows == 0 {
        if off_rows == total_rows {
            "all-off" // 开关没开 / agent 没走 proxy
        } else {
            "all-skipped" // 开关可能开了，但每次都超时/被跳过
        }
    } else {
        ""
    };

    Ok(InjectionStats {
        empty: !reason.is_empty(),
        reason: reason.to_string(),
        days,
        total_rows,
        injected_rows,
        hits_total,
        tokens_total,
        cache_hit_rows,
        skipped_rows,
        off_rows,
        timeout_rows,
        p50_ms: p50,
        p95_ms: p95,
        bad_lines,
        daily,
        dir: dir_s,
    })
}