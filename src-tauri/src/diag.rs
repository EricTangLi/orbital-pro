//! 日志落盘 + 诊断包导出（T-M5-04）。
//!
//! 两条铁律：
//! 1. **日志按天分文件**：`logs/{role}-YYYYMMDD.log`，启动时清掉 7 天前的——天然"轮转"，不需要额外依赖；
//! 2. **诊断包必须脱敏**（红线 9）：不打包记忆正文，扫到疑似凭据的行整行替换成 `<REDACTED>`。

use std::io::Write;
use std::path::PathBuf;

/// 日志/诊断根目录：`%LOCALAPPDATA%\OrbitalPro\logs`。
pub fn logs_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("OrbitalPro").join("logs")
}

/// 保留天数。
const KEEP_DAYS: u64 = 7;

/// `YYYYMMDD`（UTC）。
fn day_stamp() -> String {
    let (y, m, d, _, _, _) = civil_now();
    format!("{y:04}{m:02}{d:02}")
}

/// `YYYYMMDD-HHMMSS`（UTC）。
fn full_stamp() -> String {
    let (y, m, d, h, mi, s) = civil_now();
    format!("{y:04}{m:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// 把 Unix 秒换成公历年月日时分秒（不引 chrono，逻辑与 launcher 的同款）。
fn civil_now() -> (i64, i64, i64, i64, i64, i64) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let mut rem = secs % 86_400;
    let h = (rem / 3600) as i64;
    rem %= 3600;
    let mi = (rem / 60) as i64;
    let s = (rem % 60) as i64;
    let mut y = 1970i64;
    let mut d = days;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let dy = if leap { 366 } else { 365 };
        if d < dy {
            break;
        }
        d -= dy;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mdays = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0usize;
    while m < 12 && d >= mdays[m] as i64 {
        d -= mdays[m] as i64;
        m += 1;
    }
    (y, m as i64 + 1, d + 1, h, mi, s)
}

/// 追加一行/一段到该角色的当天日志。失败**静默**（日志不该让主流程失败）。
pub fn append_log(role: &str, bytes: &[u8]) {
    let dir = logs_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("{role}-{}.log", day_stamp()));
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(bytes);
    }
}

