//! orbital-proxy：**透明转发 + 注入**（只读，ADR-008 / ADR-024 / ADR-025 / ADR-041）。
//!
//! ## 它凭什么能"拦到" agent 的请求
//!
//! agent 默认直连模型端点。要让 proxy 注入，必须**改写 agent 的模型 base_url**
//! 指向 `http://127.0.0.1:8790`（见 ADR-024）。
//!
//! ## 硬规则
//! 1. **从不写库**（ADR-008）；注入统计只写 JSONL 文件；
//! 2. **直连兜底**：任何异常 → 原样转发，绝不阻塞对话；
//! 3. **降级可见**：响应头带 `X-Orbital-Inject`；
//! 4. 流式（SSE）**原样透传**，不缓冲；
//! 5. **URL 拼接必须去重**：上游 `.../v1` + 请求 `/v1/...` 不能变成 `/v1/v1/...`（2026-11-12 实测 bug 修复）；
//! 6. **注入 300ms 超时即跳过**（ADR-008）；召回通过 core 的本地只读端点（ADR-041）。

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use bytes::Bytes;
use futures_util::StreamExt;
use std::path::PathBuf;

#[derive(Clone)]
/// 一条注入缓存（T-M7-01）。
///
/// 只缓存"召回后要注入的文本行"，**不缓存用户请求体**——避免把对话内容留在内存里。
struct CachedInject {
    lines: Vec<String>,
    used_chars: usize,
    at: std::time::Instant,
}

/// 注入结果缓存：TTL 5s + LRU 32（T-M7-01）。
///
/// 为什么默认关闭：缓存会引入"5 秒内的旧召回"。默认行为必须与没有缓存时**完全一致**，
/// 只有显式 `ORBITAL_INJECT_CACHE=1` 才启用。
struct InjectCache {
    map: std::collections::HashMap<u64, CachedInject>,
    order: std::collections::VecDeque<u64>,
    cap: usize,
    ttl: std::time::Duration,
}

impl InjectCache {
    fn new(cap: usize, ttl: std::time::Duration) -> Self {
        Self {
            map: std::collections::HashMap::new(),
            order: std::collections::VecDeque::new(),
            cap,
            ttl,
        }
    }

    /// 命中返回 (lines, used_chars)；过期或未命中返回 None。
    fn get(&mut self, key: u64) -> Option<(Vec<String>, usize)> {
        let hit = self.map.get(&key)?;
        if hit.at.elapsed() > self.ttl {
            self.map.remove(&key);
            return None;
        }
        Some((hit.lines.clone(), hit.used_chars))
    }

    fn put(&mut self, key: u64, lines: Vec<String>, used_chars: usize) {
        if self.map.len() >= self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        self.map.insert(
            key,
            CachedInject {
                lines,
                used_chars,
                at: std::time::Instant::now(),
            },
        );
        self.order.push_back(key);
    }
}

/// FNV-1a：只为缓存键做散列，不需要密码学强度（`sha2` 会多一个依赖）。
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

#[derive(Clone)]
struct Ctx {
    /// 原始上游（可能带路径前缀，如 `https://api.deepseek.com/v1`）
    upstream: String,
    client: reqwest::Client,
    /// 是否允许注入（`ORBITAL_INJECT=1` 才开；默认关）
    inject_enabled: bool,
    /// core 的本地只读召回端点（ADR-041）
    core_recall_url: String,
    /// 注入统计落盘目录（JSONL，按日）
    metrics_dir: PathBuf,
    /// T-M7-01：注入结果缓存（ORBITAL_INJECT_CACHE=1 才启用）
    cache: std::sync::Arc<std::sync::Mutex<InjectCache>>,
    cache_enabled: bool,
}

