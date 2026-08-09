use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine as _};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageFormat};
use rand::{distributions::Alphanumeric, Rng};
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info, trace, warn};

use crate::config::AppConfig;
use crate::repository::db_manager::{
    NewReceivedImage, QQChatContextManager, ReceivedImageRecord, ReferencedMessage,
};
use crate::transport::message::IncomingMessage;

const IMAGE_DESCRIPTION_PROMPT: &str = r#"你是QQ群聊天机器人的图片转写模块。请将图片转成一段简短文本，供另一个AI理解聊天上下文。

要求：
1. 概括主体、场景、动作、表情、关键文字和情绪；不要全文OCR。
2. 如果是表情包或梗图，必须简要说明笑点、反差、讽刺或隐含意思，以及通常表达的聊天态度；不确定时用“可能”表达。
3. 普通图片不要强行解读。可作有助理解语气的有限推断，但不要编造人物身份、事件背景或图片外的事实。
4. 如果是截图，只概括类型、关键内容和明显状态。内容不清晰时，说明不清晰并描述可辨认部分。
5. 尽量控制在120字内，不要使用废话开头，不要输出Markdown、换行或判断过程。

现在直接描述图片"#;

const IMAGE_READ_FAILED_TEXT: &str = "[图片消息 读取失败]";
const REPLY_MESSAGE_PLACEHOLDER: &str = "[回复消息]";
const REPLY_PREVIEW_MAX_CHARS: usize = 16;
/// 模型请求图片只按像素缩小，最长边不超过 1920px。
const VISION_IMAGE_MAX_SIDE: u32 = 1920;
/// 图片发生缩放时使用固定 JPEG 质量，不再按体积逐级降质。
const VISION_IMAGE_JPEG_QUALITY: u8 = 82;

/// 消息增强器：先处理图片等富内容，再让消息进入聊天流程。
pub struct MessageEnricher {
    app_config: Arc<AppConfig>,
    db_manager: Arc<QQChatContextManager>,
    http_client: Client,
    description_tx: Option<mpsc::UnboundedSender<String>>,
}

/// 下载和本地索引已经完成、可以立即写入聊天数据库的消息。
pub struct EnrichedIncomingMessage {
    pub message: IncomingMessage,
    pending_image_descriptions: Vec<String>,
}

struct DownloadedImage {
    bytes: Vec<u8>,
    mime_type: Option<String>,
    original_url: String,
}

struct EnrichedImage {
    image_id: String,
    content_hash: String,
    local_path: String,
    description: String,
    needs_description: bool,
}

struct VisionImagePayload {
    bytes: Vec<u8>,
    mime_type: String,
}

impl MessageEnricher {
    /// 创建消息增强器；视觉模型名称为空时不启动后台图片描述任务。
    pub fn new(app_config: Arc<AppConfig>, db_manager: Arc<QQChatContextManager>) -> Self {
        let description_tx = Self::start_description_worker(&app_config, &db_manager);
        Self {
            app_config,
            db_manager,
            http_client: Client::new(),
            description_tx,
        }
    }

