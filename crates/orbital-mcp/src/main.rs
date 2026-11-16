//! orbital-mcp：MCP over stdio（8 个工具）。
//!
//! 协议：逐行 JSON-RPC 2.0。**stdout 只放响应**，日志一律走 stderr。
//! 身份：环境变量 `ORBITAL_AGENT_TOKEN`（由接入时生成）；每次 `tools/call` 都校验。
//! 契约：`docs/04 §10`；错误码：`crates/orbital-core/src/error.rs`。

use std::io::{BufRead, Write};
use std::path::PathBuf;

use orbital_core::error::OrbitalError;
use orbital_core::service::{MemoryService, UpdatePatch};
use orbital_core::writer::WriteInput;
use serde_json::{json, Value};

const PROTOCOL_VERSION: &str = "2025-06-18";

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::INFO)
        .init();

    let db = std::env::var("ORBITAL_DB_PATH").unwrap_or_else(|_| default_db_path());
    let journal = std::env::var("ORBITAL_JOURNAL_PATH").unwrap_or_else(|_| format!("{db}.journal"));
    let token = std::env::var("ORBITAL_AGENT_TOKEN").unwrap_or_default();

    // 接入向导用：`orbital-mcp --register <agent_id>` 生成并打印令牌（只此一次）
    let argv: Vec<String> = std::env::args().collect();
    if let Some(pos) = argv.iter().position(|a| a == "--register") {
        if let Some(agent_id) = argv.get(pos + 1) {
            let d = match orbital_core::db::Db::open(&db) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("打开数据库失败: {e}");
                    std::process::exit(1);
                }
            };
            match orbital_core::identity::register_agent(d.conn(), agent_id, agent_id) {
                Ok(t) => {
                    // 明文令牌只输出一次（stdout 仅此一行）
                    println!("{}", t);
                    return;
                }
                Err(e) => {
                    eprintln!("注册失败: {e}");
                    std::process::exit(1);
                }
            }
        }
    }

    // 一键全量维护：`orbital-mcp --maintain [--dry]`
    // ① import 所有已知 agent 的记忆文件  ② 更新静态索引（ADR-028）
    if argv.iter().any(|a| a == "--maintain") {
        let dry = argv.iter().any(|a| a == "--dry");
        match run_maintain(dry) {
            Ok(lines) => {
                for l in lines {
                    println!("{l}");
                }
            }
            Err(e) => {
                eprintln!("MAINTAIN FAIL: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // 卸载接入：`orbital-mcp --detach <agent>`
    // 删除该 agent 配置里的 orbital 条目（原子写），绝不整体覆盖用户配置（ADR-029）
    if let Some(pos) = argv.iter().position(|a| a == "--detach") {
        let agent = argv.get(pos + 1).cloned().unwrap_or_default();
        match run_detach(&agent) {
            Ok((path, r)) => {
                println!(
                    "DETACH OK: agent={agent} changed={} removed={} sha256={} -> {}",
                    r.changed, r.removed, r.sha256, path.display()
                );
            }
            Err(e) => {
                eprintln!("DETACH FAIL: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // 记忆反向导入：`orbital-mcp --import <agent> [--dry]`
    // 读各 agent 自己的记忆文件 -> 解析条目 -> 写库（幂等，ADR-028 路 B）
    if let Some(pos) = argv.iter().position(|a| a == "--import") {
        let agent = argv.get(pos + 1).cloned().unwrap_or_else(|| "dsh".into());
        let dry = argv.iter().any(|a| a == "--dry");
        match run_import(&agent, dry) {
            Ok((file, n, skipped)) => {
                println!("IMPORT OK: {n} 条新增 / {skipped} 条已存在 -> {file}");
            }
            Err(e) => {
                eprintln!("IMPORT FAIL: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // 静态索引同步：`orbital-mcp --sync <agent_id> [--limit N]`
    // 把最近记忆写成「索引 + 调用指引」，原子写入 MEMORY.md 的受控区块（ADR-027）。
    if let Some(pos) = argv.iter().position(|a| a == "--sync") {
        let agent = argv.get(pos + 1).cloned().unwrap_or_else(|| "dsh".into());
        let limit: i64 = argv
            .iter()
            .position(|a| a == "--limit")
            .and_then(|p| argv.get(p + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(20);

        match run_sync(&agent, limit) {
            Ok((path, n)) => {
                println!("SYNC OK: {} 条索引 -> {}\n\n{}", n, path.display(), render_block_preview(&agent, limit));
            }
            Err(e) => {
                eprintln!("SYNC FAIL: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let svc = match MemoryService::open(&db, &journal) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("orbital-mcp 启动失败: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!(version = orbital_core::VERSION, db = %db, "orbital-mcp ready (stdio)");

    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(
                    out,
                    "{}",
                    json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":format!("parse error: {e}")}})
                );
                let _ = out.flush();
                continue;
            }
        };
        let resp = handle(&req, &svc, &token);
        // notification（无 id）不回包
        if req.get("id").is_some() {
            let _ = writeln!(out, "{}", resp);
            let _ = out.flush();
        }
    }
}

fn handle(req: &Value, svc: &MemoryService, token: &str) -> Value {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

    match method {
        "initialize" => ok(id, json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "orbital", "version": orbital_core::VERSION }
        })),
        "notifications/initialized" => Value::Null,
        "tools/list" => ok(id, json!({ "tools": tool_defs() })),
        "tools/call" => tools_call(id, req, svc, token),
        _ => err(id, -32601, &format!("unknown method: {method}"), None),
    }
}

fn tools_call(id: Value, req: &Value, svc: &MemoryService, token: &str) -> Value {
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    // 身份：令牌 → VerifiedAgent（缺/错都会在这里被拒）
    let va = match svc.verify(token) {
        Ok(v) => v,
        Err(e) => return tool_error(id, &e),
    };

    match name {
        "memory_write" => {
            // 契约里 `facts` 属于结构化属性通道，M3 才启用；**不许静默忽略**（会让人以为存进去了）
            if let Some(f) = args.get("facts") {
                let non_empty = f.as_array().map(|a| !a.is_empty()).unwrap_or(false)
                    || f.as_object().map(|o| !o.is_empty()).unwrap_or(false);
                if non_empty {
                    return tool_error(
                        id,
                        &OrbitalError::InvalidArgument {
                            reason: "facts（结构化属性）在 M3 启用；当前请用 tags/entities，或等 T-M3 后再传"
                                .into(),
                        },
                    );
                }
            }
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let mut input = WriteInput::new(va.id().to_string(), content);
            if let Some(s) = args.get("scope").and_then(|v| v.as_str()) {
                input.scope = s.to_string();
            }
            if let Some(c) = args.get("category").and_then(|v| v.as_str()) {
                input.category = c.to_string();
            }
            if let Some(i) = args.get("importance").and_then(|v| v.as_i64()) {
                input.importance = i as i32;
            }
            if let Some(t) = args.get("tags").and_then(|v| v.as_array()) {
                input.tags = t.iter().filter_map(|x| x.as_str().map(String::from)).collect();
            }
            // T-M9+ 补：ADR-023 规则 1 要求 scope=shared 必须 origin=user_endorsed，
            // 但原来这个参数没暴露 -> agent 通过 MCP 永远写不了共享记忆（403 SCOPE_FORBIDDEN）。
            if let Some(o) = args.get("origin").and_then(|v| v.as_str()) {
                input.origin = o.to_string();
            }
            if let Some(s) = args.get("source_ref").and_then(|v| v.as_str()) {
                input.source_ref = Some(s.to_string());
            }
            match svc.write(&va, input) {
                Ok(r) => {
                    // ADR-027：写入成功后**异步**更新静态索引层（不阻塞回执、失败不影响写入）
                    if !r.duplicate {
                        spawn_index_sync(va.id().to_string());
                    }
                    tool_ok(id, json!({
                        "id": r.id,
                        "duplicate": r.duplicate,
                        "status": "committed",
                        "write_mode": r.write_mode,
                        "deletion_mode": r.deletion_mode
                    }))
                }
                Err(e) => tool_error(id, &e),
            }
        }
        "memory_recall" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(10);
            // T-M3-03：可选的结构化过滤（按实体/属性）
            let filter = args.get("fact_entity_id").and_then(|v| v.as_str()).map(|eid| {
                orbital_core::service::FactFilter {
                    entity_id: eid.to_string(),
                    property: args.get("fact_property").and_then(|v| v.as_str()).map(String::from),
                }
            });
            if let Some(f) = filter {
                let opts = orbital_core::service::RecallOptions {
                    channels: vec![orbital_core::service::RecallChannel::Fts],
                    weights: vec![1.0],
                    k: 60,
                    fact_filter: Some(f),
                    graph_decay: 0.5,
                };
                match svc.recall_fused(&va, query, limit, &opts) {
                    Ok(r) => tool_ok(id, json!(r)),
                    Err(e) => tool_error(id, &e),
                }
            } else {
                match svc.recall(&va, query, limit) {
                    Ok(hits) => tool_ok(id, json!(hits)),
                    Err(e) => tool_error(id, &e),
                }
            }
        }
        "memory_fact_add" => {
            let fw = orbital_core::writer::FactWrite {
                memory_id: args.get("memory_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                entity_type: args.get("entity_type").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                entity_id: args.get("entity_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                property: args.get("property").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                value: args.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                confidence: args.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.8),
            };
            match svc.add_fact(&va, fw) {
                Ok(r) => tool_ok(id, json!({
                    "memory_id": r.id,
                    "duplicate": r.duplicate,
                    "write_mode": r.write_mode,
                })),
                Err(e) => tool_error(id, &e),
            }
        }
        "memory_get" => {
            let mid = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            match svc.get(&va, mid) {
                Ok(rec) => tool_ok(id, json!(rec)),
                Err(e) => tool_error(id, &e),
            }
        }
        "memory_update" => {
            let mid = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let patch = UpdatePatch {
                content: args.get("content").and_then(|v| v.as_str()).map(String::from),
                importance: args.get("importance").and_then(|v| v.as_i64()),
                tags: args.get("tags").and_then(|v| v.as_array()).map(|a| {
                    a.iter().filter_map(|x| x.as_str().map(String::from)).collect()
                }),
                pinned: args.get("pinned").and_then(|v| v.as_bool()),
                // P1-2：内容与已有条相同时，用 scope 决定"升级"还是"内容已存在"
                scope: args.get("scope").and_then(|v| v.as_str()).map(String::from),
            };
            match svc.update(&va, mid, patch) {
                // 返回四态（docs/04 §10.1 承诺强度）：调用方必须能区分
                // 「真的写进去了」与「内容已存在所以没写」，不能再一律回 updated:true
                Ok(out) => tool_ok(
                    id,
                    json!({
                        "id": out.id,
                        "action": out.action,
                        "note": out.note,
                        "superseded_id": out.superseded_id,
                        "updated": out.action != "duplicate",
                    }),
                ),
                Err(e) => tool_error(id, &e),
            }
        }
        "memory_forget" => {
            let mid = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("soft");
            // 设计（docs/04 §10）：hard（物理删除）必须由调用方显式确认，
            // 否则一次模型误调用就会永久丢数据（ADR-009 可逆遗忘原则）
            if mode == "hard" && args.get("confirm").and_then(|v| v.as_bool()) != Some(true) {
                return tool_error(
                    id,
                    &OrbitalError::InvalidArgument {
                        reason: "hard 删除不可恢复，必须显式传 confirm:true".into(),
                    },
                );
            }
            match svc.forget(&va, mid, mode) {
                // T-M2-07：返回的 event_id 是"撤销句柄"；MCP 不暴露撤销工具（撤销只给本机 GUI），
                // 但仍把它回传，便于调用方在日志里定位这次操作
                Ok(event_id) => tool_ok(id, json!({ "id": mid, "mode": mode, "event_id": event_id })),
                Err(e) => tool_error(id, &e),
            }
        }
        "memory_link" => {
            let src = args.get("source_id").and_then(|v| v.as_str()).unwrap_or("");
            let dst = args.get("target_id").and_then(|v| v.as_str()).unwrap_or("");
            let et = args.get("edge_type").and_then(|v| v.as_str()).unwrap_or("");
            let w = args.get("weight").and_then(|v| v.as_f64()).unwrap_or(1.0);
            match svc.link(&va, src, dst, et, w) {
                Ok(()) => tool_ok(id, json!({ "ok": true })),
                Err(e) => tool_error(id, &e),
            }
        }
        "memory_related" => {
            let mid = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let depth = args.get("depth").and_then(|v| v.as_i64()).unwrap_or(1);
            match svc.related(&va, mid, depth) {
                Ok(hits) => tool_ok(id, json!(hits)),
                Err(e) => tool_error(id, &e),
            }
        }
        "orbital_health" => match svc.health() {
            Ok(h) => tool_ok(id, json!(h)),
            Err(e) => tool_error(id, &e),
        },
        "memory_stats" => match svc.stats(&va) {
            Ok(s) => tool_ok(id, json!(s)),
            Err(e) => tool_error(id, &e),
        },
        other => err(id, -32602, &format!("unknown tool: {other}"), None),
    }
}

fn tool_defs() -> Vec<Value> {
    vec![
        json!({
            "name": "memory_write",
            "description": "写入一条记忆（同 agent 同内容幂等；跨 agent 不合并）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": {"type": "string", "description": "记忆正文（≤64KB UTF-8 字节）"},
                    "scope": {"type": "string", "enum": ["private","shared","entity"]},
                    "category": {"type": "string"},
                    "importance": {"type": "integer", "minimum": 1, "maximum": 5},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "source_ref": {"type": "string"}
                },
                "required": ["content"]
            }
        }),
        json!({
            "name": "memory_recall",
            "description": "按关键词召回（FTS5）。可选 fact_filter 按结构化属性过滤（T-M3-03）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "default": 10},
                    "fact_entity_id": {"type": "string", "description": "只召回登记过该实体事实的记忆，如 project:orbital-pro"},
                    "fact_property": {"type": "string", "description": "配合 fact_entity_id：限定属性名，如 port"}
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "memory_fact_add",
            "description": "给一条记忆登记结构化事实（走写队列；同实体同属性不同值会自动进冲突收件箱）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": {"type": "string"},
                    "entity_type": {"type": "string", "description": "person/project/tool/config/file"},
                    "entity_id": {"type": "string", "description": "稳定标识，如 project:orbital-pro"},
                    "property": {"type": "string", "description": "如 port / install_path"},
                    "value": {"type": "string"},
                    "confidence": {"type": "number", "default": 0.8}
                },
                "required": ["memory_id", "entity_type", "entity_id", "property", "value"]
            }
        }),
        json!({
            "name": "memory_get",
            "description": "按 id 取一条记忆（受作用域门控）",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }
        }),
        json!({
            "name": "memory_update",
            "description": "更新内容/重要性/标签/Pin（仅写入者本人）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "content": {"type": "string"},
                    "importance": {"type": "integer", "minimum": 1, "maximum": 5},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "pinned": {"type": "boolean"}
                },
                "required": ["id"]
            }
        }),
        json!({
            "name": "memory_forget",
            "description": "soft=软删 / invalidate=标记失效（保留历史）/ hard=物理删除",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "mode": {"type": "string", "enum": ["soft","invalidate","hard"], "default": "soft"}
                },
                "required": ["id"]
            }
        }),
        json!({
            "name": "memory_link",
            "description": "建一条关系边（五类：temporal/semantic/causal/entity/supersedes）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source_id": {"type": "string"},
                    "target_id": {"type": "string"},
                    "edge_type": {"type": "string", "enum": ["temporal","semantic","causal","entity","supersedes"]},
                    "weight": {"type": "number", "default": 1.0}
                },
                "required": ["source_id","target_id","edge_type"]
            }
        }),
        json!({
            "name": "memory_related",
            "description": "图遍历取相关记忆（会重新应用作用域门控）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "depth": {"type": "integer", "minimum": 1, "maximum": 2, "default": 1}
                },
                "required": ["id"]
            }
        }),
        json!({
            "name": "orbital_health",
            "description": "体检：schema/缺表/向量缺失/未完成意图；degraded 为 true 时必须看 reasons",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "memory_stats",
            "description": "统计：总数/活跃/软删/边/事实 + 按 agent 与作用域分组（受门控）",
            "inputSchema": { "type": "object", "properties": {} }
        }),
    ]
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: Value, code: i64, msg: &str, data: Option<Value>) -> Value {
    let mut e = json!({ "code": code, "message": msg });
    if let Some(d) = data {
        e["data"] = d;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": e })
}

/// 工具成功：把结果序列化成 text content（MCP 约定）。
fn tool_ok(id: Value, payload: Value) -> Value {
    ok(
        id,
        json!({
            "content": [{ "type": "text", "text": payload.to_string() }],
            "isError": false
        }),
    )
}

/// 工具失败：`isError:true` + 稳定错误码（客户端可编程判定）。
fn tool_error(id: Value, e: &OrbitalError) -> Value {
    ok(
        id,
        json!({
            "content": [{
                "type": "text",
                "text": json!({ "code": e.code(), "httpStatus": e.http_status(), "message": e.to_string() }).to_string()
            }],
            "isError": true
        }),
    )
}

fn default_db_path() -> String {
    match std::env::var("LOCALAPPDATA") {
        Ok(base) => format!("{}\\OrbitalPro\\data\\orbital.db", base),
        Err(_) => ".\\orbital.db".to_string(),
    }
}

/// 让编译器检查未使用的导入（`PathBuf` 仅在类型推断时需要）
#[allow(dead_code)]
fn _unused() -> PathBuf {
    PathBuf::new()
}


// ============================================================================
// 静态索引层同步（ADR-027 决策二/三）
// ============================================================================

const BLOCK_BEGIN: &str = "<!-- orbital:begin";
const BLOCK_END: &str = "<!-- orbital:end -->";

/// 找 WorkBuddy 的个人空间 MEMORY.md（实测生效的那一个，ADR-027）
fn find_workbuddy_memory() -> Option<std::path::PathBuf> {
    let home = std::env::var("USERPROFILE").ok()?;
    let wb = std::path::Path::new(&home).join(".workbuddy");
    let entries = std::fs::read_dir(&wb).ok()?;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("user-") && name.ends_with("-personal") {
            let p = e.path().join("MEMORY.md");
            if p.exists() {
                return Some(p);
            }
        }
    }
    // 兜底：根目录
    let fallback = wb.join("MEMORY.md");
    if fallback.exists() {
        return Some(fallback);
    }
    None
}