/// 拼接上游 URL，**去掉重复的路径前缀**。
///
/// 实测 bug（2026-11-12）：上游 `http://host/v1` + 请求 `/v1/models`
/// 直接相加得到 `/v1/v1/models` → 404。用户按 OpenAI 习惯配 `/v1` 时必坏。
pub fn join_url(upstream: &str, path_and_query: &str) -> String {
    let up = upstream.trim_end_matches('/');
    let path = if path_and_query.is_empty() {
        "/"
    } else {
        path_and_query
    };
    // 若上游本身以 "/xxx" 结尾，且请求路径也以同一段开头 -> 去掉重复
    if let Some(idx) = up.rfind("://") {
        let after_scheme = &up[idx + 3..];
        if let Some(slash) = after_scheme.find('/') {
            let prefix = &after_scheme[slash..]; // 如 "/v1"
            if !prefix.is_empty() && path.starts_with(prefix) {
                let rest = &path[prefix.len()..];
                let rest = if rest.is_empty() { "/" } else { rest };
                return format!("{up}{rest}");
            }
        }
    }
    // 上游没有路径前缀：直接拼
    format!("{up}{path}")
}

/// T-M10-11（ADR-051）：GUI 可编辑的 proxy 配置。
///
/// 路径：`%LOCALAPPDATA%\OrbitalPro\proxy.conf.json`
/// 优先级：**文件 > 环境变量 > 默认**。
/// 为什么要有文件：环境变量只能在 spawn 时设定，用户在界面里改不了；
/// 写文件后 GUI 杀掉 proxy 子进程，启动器的 watchdog 会自动拉起新进程读新配置。
#[derive(Debug, Default, serde::Deserialize)]
struct ProxyConf {
    /// 上游 API 地址（如 `http://127.0.0.1:11434`）
    upstream: Option<String>,
    /// 是否开启注入
    inject: Option<bool>,
    /// core 的只读召回端点（默认 `http://127.0.0.1:8799`）
    core_url: Option<String>,
}

fn proxy_conf_path() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("OrbitalPro").join("proxy.conf.json")
}

/// 读配置；文件不存在或格式坏 → 返回默认（**不 panic**：proxy 必须能起来，
/// 配置坏了只是"按环境变量/默认跑"，不是拒绝服务）。
fn load_proxy_conf() -> ProxyConf {
    let p = proxy_conf_path();
    match std::fs::read_to_string(&p) {
        Ok(s) => match serde_json::from_str::<ProxyConf>(&s) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %p.display(), %e, "proxy.conf.json 解析失败，按环境变量/默认处理");
                ProxyConf::default()
            }
        },
        Err(_) => ProxyConf::default(),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    // T-M10-12（P2）：被 GUI 的 `--role proxy` 拉起时，stdin 是父进程持有的管道。
    // 父退出（含被 kill）→ 写端关闭 → 这里读到 EOF → 自己退出。
    // 这样"杀掉 `--role proxy`"能连带杀掉真正的 proxy，watchdog 拉起的新进程才能绑上端口。
    // 手动运行（无 ORBITAL_PARENT_PID）时不启监控，避免终端 stdin 关闭导致误退。
    if std::env::var("ORBITAL_PARENT_PID").is_ok() {
        std::thread::spawn(|| {
            use std::io::Read;
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 64];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => {
                        tracing::info!("T-M10-12: 父进程已退出（stdin EOF），proxy 随之退出");
                        std::process::exit(0);
                    }
                    Ok(_) => {}
                    Err(_) => std::process::exit(0),
                }
            }
        });
    }

    let cfg = orbital_core::config::Config::default();
    // T-M10-11（ADR-051）：优先读 GUI 写的 `proxy.conf.json`（**文件 > 环境变量 > 默认**）。
    // 为什么需要：环境变量只能在 spawn 时设定，用户在界面上改不了；
    // 有了配置文件，GUI 保存后杀掉 proxy 子进程、启动器 watchdog 会自动拉起新进程读新配置。
    let conf = load_proxy_conf();
    let upstream = conf
        .upstream
        .as_deref()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("ORBITAL_UPSTREAM")
                .ok()
                .map(|s| s.trim().trim_end_matches('/').to_string())
        })
        .unwrap_or_default();

    // 代理：默认关闭系统代理（本机 HTTP_PROXY 是 SOCKS5，会静默卡死）；
    // 需要代理的环境用 ORBITAL_PROXY_USE_SYSTEM_PROXY=1 显式开启
    let use_sys_proxy = std::env::var("ORBITAL_PROXY_USE_SYSTEM_PROXY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let mut builder = reqwest::Client::builder()
        // 只限制**建连**超时；不设总超时 —— 流式长对话不能被 300s 截断（审核发现）
        .connect_timeout(std::time::Duration::from_secs(30))
        .pool_idle_timeout(std::time::Duration::from_secs(90));
    if !use_sys_proxy {
        builder = builder.no_proxy();
    }
    let client = builder.build()?;

    // T-M3-06：注入开关（默认关）+ core 只读端点（ADR-041）+ 统计目录
    // T-M10-11：配置文件优先；环境变量作为回退（保持开发期用 env 的习惯可用）。
    let inject_enabled = conf.inject.unwrap_or_else(|| {
        std::env::var("ORBITAL_INJECT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    });
    let core_recall_url = conf
        .core_url
        .as_deref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            std::env::var("ORBITAL_CORE_URL").unwrap_or_else(|_| "http://127.0.0.1:8799".to_string())
        });
    let metrics_dir = std::env::var("LOCALAPPDATA")
        .map(|b| PathBuf::from(b).join("OrbitalPro").join("metrics"))
        .unwrap_or_else(|_| PathBuf::from(".").join("metrics"));

    // T-M10-11：upstream 现在可能来自 `proxy.conf.json`（环境变量只是回退），
    // 所以判据必须是**最终解析出来的 upstream 是否为空**，不能只看 env——
    // 否则会出现"配置了上游却打日志说未配置、所有请求返回 502"的误报。
    // 注意：`upstream` 随后会 move 进 ctx，所以这里必须先算。
    let has_up = !upstream.is_empty();
    let ctx = Ctx {
        upstream,
        client,
        inject_enabled,
        core_recall_url,
        metrics_dir,
        cache: std::sync::Arc::new(std::sync::Mutex::new(InjectCache::new(32, std::time::Duration::from_secs(5)))),
        cache_enabled: std::env::var("ORBITAL_INJECT_CACHE").ok().as_deref() == Some("1"),
    };

    let app = Router::new()
        .route("/health", get(health))
        .fallback(forward)
        .with_state(ctx);

    let addr = format!("127.0.0.1:{}", cfg.proxy_port);
    tracing::info!(
        version = orbital_core::VERSION,
        %addr,
        upstream_configured = has_up,
        inject_enabled,
        "orbital-proxy listening"
    );
    if !has_up {
        tracing::warn!("未设置 ORBITAL_UPSTREAM：所有请求返回 502（不静默假装转发）");
    }

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health(State(ctx): State<Ctx>) -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "status": "ok",
        "service": "orbital-proxy",
        "version": orbital_core::VERSION,
        "upstream_configured": !ctx.upstream.is_empty(),
        "inject": if ctx.inject_enabled { "on" } else { "off" },
        "core_url": ctx.core_recall_url,
    }))
}

