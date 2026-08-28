//! LoadSkillTool — 按名称加载 skill 的完整指令内容(渐进式披露)。
//!
//! 复用现有的 `SkillCatalog::load_skill_content` 读取机制:catalog 在会话
//! 启动时发现(仅元数据驻留内存),本工具按需从磁盘加载 SKILL.md 正文。
//! 模型不应再用 `read_file` 直接读 skill 文件路径 —— 统一走本工具,
//! 以获得 name→path 解析、信任标记与内容缓存。

use async_trait::async_trait;
use grodex_core::error::GrodexError;
use grodex_core::id::OperationId;
use grodex_core::tool::{ConcurrencyClass, SideEffectClass, ToolMetadata};
use grodex_core::tool::{Tool, ToolRuntime};
use grodex_skills::SkillCatalog;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// Arguments for the LoadSkillTool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadSkillArgs {
    /// Skill 名称(见系统提示中的 Available Skills 清单)。
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadSkillOutput {
    pub name: String,
    pub path: String,
    pub trusted: bool,
    pub content: String,
}

/// Shared, interior-mutable catalog. 与 supervisor 的 prompt 注入共用一份
/// 发现结果时由调用方传入;独立实例亦可(元数据重复开销可忽略)。
pub type SharedSkillCatalog = Arc<Mutex<SkillCatalog>>;

pub struct LoadSkillTool {
    catalog: SharedSkillCatalog,
}

impl Default for LoadSkillTool {
    fn default() -> Self {
        Self::new(Arc::new(Mutex::new(SkillCatalog::default())))
    }
}

impl LoadSkillTool {
    pub fn new(catalog: SharedSkillCatalog) -> Self {
        Self { catalog }
    }
}

impl Tool for LoadSkillTool {
    type Args = LoadSkillArgs;
    type Output = LoadSkillOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: "load_skill".into(),
            display_name: "Load Skill".into(),
            description: "Load a skill's full instructions by name. Use this instead of \
                          read_file when the system prompt lists an available skill that \
                          is relevant to the current task."
                .into(),
            concurrency_class: ConcurrencyClass::Parallel,
            side_effect_class: SideEffectClass::ReadOnly,
            default_policy: grodex_core::policy::PolicyDecision::Allow,
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the skill to load, exactly as listed in the Available Skills table"
                }
            },
            "required": ["name"]
        })
    }

    fn output_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "path": {"type": "string"},
                "trusted": {"type": "boolean"},
                "content": {"type": "string", "description": "Full skill instructions (markdown)"}
            }
        })
    }
}

#[async_trait]
impl ToolRuntime for LoadSkillTool {
    async fn execute(
        &self,
        args: serde_json::Value,
        _operation_id: OperationId,
    ) -> Result<serde_json::Value, GrodexError> {
        let args: LoadSkillArgs =
            serde_json::from_value(args).map_err(|e| GrodexError::ToolExecution(format!("invalid args: {e}")))?;

        let (path, trusted, content) = {
            let mut catalog = self
                .catalog
                .lock()
                .map_err(|_| GrodexError::ToolExecution("skill catalog lock poisoned".into()))?;
            // 找不到时给出可用清单,方便模型自查拼写。
            let available: Vec<String> =
                catalog.list().iter().map(|s| s.name.clone()).collect();
            let content = catalog
                .load_skill_content(&args.name)
                .ok_or_else(|| {
                    GrodexError::ToolExecution(format!(
                        "skill '{}' not found or failed to load. Available skills: [{}]",
                        args.name,
                        available.join(", ")
                    ))
                })?
                .to_string();
            let (path, trusted) = match catalog.find(&args.name) {
                Some(s) => (s.path.to_string_lossy().to_string(), s.trusted),
                None => (String::new(), false),
            };
            (path, trusted, content)
        };

        let output = LoadSkillOutput {
            name: args.name,
            path,
            trusted,
            content,
        };
        // 统一返回 JSON 对象,与 read_file 等工具一致。
        serde_json::to_value(output)
            .map_err(|e| GrodexError::ToolExecution(format!("serialize output: {e}")))
    }
}
