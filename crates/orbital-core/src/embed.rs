//! 嵌入（ADR-022 / 沿用旧项目 ADR-014）：**一个模型规格 + N 个端点**。
//!
//! 为什么不是"主模型 + 备模型"：主备是可用性冗余，不是"同时用两个模型"。
//! 允许配两个模型就必须回答"它们兼不兼容"，而这件事**无法靠名字判定**
//! （实测反例：云端 `Qwen/Qwen3-Embedding-0.6B` 与本地 `qwen3-embedding:0.6b` 是同一模型，名字完全不同）。
//! 把模型规格收敛成一份，兼容性问题从数据结构上就不存在；端点只负责"到哪儿算"，可以有任意多个。
//!
//! ⚠️ **换模型 = 换向量空间**：`ModelSpec.id` 或 `dimensions` 一变，已有向量全部作废，必须重建索引。
//! 所以 `EmbeddingConfig::check_dimension()` 会在写入前校验，不匹配直接拒绝。

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use crate::error::{OrbitalError, Result};

/// 一致性放行阈值：同模型不同端点的余弦相似度必须 ≥ 0.99（旧项目实测值）。
pub const BASELINE_PASS: f32 = 0.99;

/// 探测句（用于验证"不同端点算出来的向量是否同一个空间"）。
pub const PROBES: [&str; 5] = [
    "忆核 Orbital 是一个多 agent 记忆中枢",
    "The quick brown fox jumps over the lazy dog",
    "版本 2.4.1 于 2026-11-10 发布，共 1,024 条记录",
    "用户偏好浅色主题与精致卡片",
    "用户喜欢浅色主题和精致的卡片设计",
];

/// 端点的连接方式（决定请求格式）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    /// OpenAI 兼容：`POST {base}/embeddings`
    Cloud,
    /// Ollama：`POST {base}/api/embed`
    Local,
}

impl EndpointKind {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "cloud" | "remote" | "online" => Some(Self::Cloud),
            "local" | "ollama" | "offline" => Some(Self::Local),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cloud => "cloud",
            Self::Local => "local",
        }
    }
}

/// 模型规格（**全局唯一一份**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    pub id: String,
    pub name: String,
    /// 向量维度。改它 = 换向量空间。
    pub dimensions: usize,
}

impl Default for ModelSpec {
    fn default() -> Self {
        Self {
            id: "qwen3-embedding-0.6b".into(),
            name: "Qwen3-Embedding-0.6B".into(),
            dimensions: 1024,
        }
    }
}

/// 一个嵌入端点。
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub id: String,
    pub label: String,
    pub kind: EndpointKind,
    pub base_url: String,
    pub model_name: String,
    pub api_key: String,
    pub priority: u32,
    pub timeout_ms: u64,
    pub enabled: bool,
}