/// 兜底：所有路径原样转发到上游（`/v1/chat/completions` 会尝试注入）。
async fn forward(
    State(ctx): State<Ctx>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if ctx.upstream.is_empty() {
        return (
            StatusCode::BAD_GATEWAY,
            [("x-orbital-error", "upstream_not_configured")],
            axum::Json(serde_json::json!({
                "error": "ORBITAL_UPSTREAM 未配置：proxy 不知道把请求转发到哪里"
            })),
        )
            .into_response();
    }

    let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let url = join_url(&ctx.upstream, path);

    // ---- T-M3-06：注入（只对 POST /chat/completions；任何失败都退回原 body）----
    let mut inject_status = if ctx.inject_enabled {
        "skipped(not-chat-request)"
    } else {
        "off"
    }
    .to_string();
    let mut out_body = body.clone();
    if ctx.inject_enabled && method == Method::POST && path.contains("/chat/completions") {
        // T-M10-40b：测量注入总耗时。
        // 修复前错误分支硬编码 `elapsed_ms = 0`，导致 metrics 里 400 次
        // `error:timeout` 全是 0ms ⇒ 状态页 P50/P95 失真（显示 3ms 却报了 400 次超时）。
        let inject_t0 = std::time::Instant::now();
        match try_inject(&ctx, &body).await {
            Ok((new_body, n, tokens, elapsed, cache)) => {
                if n > 0 {
                    inject_status = format!("on({n})");
                    record_metric(&ctx, "openai", n, tokens, elapsed, "on", cache);
                    out_body = new_body;
                } else {
                    inject_status = "skipped(no_hits)".to_string();
                    record_metric(&ctx, "openai", 0, 0, elapsed, "no_hits", cache);
                }
            }
            Err(reason) => {
                // 红线 7：失败只记状态，原请求照发
                inject_status = format!("skipped({reason})");
                record_metric(
                    &ctx,
                    "openai",
                    0,
                    0,
                    inject_t0.elapsed().as_millis(),
                    &format!("error:{reason}"),
                    "miss",
                );
            }
        }
    }

    let mut req = ctx.client.request(method.clone(), &url);
    for (k, v) in headers.iter() {
        let name = k.as_str().to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "host" | "content-length" | "connection" | "transfer-encoding"
        ) {
            continue;
        }
        if let Ok(val) = v.to_str() {
            req = req.header(k.as_str(), val);
        }
    }
    if !out_body.is_empty() {
        req = req.body(out_body);
    }

    match req.send().await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut out_headers = HeaderMap::new();
            for (k, v) in resp.headers().iter() {
                let name = k.as_str().to_ascii_lowercase();
                if matches!(
                    name.as_str(),
                    "content-length" | "transfer-encoding" | "connection"
                ) {
                    continue;
                }
                if let Ok(val) = axum::http::HeaderValue::from_bytes(v.as_bytes()) {
                    out_headers.insert(k.clone(), val);
                }
            }
            if let Ok(val) = axum::http::HeaderValue::from_str(&inject_status) {
                out_headers.insert("x-orbital-inject", val);
            }

            let stream = resp
                .bytes_stream()
                .map(|chunk| chunk.map_err(std::io::Error::other));
            let body = Body::from_stream(stream);

            let mut r = Response::new(body);
            *r.status_mut() = status;
            *r.headers_mut() = out_headers;
            r
        }
        Err(e) => {
            tracing::error!(%url, %e, "forward failed");
            (
                StatusCode::BAD_GATEWAY,
                [("x-orbital-inject", "skipped(upstream_error)")],
                axum::Json(serde_json::json!({
                    "error": format!("转发失败: {e}"),
                    "upstream": ctx.upstream
                })),
            )
                .into_response()
        }
    }
}

