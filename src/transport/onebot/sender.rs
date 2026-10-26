use super::{
    OneBotActionResponse, OneBotApiResponse, OneBotGetMessageData, OneBotGroupInfoDto,
    OneBotMessageSegmentDto, OneBotSendMessageData,
};
use crate::config::AppConfig;
use crate::message_enricher::MessageEnricher;
use crate::repository::db_manager::{NewChatMessage, QQChatContextManager};
use crate::runtime_state::{RuntimeGroupInfo, RuntimeState};
use crate::transport::message::{readable_message_text, ConversationKind, MessageTarget};
use crate::transport::{GroupInfo, MessageSender, QqExpression, QqImage, SendOptions, SentMessage};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::Utc;
use rand::Rng;
use reqwest::{header::AUTHORIZATION, Client};
use serde_json::Value;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{debug, info, trace, warn};

const MAX_TEXT_SEGMENTS_PER_SEND: usize = 5;
const FOLLOWUP_SEGMENT_DELAY_PER_CHAR_MS: u64 = 100;
const FOLLOWUP_SEGMENT_MAX_DELAY: Duration = Duration::from_secs(6);
const QQ_RANDOM_EXPRESSION_ANIMATION_DELAY: Duration = Duration::from_secs(3);

/// OneBot 出站消息发送器。平台确认发送成功后，负责写入统一聊天记录。
pub struct OneBotMessageSender {
    image_enricher: MessageEnricher,
    client: Client,
    onebot_api_url: String,
    onebot_token: Option<String>,
    db_manager: Arc<QQChatContextManager>,
    runtime_state: Arc<RuntimeState>,
    reply_delay_random_max_secs: f64,
    send_max_attempts: u32,
}

impl OneBotMessageSender {
    pub fn new(
        config: &AppConfig,
        db_manager: Arc<QQChatContextManager>,
        runtime_state: Arc<RuntimeState>,
        image_enricher: MessageEnricher,
    ) -> Self {
        Self {
            image_enricher,
            client: Client::new(),
            onebot_api_url: config.server.onebot_api.clone(),
            onebot_token: if config.server.onebot_token.is_empty() {
                None
            } else {
                Some(config.server.onebot_token.clone())
            },
            db_manager,
            runtime_state,
            reply_delay_random_max_secs: config.app.reply_delay_random_max_secs,
            send_max_attempts: config.app.ai_request_max_attempts(),
        }
    }

    fn reply_delay(&self, options: SendOptions) -> Duration {
        let random_max_secs = self.reply_delay_random_max_secs.max(0.0);
        let total_delay_secs = if random_max_secs > 0.0 {
            rand::thread_rng().gen_range(0.0..=random_max_secs)
        } else {
            0.0
        };
        let total_delay = Duration::from_secs_f64(total_delay_secs);
        let elapsed = options
            .delay_started_at
            .map(|started_at| started_at.elapsed())
            .unwrap_or_default();

        total_delay.saturating_sub(elapsed)
    }

    /// 后续分段在随机延时上按实际字符数追加打字时间，并限制总延时最多六秒。
    fn followup_segment_delay(random_delay: Duration, text: &str) -> Duration {
        let character_count = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
        let typing_delay = Duration::from_millis(
            character_count.saturating_mul(FOLLOWUP_SEGMENT_DELAY_PER_CHAR_MS),
        );
        random_delay
            .saturating_add(typing_delay)
            .min(FOLLOWUP_SEGMENT_MAX_DELAY)
    }

    fn qq_expression_message(expression: QqExpression) -> (&'static str, Value, String) {
        match expression {
            QqExpression::Face { id, name } => (
                "face",
                serde_json::json!({ "id": id }),
                format!("[QQ表情:{}]", name),
            ),
            QqExpression::Dice => ("dice", serde_json::json!({}), "[QQ表情:骰子]".to_string()),
            QqExpression::Rps => ("rps", serde_json::json!({}), "[QQ表情:包剪锤]".to_string()),
        }
    }

    fn request_parts(
        target: &MessageTarget,
        message: Value,
    ) -> (&'static str, &'static str, Value) {
        let target_id = target
            .conversation
            .id
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(target.conversation.id.clone()));

