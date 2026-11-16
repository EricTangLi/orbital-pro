//! Agent 适配器（ADR-021）：**用数据描述"怎么接入某个 agent"**，不写代码。
//!
//! 为什么需要它：本产品要交付给其他用户。如果把 4 个 agent 的路径/格式写死在代码里，
//! 换个用户就完全不可用（别人的 Hermes 不在 `%LOCALAPPDATA%`、别人装的 agent 也不在名单里）。
//!
//! 扩展方式（**不需要改代码、不需要重新编译**）：
//!   在 `~/.orbital/adapters/` 放任意个 `*.json`，每个是一个 `AdapterSpec`：
//!   - 与内置同 `id` → **覆盖**内置（比如你的 Hermes 装在别处）
//!   - 新 `id` → **追加**（比如接 Codex / Cursor / 你自己写的 agent）
//!
//! 参考：旧项目 `agent-memory-hub` 的 `agents/adapter.rs`（同一问题已用它解决过）。
//! 本项目差异：spec 里额外要求"幂等判断比较 command 指向"，因为实测 Hermes/DSH 都存在
//! "条目已存在但指向旧版本"的情况，只看键是否存在会静默跳过。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 配置格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    /// 标准 JSON（可直接改）
    Json,
    /// YAML（文本级插入，保住注释）
    Yaml,
}

impl ConfigFormat {
    pub fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case("yaml") || s.eq_ignore_ascii_case("yml") {
            ConfigFormat::Yaml
        } else {
            ConfigFormat::Json
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            ConfigFormat::Json => "json",
            ConfigFormat::Yaml => "yaml",
        }
    }
}

/// 一个 agent 的接入方式描述。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterSpec {
    /// agent 标识（同时是记忆归属的 agent 名）
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    /// 配置文件路径，支持 `%VAR%` 与 `~/`
    pub mcp_config_path: String,
    /// `json`（默认）或 `yaml`
    #[serde(default = "default_format")]
    pub format: String,
    /// MCP server 清单所在键名（json: `mcpServers`；yaml: `mcp` / `mcpServers` / `insert`）
    #[serde(default = "default_mcp_key")]
    pub mcp_key: String,
    /// 是否支持 MCP 接入（false 时只能走 proxy）
    #[serde(default = "default_true")]
    pub supports_mcp: bool,
    #[serde(default)]
    pub supports_proxy: bool,
    /// 接入坑说明（**路径之外的信息才是接入难的地方**）
    #[serde(default)]
    pub note: String,
}

fn default_format() -> String {
    "json".to_string()
}
fn default_mcp_key() -> String {
    "mcpServers".to_string()
}
fn default_true() -> bool {
    true
}

impl AdapterSpec {
    /// 展开 `%VAR%` 与 `~/` 后的真实路径。
    pub fn resolved_path(&self) -> PathBuf {
        expand_env(&self.mcp_config_path)
    }

    pub fn config_format(&self) -> ConfigFormat {
        ConfigFormat::parse(&self.format)
    }
}

/// 展开 Windows 风格 `%VAR%` 与 `~/`。不引入 `dirs` crate（只用两个环境变量）。
pub fn expand_env(raw: &str) -> PathBuf {
    let mut out = raw.to_string();
    let mut i = 0usize;
    while i < out.len() {
        let Some(rel) = out[i..].find('%') else { break };
        let start = i + rel;
        let Some(rel_end) = out[start + 1..].find('%') else {
            break;
        };
        let end = start + 1 + rel_end;
        let name = &out[start + 1..end];
        let val = std::env::var(name).unwrap_or_default();
        out.replace_range(start..=end, &val);
        i = start + val.len();
    }
    if out == "~" || out.starts_with("~/") || out.starts_with("~\\") {
        let home = home_dir();
        let rest = out.trim_start_matches('~').trim_start_matches(['/', '\\']);
        out = if rest.is_empty() {
            home.display().to_string()
        } else {
            home.join(rest).display().to_string()
        };
    }
    PathBuf::from(out)
}

pub fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// 用户自定义适配器的默认目录：`~/.orbital/adapters/`
pub fn overlay_dir() -> PathBuf {
    home_dir().join(".orbital").join("adapters")
}