    /// 增强单条消息；图片先下载入库，描述任务必须等聊天消息写入后再调度。
    pub async fn enrich(&self, mut message: IncomingMessage) -> EnrichedIncomingMessage {
        let mut pending_image_descriptions = Vec::new();
        self.enrich_reply_parts(&mut message);

        let image_parts = message
            .content
            .parts
            .iter()
            .enumerate()
            .filter_map(|(index, part)| {
                if part.kind == "image" {
                    Some((index, false, "[图片]".to_string()))
                } else if part.kind == "file" && Self::is_image_file_part(&part.data) {
                    let placeholder = part
                        .data
                        .get("context_text")
                        .and_then(Value::as_str)
                        .unwrap_or("[文件]")
                        .to_string();
                    Some((index, true, placeholder))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        for (image_index, is_file, placeholder) in image_parts {
            let image_data = message.content.parts[image_index].data.clone();
            let replacement = match self.enrich_image_part(&image_data).await {
                Ok(Some(image)) => {
                    let image_context = if is_file {
                        Self::wrap_file_context(&placeholder, &image.context_text())
                    } else {
                        image.context_text()
                    };
                    let part = &mut message.content.parts[image_index];
                    if is_file {
                        part.kind = "image".to_string();
                    }
                    Self::attach_image_info(&mut part.data, &image, &image_context);
                    if image.needs_description {
                        pending_image_descriptions.push(image.image_id.clone());
                    }
                    image_context
                }
                Ok(None) => Self::image_enrichment_failure_text(is_file, &placeholder),
                Err(err) => {
                    warn!(error = %err, "图片增强失败");
                    Self::image_enrichment_failure_text(is_file, &placeholder)
                }
            };

            message.content.text =
                Self::replace_next_placeholder(&message.content.text, &placeholder, &replacement);
        }

        EnrichedIncomingMessage {
            message,
            pending_image_descriptions,
        }
    }

    /// 将 reply 段的平台消息 ID 还原为同一会话中的原消息摘要。
    fn enrich_reply_parts(&self, message: &mut IncomingMessage) {
        for reply_index in 0..message.content.parts.len() {
            if message.content.parts[reply_index].kind != "reply" {
                continue;
            }
            let reply_data = message.content.parts[reply_index].data.clone();
            let replacement = match Self::resolve_reply_context(
                &self.db_manager,
                &message.source,
                &message.conversation.id,
                &reply_data,
            ) {
                Ok(context_text) => context_text,
                Err(err) => {
                    warn!(error = %err, "还原回复原消息失败");
                    REPLY_MESSAGE_PLACEHOLDER.to_string()
                }
            };
            if let Value::Object(map) = &mut message.content.parts[reply_index].data {
                map.insert(
                    "context_text".to_string(),
                    Value::String(replacement.clone()),
                );
            }
            message.content.text = Self::replace_next_placeholder(
                &message.content.text,
                REPLY_MESSAGE_PLACEHOLDER,
                &replacement,
            );
        }
    }

    fn resolve_reply_context(
        db_manager: &QQChatContextManager,
        source: &str,
        source_conversation_id: &str,
        reply_data: &Value,
    ) -> Result<String> {
        let Some(reply_id) = Self::json_value_string(reply_data.get("id")) else {
            return Ok(REPLY_MESSAGE_PLACEHOLDER.to_string());
        };
        let original =
            db_manager.get_referenced_message(source, source_conversation_id, &reply_id)?;
        Ok(Self::reply_context_text(&reply_id, original.as_ref()))
    }

    fn reply_context_text(reply_id: &str, original: Option<&ReferencedMessage>) -> String {
        let Some(original) = original else {
            return format!("[reply ID：{}]", reply_id);
        };
        let content = original
            .content_text
            .as_deref()
            .map(Self::reply_content_preview)
            .filter(|content| !content.is_empty());
        let sender = original.sender_display_name.trim();

        match (sender.is_empty(), content) {
            (false, Some(content)) => format!("[reply {}：{}]", sender, content),
            (true, Some(content)) => format!("[reply：{}]", content),
            (false, None) => format!("[reply {}：ID {}]", sender, reply_id),
            (true, None) => format!("[reply ID：{}]", reply_id),
        }
    }

    fn reply_content_preview(content: &str) -> String {
        let content = content.split_whitespace().collect::<Vec<_>>().join(" ");
        if content.chars().count() <= REPLY_PREVIEW_MAX_CHARS {
            return content;
        }
        let mut preview = content
            .chars()
            .take(REPLY_PREVIEW_MAX_CHARS)
            .collect::<String>();
        preview.push('…');
        preview
    }

    fn json_value_string(value: Option<&Value>) -> Option<String> {
        match value? {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        }
        .filter(|value| !value.trim().is_empty())
    }

    /// 文件段只根据明确的图片扩展名进入图片处理，不对其他文件推断类型。
    fn is_image_file_part(file_data: &Value) -> bool {
        file_data
            .get("file")
            .and_then(Value::as_str)
            .and_then(Self::image_format_from_path)
            .and_then(Self::image_mime_type)
            .is_some()
    }

    fn image_enrichment_failure_text(is_file: bool, placeholder: &str) -> String {
        if is_file {
            Self::wrap_file_context(placeholder, IMAGE_READ_FAILED_TEXT)
        } else {
            IMAGE_READ_FAILED_TEXT.to_string()
        }
    }

    fn wrap_file_context(file_placeholder: &str, image_context: &str) -> String {
        if let Some(file_prefix) = file_placeholder.strip_suffix(']') {
            format!("{} {}]", file_prefix, image_context)
        } else {
            format!("[文件 {}]", image_context)
        }
    }

    /// 聊天消息成功入库后再投递描述任务，保证后台结果一定能回写到正文记录。
    pub fn schedule_pending_descriptions(&self, enriched: &EnrichedIncomingMessage) {
        let Some(description_tx) = &self.description_tx else {
            return;
        };
        for image_id in &enriched.pending_image_descriptions {
            if description_tx.send(image_id.clone()).is_err() {
                error!(image_id, "图片描述后台队列已关闭");
            }
        }
    }

    fn start_description_worker(
        app_config: &Arc<AppConfig>,
        db_manager: &Arc<QQChatContextManager>,
    ) -> Option<mpsc::UnboundedSender<String>> {
        let model_name = app_config.app.visual_model_name.trim();
        if model_name.is_empty() {
            info!("后台图片描述功能未启用");
            return None;
        }

        let (description_tx, description_rx) = mpsc::unbounded_channel();
        let worker_config = app_config.clone();
        let worker_db = db_manager.clone();
        tokio::spawn(async move {
            Self::run_description_worker(worker_config, worker_db, description_rx).await;
        });
        info!(model = model_name, "后台图片描述功能已启动");
        Some(description_tx)
    }

    /// 单队列顺序处理图片，重复任务会先检查数据库并复用已经生成的描述。
    async fn run_description_worker(
        app_config: Arc<AppConfig>,
        db_manager: Arc<QQChatContextManager>,
        mut description_rx: mpsc::UnboundedReceiver<String>,
    ) {
        while let Some(image_id) = description_rx.recv().await {
            if let Err(err) =
                Self::process_description_job(&app_config, &db_manager, &image_id).await
            {
                warn!(image_id, error = %err, "后台图片描述失败");
            }
        }
        warn!("后台图片描述队列已停止");
    }

    async fn process_description_job(
        app_config: &AppConfig,
        db_manager: &QQChatContextManager,
        image_id: &str,
    ) -> Result<()> {
        let image = db_manager
            .get_received_image_by_id(image_id)?
            .with_context(|| format!("找不到待描述图片 ID: {}", image_id))?;
        let description = if image.description.trim().is_empty() {
            let bytes = fs::read(&image.local_path)
                .await
                .with_context(|| format!("读取待描述图片失败: {}", image.local_path))?;
            Self::describe_image(
                app_config,
                &bytes,
                Some(Self::mime_type_from_path(&image.local_path)),
            )
            .await?
        } else {
            image.description
        };

        let described_text = EnrichedImage::context_text_for(image_id, &description);
        let updated_messages = db_manager.complete_received_image_description(
            image_id,
            &description,
            &described_text,
        )?;
        info!(image_id, updated_messages, "后台图片描述已写回数据库");
        debug!(image_id, description = %description, "后台图片描述内容");
        Ok(())
    }

    /// 处理单个图片片段，只完成下载、去重和本地入库，不同步等待视觉模型。
    async fn enrich_image_part(&self, image_data: &Value) -> Result<Option<EnrichedImage>> {
        let Some(image_url) = Self::image_download_url(image_data) else {
            warn!("图片消息缺少可下载 URL");
            trace!(image_data = %image_data, "无法下载的图片消息原始数据");
            return Ok(None);
        };
        info!("检测到图片消息，开始下载");
        trace!(image_url = %image_url, "图片完整下载地址");

        let downloaded_image = self.download_image(&image_url).await?;
        debug!(
            size_kb = downloaded_image.bytes.len() / 1024,
            mime_type = downloaded_image.mime_type.as_deref().unwrap_or("未知"),
            "图片下载完成"
        );
        let content_hash = Self::sha256_hex(&downloaded_image.bytes);
        if let Some(record) = self.db_manager.get_received_image_by_hash(&content_hash)? {
            info!(image_id = %record.image_id, "图片已存在，复用本地记录");
            let mut image = EnrichedImage::from(record);
            image.needs_description = self.description_tx.is_some() && image.description.is_empty();
            return Ok(Some(image));
        }

        let image_id = self.generate_image_id()?;
        info!(image_id = %image_id, "图片处理成功");
        let local_path = self
            .save_image_file(
                &image_id,
                downloaded_image.mime_type.as_deref(),
                &downloaded_image.bytes,
            )
            .await?;

        let image = NewReceivedImage {
            image_id: image_id.clone(),
            content_hash: content_hash.clone(),
            local_path: local_path.clone(),
            original_url: Some(downloaded_image.original_url),
            mime_type: downloaded_image.mime_type,
            file_size: downloaded_image.bytes.len() as i64,
            description: String::new(),
            metadata_json: json!({
                "source_part_data": image_data
            })
            .to_string(),
        };
        if let Err(err) = self.db_manager.insert_received_image(&image) {
            if let Err(remove_err) = fs::remove_file(&local_path).await {
                warn!(path = %local_path, error = %remove_err, "清理未入库图片文件失败");
            }
            // 其它会话可能刚好先写入了同一图片，冲突后直接复用其稳定图片 ID。
            if let Some(record) = self.db_manager.get_received_image_by_hash(&content_hash)? {
                let mut image = EnrichedImage::from(record);
                image.needs_description =
                    self.description_tx.is_some() && image.description.is_empty();
                return Ok(Some(image));
            }
            return Err(err);
        }
        info!(image_id = %image_id, "图片已入库");
        debug!(image_id = %image_id, path = %local_path, "图片本地文件");

        Ok(Some(EnrichedImage {
            image_id,
            content_hash,
            local_path,
            description: String::new(),
            needs_description: self.description_tx.is_some(),
        }))
    }

    /// 下载图片原始内容，用内容哈希做去重依据。
    async fn download_image(&self, image_url: &str) -> Result<DownloadedImage> {
        let resp = self
            .http_client
            .get(image_url)
            .send()
            .await
            .with_context(|| format!("下载图片失败: {}", image_url))?;

        if !resp.status().is_success() {
            anyhow::bail!("下载图片返回错误状态 {}: {}", resp.status(), image_url);
        }

        let response_mime_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string);
        let bytes = resp.bytes().await?.to_vec();
        if bytes.is_empty() {
            anyhow::bail!("下载到空图片: {}", image_url);
        }
        let mime_type = response_mime_type
            .filter(|mime_type| mime_type.starts_with("image/"))
            .or_else(|| Self::detect_image_mime_type(&bytes).map(str::to_string));

        Ok(DownloadedImage {
            bytes,
            mime_type,
            original_url: image_url.to_string(),
        })
    }

    /// 保存图片到本地接收目录，并返回保存路径。
    async fn save_image_file(
        &self,
        image_id: &str,
        mime_type: Option<&str>,
        bytes: &[u8],
    ) -> Result<String> {
        let image_dir = PathBuf::from(&self.app_config.app.received_image_dir);
        fs::create_dir_all(&image_dir).await?;

        let extension = Self::image_extension(mime_type);
        let image_path = image_dir.join(format!("{}.{}", image_id, extension));
        fs::write(&image_path, bytes).await?;
        Ok(image_path.to_string_lossy().to_string())
    }

    /// 调用配置中的视觉模型，把图片转成一行短描述。
    async fn describe_image(
        app_config: &AppConfig,
        bytes: &[u8],
        mime_type: Option<&str>,
    ) -> Result<String> {
        let vision_image = Self::prepare_image_for_vision(bytes, mime_type)?;
        info!(
            model = %app_config.app.visual_model_name,
            image_size_kb = vision_image.bytes.len() / 1024,
            mime_type = %vision_image.mime_type,
            "准备调用视觉模型"
        );
        let image_data_url = Self::vision_image_data_url(&vision_image);
        let visual_provider = app_config
            .ai_models
            .get(&app_config.app.visual_model_name)
            .with_context(|| format!("找不到视觉模型配置: {}", app_config.app.visual_model_name))?;

        let max_attempts = app_config.app.ai_request_max_attempts();
        let mut last_error = None;
        for attempt in 1..=max_attempts {
            match Self::run_ai_request_with_timeout(
                app_config,
                "图片识别 API 请求",
                visual_provider.describe_image(&image_data_url, IMAGE_DESCRIPTION_PROMPT),
            )
            .await
            {
                Ok(description) => return Ok(Self::sanitize_description(&description)),
                Err(err) => {
                    warn!(attempt, max_attempts, error = %err, "图片识别 API 请求失败");
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.expect("视觉识别重试循环至少应执行一次"))
    }

    async fn run_ai_request_with_timeout<T, F>(
        app_config: &AppConfig,
        request_name: &str,
        request: F,
    ) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        let timeout_seconds = app_config.app.ai_request_timeout_seconds;
        if timeout_seconds == 0 {
            return request.await;
        }

        match timeout(Duration::from_secs(timeout_seconds), request).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "{}超时，超过 {} 秒",
                request_name,
                timeout_seconds
            )),
        }
    }