        match target.conversation.kind {
            ConversationKind::Direct => (
                "send_private_msg",
                "direct",
                serde_json::json!({
                    "user_id": target_id,
                    "message": message,
                }),
            ),
            ConversationKind::Group => (
                "send_group_msg",
                "group",
                serde_json::json!({
                    "group_id": target_id,
                    "message": message,
                }),
            ),
        }
    }

    fn response_error<T>(response: &OneBotActionResponse<T>) -> String {
        response
            .wording
            .as_deref()
            .or(response.message.as_deref())
            .unwrap_or("未提供错误详情")
            .to_string()
    }

    async fn send_message_request(
        &self,
        target: &MessageTarget,
        message: Value,
    ) -> Result<(&'static str, OneBotActionResponse<OneBotSendMessageData>)> {
        let (api_path, conversation_kind, payload) = Self::request_parts(target, message);
        if payload
            .get("message")
            .and_then(Value::as_array)
            .is_some_and(|parts| parts.iter().any(|part| part["type"] == "image"))
        {
            trace!(api_path, "OneBot 发送图片消息请求");
        } else {
            trace!(api_path, payload = %payload, "OneBot 发送消息完整请求");
        }
        let request_url = format!("{}/{}", self.onebot_api_url, api_path);
        let mut request = self.client.post(&request_url).json(&payload);
        if let Some(token) = &self.onebot_token {
            request = request.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let response = request
            .send()
            .await
            .with_context(|| format!("调用 OneBot 发送消息接口失败，url={request_url}"))?;
        let http_status = response.status();
        let response_text = response.text().await.context("读取 OneBot 发送响应失败")?;
        trace!(status = %http_status, response = %response_text, "OneBot 发送消息原始响应");
        if !http_status.is_success() {
            bail!(
                "OneBot 发送消息接口返回 HTTP {}，响应正文：{}",
                http_status,
                response_text
            );
        }

        let response: OneBotActionResponse<OneBotSendMessageData> =
            serde_json::from_str(&response_text)
                .with_context(|| format!("解析 OneBot 发送响应失败，原始响应：{response_text}"))?;
        if response.status != "ok" || response.retcode != 0 {
            bail!(
                "OneBot 发送消息失败，status={}，retcode={}：{}",
                response.status,
                response.retcode,
                Self::response_error(&response)
            );
        }
        Ok((conversation_kind, response))
    }

    /// 只重试平台请求；平台确认后的数据库写入失败不能重试发送，以免产生重复消息。
    async fn send_message_request_with_retry(
        &self,
        target: &MessageTarget,
        message: Value,
    ) -> Result<(&'static str, OneBotActionResponse<OneBotSendMessageData>)> {
        let mut last_error = None;
        for attempt in 1..=self.send_max_attempts {
            match self.send_message_request(target, message.clone()).await {
                Ok(response) => return Ok(response),
                Err(error) => {
                    if attempt < self.send_max_attempts {
                        warn!(
                            attempt,
                            max_attempts = self.send_max_attempts,
                            conversation_id = %target.conversation.id,
                            error = %format!("{error:#}"),
                            "OneBot 消息发送失败，准备重试"
                        );
                    }
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.expect("OneBot 消息发送重试循环至少应执行一次"))
    }

    fn response_message_id(
        response: &OneBotActionResponse<OneBotSendMessageData>,
    ) -> Option<&Value> {
        response
            .data
            .as_ref()
            .and_then(|data| data.message_id.as_ref())
    }

    fn message_id_text(message_id: &Value) -> String {
        match message_id {
            Value::String(message_id) => message_id.clone(),
            other => other.to_string(),
        }
    }

    fn persist_sent_message(
        &self,
        target: &MessageTarget,
        conversation_kind: &str,
        response: &OneBotActionResponse<OneBotSendMessageData>,
        text: &str,
        message_type: &str,
        content_parts: Value,
    ) -> Result<SentMessage> {
        let outgoing_message = NewChatMessage {
            source: target.source.clone(),
            source_conversation_id: target.conversation.id.clone(),
            conversation_kind: conversation_kind.to_string(),
            conversation_title: target.conversation.title.clone(),
            conversation_metadata_json: "{}".to_string(),
            source_message_id: Self::response_message_id(response).map(Self::message_id_text),
            sender_id: target.bot_id.clone(),
            sender_display_name: target.bot_id.clone(),
            sender_nickname: None,
            sender_role: None,
            content_text: text.to_string(),
            message_type: message_type.to_string(),
            content_parts_json: content_parts.to_string(),
            metadata_json: serde_json::json!({
                "onebot": {
                    "status": response.status,
                    "retcode": response.retcode
                }
            })
            .to_string(),
            event_timestamp: Utc::now().timestamp(),
        };
        let stored_message = self
            .db_manager
            .write_message(&outgoing_message)
            .context("消息已发送，但写入聊天记录失败")?;
        let content = readable_message_text(
            text,
            content_parts
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| Some((part.get("kind")?.as_str()?, part.get("data")?))),
        );
        info!(content = %content, "OneBot 消息发送并入库成功");
        debug!(
            conversation_kind,
            conversation_id = %target.conversation.id,
            message_id = stored_message.id,
            "OneBot 消息入库详情"
        );

        Ok(SentMessage {
            database_id: stored_message.id,
            text: text.to_string(),
        })
    }

    /// 查询随机表情最终结果；查询失败不能重发已经成功送达的表情。
    async fn get_sent_expression_segment(
        &self,
        message_id: &Value,
        expression_type: &str,
    ) -> Result<Option<OneBotMessageSegmentDto>> {
        let payload = serde_json::json!({ "message_id": message_id });
        trace!(payload = %payload, "OneBot 查询已发送表情完整请求");
        let mut request = self
            .client
            .post(format!("{}/get_msg", self.onebot_api_url))
            .json(&payload);
        if let Some(token) = &self.onebot_token {
            request = request.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let response = request
            .send()
            .await
            .context("调用 OneBot get_msg 接口失败")?;
        let http_status = response.status();
        let response_text = response
            .text()
            .await
            .context("读取 OneBot get_msg 响应失败")?;
        trace!(status = %http_status, response = %response_text, "OneBot get_msg 原始响应");
        if !http_status.is_success() {
            bail!(
                "OneBot get_msg 接口返回 HTTP {}，响应正文：{}",
                http_status,
                response_text
            );
        }

        let response: OneBotActionResponse<OneBotGetMessageData> =
            serde_json::from_str(&response_text).with_context(|| {
                format!("解析 OneBot get_msg 响应失败，原始响应：{response_text}")
            })?;
        if response.status != "ok" || response.retcode != 0 {
            bail!(
                "OneBot get_msg 返回错误，status={}，retcode={}：{}",
                response.status,
                response.retcode,
                Self::response_error(&response)
            );
        }
        Ok(response
            .data
            .into_iter()
            .flat_map(|data| data.message)
            .find(|segment| segment.type_ == expression_type))
    }

    pub(super) fn text_segments(text: &str) -> Vec<&str> {
        text.split("<split>")
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .collect()
    }

    /// 限制一次普通文本回复的消息数量，超出部分合并到最后一段且不丢失正文。
    fn bounded_text_segments(text: &str) -> (Vec<String>, usize) {
        let segments = Self::text_segments(text);
        let original_count = segments.len();
        if original_count <= MAX_TEXT_SEGMENTS_PER_SEND {
            return (
                segments.into_iter().map(str::to_string).collect(),
                original_count,
            );
        }

        let mut bounded = segments[..MAX_TEXT_SEGMENTS_PER_SEND - 1]
            .iter()
            .map(|segment| (*segment).to_string())
            .collect::<Vec<_>>();
        bounded.push(segments[MAX_TEXT_SEGMENTS_PER_SEND - 1..].join("\n"));
        (bounded, original_count)
    }

    async fn send_text_segment(
        &self,
        target: &MessageTarget,
        text: &str,
        persist: bool,
    ) -> Result<Option<SentMessage>> {
        let (conversation_kind, response) = self
            .send_message_request_with_retry(target, Value::String(text.to_string()))
            .await?;

        if !persist {
            info!(
                content = %text,
                "OneBot 临时消息发送成功"
            );
            return Ok(None);
        }

        self.persist_sent_message(
            target,
            conversation_kind,
            &response,
            text,
            "text",
            serde_json::json!([{
                "kind": "text",
                "data": { "text": text }
            }]),
        )
        .map(Some)
    }
}

#[async_trait]
impl MessageSender for OneBotMessageSender {
    async fn get_group_info(&self, target: &MessageTarget) -> Result<Option<GroupInfo>> {
        if target.source != "onebot" || !matches!(target.conversation.kind, ConversationKind::Group)
        {
            return Ok(None);
        }

        let payload = serde_json::json!({
            "group_id": target.conversation.id,
        });
        let request_url = format!("{}/get_group_info", self.onebot_api_url);
        let mut request = self.client.post(&request_url).json(&payload);
        if let Some(token) = &self.onebot_token {
            request = request.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let response = request
            .send()
            .await
            .with_context(|| format!("调用 OneBot get_group_info 失败，url={request_url}"))?;
        let status = response.status();
        let response_text = response
            .text()
            .await
            .context("读取 OneBot get_group_info 响应失败")?;
        if !status.is_success() {
            bail!(
                "OneBot get_group_info 返回 HTTP {}，响应正文：{}",
                status,
                response_text
            );
        }
        let result: OneBotApiResponse<OneBotGroupInfoDto> = serde_json::from_str(&response_text)
            .with_context(|| {
                format!("解析 OneBot get_group_info 响应失败，原始响应：{response_text}")
            })?;
        if result.retcode != 0 {
            bail!(
                "OneBot get_group_info 返回错误，status={}，retcode={}：{}",
                result.status_text(),
                result.retcode,
                result.error_detail()
            );
        }
        Ok(result.data.map(|group| {
            self.runtime_state.update_group(RuntimeGroupInfo {
                group_id: target.conversation.id.clone(),
                name: group.group_name.clone(),
                member_count: group.member_count,
                max_member_count: None,
            });
            GroupInfo {
                name: group.group_name,
                member_count: group.member_count,
            }
        }))
    }

    async fn send_text(
        &self,
        target: &MessageTarget,
        text: &str,
        options: SendOptions,
    ) -> Result<Vec<SentMessage>> {
        if target.source != "onebot" {
            bail!("OneBot 发送器不支持消息来源：{}", target.source);
        }
        let (segments, original_segment_count) = Self::bounded_text_segments(text);
        if segments.is_empty() {
            bail!("不能发送空消息");
        }
        if original_segment_count > MAX_TEXT_SEGMENTS_PER_SEND {
            warn!(
                original_segment_count,
                max_segment_count = MAX_TEXT_SEGMENTS_PER_SEND,
                "分段消息超过上限，剩余正文已合并到最后一段"
            );
        }

        let mut sent_messages = Vec::with_capacity(segments.len());
        let segment_count = segments.len();
        for (segment_index, segment) in segments.into_iter().enumerate() {
            // 第一段沿用原计时起点；后续分段重新计算随机延时和按字数增加的打字时间。
            let delay = if segment_index == 0 {
                self.reply_delay(options)
            } else {
                Self::followup_segment_delay(self.reply_delay(SendOptions::default()), &segment)
            };
            if !delay.is_zero() {
                debug!(
                    segment = segment_index + 1,
                    segment_count,
                    delay_ms = delay.as_millis(),
                    "等待分段消息发送延时"
                );
                sleep(delay).await;
            }

            let sent_message = self
                .send_text_segment(target, &segment, true)
                .await
                .with_context(|| {
                    format!("发送第 {}/{} 段文本失败", segment_index + 1, segment_count)
                })?
                .expect("持久化发送必须返回数据库消息");
            sent_messages.push(sent_message);
        }

        Ok(sent_messages)
    }

    async fn send_transient_text(&self, target: &MessageTarget, text: &str) -> Result<()> {
        if target.source != "onebot" {
            bail!("OneBot 发送器不支持消息来源：{}", target.source);
        }
        let segments = Self::text_segments(text);
        if segments.is_empty() {
            bail!("不能发送空消息");
        }

        for segment in segments {
            self.send_text_segment(target, segment, false).await?;
        }
        Ok(())
    }

    async fn send_qq_images(
        &self,
        target: &MessageTarget,
        images: Vec<QqImage>,
        options: SendOptions,
    ) -> Result<SentMessage> {
        if target.source != "onebot" {
            bail!("OneBot 发送器不支持消息来源：{}", target.source);
        }
        if images.is_empty() {
            bail!("不能发送空图片消息");
        }
        let message = Value::Array(
            images
                .iter()
                .map(|image| {
                    serde_json::json!({
                        "type": "image",
                        "data": { "file": format!("base64://{}", STANDARD.encode(&image.bytes)) }
                    })
                })
                .collect(),
        );
        let mut registered_images = Vec::with_capacity(images.len());
        for image in &images {
            registered_images.push(
                self.image_enricher
                    .register_local_image(&image.path, &image.bytes)
                    .await?,
            );
        }
        let content_parts = Value::Array(
            registered_images
                .iter()
                .map(|image| image.content_part())
                .collect(),
        );
        let text = registered_images
            .iter()
            .map(|image| image.context_text())
            .collect::<Vec<_>>()
            .join("\n");
        drop(images);
        let delay = self.reply_delay(options);
        if !delay.is_zero() {
            sleep(delay).await;
        }
        let (conversation_kind, response) = self
            .send_message_request_with_retry(target, message)
            .await?;
        let sent = self.persist_sent_message(
            target,
            conversation_kind,
            &response,
            &text,
            "image",
            content_parts,
        )?;
        let pending = registered_images
            .iter()
            .filter_map(|image| image.pending_description_id())
            .collect::<Vec<_>>();
        self.image_enricher.schedule_image_descriptions(&pending);
        Ok(sent)
    }

    async fn send_qq_expression(
        &self,
        target: &MessageTarget,
        expression: QqExpression,
        options: SendOptions,
    ) -> Result<SentMessage> {
        if target.source != "onebot" {
            bail!("OneBot 发送器不支持消息来源：{}", target.source);
        }

        let delay = self.reply_delay(options);
        if !delay.is_zero() {
            debug!(delay_ms = delay.as_millis(), "等待 QQ 表情发送延时");
            sleep(delay).await;
        }

        let (expression_type, outgoing_data, fallback_text) =
            Self::qq_expression_message(expression);
        let message = serde_json::json!([{
            "type": expression_type,
            "data": outgoing_data.clone()
        }]);
        let (conversation_kind, response) = self
            .send_message_request_with_retry(target, message)
            .await?;

        let mut stored_data = outgoing_data;
        let mut context_text = fallback_text;
        let mut random_result_resolved = false;
        if expression_type != "face" {
            if let Some(message_id) = Self::response_message_id(&response) {
                match self
                    .get_sent_expression_segment(message_id, expression_type)
                    .await
                {
                    Ok(Some(segment)) => {
                        let resolved_context_text = match expression_type {
                            "dice" => segment.dice_context_text(),
                            "rps" => segment.rps_context_text(),
                            _ => context_text.clone(),
                        };
                        random_result_resolved = resolved_context_text != context_text;
                        context_text = resolved_context_text;
                        stored_data = segment.data;
                    }
                    Ok(None) => warn!(expression_type, "get_msg 未返回已发送的表情段"),
                    Err(error) => {
                        warn!(expression_type, error = %format!("{error:#}"), "查询已发送表情结果失败")
                    }
                }
            } else {
                warn!(
                    expression_type,
                    "发送响应缺少 message_id，无法查询随机表情结果"
                );
            }
        }

        if let Value::Object(data) = &mut stored_data {
            data.insert(
                "context_text".to_string(),
                Value::String(context_text.clone()),
            );
        }
        let sent_message = self.persist_sent_message(
            target,
            conversation_kind,
            &response,
            &context_text,
            expression_type,
            serde_json::json!([{
                "kind": expression_type,
                "data": stored_data
            }]),
        )?;

        if random_result_resolved {
            debug!(
                expression_type,
                delay_ms = QQ_RANDOM_EXPRESSION_ANIMATION_DELAY.as_millis(),
                "等待 QQ 随机表情播放完成"
            );
            sleep(QQ_RANDOM_EXPRESSION_ANIMATION_DELAY).await;
        }
        Ok(sent_message)
    }
}
