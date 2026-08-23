//! Embedding 生成后端与向量存储相关类型。
//!
//! Design 08 §12: 向量路径默认关闭，只有显式配置 `[memory.embedding]`
//! `enabled = true` 且 API key 环境变量存在才启用，任何失败 Fail-Open 降级纯 FTS。
//! 所有模型参数（endpoint / model / 维度 / 批量 / 回填上限）全部由配置文件驱动，
//! 调用点不硬编码任何模型参数。

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use thiserror::Error;

pub type EmbeddingVector = Vec<f32>;

/// Embedding 生成错误。
#[derive(Debug, Error)]
pub enum EmbeddingError {
    #[error("embedding not configured (enabled=false or missing api_key env var)")]
    NotConfigured,

    #[error("http request failed: {0}")]
    Http(String),

    #[error("invalid embedding dimension: got {got}, expected {expected}")]
    InvalidDimension { got: usize, expected: usize },

    #[error("failed to deserialize response: {0}")]
    Deserialize(String),

    #[error("api error code={code}: {msg}")]
    Api { code: String, msg: String },
}

/// Embedding 配置（从 TOML `[memory.embedding]` 小节反序列化）。
///
/// 全部字段都有配置层默认值；调用点用 `Default`/`try_into` 解析，
/// 不允许在接线代码里硬编码模型参数。
#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddingConfig {
    /// 向量路径总开关；默认关闭 = 纯 FTS5。
    #[serde(default)]
    pub enabled: bool,

    #[serde(default = "default_endpoint")]
    pub endpoint: String,

    #[serde(default = "default_model")]
    pub model: String,

    /// API key 所在环境变量名；空则回落 GRODEX_OPENAI_API_KEY。
    #[serde(default)]
    pub api_key_env_var: String,

    #[serde(default = "default_dim")]
    pub expected_dimension: usize,

    /// 单次 embed_texts 调用的最大文本数。
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,

    /// 启动时增量回填的最大文档数（防止大库阻塞启动）。
    #[serde(default = "default_backfill_max")]
    pub backfill_max_documents: usize,
}

fn default_endpoint() -> String {
    "https://api.openai.com/v1/embeddings".to_string()
}

fn default_model() -> String {
    "text-embedding-3-small".to_string()
}

fn default_dim() -> usize {
    1536
}

fn default_batch_size() -> usize {
    64
}

fn default_backfill_max() -> usize {
    200
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: default_endpoint(),
            model: default_model(),
            api_key_env_var: String::new(),
            expected_dimension: default_dim(),
            batch_size: default_batch_size(),
            backfill_max_documents: default_backfill_max(),
        }
    }
}

/// Embedding 生成后端 trait（可插拔：OpenAI 兼容 / 本地 ONNX / 以后换 Custom）。
#[async_trait]
pub trait EmbeddingModel: Send + Sync {
    async fn embed_texts(&self, texts: &[String]) -> Result<Vec<EmbeddingVector>, EmbeddingError>;
    fn dimension(&self) -> usize;
    fn model_id(&self) -> &str;
    /// 单次批量请求的文本数上限（回填器按此切批）。
    fn batch_size(&self) -> usize {
        default_batch_size()
    }
}

/// OpenAI 兼容 Embedding 实现（HTTP 调 endpoint）。
#[derive(Debug)]
pub struct OpenAiCompatibleModel {
    client: reqwest::Client,
    cfg: EmbeddingConfig,
    api_key: String,
}

impl OpenAiCompatibleModel {
    pub fn new(cfg: EmbeddingConfig) -> Result<Self, EmbeddingError> {
        if !cfg.enabled {
            return Err(EmbeddingError::NotConfigured);
        }
        let env_var = if cfg.api_key_env_var.is_empty() {
            "GRODEX_OPENAI_API_KEY".to_string()
        } else {
            cfg.api_key_env_var.clone()
        };
        let api_key = std::env::var(&env_var).map_err(|_| EmbeddingError::NotConfigured)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| EmbeddingError::Http(e.to_string()))?;
        Ok(Self {
            client,
            cfg,
            api_key,
        })
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingDataItem {
    embedding: Vec<f32>,
    index: usize,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Option<Vec<OpenAiEmbeddingDataItem>>,
    error: Option<OpenAiApiError>,
}

#[derive(Debug, Deserialize)]
struct OpenAiApiError {
    code: Option<serde_json::Value>,
    message: Option<String>,
}

#[async_trait]
impl EmbeddingModel for OpenAiCompatibleModel {
    async fn embed_texts(&self, texts: &[String]) -> Result<Vec<EmbeddingVector>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let body = serde_json::json!({
            "model": self.cfg.model,
            "input": texts,
            "encoding_format": "float",
        });

        let resp = self
            .client
            .post(&self.cfg.endpoint)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| EmbeddingError::Http(e.to_string()))?;

        let status = resp.status();
        let resp_text = resp
            .text()
            .await
            .map_err(|e| EmbeddingError::Http(e.to_string()))?;