// ---------------- 注入（T-M3-06）----------------

/// 尝试注入：返回 `(新请求体, 命中条数, 估算 token, 耗时 ms)`。
///
/// 任何失败回 `Err(原因字符串)`，调用方把原因放进 `X-Orbital-Inject` 并**照发原请求**。
async fn try_inject(
    ctx: &Ctx,
    body: &Bytes,
) -> Result<(Bytes, usize, usize, u128, &'static str), String> {
    let t0 = std::time::Instant::now();
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("bad-json:{e}"))?;
    let q = last_user_text(&v).ok_or_else(|| "no-user-msg".to_string())?;
    if q.trim().is_empty() {
        return Err("empty-query".into());
    }

    // T-M7-01：先查缓存（命中则完全跳过 HTTP 往返）
    let cache_key = fnv1a(&q);
    let mut cache_flag = "miss";
    let mut cached: Option<(Vec<String>, usize)> = None;
    if ctx.cache_enabled {
        if let Ok(mut c) = ctx.cache.lock() {
            cached = c.get(cache_key);
        }
        if cached.is_some() {
            cache_flag = "hit";
        }
    }

    let (lines, used) = match cached {
        Some(hit) => hit,
        None => {
            let (lines, used) = fetch_and_budget(ctx, &q).await?;
            if ctx.cache_enabled && !lines.is_empty() {
                if let Ok(mut c) = ctx.cache.lock() {
                    c.put(cache_key, lines.clone(), used);
                }
            }
            (lines, used)
        }
    };
    if lines.is_empty() {
        return Ok((body.clone(), 0, 0, t0.elapsed().as_millis(), cache_flag));
    }
    let injected = build_injected(&v, &lines)?;
    Ok((
        Bytes::from(injected),
        lines.len(),
        used / 3 + 1,
        t0.elapsed().as_millis(),
        cache_flag,
    ))
}