/// 内置 4 条（开箱可用）。路径一律用环境变量，**不写死本机绝对路径**。
pub fn builtin_adapters() -> Vec<AdapterSpec> {
    vec![
        AdapterSpec {
            id: "hermes".into(),
            display_name: "Hermes".into(),
            // 实测：真实配置在 %LOCALAPPDATA%，~/.hermes/config.yaml 是过时残余
            mcp_config_path: r"%LOCALAPPDATA%\hermes\config.yaml".into(),
            format: "yaml".into(),
            mcp_key: "mcp".into(),
            supports_mcp: true,
            supports_proxy: true,
            note: "YAML；`mcp:` 段下加条目。真实路径在 %LOCALAPPDATA%（~/.hermes 是旧残余）".into(),
        },
        AdapterSpec {
            id: "pi".into(),
            display_name: "pi".into(),
            mcp_config_path: r"%USERPROFILE%\.pi\agent\mcp.json".into(),
            format: "json".into(),
            mcp_key: "mcpServers".into(),
            supports_mcp: true,
            supports_proxy: true,
            note: "JSON；标准 mcpServers。代理模式改 models.json（不要改 models-store.json，那是缓存）".into(),
        },
        AdapterSpec {
            id: "dsh".into(),
            display_name: "DSH".into(),
            mcp_config_path: r"%USERPROFILE%\.dsh\profiles\desktop\cordis.patch.yml".into(),
            format: "yaml".into(),
            mcp_key: "insert".into(),
            supports_mcp: true,
            supports_proxy: true,
            note: "YAML 补丁格式（- insert: 列表）；改完需重启 DSH 才生效".into(),
        },
        AdapterSpec {
            id: "workbuddy".into(),
            display_name: "WorkBuddy（腾讯 CodeBuddy 桌面版）".into(),
            // ⚠️ 实测修正（2026-11-12 二次核实）：**不是** %APPDATA%\WorkBuddy\User\settings.json
            // （那是 VSCode 系设置，只有 codingcopilot.*，无 MCP）。
            // 真正的 MCP 配置在 ~/.workbuddy/mcp.json —— 首版据此错判为"不支持 MCP"，已纠正。
            mcp_config_path: r"%USERPROFILE%\.workbuddy\mcp.json".into(),
            format: "json".into(),
            mcp_key: "mcpServers".into(),
            supports_mcp: true,
            supports_proxy: true,
            note: "JSON（~/.workbuddy/mcp.json）。Electron/VSCode 系，但 MCP 配置独立于 settings.json".into(),
        },
    ]
}

/// 一个 agent 的接入现状（T-M6-04：给 GUI 显示"未接入 / 已接入"）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentConnection {
    pub id: String,
    pub display_name: String,
    /// 展开后的真实配置路径（已把 `%VAR%` / `~/` 换成实际值）
    pub config_path: String,
    pub format: String,
    /// 配置文件是否存在（不存在 = 该 agent 可能没装）
    pub exists: bool,
    /// 配置里是否已有 orbital 的 server 项
    pub attached: bool,
    pub supports_mcp: bool,
    pub note: String,
}

/// 配置文本里是否已有 orbital 的 server 项（JSON/YAML 都算）。
///
/// 抽成独立函数是为了可单测——`detect_connections` 读的是**真实** agent 配置路径，
/// 测试里没法安全注入；判断逻辑本身可以脱离文件测。
pub fn looks_attached(text: &str) -> bool {
    !text.is_empty()
        && (text.contains("\"orbital\"")
            || text.contains("orbital:")
            || text.contains("orbital\n")
            || text.contains("- orbital"))
}

/// 探测所有内置 agent 的接入现状。
///
/// ⚠️ `attached` 用**文本匹配**（JSON 看 `"orbital"`、YAML 看 `orbital` 字样）判断，只作提示：
/// 精确解析要区分 JSON/YAML 两套语法，而这里的目的只是"让用户知道该不该点接入"。
/// 真正写入时 `attach_json`/`attach_yaml` 会做结构化/文本级校验（幂等，已接入会返回 changed=false）。
pub fn detect_connections() -> Vec<AgentConnection> {
    builtin_adapters()
        .into_iter()
        .map(|spec| {
            let path = spec.resolved_path();
            let exists = path.exists();
            let text = if exists {
                std::fs::read_to_string(&path).unwrap_or_default()
            } else {
                String::new()
            };
            let attached = looks_attached(&text);
            AgentConnection {
                id: spec.id.clone(),
                display_name: spec.display_name.clone(),
                config_path: path.display().to_string(),
                format: spec.format.clone(),
                exists,
                attached,
                supports_mcp: spec.supports_mcp,
                note: spec.note.clone(),
            }
        })
        .collect()
}

