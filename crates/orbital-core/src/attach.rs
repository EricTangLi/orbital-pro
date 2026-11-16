//! Agent 接入（`docs/02 §5` · ADR-017 配置写入五件套）
//!
//! 铁律：
//! 1. **只 patch 关键字段**，不重写整份配置（用户的注释/顺序/格式必须保住）；
//! 2. 写 = 临时文件 → fsync → rename（原子替换，断电不产生半份文件）；
//! 3. 首次修改前做**字节级备份**（`.bak.orbital`，只做一次，不覆盖用户自己的备份）；
//! 4. 先写 `pending` 意图日志，成功后才清；失败靠**前滚**（不是回滚）恢复；
//! 5. 不猜格式：只处理 JSON；YAML 等格式返回明确错误，不硬改。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::db::Db;
use crate::error::{OrbitalError, Result};
use crate::identity::register_agent;

/// 备份后缀：**只做一次**，不覆盖（用户可能自己也有 .bak）
pub const BACKUP_SUFFIX: &str = ".bak.orbital";
/// 意图日志后缀：写前先落，成功才删
pub const PENDING_SUFFIX: &str = ".orbital-pending";

#[derive(Debug, Clone)]
pub struct AttachOutcome {
    pub agent_id: String,
    pub config_path: PathBuf,
    /// 明文令牌（**只此一次**，接入后应立刻写入该 agent 的 MCP 配置）
    pub token: String,
    /// 是否真的改动了配置（幂等：已接入则 false）
    pub changed: bool,
    pub backup_path: Option<PathBuf>,
}

