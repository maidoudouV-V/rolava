use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};
use crate::transport::{QqExpression, SendOptions};

const DESCRIPTION: &str = r#"向当前聊天窗口发送一个 QQ 原生表情。

expression 可选 face、dice、rps。dice 会随机投掷骰子，rps 会随机进行包剪锤；这两种不需要 face_name。face 必须提供准确的表情名称。
常用 face_name：流泪，打call，变形，仔细分析，菜汪，崇拜，比心，庆祝，惊吓，花朵脸，打招呼，大怨种，贴贴，蛋糕，鞭炮，烟花，求放过，偷感，给你一拳，散味儿，热化了，比爱心。
face_name 可以是QQ经典表情的表情名称，如：撇嘴，大哭，尴尬等，或者也可以使用群友之前发送的表情名称
一次只发送一个表情，不要用该工具发送文字。"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendQqExpressionArgs {
    pub expression: String,
    #[serde(default)]
    pub face_name: Option<String>,
}

pub struct SendQqExpressionTool;

impl SendQqExpressionTool {
    fn resolve_expression(
        arguments: SendQqExpressionArgs,
        face_id_map: &HashMap<String, String>,
    ) -> Result<QqExpression> {
        match arguments.expression.as_str() {
            "face" => {
                let face_name = arguments
                    .face_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("face 必须提供 face_name"))?;
                let face_id = face_id_map
                    .iter()
                    .find_map(|(id, name)| (name == face_name).then(|| id.clone()))
                    .ok_or_else(|| anyhow::anyhow!("没有这个表情：{}", face_name))?;
                Ok(QqExpression::Face {
                    id: face_id,
                    name: face_name.to_string(),
                })
            }
            "dice" => {
                Self::reject_face_name(&arguments.face_name, "dice")?;
                Ok(QqExpression::Dice)
            }
            "rps" => {
                Self::reject_face_name(&arguments.face_name, "rps")?;
                Ok(QqExpression::Rps)
            }
            expression => anyhow::bail!("不支持的 QQ 表情类型：{}", expression),
        }
    }

    fn reject_face_name(face_name: &Option<String>, expression: &str) -> Result<()> {
        if face_name
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty())
        {
            anyhow::bail!("{} 不接受 face_name", expression);
        }
        Ok(())
    }
}

#[async_trait]
impl Tool for SendQqExpressionTool {
    fn name(&self) -> &'static str {
        "send_qq_expression"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "expression": {
                    "type": "string",
                    "enum": ["face", "dice", "rps"],
                    "description": "要发送的 QQ 表情类型"
                },
                "face_name": {
                    "type": "string",
                    "description": "expression 为 face 时必填，必须是准确的 QQ 表情名称"
                }
            },
            "required": ["expression"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let arguments: SendQqExpressionArgs = parse_arguments(self.name(), arguments)?;
        let expression =
            Self::resolve_expression(arguments, &context.services.app_config.face_id_map)?;
        let sent = context
            .services
            .message_sender
            .send_qq_expression(
                &context.conversation.target,
                expression,
                SendOptions::default(),
            )
            .await?;
        Ok(ToolOutput::text(format!("QQ 表情发送成功：{}", sent.text)))
    }
}
