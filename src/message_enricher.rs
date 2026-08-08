use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine as _};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};
use rand::{distributions::Alphanumeric, Rng};
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::time::{timeout, Duration};
use tracing::{debug, info, warn};

use crate::config::AppConfig;
use crate::repository::db_manager::{NewReceivedImage, QQChatContextManager, ReceivedImageRecord};
use crate::transport::message::IncomingMessage;

const IMAGE_DESCRIPTION_PROMPT: &str = r#"你是QQ群聊天机器人中的图片内容转写模块。你的任务是把用户发来的图片描述成一段简短文本，供另一个AI角色理解上下文。

要求：
1. 只描述图片中能直接看到的内容，不要编造、不要猜测图片外的信息。
2. 描述要包含主体、场景、动作、表情、文字、情绪或用途等关键信息。
3. 输出优先控制在80字以内，复杂图片最多120字，简单图片简单描述，复杂图片抓住细节。
4. 如果图片里有文字，提取影响理解的关键文字，不要全文OCR。
5. 如果图片是表情包/梗图，描述画面、关键文字、情绪和可能的聊天语气。
6. 如果图片是截图，概括截图类型、关键内容和明显的界面状态，不要逐条复述。
7. 不要使用“这张图片显示了”“图片中可以看到”等废话开头。
8. 不要解释你的判断过程。
9. 不要输出Markdown。
10. 如果图片内容不清晰，就说明“不清晰”，并描述能辨认出的部分。
11. 输出必须只有一段，不要换行。

现在直接描述图片内容"#;

const IMAGE_READ_FAILED_TEXT: &str = "[图片消息 读取失败]";
/// 模型请求图片只按像素缩小，最长边不超过 1920px。
const VISION_IMAGE_MAX_SIDE: u32 = 1920;
/// 图片发生缩放时使用固定 JPEG 质量，不再按体积逐级降质。
const VISION_IMAGE_JPEG_QUALITY: u8 = 82;

/// 消息增强器：先处理图片等富内容，再让消息进入聊天流程。
pub struct MessageEnricher {
    app_config: Arc<AppConfig>,
    db_manager: Arc<QQChatContextManager>,
    http_client: Client,
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
}

struct VisionImagePayload {
    bytes: Vec<u8>,
    mime_type: String,
}

impl MessageEnricher {
    /// 创建消息增强器，图片会按配置保存到本地，必要时交给视觉模型转写。
    pub fn new(app_config: Arc<AppConfig>, db_manager: Arc<QQChatContextManager>) -> Self {
        Self {
            app_config,
            db_manager,
            http_client: Client::new(),
        }
    }

    /// 增强单条消息；图片会按主模型能力生成 Markdown 描述并保留图片 ID。
    pub async fn enrich(&self, mut message: IncomingMessage) -> IncomingMessage {
        let include_description = !self.app_config.chat_model_supports_vision();
        let image_indexes: Vec<usize> = message
            .content
            .parts
            .iter()
            .enumerate()
            .filter_map(|(index, part)| {
                if part.kind == "image" {
                    Some(index)
                } else {
                    None
                }
            })
            .collect();

        for image_index in image_indexes {
            let image_data = message.content.parts[image_index].data.clone();
            let replacement = match self.enrich_image_part(&image_data).await {
                Ok(Some(image)) => {
                    if let Some(part) = message.content.parts.get_mut(image_index) {
                        Self::attach_image_info(&mut part.data, &image);
                    }
                    image.context_text(include_description)
                }
                Ok(None) => IMAGE_READ_FAILED_TEXT.to_string(),
                Err(err) => {
                    warn!(error = %err, "图片增强失败");
                    IMAGE_READ_FAILED_TEXT.to_string()
                }
            };

            message.content.text =
                Self::replace_next_image_placeholder(&message.content.text, &replacement);
        }

        message
    }

    /// 处理单个图片片段，优先用图片哈希复用数据库中已有描述。
    async fn enrich_image_part(&self, image_data: &Value) -> Result<Option<EnrichedImage>> {
        let Some(image_url) = Self::image_download_url(image_data) else {
            warn!("图片消息缺少可下载 URL");
            debug!(image_data = %image_data, "无法下载的图片消息数据");
            return Ok(None);
        };
        info!("检测到图片消息，开始下载");
        debug!(image_url = %image_url, "图片下载地址");

        let downloaded_image = self.download_image(&image_url).await?;
        debug!(
            size_kb = downloaded_image.bytes.len() / 1024,
            mime_type = downloaded_image.mime_type.as_deref().unwrap_or("未知"),
            "图片下载完成"
        );
        let content_hash = Self::sha256_hex(&downloaded_image.bytes);
        if let Some(record) = self.db_manager.get_received_image_by_hash(&content_hash)? {
            info!(image_id = %record.image_id, "图片已存在，复用识别结果");
            debug!(image_id = %record.image_id, description = %record.description, "复用的图片描述");
            return Ok(Some(EnrichedImage::from(record)));
        }

        let description = if self.app_config.chat_model_supports_vision() {
            String::new()
        } else {
            self.describe_image(
                &downloaded_image.bytes,
                downloaded_image.mime_type.as_deref(),
            )
            .await?
        };
        let image_id = self.generate_image_id()?;
        info!(image_id = %image_id, "图片处理成功");
        if !description.is_empty() {
            debug!(image_id = %image_id, description = %description, "图片识别内容");
        }
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
            description: description.clone(),
            metadata_json: json!({
                "source_part_data": image_data
            })
            .to_string(),
        };
        if let Err(err) = self.db_manager.insert_received_image(&image) {
            if let Err(remove_err) = fs::remove_file(&local_path).await {
                warn!(path = %local_path, error = %remove_err, "清理未入库图片文件失败");
            }
            return Err(err);
        }
        info!(image_id = %image_id, "图片已入库");
        debug!(image_id = %image_id, path = %local_path, "图片本地文件");