// WorkBuddy 用户级 MEMORY.md 的注入硬上限（**源码级**：app.asar 里 `let r=4e3`，即 4,000 字符）。
// 超过就被 `truncateUtf16` 静默截断，后面的内容模型完全看不到。
// ⇒ 我们的区块必须 ① 短 ② 放在**文件开头**（追加到末尾必然被截掉）。
const WB_USER_MEMORY_LIMIT: usize = 4000;
/// 留给 Orbital 区块的字符预算（给用户自己的记忆留 3,700+）
const WB_BLOCK_BUDGET: usize = 300;
/// 区块起始位置必须小于这个值（留 200 字符余量给后续编辑）
const WB_SAFE_HEAD: usize = 2800;

fn render_block(agent: &str, items: &[orbital_core::service::RecentItem]) -> String {
    // T-M10-19（P0）：极简渲染 —— 总长必须 ≤ WB_BLOCK_BUDGET。
    // 理由：4,000 字符是全文件共享的，用户的 MEMORY.md 已有 6,768 字符，
    // 我们多占 1 字符，用户自己的记忆就少 1 字符的可见性。
    let mut s = String::new();
    s.push_str(&format!("{BLOCK_BEGIN} · Orbital 自动维护，勿手编辑 -->\n"));
    s.push_str("记忆库用 MCP：memory_recall / memory_get。最近：\n");
    for it in items.iter().take(3) {
        let title: String = it.title.chars().take(34).collect();
        s.push_str(&format!("- {title}（{}）\n", it.agent_id));
    }
    s.push_str(&format!("{BLOCK_END}\n"));
    if s.chars().count() > WB_BLOCK_BUDGET {
        // 超预算就彻底降级成一行，绝不让区块挤占用户记忆
        return format!(
            "{BLOCK_BEGIN} · Orbital 记忆库用 MCP memory_recall 查（索引已降级） -->\n{BLOCK_END}\n"
        );
    }
    let _ = agent;
    s
}