/// 启动时清理过期日志（> KEEP_DAYS 天前）。
///
/// 判据用**文件名里的日期**（`role-YYYYMMDD.log`），不是 mtime——
/// mtime 会被拷贝/解压改掉，而文件名是我们自己写的。
pub fn prune_old_logs() -> usize {
    let dir = logs_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let today = day_stamp();
    let cutoff = day_str_minus(&today, KEEP_DAYS);
    let mut removed = 0;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("log") {
            continue;
        }
        let Some(name) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // 形如 `core-20261113`：取最后 8 位
        if name.len() < 8 {
            continue;
        }
        let stamp = &name[name.len() - 8..];
        if !stamp.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if stamp < cutoff.as_str() && std::fs::remove_file(&p).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 把 `YYYYMMDD` 往前推 n 天（用整数日期换算，避免引依赖）。
fn day_str_minus(stamp: &str, n: u64) -> String {
    let Ok(days) = stamp.parse::<i64>() else {
        return stamp.to_string();
    };
    // 直接按"日历日"减：先转成 (y,m,d) 再回退
    let y = days / 10_000;
    let m = (days / 100) % 100;
    let d = days % 100;
    let mut ty = y;
    let mut tm = m;
    let mut td = d - n as i64;
    while td <= 0 {
        tm -= 1;
        if tm == 0 {
            tm = 12;
            ty -= 1;
        }
        let leap = (ty % 4 == 0 && ty % 100 != 0) || ty % 400 == 0;
        let mdays = [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        td += mdays[(tm - 1) as usize] as i64;
    }
    format!("{ty:04}{tm:02}{td:02}")
}

/// 诊断包导出结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DiagReport {
    pub dir: String,
    pub files: Vec<String>,
    /// 命中疑似凭据并整行脱敏的行数
    pub redacted_lines: usize,
}

/// 疑似凭据关键词（命中即整行替换；**不打印内容**）。
const SECRET_MARKERS: &[&str] = &["sk-", "Bearer ", "token=", "token:", "api_key", "apikey", "password"];

/// 扫描并脱敏一行；返回 (行, 是否命中)。
fn redact_line(line: &str) -> (String, bool) {
    let lower = line.to_ascii_lowercase();
    for m in SECRET_MARKERS {
        if lower.contains(&m.to_ascii_lowercase()) {
            return (format!("<REDACTED len={}>\n", line.len()), true);
        }
    }
    (line.to_string(), false)
}

/// 导出诊断包到一个新目录（不引 zip，目录形式更易排查）。
pub fn export_diagnostics() -> Result<DiagReport, String> {
    prune_old_logs();
    let dir = logs_dir().join(format!("diagnostics-{}", full_stamp()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("建诊断目录失败: {e}"))?;
    let mut files = Vec::new();
    let mut redacted = 0usize;

    // ① 最近 2 天的日志（脱敏后写入）
    let today = day_stamp();
    let yesterday = day_str_minus(&today, 1);
    let logs = logs_dir();
    if let Ok(rd) = std::fs::read_dir(&logs) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("log") {
                continue;
            }
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !(name.contains(&today) || name.contains(&yesterday)) {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            let mut out = String::new();
            for line in text.lines() {
                let (l, hit) = redact_line(line);
                if hit {
                    redacted += 1;
                }
                out.push_str(&l);
                if !l.ends_with('\n') {
                    out.push('\n');
                }
            }
            let dst = dir.join(name);
            if std::fs::write(&dst, out).is_ok() {
                files.push(name.to_string());
            }
        }
    }

    // ② launcher 运行态与重启事件（整体复制；这两份只含 pid/port/退出码，不含内容）
    let runtime = crate::launcher::runtime_dir();
    for name in ["launcher.ndjson", "launcher-events.ndjson"] {
        let src = runtime.join(name);
        if src.exists() && std::fs::copy(&src, dir.join(name)).is_ok() {
            files.push(name.to_string());
        }
    }

    // ③ db-info.json：**只有数字**（schema 版本 + 各表计数），不含任何记忆正文
    let db = crate::data::db_path();
    let mut info = serde_json::json!({ "db_path_exists": db.exists(), "schema_version": null });
    if let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        info["schema_version"] = serde_json::json!(v);
        let mut counts = serde_json::Map::new();
        for t in [
            "memories",
            "memory_events",
            "memory_facts",
            "edges",
            "memory_conflicts",
            "conversations",
            "agents",
        ] {
            let n: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))
                .unwrap_or(-1);
            counts.insert(t.to_string(), serde_json::json!(n));
        }
        info["counts"] = serde_json::Value::Object(counts);
    }
    if std::fs::write(
        dir.join("db-info.json"),
        serde_json::to_string_pretty(&info).unwrap_or_default(),
    )
    .is_ok()
    {
        files.push("db-info.json".to_string());
    }

    // ④ env.json：版本/OS/端口/pid（端口表是 M5-03 P0 之后确认的口径）
    // T-M10-47：端口取**运行态实际值**（launcher 顺延后 8791 可能不是它在听），
    // 否则用户导出的诊断会与真实运行态不一致。
    let rt = crate::launcher::runtime_state();
    let port_of = |role: &str, fallback: u16| -> u16 {
        rt.iter()
            .find(|e| e.role == role)
            .map(|e| e.port)
            .unwrap_or(fallback)
    };
    let env = serde_json::json!({
        "app_version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "pid": std::process::id(),
        "ports": {
            "core": port_of("core", 8791),
            "proxy": port_of("proxy", 8790),
            "recall": 8799
        },
        "logs_dir": logs.display().to_string(),
    });
    if std::fs::write(
        dir.join("env.json"),
        serde_json::to_string_pretty(&env).unwrap_or_default(),
    )
    .is_ok()
    {
        files.push("env.json".to_string());
    }

    Ok(DiagReport {
        dir: dir.display().to_string(),
        files,
        redacted_lines: redacted,
    })
}

/// 供 Tauri command 使用：导出后返回报告。
pub fn export() -> Result<DiagReport, String> {
    export_diagnostics()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secret_lines_and_keeps_others() {
        assert!(!redact_line("GET /health 200").1);
        let (l, hit) = redact_line("Authorization: Bearer abcdef");
        assert!(hit);
        assert!(l.starts_with("<REDACTED"));
        assert!(!l.contains("abcdef"), "脱敏后不能残留原值");
        let (l2, hit2) = redact_line("openai key sk-123456");
        assert!(hit2 && !l2.contains("sk-123456"));
    }

    #[test]
    fn day_arith_crosses_month() {
        assert_eq!(day_str_minus("20261113", 7), "20261106");
        assert_eq!(day_str_minus("20261103", 3), "20261031");
        assert_eq!(day_str_minus("20260301", 1), "20260228");
    }

    #[test]
    fn prune_uses_filename_date_not_mtime() {
        // 直接测判据：8 天前的文件名 < 截止日 → 应删
        let today = day_stamp();
        let cutoff = day_str_minus(&today, KEEP_DAYS);
        let old = day_str_minus(&today, 9);
        assert!(old.as_str() < cutoff.as_str());
        let fresh = day_str_minus(&today, 1);
        assert!(fresh.as_str() >= cutoff.as_str());
    }
}