impl Endpoint {
    fn url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        match self.kind {
            EndpointKind::Cloud => format!("{base}/embeddings"),
            EndpointKind::Local => format!("{base}/api/embed"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EmbeddingConfig {
    pub model: ModelSpec,
    pub endpoints: Vec<Endpoint>,
    /// 云端端点是否允许发送内容（隐私开关）
    pub allow_remote_content: bool,
    pub batch_size: usize,
}

impl EmbeddingConfig {
    /// 按 priority 排序的可用端点。
    pub fn active_endpoints(&self) -> Vec<&Endpoint> {
        let mut v: Vec<&Endpoint> = self
            .endpoints
            .iter()
            .filter(|e| e.enabled)
            .filter(|e| self.allow_remote_content || e.kind == EndpointKind::Local)
            .collect();
        v.sort_by_key(|e| e.priority);
        v
    }

    /// 写入前校验维度（换模型必须重建索引，不能混写）。
    pub fn check_dimension(&self, actual: usize) -> Result<()> {
        if actual != self.model.dimensions {
            return Err(OrbitalError::InvalidArgument {
                reason: format!(
                    "向量维度不符：返回 {actual}，期望 {}（模型 {}）。换模型必须重建向量索引",
                    self.model.dimensions, self.model.id
                ),
            });
        }
        Ok(())
    }
}

// ---------------- 配置加载：环境变量 > secrets.json > 内置默认 ----------------

#[derive(Debug, Deserialize)]
struct SecretsFile {
    #[serde(default)]
    embedding: Option<SecretsEmbedding>,
}

#[derive(Debug, Deserialize)]
struct SecretsEmbedding {
    #[serde(default)]
    model: Option<SecretsModel>,
    #[serde(default)]
    dimensions: Option<usize>,
    #[serde(default, rename = "allowRemoteContent")]
    allow_remote_content: Option<bool>,
    #[serde(default, rename = "batchSize")]
    batch_size: Option<usize>,
    #[serde(default)]
    endpoints: Option<Vec<SecretsEndpoint>>,
}

#[derive(Debug, Deserialize)]
struct SecretsModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SecretsEndpoint {
    id: String,
    #[serde(default)]
    label: Option<String>,
    kind: String,
    #[serde(rename = "baseUrl")]
    base_url: String,
    #[serde(default, rename = "modelName")]
    model_name: Option<String>,
    #[serde(default, rename = "apiKey")]
    api_key: Option<String>,
    #[serde(default)]
    priority: Option<u32>,
    #[serde(default, rename = "timeoutMs")]
    timeout_ms: Option<u64>,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

/// secrets 文件默认位置：`%USERPROFILE%\.orbital\secrets.json`（仓库外，不进 git）
pub fn secrets_path() -> PathBuf {
    crate::adapter::home_dir().join(".orbital").join("secrets.json")
}

/// 读取配置。文件不存在或字段缺失都用默认（**不阻塞启动**）。
pub fn load_config() -> EmbeddingConfig {
    let mut cfg = EmbeddingConfig {
        model: ModelSpec::default(),
        endpoints: Vec::new(),
        allow_remote_content: true,
        batch_size: 32,
    };

    let path = std::env::var("ORBITAL_SECRETS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| secrets_path());

    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(file) = serde_json::from_str::<SecretsFile>(&text) {
            if let Some(e) = file.embedding {
                if let Some(m) = e.model {
                    cfg.model = ModelSpec {
                        id: m.id,
                        name: m.name.unwrap_or_else(|| cfg.model.name.clone()),
                        dimensions: e.dimensions.unwrap_or(cfg.model.dimensions),
                    };
                }
                if let Some(d) = e.dimensions {
                    cfg.model.dimensions = d;
                }
                if let Some(a) = e.allow_remote_content {
                    cfg.allow_remote_content = a;
                }
                if let Some(b) = e.batch_size {
                    cfg.batch_size = b;
                }
                for ep in e.endpoints.unwrap_or_default() {
                    let Some(kind) = EndpointKind::parse(&ep.kind) else {
                        continue; // 未知 kind 跳过（不猜）
                    };
                    cfg.endpoints.push(Endpoint {
                        id: ep.id.clone(),
                        label: ep.label.unwrap_or_else(|| ep.id.clone()),
                        kind,
                        base_url: ep.base_url,
                        model_name: ep
                            .model_name
                            .unwrap_or_else(|| "qwen3-embedding:0.6b".into()),
                        api_key: ep.api_key.unwrap_or_default(),
                        priority: ep.priority.unwrap_or(100),
                        timeout_ms: ep.timeout_ms.unwrap_or(5000),
                        enabled: ep.enabled,
                    });
                }
            }
        }
    }

    // 环境变量覆盖（允许不写文件就临时指定）
    if let Ok(v) = std::env::var("ORBITAL_EMBED_DIMENSIONS") {
        if let Ok(d) = v.parse() {
            cfg.model.dimensions = d;
        }
    }
    cfg
}

// ---------------- 调用 ----------------

/// 对一批文本算向量。云端用 `/embeddings`，本地 Ollama 用 `/api/embed`。
pub fn embed(
    ep: &Endpoint,
    texts: &[String],
    cfg: &EmbeddingConfig,
) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    // ⚠️ 本机实测：HTTP_PROXY 指向 127.0.0.1:10808 实为 SOCKS5，reqwest 当 HTTP 代理用会**静默失败**
    // （请求 2s/5s 超时，没有任何可用错误信息）。嵌入端点必须直连：
    //   - 本地 Ollama 127.0.0.1 本就不该走代理
    //   - 云端 API 在本机实测直连可通（401 才是没带 key）
    // 需要代理的用户可通过 ORBITAL_EMBED_USE_PROXY=1 开启。
    let use_proxy = std::env::var("ORBITAL_EMBED_USE_PROXY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(ep.timeout_ms));
    if !use_proxy {
        builder = builder.no_proxy();
    }
    let client = builder
        .build()
        .map_err(|e| OrbitalError::Internal(format!("建 HTTP 客户端失败: {e}")))?;

    let url = ep.url();
    let (body, is_cloud) = match ep.kind {
        EndpointKind::Cloud => (
            serde_json::json!({
                "model": ep.model_name,
                "input": texts,
                "encoding_format": "float",
            }),
            true,
        ),
        EndpointKind::Local => (
            serde_json::json!({
                "model": ep.model_name,
                "input": texts,
            }),
            false,
        ),
    };

    let mut req = client.post(&url).json(&body);
    if is_cloud && !ep.api_key.is_empty() {
        req = req.bearer_auth(&ep.api_key);
    }

    let resp = req.send().map_err(|e| {
        OrbitalError::EmbeddingUnavailable {
            reason: format!("端点 {} 请求失败: {e}", ep.id),
        }
    })?;

    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(OrbitalError::EmbeddingUnavailable {
            reason: format!("端点 {} 返回 {status}: {}", ep.id, truncate(&text, 200)),
        });
    }

    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        OrbitalError::EmbeddingUnavailable {
            reason: format!("端点 {} 响应不是 JSON: {e}", ep.id),
        }
    })?;

    let arr = v
        .get("embeddings")
        .or_else(|| v.get("data"))
        .and_then(|x| x.as_array())
        .ok_or_else(|| OrbitalError::EmbeddingUnavailable {
            reason: format!("端点 {} 响应缺 embeddings/data", ep.id),
        })?;

    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let vec = if item.is_array() {
            item.as_array()
        } else {
            item.get("embedding").and_then(|x| x.as_array())
        }
        .ok_or_else(|| OrbitalError::EmbeddingUnavailable {
            reason: format!("端点 {} 单条向量格式未知", ep.id),
        })?;
        let floats: Vec<f32> = vec
            .iter()
            .filter_map(|x| x.as_f64().map(|f| f as f32))
            .collect();
        cfg.check_dimension(floats.len())?;
        out.push(floats);
    }
    Ok(out)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

/// 余弦相似度（两端都已归一化时等价于点积，但这里显式算）。
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// 单个端点的验证结果。
#[derive(Debug, Clone)]
pub struct EndpointCheck {
    pub endpoint_id: String,
    pub ok: bool,
    pub dimensions: usize,
    pub elapsed_ms: u128,
    pub reason: String,
}

/// 端点一致性验证：所有可用端点对同一组探测句算向量，
/// 与**第一个端点**逐条比余弦，全部 ≥ [`BASELINE_PASS`] 才算同一向量空间。
pub fn verify_endpoints(cfg: &EmbeddingConfig) -> Vec<EndpointCheck> {
    let probes: Vec<String> = PROBES.iter().map(|s| s.to_string()).collect();
    let active = cfg.active_endpoints();
    let mut out = Vec::new();
    let mut baseline: Option<Vec<Vec<f32>>> = None;

    for ep in active {
        let t0 = std::time::Instant::now();
        match embed(ep, &probes, cfg) {
            Ok(vecs) => {
                let dim = vecs.first().map(|v| v.len()).unwrap_or(0);
                let elapsed = t0.elapsed().as_millis();
                match &baseline {
                    None => {
                        baseline = Some(vecs);
                        out.push(EndpointCheck {
                            endpoint_id: ep.id.clone(),
                            ok: true,
                            dimensions: dim,
                            elapsed_ms: elapsed,
                            reason: "基准端点".into(),
                        });
                    }
                    Some(base) => {
                        if base.len() != vecs.len() {
                            out.push(EndpointCheck {
                                endpoint_id: ep.id.clone(),
                                ok: false,
                                dimensions: dim,
                                elapsed_ms: elapsed,
                                reason: "向量条数不一致".into(),
                            });
                            continue;
                        }
                        let mut min_sim = 1.0f32;
                        for (a, b) in base.iter().zip(vecs.iter()) {
                            let s = cosine(a, b);
                            if s < min_sim {
                                min_sim = s;
                            }
                        }
                        out.push(EndpointCheck {
                            endpoint_id: ep.id.clone(),
                            ok: min_sim >= BASELINE_PASS,
                            dimensions: dim,
                            elapsed_ms: elapsed,
                            reason: format!(
                                "最小余弦 {min_sim:.4}（阈值 {BASELINE_PASS}）{}",
                                if min_sim >= BASELINE_PASS {
                                    ""
                                } else {
                                    " -> 不是同一向量空间，禁止混用"
                                }
                            ),
                        });
                    }
                }
            }
            Err(e) => out.push(EndpointCheck {
                endpoint_id: ep.id.clone(),
                ok: false,
                dimensions: 0,
                elapsed_ms: t0.elapsed().as_millis(),
                reason: format!("{e}"),
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_1024_and_single_model_spec() {
        let m = ModelSpec::default();
        assert_eq!(m.dimensions, 1024);
        assert_eq!(m.id, "qwen3-embedding-0.6b");
    }

    #[test]
    fn dimension_mismatch_is_rejected() {
        let cfg = EmbeddingConfig {
            model: ModelSpec::default(),
            endpoints: vec![],
            allow_remote_content: true,
            batch_size: 32,
        };
        let e = cfg.check_dimension(768).unwrap_err();
        assert_eq!(e.code(), "INVALID_ARGUMENT");
        assert!(e.to_string().contains("768"));
        assert!(cfg.check_dimension(1024).is_ok());
    }

    #[test]
    fn cosine_sanity() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![1.0f32, 0.0, 0.0];
        let c = vec![0.0f32, 1.0, 0.0];
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-6);
        assert!(cosine(&a, &c).abs() < 1e-6);
        assert_eq!(cosine(&a, &[1.0f32, 0.0]), 0.0, "维度不同返回 0");
    }

    #[test]
    fn load_config_does_not_panic_without_file() {
        let cfg = load_config();
        // 没有文件时用默认；有文件时读到端点
        assert_eq!(cfg.model.dimensions, 1024);
    }
}