fn render_block_preview(agent: &str, limit: i64) -> String {
    format!("（索引最多 {limit} 条，agent={agent}）")
}

/// 时间戳（简单实现：用 SystemTime 的秒数，避免引 chrono）
fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("unix:{secs}")
}

/// 把受控区块写入 MEMORY.md（原子写：temp + rename），**不碰区块外的用户内容**。
///
/// T-M10-19（P0）：区块必须落在文件**开头**。原因：WorkBuddy 用户级 MEMORY.md 有 4,000
/// 字符硬上限（源码 `let r=4e3`），追加到末尾等于**必然被截断**——2026-11-14 实测
/// 区块起始位置 4,977 > 4,000，模型根本看不到，但日志仍报 `injection_success`（假绿）。
fn write_block_atomic(path: &std::path::Path, block: &str) -> std::io::Result<()> {
    let original = std::fs::read_to_string(path).unwrap_or_default();

    let existing = match (original.find(BLOCK_BEGIN), original.find(BLOCK_END)) {
        (Some(b), Some(e)) if e > b => Some((b, e)),
        _ => None,
    };

    let new_content = match existing {
        Some((b, e)) if b <= WB_SAFE_HEAD => {
            // 已在安全位置（开头 2,800 字符内）：原位替换，其余字节不动
            format!(
                "{}{}{}",
                &original[..b],
                block,
                &original[e + BLOCK_END.len()..]
            )
        }
        Some((b, e)) => {
            // 在危险位置（如历史遗留的末尾区块）：**搬到开头**，避免被截断
            let rest = format!("{}{}", &original[..b], &original[e + BLOCK_END.len()..]);
            format!("{block}\n{rest}")
        }
        None => {
            // 新写入：插到开头（插末尾必被截断）
            format!("{block}\n{original}")
        }
    };

    // 预算自检：区块起点 + 区块长度必须 < 4,000，否则本轮的注入就是无效的。
    // 这里只在超限时打日志——不因此拒绝写入（拒绝会让"记忆索引"永远停在旧版本）。
    let block_chars = block.chars().count();
    let head_chars = new_content[..new_content.find(BLOCK_BEGIN).unwrap_or(0)]
        .chars()
        .count();
    if head_chars + block_chars >= WB_USER_MEMORY_LIMIT {
        eprintln!(
            "T-M10-19 WARN: Orbital 区块可能被 WorkBuddy 截断（head={head_chars} + block={block_chars} >= {WB_USER_MEMORY_LIMIT}）"
        );
    }

    // 原子写：同目录临时文件 -> rename
    let tmp = path.with_extension("md.orbital-tmp");
    std::fs::write(&tmp, new_content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn run_sync(agent: &str, limit: i64) -> Result<(std::path::PathBuf, usize), String> {
    let db = std::env::var("ORBITAL_DB_PATH")
        .unwrap_or_else(|_| {
            let home = std::env::var("LOCALAPPDATA").unwrap_or_default();
            format!("{home}\\OrbitalPro\\data\\orbital.db")
        });
    let token = std::env::var("ORBITAL_AGENT_TOKEN").map_err(|_| "需要 ORBITAL_AGENT_TOKEN".to_string())?;

    let dir = std::path::Path::new(&db).parent().unwrap_or(std::path::Path::new(".")).to_path_buf();
    std::fs::create_dir_all(&dir).ok();
    let journal = dir.join("journal.ndjson");
    let svc = orbital_core::service::MemoryService::open(&db, &journal).map_err(|e| e.to_string())?;
    let va = svc.verify(&token).map_err(|e| e.to_string())?;
    let items = svc.recent(&va, limit).map_err(|e| e.to_string())?;

    let path = find_workbuddy_memory().ok_or_else(|| "未找到 WorkBuddy 的 MEMORY.md".to_string())?;
    let block = render_block(agent, &items);
    write_block_atomic(&path, &block).map_err(|e| e.to_string())?;
    Ok((path, items.len()))
}


/// 写入后异步刷新静态索引层（ADR-027）。
///
/// **绝不阻塞写入回执**，也**绝不因失败影响写入**——索引只是"让 agent 想起来有记忆"的辅助层。
fn spawn_index_sync(agent_id: String) {
    std::thread::spawn(move || {
        // 稍等，让写入事务落盘
        std::thread::sleep(std::time::Duration::from_millis(300));
        match run_sync(&agent_id, 20) {
            Ok((path, n)) => {
                tracing::info!(path = %path.display(), count = n, "static index synced");
            }
            Err(e) => {
                tracing::warn!(%e, "static index sync failed (不影响写入)");
            }
        }
    });
}


// ============================================================================
// 文件反向导入（ADR-028 路 B）：把各 agent 自己沉淀的记忆采集进 Orbital
// ============================================================================

/// 各 agent 自己的记忆文件（本机实测路径）。
fn agent_memory_paths(agent: &str) -> Vec<std::path::PathBuf> {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let mut v = Vec::new();
    match agent {
        "dsh" | "pi" => {
            v.push(std::path::Path::new(&home).join(".mnemon").join("runtime").join("MEMORY.md"));
        }
        "hermes" => {
            v.push(std::path::Path::new(&local).join("hermes").join("memories").join("MEMORY.md"));
            v.push(std::path::Path::new(&local).join("hermes").join("memories").join("USER.md"));
        }
        "workbuddy" => {
            // 个人空间 MEMORY.md（实测生效的那份），但**要排除我们自己写的 orbital 区块**
            let wb = std::path::Path::new(&home).join(".workbuddy");
            if let Ok(entries) = std::fs::read_dir(&wb) {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name.starts_with("user-") && name.ends_with("-personal") {
                        let p = e.path().join("MEMORY.md");
                        if p.exists() {
                            v.push(p);
                        }
                    }
                }
            }
        }
        _ => {}
    }
    v
}

/// 从记忆文件里切出「条目」。
///
/// 实测本机三种格式（2026-11-12）：
/// - WorkBuddy：`## ` 标题分节（7 个标题）
/// - DSH/mnemon：段落 + `§` 分隔（14 个 `§`）
/// - Hermes：段落 + `§` 分隔（9 个 `§`）
///
/// ⇒ **先按标题切；切不出来就降级按 `§` 切**（一个 § 就是一条）。
/// 跳过 `<!-- orbital:begin/end -->` 区块（那是我们自己写的，不能反向导回形成回环）。
fn parse_entries(text: &str) -> Vec<String> {
    // 先剥掉 orbital 区块（含标记行）
    let mut cleaned = String::new();
    let mut in_orbital = false;
    for line in text.lines() {
        if line.trim_start().starts_with("<!-- orbital:begin") {
            in_orbital = true;
            continue;
        }
        if line.trim_start().starts_with("<!-- orbital:end") {
            in_orbital = false;
            continue;
        }
        if !in_orbital {
            cleaned.push_str(line);
            cleaned.push('\n');
        }
    }

    let by_heading = parse_by_heading(&cleaned);
    if !by_heading.is_empty() {
        return by_heading;
    }
    parse_by_separator(&cleaned, '§')
}

/// 格式 A：`## ` 标题分节，每节 >= 20 字符。
fn parse_by_heading(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Option<(String, Vec<String>)> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if let Some((title, body)) = cur.take() {
                push_if_long(&mut out, &title, &body);
            }
            cur = Some((rest.trim().to_string(), Vec::new()));
        } else if let Some((_, body)) = cur.as_mut() {
            body.push(line.to_string());
        }
    }
    if let Some((title, body)) = cur.take() {
        push_if_long(&mut out, &title, &body);
    }
    out
}