        Ok(Some(EnrichedImage {
            image_id,
            content_hash,
            local_path,
            description,
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

        let mime_type = resp
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
    async fn describe_image(&self, bytes: &[u8], mime_type: Option<&str>) -> Result<String> {
        let vision_image = Self::prepare_image_for_vision(bytes, mime_type)?;
        info!(
            model = %self.app_config.app.visual_model_name,
            image_size_kb = vision_image.bytes.len() / 1024,
            mime_type = %vision_image.mime_type,
            "准备调用视觉模型"
        );
        let image_data_url = Self::vision_image_data_url(&vision_image);
        let visual_provider = self
            .app_config
            .ai_models
            .get(&self.app_config.app.visual_model_name)
            .with_context(|| {
                format!(
                    "找不到视觉模型配置: {}",
                    self.app_config.app.visual_model_name
                )
            })?;

        let max_attempts = self.app_config.app.ai_request_max_attempts();
        let mut last_error = None;
        for attempt in 1..=max_attempts {
            match Self::run_ai_request_with_timeout(
                &self.app_config,
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

    /// 为工具等调用方准备可直接提交给视觉模型的图片 data URL。
    pub(crate) fn prepare_vision_image_data_url(
        bytes: &[u8],
        mime_type: Option<&str>,
    ) -> Result<String> {
        let vision_image = Self::prepare_image_for_vision(bytes, mime_type)?;
        Ok(Self::vision_image_data_url(&vision_image))
    }

    fn vision_image_data_url(image: &VisionImagePayload) -> String {
        format!(
            "data:{};base64,{}",
            image.mime_type,
            general_purpose::STANDARD.encode(&image.bytes),
        )
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
    fn attach_image_info(image_data: &mut Value, image: &EnrichedImage) {
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
        }
    }

    /// 替换下一处图片占位符；异常情况下把图片引用追加到消息末尾。
    fn replace_next_image_placeholder(text: &str, replacement: &str) -> String {
        if let Some(index) = text.find("[图片]") {
            let placeholder_len = "[图片]".len();
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
    /// 非视觉模型读取预识别描述；视觉模型只需固定说明，图片会作为原图块提交。
    fn context_text(&self, include_description: bool) -> String {
        let alt_text = if include_description && !self.description.trim().is_empty() {
            self.description.as_str()
        } else {
            "图片"
        };
        let alt_text = alt_text
            .replace('\\', "\\\\")
            .replace('[', "\\[")
            .replace(']', "\\]");
        format!("![{}](attachment://{})", alt_text, self.image_id)
    }
}

impl From<ReceivedImageRecord> for EnrichedImage {
    fn from(record: ReceivedImageRecord) -> Self {
        Self {
            image_id: record.image_id,
            content_hash: record.content_hash,
            local_path: record.local_path,
            description: record.description,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EnrichedImage, MessageEnricher, VISION_IMAGE_MAX_SIDE};
    use image::{DynamicImage, GenericImageView, RgbImage};

    #[test]
    fn image_context_uses_markdown_attachment_reference() {
        let image = EnrichedImage {
            image_id: "img_7Kf3aQ9B".to_string(),
            content_hash: String::new(),
            local_path: String::new(),
            description: "一只白猫趴在窗台上".to_string(),
        };

        assert_eq!(
            image.context_text(true),
            "![一只白猫趴在窗台上](attachment://img_7Kf3aQ9B)"
        );
        assert_eq!(
            image.context_text(false),
            "![图片](attachment://img_7Kf3aQ9B)"
        );
    }

    #[test]
    fn image_context_escapes_description_for_non_vision_model() {
        let image = EnrichedImage {
            image_id: "img_A1b2C3d4".to_string(),
            content_hash: String::new(),
            local_path: String::new(),
            description: r"截图包含 [确定] 和 C:\temp".to_string(),
        };

        assert_eq!(
            image.context_text(true),
            r"![截图包含 \[确定\] 和 C:\\temp](attachment://img_A1b2C3d4)"
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