    /// 为模型准备图片：小图保持原样，大图只按最长边等比例缩小。
    fn prepare_image_for_vision(
        bytes: &[u8],
        mime_type: Option<&str>,
    ) -> Result<VisionImagePayload> {
        let original_mime_type = mime_type.unwrap_or("image/jpeg").to_string();
        let decoded_image = image::load_from_memory(bytes)
            .map_err(|err| anyhow::anyhow!("解析待缩放图片失败：{}", err))?;

        let max_side = decoded_image.width().max(decoded_image.height());
        if max_side <= VISION_IMAGE_MAX_SIDE {
            debug!(
                width = decoded_image.width(),
                height = decoded_image.height(),
                "图片无需缩放"
            );
            return Ok(VisionImagePayload {
                bytes: bytes.to_vec(),
                mime_type: original_mime_type,
            });
        }
        debug!(
            width = decoded_image.width(),
            height = decoded_image.height(),
            "图片需要缩放"
        );

        let compressed = Self::resize_and_encode_jpeg(
            &decoded_image,
            VISION_IMAGE_MAX_SIDE,
            VISION_IMAGE_JPEG_QUALITY,
        )?;
        debug!(
            max_side = VISION_IMAGE_MAX_SIDE,
            quality = VISION_IMAGE_JPEG_QUALITY,
            "图片缩放完成"
        );
        Ok(Self::jpeg_vision_payload(compressed))
    }