/// 格式 B：`§` 分隔（每条一个段落），每段 >= 20 字符。
fn parse_by_separator(text: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    for seg in text.split(sep) {
        let t = seg.trim();
        if t.chars().count() >= 20 {
            out.push(t.to_string());
        }
    }
    out
}

fn push_if_long(out: &mut Vec<String>, title: &str, body: &[String]) {
    let joined = body.join("\n").trim().to_string();
    if joined.chars().count() >= 20 {
        out.push(format!("{title}\n{joined}"));
    }
}

fn run_import(agent: &str, dry: bool) -> Result<(String, usize, usize), String> {
    let db = std::env::var("ORBITAL_DB_PATH").unwrap_or_else(|_| {
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        format!("{local}\\OrbitalPro\\data\\orbital.db")
    });
    let token = std::env::var("ORBITAL_AGENT_TOKEN").map_err(|_| "需要 ORBITAL_AGENT_TOKEN".to_string())?;
    let dir = std::path::Path::new(&db).parent().unwrap_or(std::path::Path::new(".")).to_path_buf();
    std::fs::create_dir_all(&dir).ok();
    let journal = dir.join("journal.ndjson");

    let svc = orbital_core::service::MemoryService::open(&db, &journal).map_err(|e| e.to_string())?;
    let va = svc.verify(&token).map_err(|e| e.to_string())?;

    let paths = agent_memory_paths(agent);
    if paths.is_empty() {
        return Err(format!("未找到 {agent} 的记忆文件"));
    }

    let mut added = 0usize;
    let mut skipped = 0usize;
    let mut used = String::new();

    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        used = path.display().to_string();
        for entry in parse_entries(&text) {
            // ⚠️ 身份硬门控（ADR-005）：只能用**当前 token 的 agent** 写入。
            // 想导入别的 agent 的记忆，必须用那个 agent 自己的 token 跑 `--import`。
            // 来源 agent 记在 source_ref 里，可追溯。
            let mut input = orbital_core::writer::WriteInput::new(va.id().to_string(), entry.clone());
            input.category = "imported".to_string();
            input.source_ref = Some(format!("import:{agent}:{}", path.display()));
            if dry {
                added += 1;
                continue;
            }
            match svc.write(&va, input) {
                Ok(r) => {
                    if r.duplicate {
                        skipped += 1;
                    } else {
                        added += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!(%e, "import entry failed");
                }
            }
        }
    }
    Ok((used, added, skipped))
}