/// 召回 + 预算裁剪（从 `try_inject` 抽出来：缓存命中时不需要它）。
async fn fetch_and_budget(ctx: &Ctx, q: &str) -> Result<(Vec<String>, usize), String> {
    // T-M10-21（性能兜底）：三路融合在 0.92.0 实测 P50 223ms / P95 243ms / **max 519ms**，
    // 会打穿 300ms 注入预算。策略：**先试三路（准），超时/失败立刻用 FTS-only 兜底（~16ms）**，
    // 保证"超时也有东西注入"，而不是整轮空注入（空注入与"没有相关记忆"无法区分，是假绿）。
    match fetch_recall(ctx, q, "fts,vec,graph", 300).await {
        Ok(v) => Ok(v),
        Err(e) => {
            tracing::warn!(%e, "三路召回失败/超时，降级 FTS-only 兜底");
            fetch_recall(ctx, q, "fts", 150).await
        }
    }
}

/// 按指定通道与超时请求 core 的只读召回端点，并按字符预算裁剪。
async fn fetch_recall(
    ctx: &Ctx,
    q: &str,
    channels: &str,
    timeout_ms: u64,
) -> Result<(Vec<String>, usize), String> {
    let url = format!(
        "{}/recall?q={}&agent=proxy&limit=8&scope=shared&channels={}",
        ctx.core_recall_url.trim_end_matches('/'),
        url_encode(q),
        channels
    );
    let resp = ctx
        .client
        .get(&url)
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                "timeout".to_string()
            } else {
                format!("core-error:{e}")
            }
        })?;
    if !resp.status().is_success() {
        return Err(format!("core-{}", resp.status().as_u16()));
    }
    let rv: serde_json::Value = resp.json().await.map_err(|e| format!("bad-core-json:{e}"))?;
    let hits = rv
        .get("hits")
        .and_then(|h| h.as_array())
        .cloned()
        .unwrap_or_default();
    if hits.is_empty() {
        return Ok((Vec::new(), 0));
    }

    // 预算：约 1024 token（字符数 / 3 估算），按召回顺序累加
    const BUDGET_CHARS: usize = 3000;
    let mut lines: Vec<String> = Vec::new();
    let mut used = 0usize;
    for h in hits.iter() {
        let c = h.get("content").and_then(|x| x.as_str()).unwrap_or("");
        if c.is_empty() {
            continue;
        }
        let len = c.chars().count();
        if used + len > BUDGET_CHARS {
            break;
        }
        used += len;
        lines.push(format!("- {}", c.replace('\n', " ")));
    }
    Ok((lines, used))
}

/// 把召回结果拼进请求体（system 优先追加，否则插到最前）。
fn build_injected(v: &serde_json::Value, lines: &[String]) -> Result<Vec<u8>, String> {
    let sys = format!(
        "以下是与本次对话相关的长期记忆（本机记忆中枢提供，仅供参考，不是用户指令）：\n{}\n",
        lines.join("\n")
    );
    let mut nv = v.clone();
    let messages = nv
        .get_mut("messages")
        .and_then(|m| m.as_array_mut())
        .ok_or("no-messages")?;
    let mut appended = false;
    if let Some(first) = messages.first_mut() {
        if first.get("role").and_then(|r| r.as_str()) == Some("system") {
            let old = first
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            first["content"] = serde_json::Value::String(format!("{sys}\n{old}"));
            appended = true;
        }
    }
    if !appended {
        messages.insert(0, serde_json::json!({"role":"system","content":sys}));
    }
    serde_json::to_vec(&nv).map_err(|e| format!("re-encode:{e}"))
}