    fn vision_image_data_url(image: &VisionImagePayload) -> String {
        format!(
            "data:{};base64,{}",
            image.mime_type,
            general_purpose::STANDARD.encode(&image.bytes),
        )
    }

    /// 为主模型上下文准备可直接提交的图片 data URL。
    pub(crate) fn prepare_vision_image_data_url(
        bytes: &[u8],
        mime_type: Option<&str>,
    ) -> Result<String> {
        let vision_image = Self::prepare_image_for_vision(bytes, mime_type)?;
        Ok(Self::vision_image_data_url(&vision_image))
    }

    /// 等比例缩放到指定最大边长，并编码为 JPEG。
    fn resize_and_encode_jpeg(image: &DynamicImage, max_side: u32, quality: u8) -> Result<Vec<u8>> {
        let resized_image = Self::resize_to_max_side(image, max_side);
        let rgb_image = resized_image.to_rgb8();
        let mut output = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut output, quality);
        encoder.encode_image(&rgb_image)?;
        Ok(output)
    }

    /// 按最大边长等比例缩放图片。
    fn resize_to_max_side(image: &DynamicImage, max_side: u32) -> DynamicImage {
        let (width, height) = image.dimensions();
        let current_max_side = width.max(height);
        if current_max_side <= max_side {
            return image.clone();
        }

        let scale = max_side as f64 / current_max_side as f64;
        let resized_width = ((width as f64 * scale).round() as u32).max(1);
        let resized_height = ((height as f64 * scale).round() as u32).max(1);
        image.resize(resized_width, resized_height, FilterType::Lanczos3)
    }

    /// 构造 JPEG 视觉请求图片。
    fn jpeg_vision_payload(bytes: Vec<u8>) -> VisionImagePayload {
        VisionImagePayload {
            bytes,
            mime_type: "image/jpeg".to_string(),
        }
    }

    /// 生成短图片 ID，格式如 img_7Kf3aQ9B。
    fn generate_image_id(&self) -> Result<String> {
        let mut rng = rand::thread_rng();
        for _ in 0..20 {
            let suffix: String = (&mut rng)
                .sample_iter(Alphanumeric)
                .take(8)
                .map(char::from)
                .collect();
            let image_id = format!("img_{}", suffix);
            if !self.db_manager.received_image_id_exists(&image_id)? {
                return Ok(image_id);
            }
        }
        anyhow::bail!("生成图片 ID 连续碰撞")
    }

    /// 从 OneBot 图片片段里提取可下载地址。
    fn image_download_url(image_data: &Value) -> Option<String> {
        for key in ["url", "file"] {
            let Some(value) = image_data.get(key) else {
                continue;
            };
            let Some(text) = value.as_str() else {
                continue;
            };
            let text = text.trim();
            if text.starts_with("http://") || text.starts_with("https://") {
                return Some(text.to_string());
            }
        }
        None
    }

    /// 给结构化图片片段补充本地索引信息，便于后续按图片 ID 找回原图。
    fn attach_image_info(image_data: &mut Value, image: &EnrichedImage, context_text: &str) {
        if let Value::Object(map) = image_data {
            map.insert(
                "image_id".to_string(),
                Value::String(image.image_id.clone()),
            );
            map.insert(
                "content_hash".to_string(),
                Value::String(image.content_hash.clone()),
            );
            map.insert(
                "local_path".to_string(),
                Value::String(image.local_path.clone()),
            );
            map.insert(
                "description".to_string(),
                Value::String(image.description.clone()),
            );
            map.insert(
                "context_text".to_string(),
                Value::String(context_text.to_string()),
            );
        }
    }

    /// 替换下一处富内容占位符；异常情况下把解析结果追加到消息末尾。
    fn replace_next_placeholder(text: &str, placeholder: &str, replacement: &str) -> String {
        if let Some(index) = text.find(placeholder) {
            let placeholder_len = placeholder.len();
            format!(
                "{}{}{}",
                &text[..index],
                replacement,
                &text[index + placeholder_len..],
            )
        } else if text.trim().is_empty() {
            replacement.to_string()
        } else {
            format!("{} {}", text.trim_end(), replacement)
        }
    }

    /// 计算图片内容 SHA-256，用于跨会话去重。
    fn sha256_hex(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        digest.iter().map(|byte| format!("{:02x}", byte)).collect()
    }

    /// 根据 MIME 类型选择本地文件扩展名。
    fn image_extension(mime_type: Option<&str>) -> &'static str {
        match mime_type {
            Some("image/jpeg") | Some("image/jpg") => "jpg",
            Some("image/png") => "png",
            Some("image/gif") => "gif",
            Some("image/webp") => "webp",
            Some("image/bmp") => "bmp",
            _ => "img",
        }
    }

    fn detect_image_mime_type(bytes: &[u8]) -> Option<&'static str> {
        Self::image_mime_type(image::guess_format(bytes).ok()?)
    }

    fn image_format_from_path(path: &str) -> Option<ImageFormat> {
        ImageFormat::from_extension(Path::new(path).extension()?.to_str()?)
    }

    fn image_mime_type(format: ImageFormat) -> Option<&'static str> {
        match format {
            ImageFormat::Jpeg => Some("image/jpeg"),
            ImageFormat::Png => Some("image/png"),
            ImageFormat::Gif => Some("image/gif"),
            ImageFormat::WebP => Some("image/webp"),
            ImageFormat::Bmp => Some("image/bmp"),
            _ => None,
        }
    }

    fn mime_type_from_path(path: &str) -> &'static str {
        Self::image_format_from_path(path)
            .and_then(Self::image_mime_type)
            .unwrap_or("image/jpeg")
    }

    /// 清理模型输出，保证插入聊天记录时是一行短文本。
    fn sanitize_description(description: &str) -> String {
        let description = description
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if description.is_empty() {
            "图片内容为空".to_string()
        } else {
            description
        }
    }
}