        let parsed: OpenAiEmbeddingResponse = serde_json::from_str(&resp_text)
            .map_err(|e| EmbeddingError::Deserialize(format!("{e}: {resp_text}")))?;

        if !status.is_success() {
            if let Some(err) = parsed.error {
                let code = err
                    .code
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| status.to_string());
                let msg = err.message.unwrap_or_else(|| resp_text.clone());
                return Err(EmbeddingError::Api { code, msg });
            }
            return Err(EmbeddingError::Api {
                code: status.to_string(),
                msg: resp_text,
            });
        }

        let mut data = parsed
            .data
            .ok_or_else(|| EmbeddingError::Deserialize("missing 'data' field".to_string()))?;

        data.sort_by_key(|d| d.index);

        let mut result = Vec::with_capacity(data.len());
        for item in data {
            let dim = item.embedding.len();
            if dim != self.cfg.expected_dimension {
                return Err(EmbeddingError::InvalidDimension {
                    got: dim,
                    expected: self.cfg.expected_dimension,
                });
            }
            result.push(item.embedding);
        }

        Ok(result)
    }

    fn dimension(&self) -> usize {
        self.cfg.expected_dimension
    }

    fn model_id(&self) -> &str {
        &self.cfg.model
    }

    fn batch_size(&self) -> usize {
        self.cfg.batch_size
    }
}

/// 余弦相似度计算。
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..n {
        let ai = a[i];
        let bi = b[i];
        dot += ai * bi;
        na += ai * ai;
        nb += bi * bi;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom < f32::EPSILON {
        0.0
    } else {
        dot / denom
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identical_is_one() {
        let v = vec![1.0f32, 2.0, 3.0, 4.0];
        let s = cosine_similarity(&v, &v);
        assert!((s - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        let a = vec![1.0f32, 0.0];
        let b = vec![0.0f32, 1.0];
        let s = cosine_similarity(&a, &b);
        assert!(s.abs() < 1e-5);
    }

    #[test]
    fn cosine_opposite_is_neg_one() {
        let a = vec![1.0f32, 0.0];
        let b = vec![-1.0f32, 0.0];
        let s = cosine_similarity(&a, &b);
        assert!((s + 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_empty_returns_zero() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn not_configured_when_disabled() {
        let cfg = EmbeddingConfig::default();
        let err = OpenAiCompatibleModel::new(cfg).unwrap_err();
        assert!(matches!(err, EmbeddingError::NotConfigured));
    }

    #[test]
    fn config_defaults() {
        let cfg = EmbeddingConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.endpoint, "https://api.openai.com/v1/embeddings");
        assert_eq!(cfg.model, "text-embedding-3-small");
        assert_eq!(cfg.expected_dimension, 1536);
        assert_eq!(cfg.batch_size, 64);
        assert_eq!(cfg.backfill_max_documents, 200);
    }

    /// `[memory.embedding]` 小节部分配置：未给的字段落配置层默认值。
    #[test]
    fn partial_config_falls_back_to_defaults() {
        let json = r#"{"enabled": true, "model": "bge-m3", "expected_dimension": 1024}"#;
        let cfg: EmbeddingConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.model, "bge-m3");
        assert_eq!(cfg.expected_dimension, 1024);
        // 未配置字段 → 配置层默认值，而非调用点硬编码。
        assert_eq!(cfg.endpoint, "https://api.openai.com/v1/embeddings");
        assert_eq!(cfg.batch_size, 64);
        assert_eq!(cfg.backfill_max_documents, 200);
    }

    /// 空表 = 全默认 = 关闭状态。
    #[test]
    fn empty_table_is_disabled_default() {
        let cfg: EmbeddingConfig = serde_json::from_str("{}").unwrap();
        assert!(!cfg.enabled);
        assert!(matches!(
            OpenAiCompatibleModel::new(cfg).unwrap_err(),
            EmbeddingError::NotConfigured
        ));
    }

    /// TOML `[memory.embedding]` 小节 → EmbeddingConfig（与 config.example.toml 同构）。
    #[test]
    fn toml_section_round_trip() {
        let toml_str = r#"
            enabled = true
            endpoint = "http://127.0.0.1:11434/v1/embeddings"
            model = "bge-m3"
            api_key_env_var = "MY_KEY"
            expected_dimension = 1024
            batch_size = 16
            backfill_max_documents = 50
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let cfg: EmbeddingConfig = value.try_into().unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.endpoint, "http://127.0.0.1:11434/v1/embeddings");
        assert_eq!(cfg.model, "bge-m3");
        assert_eq!(cfg.api_key_env_var, "MY_KEY");
        assert_eq!(cfg.expected_dimension, 1024);
        assert_eq!(cfg.batch_size, 16);
        assert_eq!(cfg.backfill_max_documents, 50);

        // 只给开关，其余落默认值。
        let minimal: toml::Value = toml::from_str("enabled = true").unwrap();
        let cfg: EmbeddingConfig = minimal.try_into().unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.model, "text-embedding-3-small");
    }
}