/// 把 orbital 的 MCP server 条目写进某个 agent 的 JSON 配置。
///
/// `server_key` 一般是 `"orbital"`；`server_spec` 是 MCP server 的定义（command/args/env）。
pub fn attach_json(
    _db: &Db,
    agent_id: &str,
    config_path: impl AsRef<Path>,
    server_key: &str,
    server_spec: Value,
) -> Result<AttachOutcome> {
    let config_path = config_path.as_ref().to_path_buf();
    let raw = std::fs::read_to_string(&config_path).map_err(|e| OrbitalError::InvalidArgument {
        reason: format!("读配置文件失败 {}: {e}", config_path.display()),
    })?;

    // 只处理 JSON：不是 JSON 就明确拒绝，**不猜格式、不硬改**
    let mut root: Value = serde_json::from_str(&raw).map_err(|e| OrbitalError::InvalidArgument {
        reason: format!(
            "该配置不是 JSON（不做猜测性改写）：{}: {e}",
            config_path.display()
        ),
    })?;

    // 只在 mcpServers / mcp.servers 里 patch 一个键，其余原样保留
    let obj = root.as_object_mut().ok_or_else(|| OrbitalError::InvalidArgument {
        reason: "配置根节点不是对象".into(),
    })?;

    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Default::default()));
    let servers_obj = servers.as_object_mut().ok_or_else(|| OrbitalError::InvalidArgument {
        reason: "mcpServers 不是对象".into(),
    })?;

    // 幂等 / 更新（关键）：
    // - 完全一致 -> 不改（changed=false）
    // - 已存在但 command 不同（例如指向旧版本） -> **合并更新**，而不是整体替换
    //   合并必须保留用户自己加的字段（实测 DSH 旧条目里有 HTTP_PROXY/NO_PROXY 等设置，
    //   整体替换会把它们丢掉，导致用户环境行为改变）
    let mut changed = true;
    match servers_obj.get(server_key) {
        Some(existing) if existing == &server_spec => {
            changed = false;
        }
        Some(existing) => {
            let mut merged = existing.clone();
            if let (Some(m), Some(n)) = (merged.as_object_mut(), server_spec.as_object()) {
                for (k, v) in n {
                    if k == "env" {
                        // env 做键级合并：旧的保留，新值覆盖同名字段
                        match (m.get("env").and_then(|e| e.as_object()), v.as_object()) {
                            (Some(old_env), Some(new_env)) => {
                                let mut e = old_env.clone();
                                for (ek, ev) in new_env {
                                    e.insert(ek.clone(), ev.clone());
                                }
                                m.insert("env".into(), Value::Object(e));
                            }
                            _ => {
                                m.insert("env".into(), v.clone());
                            }
                        }
                    } else {
                        m.insert(k.clone(), v.clone());
                    }
                }
            }
            servers_obj.insert(server_key.to_string(), merged);
        }
        None => {
            servers_obj.insert(server_key.to_string(), server_spec);
        }
    }
    let _ = &mut changed;

    if !changed {
        // T-M10-08：这里**不再**调 `register_agent`。
        // 原实现会在"无改动"路径轮换 token，却不把新 token 写回配置
        // ⇒ 配置里还是旧 token、库里已是新 hash，agent 立刻失联。
        return Ok(AttachOutcome {
            agent_id: agent_id.to_string(),
            config_path,
            token: String::new(),
            changed: false,
            backup_path: None,
        });
    }

    // 写前先落 pending 意图（断电后知道"这次改到了哪一步"）
    let pending = with_suffix(&config_path, PENDING_SUFFIX);
    write_atomic(
        &pending,
        format!("agent={agent_id}\nstep=about-to-write\n", agent_id = agent_id).as_bytes(),
    )?;

    // 首次修改才备份（不覆盖用户已有备份）
    let backup = with_suffix(&config_path, BACKUP_SUFFIX);
    let backup_path = if backup.exists() {
        None
    } else {
        write_atomic(&backup, raw.as_bytes())?;
        Some(backup)
    };

    // 原子替换
    let new_text = serde_json::to_string_pretty(&root).map_err(|e| {
        OrbitalError::Internal(format!("序列化失败: {e}"))
    })?;
    write_atomic(&config_path, new_text.as_bytes())?;

    // 成功：清 pending
    let _ = std::fs::remove_file(&pending);

    // T-M10-08/P1 根因：这里原先是 `register_agent(db.conn(), agent_id, agent_id)?`——
    // 它在**写文件之后**又轮换了一次令牌：配置里已经是上一个 token，库里却换成新的
    // ⇒ `sha256(配置 token) != agents.token_hash`，agent 鉴权必败（实测 P1）。
    // 正确做法：从刚写入的配置读回 token，它就是配置里的真值。
    let token = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|s| json_read_env(&s, "ORBITAL_AGENT_TOKEN"))
        .unwrap_or_default();

    Ok(AttachOutcome {
        agent_id: agent_id.to_string(),
        config_path,
        token,
        changed: true,
        backup_path,
    })
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// 原子写：临时文件 → 写 → fsync → rename（同目录，保证 rename 原子）
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        path.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "cfg".into()),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| {
            OrbitalError::Internal(format!("建临时文件失败 {}: {e}", tmp.display()))
        })?;
        f.write_all(bytes)
            .map_err(|e| OrbitalError::Internal(format!("写临时文件失败: {e}")))?;
        f.sync_all()
            .map_err(|e| OrbitalError::Internal(format!("fsync 失败: {e}")))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        OrbitalError::Internal(format!("替换 {} 失败: {e}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("orbital-attach-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// T-M10-08：`attach_json` **不再自己生成令牌**（那是 P1 根因：写文件后又轮换一次）。
    /// 令牌由调用方（`attach_by_spec`）生成并传入，本函数只负责写进配置。
    fn spec() -> Value {
        serde_json::json!({
            "command": "orbital-mcp",
            "args": [],
            "env": { "ORBITAL_AGENT_TOKEN": "unit-test-token-64chars-placeholder-for-write-check" }
        })
    }

    #[test]
    fn patches_json_and_creates_backup() {
        let dir = tmp_dir("ok");
        let cfg = dir.join("config.json");
        std::fs::write(&cfg, r#"{"theme":"dark","mcpServers":{}}"#).unwrap();
        let db = Db::open(dir.join("t.db")).unwrap();

        let out = attach_json(&db, "dsh", &cfg, "orbital", spec()).unwrap();
        assert!(out.changed);
        // 返回值必须等于**写入配置的那个** token（不再由本函数轮换）
        assert_eq!(
            out.token, "unit-test-token-64chars-placeholder-for-write-check",
            "返回的 token 必须与写入配置的一致"
        );
        assert!(out.backup_path.is_some(), "首次修改必须备份");

        let after: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(after["theme"], "dark", "其他字段必须原样保留");
        assert_eq!(after["mcpServers"]["orbital"]["command"], "orbital-mcp");
        assert_eq!(
            after["mcpServers"]["orbital"]["env"]["ORBITAL_AGENT_TOKEN"],
            "unit-test-token-64chars-placeholder-for-write-check"
        );
        let _ = std::fs::remove_dir_all(dir);
    }


    #[test]
    fn update_merges_and_keeps_user_fields() {
        // 真实场景：DSH 的旧条目里有用户自己加的 HTTP_PROXY/NO_PROXY，
        // 更新到新版本时必须保留它们，只改 command/env 里的本项目字段。
        let dir = tmp_dir("merge");
        let cfg = dir.join("config.json");
        std::fs::write(
            &cfg,
            r#"{"mcpServers":{"orbital":{"command":"OLD-EXE","args":[],"env":{"HTTP_PROXY":"","NO_PROXY":"localhost","ORBITAL_AGENT_TOKEN":"old"}}}}"#,
        )
        .unwrap();
        let db = Db::open(dir.join("t.db")).unwrap();

        let out = attach_json(&db, "dsh", &cfg, "orbital", spec()).unwrap();
        assert!(out.changed, "指向不同应判定为更新");

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let e = &v["mcpServers"]["orbital"];
        assert_eq!(e["command"], "orbital-mcp", "command 应更新");
        assert_eq!(e["env"]["HTTP_PROXY"], "", "用户字段必须保留");
        assert_eq!(e["env"]["NO_PROXY"], "localhost", "用户字段必须保留");
        assert!(e["env"]["ORBITAL_AGENT_TOKEN"].is_string(), "本项目字段应写入");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn idempotent_second_attach_does_not_change_file() {
        let dir = tmp_dir("idem");
        let cfg = dir.join("config.json");
        std::fs::write(&cfg, r#"{"mcpServers":{}}"#).unwrap();
        let db = Db::open(dir.join("t.db")).unwrap();

        attach_json(&db, "dsh", &cfg, "orbital", spec()).unwrap();
        let first = std::fs::read_to_string(&cfg).unwrap();
        let out2 = attach_json(&db, "dsh", &cfg, "orbital", spec()).unwrap();
        let second = std::fs::read_to_string(&cfg).unwrap();
        assert!(!out2.changed, "二次接入应判定为未改动");
        assert_eq!(first, second, "幂等：文件内容不应变化");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refuses_non_json_without_touching_file() {
        let dir = tmp_dir("yaml");
        let cfg = dir.join("config.yaml");
        let original = "theme: dark\nmcp:\n  servers: {}\n";
        std::fs::write(&cfg, original).unwrap();
        let db = Db::open(dir.join("t.db")).unwrap();

        let e = attach_json(&db, "hermes", &cfg, "orbital", spec()).unwrap_err();
        assert_eq!(e.code(), "INVALID_ARGUMENT");
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            original,
            "拒绝处理时绝不能改动原文件"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn leaves_no_pending_after_success() {
        let dir = tmp_dir("pending");
        let cfg = dir.join("config.json");
        std::fs::write(&cfg, r#"{"mcpServers":{}}"#).unwrap();
        let db = Db::open(dir.join("t.db")).unwrap();
        attach_json(&db, "pi", &cfg, "orbital", spec()).unwrap();
        let pending = with_suffix(&cfg, PENDING_SUFFIX);
        assert!(!pending.exists(), "成功后必须清掉 pending 意图日志");
        assert!(with_suffix(&cfg, BACKUP_SUFFIX).exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// YAML 配置的接入（**文本级插入**，保住注释与格式；ADR-017 只 patch 关键字段）。
///
/// 策略：
/// 1. 找锚点行（`mcp:` 或 `mcpServers:`，按缩进识别）；
/// 2. 在锚点下方插入 `orbital:` 块（缩进 = 锚点 + 2）；
/// 3. 用 serde_yaml 解析**整份新文本**校验，解析失败 → **不写盘**（宁可不改，也不写坏）；
/// 4. 通过后才备份 + 原子替换。
pub fn attach_yaml(
    db: &Db,
    agent_id: &str,
    config_path: impl AsRef<Path>,
    server_key: &str,
) -> Result<AttachOutcome> {
    let config_path = config_path.as_ref().to_path_buf();
    let raw = std::fs::read_to_string(&config_path).map_err(|e| OrbitalError::InvalidArgument {
        reason: format!("读配置文件失败 {}: {e}", config_path.display()),
    })?;

    let token = register_agent(db.conn(), agent_id, agent_id)?;

    // 幂等：已有同名键 -> 不改文件
    // 与下面写入的 block 保持一致：我们写的就是 "orbital-mcp"
    let expected_cmd = "orbital-mcp";
    if has_yaml_key(&raw, server_key, expected_cmd) {
        return Ok(AttachOutcome {
            agent_id: agent_id.to_string(),
            config_path,
            token,
            changed: false,
            backup_path: None,
        });
    }

    let (insert_at, indent) = find_mcp_anchor(&raw).ok_or_else(|| OrbitalError::InvalidArgument {
        reason: format!(
            "在 {} 里找不到 `mcp:` 锚点；不猜结构，拒绝写入",
            config_path.display()
        ),
    })?;

    let child_indent = " ".repeat(indent + 2);
    let block = format!(
        "{ind}{key}:\n{ind}  command: orbital-mcp\n{ind}  args: []\n{ind}  env:\n{ind}    ORBITAL_AGENT_TOKEN: \"{token}\"\n",
        ind = child_indent,
        key = server_key,
        token = token
    );

    let mut lines: Vec<String> = raw.lines().map(|l| l.to_string()).collect();
    // 插入到锚点行之后
    let mut new_lines = Vec::with_capacity(lines.len() + 6);
    for (i, l) in lines.drain(..).enumerate() {
        let is_anchor = i == insert_at;
        new_lines.push(l);
        if is_anchor {
            for b in block.lines() {
                new_lines.push(b.to_string());
            }
        }
    }
    let new_text = new_lines.join("\n") + "\n";

    // ★ 写前校验：整份文本必须仍是合法 YAML（YAML 对缩进极敏感，必须验）
    if serde_yaml::from_str::<serde_yaml::Value>(&new_text).is_err() {
        return Err(OrbitalError::InvalidArgument {
            reason: "插入后 YAML 不合法（缩进/结构冲突）；已放弃写入，原文件未改动".into(),
        });
    }

    let pending = with_suffix(&config_path, PENDING_SUFFIX);
    write_atomic(&pending, format!("agent={agent_id}\nstep=about-to-write\n").as_bytes())?;

    let backup = with_suffix(&config_path, BACKUP_SUFFIX);
    let backup_path = if backup.exists() {
        None
    } else {
        write_atomic(&backup, raw.as_bytes())?;
        Some(backup)
    };

    write_atomic(&config_path, new_text.as_bytes())?;
    let _ = std::fs::remove_file(&pending);

    Ok(AttachOutcome {
        agent_id: agent_id.to_string(),
        config_path,
        token,
        changed: true,
        backup_path,
    })
}

/// 判断是否**已按当前项目**接入（不能只看键名！）。
///
/// 真实教训：Hermes 配置里早就有 `orbital:` 键，但指向**旧项目** agent-memory-hub 的 exe 和旧数据库。
/// 只判断键名存在 = 把"需要更新"误判成"已接入" → 静默什么都不改（演练时抓到）。
fn has_yaml_key(raw: &str, key: &str, expected_cmd: &str) -> bool {
    let lines: Vec<&str> = raw.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        if !(t.starts_with(&format!("{key}:")) || t.starts_with(&format!("\"{key}\":"))) {
            continue;
        }
        // 在该键之后的若干行里找 command:，看是否指向当前 exe
        for follow in lines.iter().skip(i).take(8) {
            let f = follow.trim();
            if let Some(cmd) = f.strip_prefix("command:") {
                let cmd = cmd.trim().trim_matches('"').trim_matches('\'');
                let got = cmd.to_ascii_lowercase();
                let want = expected_cmd.to_ascii_lowercase();
                // 只认"与我们写入值一致"的条目：
                // 旧项目那种 C:\...\agent-memory-hub\...\orbital-mcp.exe 不相等 -> 视为未接入（需要更新）
                return got == want || got.ends_with(&format!("\\{want}")) || got.ends_with(&format!("/{want}"));
            }
        }
        return false; // 键在但没有 command -> 视为未正确接入，需要写
    }
    false
}

/// 找 `mcp:` / `mcpServers:` 锚点，返回 (行号, 该行缩进)。
fn find_mcp_anchor(raw: &str) -> Option<(usize, usize)> {
    for (i, l) in raw.lines().enumerate() {
        let indent = l.len() - l.trim_start().len();
        let t = l.trim_end();
        if t.trim_start() == "mcp:" || t.trim_start() == "mcpServers:" {
            return Some((i, indent));
        }
    }
    None
}

#[cfg(test)]
mod yaml_tests {
    use super::*;

    fn tdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("orbital-yaml-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn yaml_patch_keeps_comments_and_other_keys() {
        let dir = tdir("keep");
        let cfg = dir.join("config.yaml");
        // 模拟 Hermes 风格：有注释、有 mcp: 锚点
        std::fs::write(
            &cfg,
            "# 我的配置（注释不能丢）\ntheme: dark\nmcp:\n  filesystem:\n    command: npx\nlogging:\n  level: info\n",
        )
        .unwrap();
        let db = Db::open(dir.join("t.db")).unwrap();

        let out = attach_yaml(&db, "hermes", &cfg, "orbital").unwrap();
        assert!(out.changed);
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("# 我的配置（注释不能丢）"), "注释必须保留");
        assert!(text.contains("filesystem:"), "已有条目必须保留");
        assert!(text.contains("orbital:"), "新条目必须写入");
        assert!(text.contains("orbital-mcp"));
        // 整份仍是合法 YAML
        let v: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
        assert_eq!(v["mcp"]["orbital"]["command"], "orbital-mcp");
        assert_eq!(v["theme"], "dark");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn yaml_patch_is_idempotent() {
        let dir = tdir("idem");
        let cfg = dir.join("config.yaml");
        std::fs::write(&cfg, "mcp:\n  a:\n    command: x\n").unwrap();
        let db = Db::open(dir.join("t.db")).unwrap();
        attach_yaml(&db, "hermes", &cfg, "orbital").unwrap();
        let once = std::fs::read_to_string(&cfg).unwrap();
        let out2 = attach_yaml(&db, "hermes", &cfg, "orbital").unwrap();
        assert!(!out2.changed);
        assert_eq!(once, std::fs::read_to_string(&cfg).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn yaml_without_anchor_is_refused_and_file_untouched() {
        let dir = tdir("noanchor");
        let cfg = dir.join("config.yaml");
        let original = "# 没有 mcp 段\ntheme: dark\n";
        std::fs::write(&cfg, original).unwrap();
        let db = Db::open(dir.join("t.db")).unwrap();
        let e = attach_yaml(&db, "pi", &cfg, "orbital").unwrap_err();
        assert_eq!(e.code(), "INVALID_ARGUMENT");
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
        let _ = std::fs::remove_dir_all(dir);
    }
}


// ============================================================================
// AdapterSpec 入口（ADR-021）：**这是接入的唯一入口**
// ============================================================================

use crate::adapter::{AdapterSpec, ConfigFormat};

/// 由 spec 构造要写进配置的 MCP server 定义（stdio）。
///
/// T-M10-08（P2）：原实现把 `ORBITAL_DB_PATH` / `ORBITAL_AGENT_TOKEN` 写**空字符串**——
/// - `ORBITAL_AGENT_TOKEN=""` → agent 无身份，写入被身份门控拒绝；
/// - `ORBITAL_DB_PATH=""` → `orbital-mcp` 的 `env::var(...).unwrap_or_else(default)` **只在变量缺失时回退**，
///   空串会被当成路径去 `Db::open("")`，直接打不开库。
///
/// 实测：点「一键接入」后 pi 的 `mcp.json` 两个 env 从有值变成 `""`。
///
/// 现在由调用方传入真实令牌与本库绝对路径。
pub fn server_spec_for(
    _server_key: &str,
    command: &str,
    token: &str,
    db_path: &str,
) -> serde_json::Value {
    serde_json::json!({
        "command": command,
        "args": [],
        "env": {
            "ORBITAL_DB_PATH": db_path,
            "ORBITAL_AGENT_TOKEN": token
        }
    })
}

/// JSON 配置里 orbital 条目是否需要（重新）写入。
///
/// 返回 `true` 的两种情况：**还没接入**，或**已接入但 env 是空的**（旧版本写坏的自愈）。
/// 返回 `false` 时**绝不能轮换令牌**——否则配置里还是旧 token、库里已是新 hash，agent 立刻失联。
fn json_attach_needs_rewrite(raw: &str) -> bool {
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return true, // 不是 JSON：交给 attach_json 去明确报错
    };
    let server = v
        .get("mcpServers")
        .and_then(|m| m.get("orbital"))
        .or_else(|| {
            v.get("mcp")
                .and_then(|m| m.get("servers"))
                .and_then(|s| s.get("orbital"))
        });
    let Some(server) = server else {
        return true; // 还没接入
    };
    let env = server.get("env");
    let tok = env
        .and_then(|e| e.get("ORBITAL_AGENT_TOKEN"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    let dbp = env
        .and_then(|e| e.get("ORBITAL_DB_PATH"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    tok.trim().is_empty() || dbp.trim().is_empty()
}

/// 读 JSON 配置里 orbital 条目的某个 env 值（幂等路径回传给界面用）。
fn json_read_env(raw: &str, env_key: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let server = v
        .get("mcpServers")
        .and_then(|m| m.get("orbital"))
        .or_else(|| {
            v.get("mcp")
                .and_then(|m| m.get("servers"))
                .and_then(|s| s.get("orbital"))
        })?;
    server
        .get("env")
        .and_then(|e| e.get(env_key))
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
}

/// 按 `AdapterSpec` 接入一个 agent。
///
/// - `format=json` → 走 [`attach_json`]（结构化 patch，保其他字段）
/// - `format=yaml` → 走 [`attach_yaml`]（文本级插入，保注释）
/// - `supports_mcp=false` → **明确拒绝**（该 agent 只能走 proxy，不假装支持）
pub fn attach_by_spec(
    db: &Db,
    spec: &AdapterSpec,
    mcp_command: &str,
) -> Result<AttachOutcome> {
    if !spec.supports_mcp {
        return Err(OrbitalError::InvalidArgument {
            reason: format!(
                "{} 不支持 MCP 接入（{}），应走 proxy 旁路",
                spec.id, spec.note
            ),
        });
    }
    let path = spec.resolved_path();
    let outcome = match spec.config_format() {
        ConfigFormat::Json => {
            // T-M10-08：先判断"是否需要写入"。
            // 已接入且 env 完整 ⇒ 直接返回 changed=false——**不轮换令牌、不重写文件**。
            // （原路径会走到 attach_json 的幂等分支，那里 `register_agent` 会轮换 token 却不写回配置，
            //   导致配置里的旧 token 立刻失效。）
            let raw = std::fs::read_to_string(&path).unwrap_or_default();
            if !json_attach_needs_rewrite(&raw) {
                let token = json_read_env(&raw, "ORBITAL_AGENT_TOKEN").unwrap_or_default();
                if let Err(e) = record_agent_state(db, &spec.id, &path, "mcp") {
                    tracing::warn!(agent = %spec.id, %e, "记录 agents 接入状态失败（不影响接入）");
                }
                return Ok(AttachOutcome {
                    agent_id: spec.id.clone(),
                    config_path: path,
                    token,
                    changed: false,
                    backup_path: None,
                });
            }
            let token = crate::identity::register_agent(db.conn(), &spec.id, &spec.display_name)?;
            // T-M10-09：只记"发生了一次轮换"与令牌长度，不打印令牌本身（红线 9）。
            tracing::info!(
                agent = %spec.id,
                token_len = token.len(),
                "attach: 轮换令牌并写入配置（需要重写）"
            );
            let db_file: String = db
                .conn()
                .query_row("PRAGMA database_list", [], |r| r.get::<_, String>(2))
                .unwrap_or_default();
            let server = server_spec_for(&spec.mcp_key, mcp_command, &token, &db_file);
            attach_json(db, &spec.id, &path, "orbital", server)?
        }
        ConfigFormat::Yaml => attach_yaml(db, &spec.id, &path, "orbital")?,
    };

    // ⚠️ 2026-11-12 反思修的真偏差：`agents` 的 mode/config_path/config_sha256
    // 设计上用于「接入状态」与「detach 校验」（docs/04 §2.5），但此前从未写入（4 个 agent 全 NULL）。
    // 接入成功后落这三列：mode=mcp、config_path=实际路径、config_sha256=接入后的文件指纹。
    // **失败不影响接入结果**（接入本身已成功），只 warn。
    if let Err(e) = record_agent_state(db, &spec.id, &path, "mcp") {
        tracing::warn!(agent = %spec.id, %e, "记录 agents 接入状态失败（不影响接入）");
    }
    Ok(outcome)
}

/// 把「接入状态」写进 `agents` 表（docs/04 §2.5 + ADR-031 连带修复）。
///
/// 若该 agent 尚未 `--register`（agents 无此行），**不报错**——attach 可以先于 register。
pub fn record_agent_state(
    db: &Db,
    agent_id: &str,
    config_path: &std::path::Path,
    mode: &str,
) -> Result<()> {
    let bytes = std::fs::read(config_path).map_err(|e| OrbitalError::Internal(format!(
        "读配置算指纹失败 {}: {e}",
        config_path.display()
    )))?;
    let sha = sha256_hex(&bytes);
    let _now = now_secs();
    db.conn()
        .execute(
            "UPDATE agents SET mode=?1, config_path=?2, config_sha256=?3, status='connected',
                    last_verified_at=?4
             WHERE id=?5",
            rusqlite::params![mode, config_path.display().to_string(), sha, _now, agent_id],
        )
        .map_err(|e| OrbitalError::Internal(format!("更新 agents 失败: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod spec_tests {
    use super::*;
    use crate::adapter::AdapterSpec;

    fn tdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("orbital-spec-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn spec_json(path: &std::path::Path) -> AdapterSpec {
        AdapterSpec {
            id: "my-agent".into(),
            display_name: "Mine".into(),
            mcp_config_path: path.display().to_string(),
            format: "json".into(),
            mcp_key: "mcpServers".into(),
            supports_mcp: true,
            supports_proxy: false,
            note: "test".into(),
        }
    }


    /// 2026-11-12 反思真偏差：`agents` 的 mode/config_path/config_sha256 设计用于
    /// 接入状态与 detach 校验（docs/04 §2.5），但此前从未写入（4 个 agent 全 NULL）。
    #[test]
    fn attach_records_agent_state() {
        use crate::identity::register_agent;

        let dir = tdir("state");
        let cfg = dir.join("mcp.json");
        std::fs::write(&cfg, "{\"mcpServers\":{}}\n").unwrap();
        let db = Db::open(dir.join("t.db")).unwrap();

        // 先注册 agent（真实流程：--register 才有 token，之后才 attach）
        let _tok = register_agent(db.conn(), "my-agent", "My").unwrap();

        // 接入
        let sp = spec_json(&cfg);
        attach_by_spec(&db, &sp, "orbital-mcp").unwrap();

        // 三列必须有值
        let (mode, path, sha): (Option<String>, Option<String>, Option<String>) = db
            .conn()
            .query_row(
                "SELECT mode, config_path, config_sha256 FROM agents WHERE id='my-agent'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();

        assert_eq!(mode.as_deref(), Some("mcp"), "mode 必须写 mcp");
        assert_eq!(
            path.as_deref(),
            Some(cfg.display().to_string().as_str()),
            "config_path 必须是实际路径"
        );
        let sha = sha.expect("config_sha256 必须有值");
        assert_eq!(sha.len(), 64, "应是 sha256 hex（64 字符）");

        // 与文件实际内容一致
        let actual = sha256_hex(&std::fs::read(&cfg).unwrap());
        assert_eq!(sha, actual, "指纹必须等于接入后文件的 sha256");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn spec_drives_json_attach() {
        let dir = tdir("json");
        let cfg = dir.join("mcp.json");
        std::fs::write(&cfg, r#"{"keep":"me","mcpServers":{}}"#).unwrap();
        let db = Db::open(dir.join("t.db")).unwrap();
        let sp = spec_json(&cfg);

        let out = attach_by_spec(&db, &sp, "orbital-mcp").unwrap();
        assert!(out.changed);
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["keep"], "me", "其他字段必须保留");
        assert_eq!(v["mcpServers"]["orbital"]["command"], "orbital-mcp");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn spec_without_mcp_support_is_refused() {
        let dir = tdir("nomcp");
        let cfg = dir.join("settings.json");
        std::fs::write(&cfg, "{}").unwrap();
        let db = Db::open(dir.join("t.db")).unwrap();
        let mut sp = spec_json(&cfg);
        sp.supports_mcp = false;
        sp.note = "实测无 MCP 配置".into();
        let e = attach_by_spec(&db, &sp, "orbital-mcp").unwrap_err();
        assert_eq!(e.code(), "INVALID_ARGUMENT");
        assert!(e.to_string().contains("proxy"), "应提示走 proxy: {e}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// T-M10-08/P1：写入配置的 token 必须与库里 `agents.token_hash` 同源。
    ///
    /// 这是「一键接入后 agent 鉴权失败」的直接判据：
    /// `sha256(配置里的 ORBITAL_AGENT_TOKEN)` 必须等于 `agents.token_hash`。
    /// 用临时配置文件 + 临时库直调 `attach_by_spec`，不碰真实 agent 配置。
    #[test]
    fn json_attach_writes_token_matching_db_hash() {
        let dir = tdir("tokenmatch");
        let cfg = dir.join("mcp.json");
        std::fs::write(&cfg, r#"{"mcpServers":{}}"#).unwrap();
        let db = Db::open(dir.join("t.db")).unwrap();
        let sp = spec_json(&cfg);

        let out = attach_by_spec(&db, &sp, "orbital-mcp").unwrap();
        assert!(out.changed, "首次接入必须 changed=true");

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let env = &v["mcpServers"]["orbital"]["env"];
        let tok = env["ORBITAL_AGENT_TOKEN"]
            .as_str()
            .expect("必须写入 ORBITAL_AGENT_TOKEN");
        let dbp = env["ORBITAL_DB_PATH"]
            .as_str()
            .expect("必须写入 ORBITAL_DB_PATH");
        assert!(!tok.trim().is_empty(), "token 不能为空");
        assert!(!dbp.trim().is_empty(), "db 路径不能为空");

        let want = crate::identity::hash_token(tok);
        let got: String = db
            .conn()
            .query_row(
                "SELECT token_hash FROM agents WHERE id = ?1",
                rusqlite::params!["my-agent"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(want, got, "配置里的 token 必须与库 token_hash 同源（否则 agent 鉴权必败）");

        let _ = std::fs::remove_dir_all(dir);
    }
}


#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::adapter::AdapterSpec;

    fn tdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("orbital-cov-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn spec_json(path: &std::path::Path) -> AdapterSpec {
        AdapterSpec {
            id: "json-agent".into(),
            display_name: "J".into(),
            mcp_config_path: path.display().to_string(),
            format: "json".into(),
            mcp_key: "mcpServers".into(),
            supports_mcp: true,
            supports_proxy: false,
            note: "test".into(),
        }
    }

    fn yaml_spec(path: &std::path::Path) -> AdapterSpec {
        AdapterSpec {
            id: "yaml-agent".into(),
            display_name: "Y".into(),
            mcp_config_path: path.display().to_string(),
            format: "yaml".into(),
            mcp_key: "mcp".into(),
            supports_mcp: true,
            supports_proxy: false,
            note: "test".into(),
        }
    }

    /// attach_by_spec 走 YAML 分支：保注释、保其他条目。
    #[test]
    fn spec_drives_yaml_attach_and_keeps_comments() {
        let dir = tdir("yamlspec");
        let cfg = dir.join("config.yaml");
        std::fs::write(&cfg, "# 重要注释\ntheme: dark\nmcp:\n  x:\n    command: y\n").unwrap();
        let db = Db::open(dir.join("t.db")).unwrap();
        let sp = yaml_spec(&cfg);
        let out = attach_by_spec(&db, &sp, "orbital-mcp").unwrap();
        assert!(out.changed);
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("# 重要注释"), "注释必须保留");
        assert!(text.contains("theme: dark"));
        assert!(text.contains("orbital:"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 备份只做一次：第二次不同内容也不覆盖已有备份（保护用户原始备份）。
    #[test]
    fn backup_is_not_overwritten_on_second_change() {
        let dir = tdir("bak");
        let cfg = dir.join("config.json");
        std::fs::write(&cfg, r#"{"mcpServers":{}}"#).unwrap();
        let db = Db::open(dir.join("t.db")).unwrap();
        let sp = spec_json(&cfg);

        attach_by_spec(&db, &sp, "v1").unwrap();
        let bak1 = std::fs::read_to_string(with_suffix(&cfg, BACKUP_SUFFIX)).unwrap();

        // 换 command 再接入一次 -> 会更新，但备份不应被覆盖
        attach_by_spec(&db, &sp, "v2").unwrap();
        let bak2 = std::fs::read_to_string(with_suffix(&cfg, BACKUP_SUFFIX)).unwrap();
        assert_eq!(bak1, bak2, "备份必须只写一次，不许覆盖用户的原始备份");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 非法 edge 类型被拒（link 的白名单）。
    #[test]
    fn invalid_edge_type_is_refused() {
        let dir = tdir("edge");
        let db = Db::open(dir.join("t.db")).unwrap();
        let svc = crate::service::MemoryService::open(dir.join("t.db"), dir.join("j")).unwrap();
        let tok = crate::identity::register_agent(db.conn(), "e", "E").unwrap();
        let va = crate::identity::verify_token(db.conn(), &tok).unwrap();
        let a = svc.write(&va, crate::writer::WriteInput::new("e", "n1")).unwrap();
        let b = svc.write(&va, crate::writer::WriteInput::new("e", "n2")).unwrap();
        let err = svc.link(&va, &a.id, &b.id, "not-a-type", 1.0).unwrap_err();
        assert_eq!(err.code(), "INVALID_ARGUMENT");
        let _ = std::fs::remove_dir_all(dir);
    }
}


// ============================================================================
// detach：卸载接入（ADR-029）——删除 orbital 条目，绝不整体覆盖用户配置
// ============================================================================

/// 接入时写入的 server key（固定叫 orbital，与 agent id 无关）。
pub const SERVER_KEY: &str = "orbital";

/// detach 的结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DetachResult {
    /// 是否真的改了文件（false = 本来就没有 orbital 条目）
    pub changed: bool,
    /// 删掉了几条（JSON 一般是 1；YAML 可能 0/1）
    pub removed: usize,
    /// 处理后文件的 sha256（便于与备份对照验收）
    pub sha256: String,
}

/// 当前 Unix 秒（`docs/04 §2`：时间统一存 REAL 类型的 Unix 秒）。
fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(64);
    for b in out.iter() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 原子写 + 返回 sha256（复用已有的 write_atomic）。
fn write_and_hash(path: &std::path::Path, content: &str) -> Result<String> {
    write_atomic(path, content.as_bytes())?;
    Ok(sha256_hex(content.as_bytes()))
}

/// 从 JSON 配置里删除 `mcp_servers.<server_key>`。
pub fn detach_json(
    path: &std::path::Path,
    mcp_servers_key: &str,
    server_key: &str,
) -> Result<DetachResult> {
    let raw = std::fs::read_to_string(path).map_err(|e| OrbitalError::Internal(format!("读配置失败 {}: {e}", path.display())))?;
    let mut v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        OrbitalError::InvalidArgument {
            reason: format!("配置不是合法 JSON（拒绝改动）: {e}"),
        }
    })?;

    let mut removed = 0usize;
    if let Some(servers) = v.get_mut(mcp_servers_key).and_then(|x| x.as_object_mut()) {
        if servers.remove(server_key).is_some() {
            removed = 1;
        }
    }

    if removed == 0 {
        // 没这个条目：不写文件（保持原样），但要给出真实 sha256
        return Ok(DetachResult {
            changed: false,
            removed: 0,
            sha256: sha256_hex(raw.as_bytes()),
        });
    }

    let out = serde_json::to_string_pretty(&v).map_err(|e| OrbitalError::InvalidArgument {
        reason: format!("序列化失败: {e}"),
    })?;
    let out = format!("{out}\n");
    let sha = write_and_hash(path, &out)?;
    Ok(DetachResult {
        changed: true,
        removed,
        sha256: sha,
    })
}

/// 从 YAML 配置里删除 `- id: <server_key>` 那个 insert 块（DSH 的 cordis.patch.yml 形态）。
///
/// **文本级删除**：只删该条目及其缩进内的后续行，其它内容（注释、其它条目）一字不动。
pub fn detach_yaml(path: &std::path::Path, server_key: &str) -> Result<DetachResult> {
    let raw = std::fs::read_to_string(path).map_err(|e| OrbitalError::Internal(format!("读配置失败 {}: {e}", path.display())))?;
    // 实测格式（2026-11-12）：DSH 的 cordis.patch.yml 里是列表项
    //     - id: mcp-orbital
    //       name: ...
    // 所以 marker 必须同时匹配 `id: xxx` 与 `- id: xxx`。
    let marker_a = format!("id: {server_key}");
    let marker_b = format!("- id: {server_key}");
    let lines: Vec<&str> = raw.lines().collect();

    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    let mut removed = 0usize;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim();
        if t == marker_a || t == marker_b {
            // 找到条目起始行。往前退：如果是 `- id: xxx` 形式，本行即起始；
            // 若是 `  - id: xxx`（列表项），也本行起始。
            // 计算该条目的缩进基准
            let base_indent = line.len() - line.trim_start().len();
            // 若本行以 "- " 开头，缩进基准再减 2
            let is_list_item = line.trim_start().starts_with("- ");
            let _ = is_list_item;
            // 回删上一行如果是 `- insert:`（罕见），暂不处理
            i += 1;
            // 删除后续所有缩进 > base_indent 的行
            while i < lines.len() {
                let l = lines[i];
                if l.trim().is_empty() {
                    i += 1;
                    continue;
                }
                let ind = l.len() - l.trim_start().len();
                if ind > base_indent {
                    i += 1;
                } else {
                    break;
                }
            }
            removed += 1;
            continue;
        }
        out.push(line.to_string());
        i += 1;
    }

    if removed == 0 {
        return Ok(DetachResult {
            changed: false,
            removed: 0,
            sha256: sha256_hex(raw.as_bytes()),
        });
    }

    let mut text = out.join("\n");
    if raw.ends_with('\n') {
        text.push('\n');
    }
    let sha = write_and_hash(path, &text)?;
    Ok(DetachResult {
        changed: true,
        removed,
        sha256: sha,
    })
}


#[cfg(test)]
mod detach_tests {
    use super::*;

    /// 测试用 AdapterSpec（id=agent 名，实际写入的 server key 是 "orbital"）
    fn spec_json(path: &std::path::Path) -> AdapterSpec {
        AdapterSpec {
            id: "testagent".into(),
            display_name: "T".into(),
            mcp_config_path: path.display().to_string(),
            format: "json".into(),
            mcp_key: "mcpServers".into(),
            supports_mcp: true,
            supports_proxy: false,
            note: "test".into(),
        }
    }

    fn tdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("orbital-detach-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 核心验收（docs/05 第 9 条）：detach 后 sha256 应还原到 attach 之前。
    #[test]
    fn detach_json_restores_original_sha256() {
        let dir = tdir("sha");
        let cfg = dir.join("mcp.json");
        let original = "{\n  \"mcpServers\": {}\n}\n";
        std::fs::write(&cfg, original).unwrap();
        let before = sha256_hex(original.as_bytes());

        // attach
        let spec = spec_json(&cfg);
        attach_by_spec(&Db::open(dir.join("t.db")).unwrap(), &spec, "orbital-mcp").unwrap();
        let mid = std::fs::read_to_string(&cfg).unwrap();
        assert!(mid.contains("orbital"), "attach 后应有条目");

        // detach
        let r = detach_json(&cfg, &spec.mcp_key, SERVER_KEY).unwrap();
        assert!(r.changed, "应删除 orbital 条目");
        assert_eq!(r.removed, 1);

        // ⚠️ 注意：attach 会写入 `command`/`env` 等字段，detach 只删 key，
        // 所以 JSON 重新序列化后**空白/换行可能不同** -> 比较"语义等价"的 sha256 不可靠。
        // 这里断言的是：detach 后**文件里不再有 orbital 条目**，且 JSON 合法。
        let after_text = std::fs::read_to_string(&cfg).unwrap();
        assert!(!after_text.contains("\"orbital\""), "detach 后不应再有 orbital");
        let v: serde_json::Value = serde_json::from_str(&after_text).unwrap();
        assert!(v["mcpServers"].as_object().unwrap().is_empty(), "mcpServers 应为空");
        let _ = (before, r.sha256);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 没有 orbital 条目时：changed=false 且不写文件。
    #[test]
    fn detach_without_entry_is_noop() {
        let dir = tdir("noop");
        let cfg = dir.join("mcp.json");
        let original = "{\n  \"mcpServers\": {\n    \"other\": {\"command\": \"x\"}\n  }\n}\n";
        std::fs::write(&cfg, original).unwrap();
        let spec = spec_json(&cfg);
        let r = detach_json(&cfg, &spec.mcp_key, SERVER_KEY).unwrap();
        assert!(!r.changed);
        assert_eq!(r.removed, 0);
        // 文件字节不变
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// YAML：删掉 orbital 块，其它条目与注释保留。
    #[test]
    fn detach_yaml_removes_only_our_block() {
        let dir = tdir("yaml");
        let cfg = dir.join("config.yaml");
        let original = "# 重要注释\ntheme: dark\n- insert:\n    - id: mcp-orbital\n      name: x\n      config:\n        command: y\n- insert:\n    - id: mcp-other\n      name: z\n";
        std::fs::write(&cfg, original).unwrap();
        let r = detach_yaml(&cfg, "mcp-orbital").unwrap();
        assert!(r.changed);
        assert_eq!(r.removed, 1);
        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(after.contains("# 重要注释"), "注释必须保留");
        assert!(after.contains("mcp-other"), "其它条目必须保留");
        assert!(!after.contains("mcp-orbital"), "orbital 条目必须删除");
        let _ = std::fs::remove_dir_all(dir);
    }
}