/// 一键全量维护：对所有已知 agent 跑 import，然后更新静态索引。
///
/// 设计要点（ADR-028）：
/// - **幂等**：import 本身幂等，重复跑不会产生重复记忆；
/// - **失败隔离**：某个 agent 的文件不存在/解析失败，只跳过它，不影响其它；
/// - **不阻塞**：由调用方决定何时跑（CLI 手动 或 定时任务）。
fn run_maintain(dry: bool) -> Result<Vec<String>, String> {
    let agents = ["dsh", "pi", "hermes", "workbuddy"];
    let mut out = Vec::new();
    let mut total_added = 0usize;
    let mut total_skipped = 0usize;

    for a in agents {
        match run_import(a, dry) {
            Ok((_file, added, skipped)) => {
                total_added += added;
                total_skipped += skipped;
                out.push(format!("  {a:<10} +{added} / ~{skipped}"));
            }
            Err(e) => {
                out.push(format!("  {a:<10} SKIP ({e})"));
            }
        }
    }

    // 更新索引（用第一个有 token 的 agent 身份）
    let agent = std::env::var("ORBITAL_MAINTAIN_AS").unwrap_or_else(|_| "dsh".to_string());
    let sync_msg = match run_sync(&agent, 20) {
        Ok((path, n)) => format!("  索引已更新: {n} 条 -> {}", path.display()),
        Err(e) => format!("  索引更新失败: {e}"),
    };

    out.push(String::new());
    out.push(format!("MAINTAIN {}: 新增 {total_added} / 已存在 {total_skipped}", if dry { "(dry-run)" } else { "OK" }));
    out.push(sync_msg);
    Ok(out)
}


/// 卸载接入：按 AdapterSpec 找到配置，删除 orbital 条目。
fn run_detach(agent: &str) -> Result<(std::path::PathBuf, orbital_core::attach::DetachResult), String> {
    let overlays = orbital_core::adapter::overlay_dir();
    let specs = orbital_core::adapter::load_adapters(&overlays)
        .map_err(|e| format!("加载适配器失败: {e}"))?;
    let spec = orbital_core::adapter::find(&specs, agent)
        .ok_or_else(|| format!("未找到适配器: {agent}"))?
        .clone();

    let path = spec.resolved_path();
    if !path.exists() {
        return Err(format!("配置文件不存在: {}", path.display()));
    }

    let r = if spec.format == "yaml" {
        orbital_core::attach::detach_yaml(&path, "mcp-orbital")
    } else {
        orbital_core::attach::detach_json(&path, &spec.mcp_key, orbital_core::attach::SERVER_KEY)
    }
    .map_err(|e| e.to_string())?;

    Ok((path, r))
}
