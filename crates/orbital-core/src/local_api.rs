//! core 的**本地只读 HTTP 端点**（ADR-041）——只给 proxy 用。
//!
//! 为什么需要它：注入要在 proxy 里完成，但 proxy 与 core 是两个进程（`docs/04 §5`）。
//! 三条被否方案见 ADR-041；本模块只实现"只读、只绑回环、一个端点"的方案。
//!
//! 实现说明：`rusqlite::Connection` 不是 `Sync`，所以端点在**自己的线程里 open 一个
//! `MemoryService`** 并串行处理请求（注入是 300ms 预算内的低频调用，串行足够）。
//! 端点不调用任何写方法 ⇒ ADR-008"proxy 从不写库"仍成立。
//!
//! 硬约束：
//! 1. **只绑 `127.0.0.1`**（`spawn_recall_server` 会拒绝非回环地址）；
//! 2. **只有 `GET /recall`**，没有任何写路径；
//! 3. 复用 `MemoryService::recall` 的全部门控（作用域/冲突排除/访问打点）。

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

use crate::identity::VerifiedAgent;
use crate::service::{MemoryService, RecallChannel, RecallOptions};

/// 起一个只读端点（后台线程），返回实际端口。
///
/// `bind` 必须是回环地址（`127.0.0.1:<port>`）；不是则回 `InvalidInput`。
pub fn spawn_recall_server(
    db_path: impl Into<PathBuf>,
    journal_path: impl Into<PathBuf>,
    bind: &str,
) -> std::io::Result<u16> {
    if !bind.starts_with("127.0.0.1:") && !bind.starts_with("localhost:") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "ADR-041：本地端点只允许绑定 127.0.0.1",
        ));
    }
    let listener = TcpListener::bind(bind)?;
    let port = listener.local_addr()?.port();
    let db_path = db_path.into();
    let journal_path = journal_path.into();
    std::thread::spawn(move || {
        // 在端点自己的线程里 open（Connection 不跨线程）；一次 open 服务所有请求
        let svc = match MemoryService::open(&db_path, &journal_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("local api 打开数据库失败，端点不可用: {e}");
                return;
            }
        };
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    if let Err(e) = handle(&svc, s) {
                        tracing::warn!("local api 连接处理失败: {e}");
                    }
                }
                Err(e) => tracing::warn!("local api accept 失败: {e}"),
            }
        }
    });
    Ok(port)
}