/// 内置 + 用户覆盖。`overlay` 不存在时只返回内置（不算错误）。
pub fn load_adapters(overlay: &Path) -> crate::error::Result<Vec<AdapterSpec>> {
    let mut list = builtin_adapters();
    if !overlay.is_dir() {
        return Ok(list);
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(overlay)
        .map_err(|e| crate::error::OrbitalError::Internal(format!("读 adapters 目录失败: {e}")))?
        .filter_map(|e| e.ok().map(|x| x.path()))
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    files.sort();

    for f in files {
        let text = std::fs::read_to_string(&f).map_err(|e| {
            crate::error::OrbitalError::InvalidArgument {
                reason: format!("读 {} 失败: {e}", f.display()),
            }
        })?;
        let spec: AdapterSpec = serde_json::from_str(&text).map_err(|e| {
            crate::error::OrbitalError::InvalidArgument {
                reason: format!("{} 不是合法 AdapterSpec: {e}", f.display()),
            }
        })?;
        if spec.id.trim().is_empty() {
            return Err(crate::error::OrbitalError::InvalidArgument {
                reason: format!("{} 缺 id", f.display()),
            });
        }
        match list.iter().position(|a| a.id == spec.id) {
            Some(pos) => list[pos] = spec, // 同名覆盖
            None => list.push(spec),       // 新 id 追加
        }
    }
    Ok(list)
}

pub fn find<'a>(list: &'a [AdapterSpec], id: &str) -> Option<&'a AdapterSpec> {
    list.iter().find(|a| a.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_has_four_and_expands_env() {
        let list = builtin_adapters();
        assert_eq!(list.len(), 4, "内置应为 4 条");
        let hermes = find(&list, "hermes").unwrap();
        let p = hermes.resolved_path();
        assert!(!p.to_string_lossy().contains('%'), "路径必须展开: {p:?}");
        assert!(hermes.config_format() == ConfigFormat::Yaml);
        // workbuddy 实测支持 MCP（配置在 ~/.workbuddy/mcp.json）
        let wb = find(&list, "workbuddy").unwrap();
        assert!(wb.supports_mcp, "workbuddy 支持 MCP");
        assert!(wb.resolved_path().to_string_lossy().contains(".workbuddy"));
    }

    #[test]
    fn user_overlay_overrides_and_appends() {
        let dir = std::env::temp_dir().join(format!("orbital-adapters-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 覆盖内置 hermes
        std::fs::write(
            dir.join("hermes.json"),
            r#"{"id":"hermes","display_name":"My Hermes","mcp_config_path":"D:/my/config.yaml","format":"yaml","mcp_key":"mcp","supports_mcp":true,"supports_proxy":false,"note":"自定义安装位置"}"#,
        )
        .unwrap();
        // 新增一个不存在的 agent
        std::fs::write(
            dir.join("my-agent.json"),
            r#"{"id":"my-agent","display_name":"Mine","mcp_config_path":"%USERPROFILE%/.mine/mcp.json","supports_mcp":true}"#,
        )
        .unwrap();

        let list = load_adapters(&dir).unwrap();
        assert_eq!(list.len(), 5, "4 内置 + 1 新增");
        let h = find(&list, "hermes").unwrap();
        assert_eq!(h.display_name, "My Hermes", "同名应被覆盖");
        assert!(!h.supports_proxy);
        assert!(find(&list, "my-agent").is_some(), "新 id 应追加");
        // 默认值
        let a = find(&list, "my-agent").unwrap();
        assert_eq!(a.format, "json");
        assert_eq!(a.mcp_key, "mcpServers");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_overlay_dir_is_not_an_error() {
        let list = load_adapters(Path::new("Z:/definitely/not/exists")).unwrap();
        assert_eq!(list.len(), 4);
    }

    #[test]
    fn invalid_json_in_overlay_is_reported() {
        let dir = std::env::temp_dir().join(format!("orbital-adapters-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bad.json"), "{not json").unwrap();
        let e = load_adapters(&dir).unwrap_err();
        assert_eq!(e.code(), "INVALID_ARGUMENT");
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// T-M6-04：接入状态判断（文本级）。
#[cfg(test)]
mod connection_tests {
    use super::*;

    #[test]
    fn json_config_detects_attached() {
        let attached = r#"{"mcpServers":{"orbital":{"command":"x.exe"}}}"#;
        assert!(looks_attached(attached));
        let not_attached = r#"{"mcpServers":{"other":{"command":"y.exe"}}}"#;
        assert!(!looks_attached(not_attached));
    }

    #[test]
    fn yaml_config_detects_attached() {
        assert!(looks_attached("mcp:\n  orbital:\n    command: x.exe\n"));
        assert!(looks_attached("- insert:\n  name: orbital\n"));
        assert!(!looks_attached("mcp:\n  other:\n    command: y.exe\n"));
    }

    #[test]
    fn empty_text_is_not_attached() {
        assert!(!looks_attached(""));
        assert!(!looks_attached("   \n"));
    }

    /// 4 条内置 agent 都必须能报出真实路径（不写死本机绝对路径）。
    #[test]
    fn builtin_specs_resolve_to_absolute_paths() {
        let conns = detect_connections();
        assert_eq!(conns.len(), 4, "内置应有 4 条");
        for c in &conns {
            assert!(
                std::path::Path::new(&c.config_path).is_absolute(),
                "{} 的配置路径应已展开成绝对路径: {}",
                c.id,
                c.config_path
            );
            assert!(!c.config_path.contains('%'), "{} 仍有未展开变量", c.id);
        }
    }
}