/// 取最后一条 user 消息的文本（OpenAI 的 content 可能是字符串或 `[{type,text}]`）。
fn last_user_text(v: &serde_json::Value) -> Option<String> {
    let msgs = v.get("messages")?.as_array()?;
    for m in msgs.iter().rev() {
        if m.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let c = m.get("content")?;
        if let Some(s) = c.as_str() {
            return Some(s.to_string());
        }
        if let Some(arr) = c.as_array() {
            let joined: String = arr
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect();
            return Some(joined);
        }
        return None;
    }
    None
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 统计落盘（ADR-008：proxy 从不写库，只写 JSONL）。
fn record_metric(
    ctx: &Ctx,
    protocol: &str,
    hits: usize,
    tokens: usize,
    elapsed_ms: u128,
    status: &str,
    cache: &str,
) {
    use std::io::Write;
    let _ = std::fs::create_dir_all(&ctx.metrics_dir);
    let day = utc_day();
    let path = ctx.metrics_dir.join(format!("injection-{day}.jsonl"));
    let line = serde_json::json!({
        "ts": now_secs(),
        "protocol": protocol,
        "hits": hits,
        "tokens": tokens,
        "elapsed_ms": elapsed_ms,
        "status": status,
        "cache": cache,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `YYYY-MM-DD`（UTC）——统计按日分文件，避免单文件无限增长。
fn utc_day() -> String {
    let secs = now_secs() as u64;
    let days = secs / 86_400;
    // 1970-01-01 起的天数 → 公历（简单算法，够用）
    let mut y = 1970i64;
    let mut d = days as i64;
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
    let mdays = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0;
    while m < 12 && d >= mdays[m] {
        d -= mdays[m];
        m += 1;
    }
    format!("{y:04}-{:02}-{:02}", m + 1, d + 1)
}

#[cfg(test)]
mod tests {
    use super::{fnv1a, join_url, last_user_text, url_encode, utc_day, InjectCache};

    #[test]
    fn join_url_dedups_prefix() {
        // 实测 bug 场景：上游带 /v1、请求也带 /v1
        assert_eq!(
            join_url("http://h:1/v1", "/v1/models"),
            "http://h:1/v1/models"
        );
        assert_eq!(
            join_url("https://api.deepseek.com/v1", "/v1/chat/completions"),
            "https://api.deepseek.com/v1/chat/completions"
        );
    }

    #[test]
    fn join_url_plain() {
        assert_eq!(
            join_url("https://api.deepseek.com", "/v1/chat/completions"),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(join_url("http://h:1", "/api/tags"), "http://h:1/api/tags");
    }

    #[test]
    fn join_url_handles_query_and_trailing_slash() {
        assert_eq!(
            join_url("http://h:1/v1/", "/v1/models?x=1"),
            "http://h:1/v1/models?x=1"
        );
        assert_eq!(join_url("http://h:1/v1", "/"), "http://h:1/v1/");
    }

    #[test]
    fn url_encode_handles_chinese_and_space() {
        assert_eq!(url_encode("a b"), "a%20b");
        assert_eq!(url_encode("中文"), "%E4%B8%AD%E6%96%87");
    }

    #[test]
    fn last_user_text_prefers_latest_user() {
        let v = serde_json::json!({
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "第一个问题"},
                {"role": "assistant", "content": "回答"},
                {"role": "user", "content": [{"type":"text","text":"最后一个问题"}]}
            ]
        });
        assert_eq!(last_user_text(&v).as_deref(), Some("最后一个问题"));
    }

    #[test]
    fn utc_day_is_iso_like() {
        let d = utc_day();
        assert_eq!(d.len(), 10);
        assert!(d.contains('-'));
    }

    // ---- T-M7-01：注入缓存 ----

    #[test]
    fn cache_hit_within_ttl() {
        let mut c = InjectCache::new(4, std::time::Duration::from_secs(5));
        c.put(1, vec!["记忆 A".into()], 10);
        let got = c.get(1).expect("未过期应命中");
        assert_eq!(got.0, vec!["记忆 A".to_string()]);
        assert_eq!(got.1, 10);
    }

    #[test]
    fn cache_expires_after_ttl() {
        let mut c = InjectCache::new(4, std::time::Duration::from_millis(1));
        c.put(1, vec!["记忆 A".into()], 10);
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(c.get(1).is_none(), "过期后必须 miss");
    }

    #[test]
    fn cache_evicts_oldest_when_full() {
        let mut c = InjectCache::new(2, std::time::Duration::from_secs(60));
        c.put(1, vec!["1".into()], 1);
        c.put(2, vec!["2".into()], 1);
        c.put(3, vec!["3".into()], 1); // 超容量 -> 淘汰 1
        assert!(c.get(1).is_none(), "最老的应被淘汰");
        assert!(c.get(2).is_some());
        assert!(c.get(3).is_some());
    }

    #[test]
    fn different_text_maps_to_different_key() {
        assert_ne!(fnv1a("用户问的是 A"), fnv1a("用户问的是 B"));
        assert_eq!(fnv1a("同一句话"), fnv1a("同一句话"));
        assert_ne!(fnv1a(""), fnv1a(" "), "空串与空格不能撞键");
    }
}