fn handle(svc: &MemoryService, mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    // 读掉请求头（本端点不需要任何头）
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    if method != "GET" {
        return write_json(&mut stream, 405, r#"{"error":"only GET"}"#);
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    if path != "/recall" {
        return write_json(&mut stream, 404, r#"{"error":"only /recall"}"#);
    }

    let params = parse_query(query);
    let q = params.get("q").map(String::as_str).unwrap_or("");
    let agent = params.get("agent").map(String::as_str).unwrap_or("proxy");
    let limit: i64 = params
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    // ADR-042：proxy 注入只取 shared（它无法确认请求来自哪个 agent，private 不能泄漏）
    let shared_only = params.get("scope").map(|s| s == "shared").unwrap_or(false);
    if q.trim().is_empty() {
        return write_json(&mut stream, 400, r#"{"error":"q 不能为空"}"#);
    }

    // 本地特权读路径（ADR-034）：proxy 只读，只查不写
    let va = VerifiedAgent::for_local_owner(agent.to_string());
    // T-M10-21（性能兜底）：`channels` 可选参数（`fts,vec,graph` / `fts`）。
    // 为什么需要：三路融合 P95 243ms、max 519ms，注入热路径 300ms 预算会被打穿；
    // 超时后调用方可以立刻用 `channels=fts`（~16ms）重试一次，保证"超时也有东西注入"，
    // 而不是整轮空注入。
    let default_opts = RecallOptions::default();
    let channels: Vec<RecallChannel> = match params.get("channels") {
        Some(s) if !s.trim().is_empty() => {
            let mut v = Vec::new();
            for c in s.split(',') {
                match c.trim() {
                    "fts" => v.push(RecallChannel::Fts),
                    "vec" | "vector" => v.push(RecallChannel::Vector),
                    "graph" => v.push(RecallChannel::Graph),
                    _ => {}
                }
            }
            if v.is_empty() {
                default_opts.channels.clone()
            } else {
                v
            }
        }
        _ => default_opts.channels.clone(),
    };
    let opts = RecallOptions {
        weights: vec![1.0; channels.len()],
        channels,
        ..default_opts
    };
    // T-M10-18（2026-11-14 审核修复）：原实现调单路 `recall()`（只走 FTS）。
    // 实测：FTS5 trigram 的**最小匹配是 3 字符** ⇒ 中文 2 字词（"记忆"/"端口"/"注入"）
    // 召回 **0 条**；而库里 `embedding_status` 89/89 全 `ok`、`memories_vec` 也有数据，
    // 语义通道却完全没用上。注入若走这条，用户问"端口是多少"会得到空注入。
    // ⇒ 改为三路融合（Fts + Vector + Graph，RRF 倒数排名融合），并返回 `degraded`
    //   （"某一路没跑"必须与"没匹配"区分，docs/03 §5.4 的硬失败契约）。
    match svc.recall_fused(&va, q, limit, &opts) {
        Ok(mut fused) => {
            if shared_only {
                fused.hits.retain(|h| h.scope == "shared");
            }
            // 手工映射：FusedHit 的 `channels` 是诊断字段（"这条是哪一路带来的"），
            // 对 hook 适配器有用（能看出向量通道是否在起作用）。
            let hits: Vec<serde_json::Value> = fused
                .hits
                .iter()
                .map(|h| {
                    serde_json::json!({
                        "id": h.id,
                        "content": h.content,
                        "scope": h.scope,
                        "score": h.score,
                        "channels": h.channels,
                    })
                })
                .collect();
            let body = serde_json::json!({
                "count": hits.len(),
                "hits": hits,
                "degraded": fused.degraded,
            });
            let text = serde_json::to_string(&body).unwrap_or_else(|_| "{}".into());
            write_json(&mut stream, 200, &text)
        }
        Err(e) => {
            let text = serde_json::json!({ "error": e.to_string() }).to_string();
            write_json(&mut stream, 500, &text)
        }
    }
}

fn write_json(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        if status == 200 { "OK" } else { "ERR" },
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

fn parse_query(q: &str) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        m.insert(url_decode(k), url_decode(v));
    }
    m
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = &s[i + 1..i + 3];
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_decode_handles_utf8_and_plus() {
        assert_eq!(url_decode("a+b"), "a b");
        assert_eq!(url_decode("%E4%B8%AD%E6%96%87"), "中文");
        assert_eq!(url_decode("no-encoding"), "no-encoding");
    }

    /// ADR-041 判据 ③：端点绝不允许绑到非回环地址。
    #[test]
    fn rejects_non_loopback_bind() {
        let dir = std::env::temp_dir().join(format!("orbital-la-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = spawn_recall_server(dir.join("t.db"), dir.join("j"), "0.0.0.0:0");
        assert!(err.is_err(), "非回环地址必须被拒绝");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 真起端点、真发 HTTP（只读端点的可用性正对照）。
    #[test]
    fn server_serves_recall_over_http() {
        let dir = std::env::temp_dir().join(format!("orbital-la2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ORBITAL_SECRETS", dir.join("no-secrets.json").display().to_string());

        let port = spawn_recall_server(dir.join("t.db"), dir.join("j2"), "127.0.0.1:0").unwrap();
        // 等端点线程起来（open + 监听）
        std::thread::sleep(std::time::Duration::from_millis(400));
        let resp = reqwest_blocking_get(&format!(
            "http://127.0.0.1:{port}/recall?q=zephyr&agent=proxy&limit=5"
        ))
        .expect("端点应可用");
        assert!(resp.contains("200 OK"), "应返回 200: {resp}");
        assert!(resp.contains("\"hits\""), "响应应含 hits: {resp}");

        // 空 query 必须 400（不是 500、不是静默空结果）
        let bad = reqwest_blocking_get(&format!(
            "http://127.0.0.1:{port}/recall?q=&agent=proxy"
        ))
        .expect("端点应可用");
        assert!(bad.contains("400"), "空 query 应回 400: {bad}");

        std::env::remove_var("ORBITAL_SECRETS");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn reqwest_blocking_get(url: &str) -> Result<String, String> {
        use std::io::{Read, Write};
        let addr = url
            .trim_start_matches("http://")
            .split('/')
            .next()
            .ok_or_else(|| "bad url".to_string())?;
        let path = &url["http://".len() + addr.len()..];
        let mut stream =
            TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes())
            .map_err(|e| format!("write: {e}"))?;
        let mut out = String::new();
        stream.read_to_string(&mut out).map_err(|e| format!("read: {e}"))?;
        Ok(out)
    }
}
