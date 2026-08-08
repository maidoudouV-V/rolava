use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::fs;
use tokio::time::{timeout, Duration};
use tracing::{debug, info, warn};

use crate::message_enricher::MessageEnricher;

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"调用视觉识别模型进一步查看一张已经收到的图片，并把识别结果返回给当前工具调用。

必须使用聊天记录中真实存在的图片 ID，并提出具体、简短的问题。"#;

#[derive(Debug, Deserialize)]
pub struct RecognizeImageArgs {
    pub image_id: String,
    pub question: String,
}

pub struct RecognizeImageTool;

impl RecognizeImageTool {
    fn history_contains_image_id(content_parts_json: &str, image_id: &str) -> bool {
        let Ok(Value::Array(parts)) = serde_json::from_str::<Value>(content_parts_json) else {
            return false;
        };

        parts.iter().any(|part| {
            part.get("kind").and_then(Value::as_str) == Some("image")
                && part
                    .get("data")
                    .and_then(|data| data.get("image_id"))
                    .and_then(Value::as_str)
                    == Some(image_id)
        })
    }

    fn mime_type_from_path(path: &str) -> &'static str {
        match Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("jpg") | Some("jpeg") => "image/jpeg",
            Some("png") => "image/png",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            Some("bmp") => "image/bmp",
            _ => "image/jpeg",
        }
    }

    fn question_prompt(initial_description: &str, question: &str) -> String {
        format!(
            "你是聊天机器人的图片识别模块。请根据提供的图片回答主模型提出的问题。\n\
要求：\n\
1. 只依据图片中能直接看到的内容，不要补充图片外的信息。\n\
2. 无法确定时明确说明不确定，并简述能确认的视觉依据。\n\
3. 直接给出简洁答案，不要输出 Markdown，不要描述内部推理过程。\n\
4. 图片的初步描述仅供定位问题，若与图片本身冲突，以图片为准。\n\n\
图片初步描述：{}\n\
需要回答的问题：{}",
            initial_description, question
        )
    }

    fn normalize_answer(answer: &str) -> Result<String> {
        let answer = answer
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if answer.is_empty() {
            anyhow::bail!("视觉模型返回了空结果");
        }
        Ok(answer)
    }
}

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
                    "description": "聊天记录中出现的图片 ID，例如 img_7Kf3aQ9B"
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

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let RecognizeImageArgs { image_id, question } =
            parse_arguments::<RecognizeImageArgs>(self.name(), arguments)?;
        let image_id = image_id.trim();
        let question = question.trim();
        if image_id.is_empty() {
            anyhow::bail!("图片 ID 不能为空");
        }
        if question.is_empty() {
            anyhow::bail!("图片识别问题不能为空");
        }

        let target = &context.conversation.target;
        let history = context.services.db_manager.get_conversation_history(
            &target.source,
            &target.conversation.id,
            context.services.app_config.app.max_history_messages,
        )?;
        let image_is_visible = history
            .iter()
            .any(|message| Self::history_contains_image_id(&message.content_parts_json, image_id));
        if !image_is_visible {
            anyhow::bail!("图片 {} 不在当前聊天上下文中", image_id);
        }

        let image = context
            .services
            .db_manager
            .get_received_image_by_id(image_id)?
            .with_context(|| format!("找不到图片 ID: {}", image_id))?;
        let bytes = fs::read(&image.local_path)
            .await
            .with_context(|| format!("读取图片文件失败: {}", image.local_path))?;
        if bytes.is_empty() {
            anyhow::bail!("图片文件为空: {}", image.local_path);
        }

        let image_data_url = MessageEnricher::prepare_vision_image_data_url(
            &bytes,
            Some(Self::mime_type_from_path(&image.local_path)),
        )?;
        let prompt = Self::question_prompt(&image.description, question);
        let app_config = &context.services.app_config;
        let model_name = &app_config.app.visual_model_name;
        let visual_provider = app_config
            .ai_models
            .get(model_name)
            .with_context(|| format!("找不到视觉模型配置: {}", model_name))?;

        info!(model = %model_name, image_id = %image_id, "调用视觉模型识别图片细节");
        debug!(image_id = %image_id, question = %question, "图片识别问题");
        let max_attempts = app_config.app.ai_request_max_attempts();
        let timeout_seconds = app_config.app.ai_request_timeout_seconds;
        let mut last_error = None;
        for attempt in 1..=max_attempts {
            let result = if timeout_seconds == 0 {
                visual_provider
                    .describe_image(&image_data_url, &prompt)
                    .await
            } else {
                match timeout(
                    Duration::from_secs(timeout_seconds),
                    visual_provider.describe_image(&image_data_url, &prompt),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(anyhow::anyhow!(
                        "图片识别 API 请求超时，超过 {} 秒",
                        timeout_seconds
                    )),
                }
            };

            match result {
                Ok(answer) => return Ok(ToolOutput::text(Self::normalize_answer(&answer)?)),
                Err(error) => {
                    warn!(attempt, max_attempts, error = %error, "图片识别 API 请求失败");
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.expect("图片识别重试循环至少应执行一次"))
    }
}