impl EnrichedImage {
    fn context_text(&self) -> String {
        Self::context_text_for(&self.image_id, &self.description)
    }

    fn context_text_for(image_id: &str, description: &str) -> String {
        let alt_text = if description.trim().is_empty() {
            "图片"
        } else {
            description
        };
        let alt_text = alt_text
            .replace('\\', "\\\\")
            .replace('[', "\\[")
            .replace(']', "\\]");
        format!("![{}](attachment://{})", alt_text, image_id)
    }
}

impl From<ReceivedImageRecord> for EnrichedImage {
    fn from(record: ReceivedImageRecord) -> Self {
        Self {
            image_id: record.image_id,
            content_hash: record.content_hash,
            local_path: record.local_path,
            description: record.description,
            needs_description: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EnrichedImage, MessageEnricher, REPLY_MESSAGE_PLACEHOLDER, REPLY_PREVIEW_MAX_CHARS,
        VISION_IMAGE_MAX_SIDE,
    };
    use crate::repository::db_manager::{NewChatMessage, QQChatContextManager};
    use image::{DynamicImage, GenericImageView, RgbImage};
    use serde_json::json;

    #[test]
    fn reply_context_resolves_original_message_and_keeps_current_text() {
        let path = std::env::temp_dir().join(format!(
            "rolava-reply-test-{}-{}.db",
            std::process::id(),
            rand::random::<u64>()
        ));
        let manager = QQChatContextManager::new(path.to_str().unwrap()).unwrap();
        manager
            .write_message(&NewChatMessage {
                source: "onebot".to_string(),
                source_conversation_id: "720206567".to_string(),
                conversation_kind: "group".to_string(),
                conversation_title: None,
                conversation_metadata_json: "{}".to_string(),
                source_message_id: Some("2026419509".to_string()),
                sender_id: "10001".to_string(),
                sender_display_name: "小明".to_string(),
                sender_nickname: None,
                sender_role: None,
                content_text: "原始消息内容".to_string(),
                message_type: "text".to_string(),
                content_parts_json: "[]".to_string(),
                metadata_json: "{}".to_string(),
                event_timestamp: 1,
            })
            .unwrap();

        let context = MessageEnricher::resolve_reply_context(
            &manager,
            "onebot",
            "720206567",
            &json!({"id": "2026419509"}),
        )
        .unwrap();
        assert_eq!(context, "[reply 小明：原始消息内容]");
        assert_eq!(
            MessageEnricher::replace_next_placeholder(
                "[回复消息] 吃饱了撑的",
                REPLY_MESSAGE_PLACEHOLDER,
                &context,
            ),
            "[reply 小明：原始消息内容] 吃饱了撑的"
        );

        drop(manager);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reply_preview_collapses_whitespace_and_truncates_by_characters() {
        assert_eq!(
            MessageEnricher::reply_content_preview("第一行\n  第二行"),
            "第一行 第二行"
        );

        let long_content = "长".repeat(REPLY_PREVIEW_MAX_CHARS + 5);
        let preview = MessageEnricher::reply_content_preview(&long_content);
        assert_eq!(preview.chars().count(), REPLY_PREVIEW_MAX_CHARS + 1);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn only_explicit_image_file_extensions_enter_image_pipeline() {
        assert!(MessageEnricher::is_image_file_part(&json!({
            "file": "Image_1785858162735_686.PNG"
        })));
        assert!(!MessageEnricher::is_image_file_part(&json!({
            "file": "2026-08-01 12-25-48.mp4"
        })));
    }

    #[test]
    fn image_context_uses_markdown_attachment_reference() {
        let image = EnrichedImage {
            image_id: "img_7Kf3aQ9B".to_string(),
            content_hash: String::new(),
            local_path: String::new(),
            description: "一只白猫趴在窗台上".to_string(),
            needs_description: false,
        };

        assert_eq!(
            image.context_text(),
            "![一只白猫趴在窗台上](attachment://img_7Kf3aQ9B)"
        );
        assert_eq!(
            EnrichedImage::context_text_for("img_7Kf3aQ9B", ""),
            "![图片](attachment://img_7Kf3aQ9B)"
        );
    }

    #[test]
    fn image_context_escapes_generated_description() {
        let image = EnrichedImage {
            image_id: "img_A1b2C3d4".to_string(),
            content_hash: String::new(),
            local_path: String::new(),
            description: r"截图包含 [确定] 和 C:\temp".to_string(),
            needs_description: false,
        };

        assert_eq!(
            image.context_text(),
            r"![截图包含 \[确定\] 和 C:\\temp](attachment://img_A1b2C3d4)"
        );
    }

    #[test]
    fn file_image_context_wraps_one_image_reference() {
        let mut image = EnrichedImage {
            image_id: "img_A1b2C3d4".to_string(),
            content_hash: String::new(),
            local_path: String::new(),
            description: String::new(),
            needs_description: false,
        };
        let file_context = "[文件 Image_1785858162735_686.png；30.71 MB]";

        assert_eq!(
            MessageEnricher::wrap_file_context(file_context, &image.context_text()),
            "[文件 Image_1785858162735_686.png；30.71 MB ![图片](attachment://img_A1b2C3d4)]"
        );
        image.description = "一张测试图片".to_string();
        assert_eq!(
            MessageEnricher::wrap_file_context(file_context, &image.context_text()),
            "[文件 Image_1785858162735_686.png；30.71 MB ![一张测试图片](attachment://img_A1b2C3d4)]"
        );
    }

    #[test]
    fn image_request_resizes_only_when_longest_side_exceeds_1920() {
        let image = DynamicImage::ImageRgb8(RgbImage::new(2400, 1200));
        let mut bytes = Vec::new();
        image
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();

        let payload = MessageEnricher::prepare_image_for_vision(&bytes, Some("image/png")).unwrap();
        let resized = image::load_from_memory(&payload.bytes).unwrap();

        assert_eq!(resized.dimensions(), (VISION_IMAGE_MAX_SIDE, 960));
    }
}
