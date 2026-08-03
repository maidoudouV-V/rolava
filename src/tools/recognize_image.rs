use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{execution_not_implemented, parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"调用视觉识别模型进一步查看一张已经收到的图片，并把识别结果返回给当前工具调用。

仅当聊天记录中的简短图片描述不足以完成判断时调用。必须使用聊天记录中真实存在的图片 ID，并提出具体、简短的问题。"#;

#[derive(Debug, Deserialize)]
pub struct RecognizeImageArgs {
    pub image_id: String,
    pub question: String,
}

pub struct RecognizeImageTool;

#[async_trait]
impl Tool for RecognizeImageTool {
    fn name(&self) -> &'static str {
        "recognize_image"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "image_id": {
                    "type": "string",
                    "description": "聊天记录中出现的图片 ID，例如 img_7Kf3aQ"
                },
                "question": {
                    "type": "string",
                    "description": "需要视觉模型回答的具体问题"
                }
            },
            "required": ["image_id", "question"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, _context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let _arguments: RecognizeImageArgs = parse_arguments(self.name(), arguments)?;

        /* 旧动作实现暂存，等待工具运行时依赖注入后再接入。
        let RecognizeImageArgs { image_id, question } = _arguments;
        let output = MessageEnricher::answer_received_image_question(
            context.app_config.clone(),
            context.db_manager.clone(),
            &image_id,
            &question,
        )
        .await?;
        return Ok(ToolOutput { content: output });
        */

        execution_not_implemented(self.name())
    }
}
