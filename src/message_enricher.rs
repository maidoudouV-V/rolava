use std::path::{Path, PathBuf};
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
/// 视觉模型请求图片的目标最大边长。
const VISION_IMAGE_TARGET_MAX_SIDE: u32 = 1280;
/// 视觉模型请求图片超过大小限制后进一步压缩到的最大边长。
const VISION_IMAGE_FALLBACK_MAX_SIDE: u32 = 1024;
/// 视觉模型请求图片的目标最大体积。
const VISION_IMAGE_MAX_BYTES: usize = 800 * 1024;
/// 首次压缩使用的 JPEG 质量。
const VISION_IMAGE_INITIAL_JPEG_QUALITY: u8 = 82;
/// 超过大小限制后使用的 JPEG 质量。
const VISION_IMAGE_FALLBACK_JPEG_QUALITY: u8 = 74;
/// 再次超过大小限制后使用的最低 JPEG 质量。
const VISION_IMAGE_LOW_JPEG_QUALITY: u8 = 66;

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
    /// 创建消息增强器，图片会按配置保存到本地并交给视觉模型转写。
    pub fn new(app_config: Arc<AppConfig>, db_manager: Arc<QQChatContextManager>) -> Self {
        Self {
            app_config,
            db_manager,
            http_client: Client::new(),
        }
    }

    /// 增强单条消息；图片会被替换成带图片 ID 和描述的文本。
    pub async fn enrich(&self, mut message: IncomingMessage) -> IncomingMessage {
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
                    image.context_text()
                }
                Ok(None) => IMAGE_READ_FAILED_TEXT.to_string(),
                Err(err) => {
                    eprintln!("图片增强失败: {}", err);
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
            eprintln!("图片消息缺少可下载 URL: {}", image_data);
            return Ok(None);
        };
        println!("检测到图片消息，开始下载图片: {}", image_url);

        let downloaded_image = self.download_image(&image_url).await?;
        println!(
            "图片下载完成，大小={}KB，类型={}",
            downloaded_image.bytes.len() / 1024,
            downloaded_image.mime_type.as_deref().unwrap_or("未知")
        );
        let content_hash = Self::sha256_hex(&downloaded_image.bytes);
        if let Some(record) = self.db_manager.get_received_image_by_hash(&content_hash)? {
            println!(
                "图片已存在，复用识别结果: image_id={}，内容={}",
                record.image_id, record.description
            );
            return Ok(Some(EnrichedImage::from(record)));
        }

        let description = self
            .describe_image(
                &downloaded_image.bytes,
                downloaded_image.mime_type.as_deref(),
            )
            .await?;
        let image_id = self.generate_image_id()?;
        println!("图片识别成功: image_id={}，内容={}", image_id, description);
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
                eprintln!("清理未入库图片文件失败 path={}: {}", local_path, remove_err);
            }
            return Err(err);
        }
        println!("图片已入库: image_id={}，path={}", image_id, local_path);

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
        println!(
            "准备调用视觉模型: model={}，请求图片大小={}KB，类型={}",
            self.app_config.app.visual_model_name,
            vision_image.bytes.len() / 1024,
            vision_image.mime_type
        );
        let image_data_url = format!(
            "data:{};base64,{}",
            vision_image.mime_type,
            general_purpose::STANDARD.encode(&vision_image.bytes),
        );
        let visual_provider = self
            .app_config
            .ai_providers
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
                    eprintln!(
                        "图片识别 API 请求失败，第 {}/{} 次: {}",
                        attempt, max_attempts, err
                    );
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.expect("视觉识别重试循环至少应执行一次"))
    }

    /// 根据已入库图片 ID 和自然语言问题，再次调用视觉模型回答图片相关问题。
    pub async fn answer_received_image_question(
        app_config: Arc<AppConfig>,
        db_manager: Arc<QQChatContextManager>,
        image_id: &str,
        question: &str,
    ) -> Result<String> {
        let image = db_manager
            .get_received_image_by_id(image_id)?
            .with_context(|| format!("找不到图片 ID: {}", image_id))?;
        let bytes = fs::read(&image.local_path)
            .await
            .with_context(|| format!("读取图片文件失败: {}", image.local_path))?;
        if bytes.is_empty() {
            anyhow::bail!("图片文件为空: {}", image.local_path);
        }

        let mime_type = Self::mime_type_from_path(&image.local_path);
        let vision_image = Self::prepare_image_for_vision(&bytes, mime_type.as_deref())?;
        println!(
            "准备调用视觉模型回答图片问题: model={}，image_id={}，请求图片大小={}KB，类型={}",
            app_config.app.visual_model_name,
            image_id,
            vision_image.bytes.len() / 1024,
            vision_image.mime_type
        );

        let image_data_url = format!(
            "data:{};base64,{}",
            vision_image.mime_type,
            general_purpose::STANDARD.encode(&vision_image.bytes),
        );
        let prompt = Self::image_question_prompt(&image.description, question);
        let visual_provider = app_config
            .ai_providers
            .get(&app_config.app.visual_model_name)
            .with_context(|| format!("找不到视觉模型配置: {}", app_config.app.visual_model_name))?;

        let max_attempts = app_config.app.ai_request_max_attempts();
        let mut last_error = None;
        for attempt in 1..=max_attempts {
            match Self::run_ai_request_with_timeout(
                &app_config,
                "图片问题识别 API 请求",
                visual_provider.describe_image(&image_data_url, &prompt),
            )
            .await
            {
                Ok(answer) => return Ok(Self::sanitize_description(&answer)),
                Err(err) => {
                    eprintln!(
                        "图片问题识别 API 请求失败，第 {}/{} 次: {}",
                        attempt, max_attempts, err
                    );
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

    /// 给视觉模型的图片追问提示词。
    fn image_question_prompt(initial_description: &str, question: &str) -> String {
        format!(
            "你是QQ群聊天机器人中的图片识别模块。另一个AI角色需要你根据图片回答一个具体问题。\n\
要求：\n\
1. 只根据图片中能直接看到的内容回答，不要编造图片外的信息。\n\
2. 如果无法确定，直接说明无法确定，并给出能看出的依据。\n\
3. 回答要短，优先一两句话，不要输出Markdown，不要解释内部推理过程。\n\
4. 如果问题是识别品种、型号、地点等细分类别，请在不确定时保守表达。\n\n\
图片初步描述：{}\n\
问题：{}\n\n\
现在直接回答问题。",
            initial_description, question
        )
    }

    /// 根据本地图片路径推断 MIME 类型，无法判断时使用 JPEG 作为默认类型。
    fn mime_type_from_path(path: &str) -> Option<String> {
        let extension = Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        let mime_type = match extension.as_deref() {
            Some("jpg") | Some("jpeg") => "image/jpeg",
            Some("png") => "image/png",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            Some("bmp") => "image/bmp",
            _ => "image/jpeg",
        };
        Some(mime_type.to_string())
    }

    /// 为视觉模型准备图片：小图不动，大图等比例压缩并降低 JPEG 质量。
    fn prepare_image_for_vision(
        bytes: &[u8],
        mime_type: Option<&str>,
    ) -> Result<VisionImagePayload> {
        let original_mime_type = mime_type.unwrap_or("image/jpeg").to_string();
        let decoded_image = match image::load_from_memory(bytes) {
            Ok(image) => image,
            Err(err) if bytes.len() <= VISION_IMAGE_MAX_BYTES => {
                eprintln!(
                    "解析待压缩图片失败，图片体积未超过限制，使用原图发送视觉模型: {}",
                    err
                );
                return Ok(VisionImagePayload {
                    bytes: bytes.to_vec(),
                    mime_type: original_mime_type,
                });
            }
            Err(err) => {
                anyhow::bail!("解析待压缩图片失败：{}", err);
            }
        };

        let max_side = decoded_image.width().max(decoded_image.height());
        if max_side <= VISION_IMAGE_TARGET_MAX_SIDE && bytes.len() <= VISION_IMAGE_MAX_BYTES {
            println!(
                "图片无需压缩: 尺寸={}x{}，大小={}KB",
                decoded_image.width(),
                decoded_image.height(),
                bytes.len() / 1024
            );
            return Ok(VisionImagePayload {
                bytes: bytes.to_vec(),
                mime_type: original_mime_type,
            });
        }
        println!(
            "图片需要压缩: 原尺寸={}x{}，原大小={}KB",
            decoded_image.width(),
            decoded_image.height(),
            bytes.len() / 1024
        );

        let first_target_side = max_side.min(VISION_IMAGE_TARGET_MAX_SIDE);
        let compressed = Self::resize_and_encode_jpeg(
            &decoded_image,
            first_target_side,
            VISION_IMAGE_INITIAL_JPEG_QUALITY,
        )?;
        if compressed.len() <= VISION_IMAGE_MAX_BYTES {
            println!(
                "图片压缩完成: 最大边={}，质量={}，大小={}KB",
                first_target_side,
                VISION_IMAGE_INITIAL_JPEG_QUALITY,
                compressed.len() / 1024
            );
            return Ok(Self::jpeg_vision_payload(compressed));
        }

        let fallback_target_side = first_target_side.min(VISION_IMAGE_FALLBACK_MAX_SIDE);
        let compressed = Self::resize_and_encode_jpeg(
            &decoded_image,
            fallback_target_side,
            VISION_IMAGE_FALLBACK_JPEG_QUALITY,
        )?;
        if compressed.len() <= VISION_IMAGE_MAX_BYTES {
            println!(
                "图片二次压缩完成: 最大边={}，质量={}，大小={}KB",
                fallback_target_side,
                VISION_IMAGE_FALLBACK_JPEG_QUALITY,
                compressed.len() / 1024
            );
            return Ok(Self::jpeg_vision_payload(compressed));
        }

        let compressed = Self::resize_and_encode_jpeg(
            &decoded_image,
            fallback_target_side,
            VISION_IMAGE_LOW_JPEG_QUALITY,
        )?;
        println!(
            "图片低质量压缩完成: 最大边={}，质量={}，大小={}KB",
            fallback_target_side,
            VISION_IMAGE_LOW_JPEG_QUALITY,
            compressed.len() / 1024
        );
        Ok(Self::jpeg_vision_payload(compressed))
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

    /// 生成短图片 ID，格式如 img_7Kf3aQ。
    fn generate_image_id(&self) -> Result<String> {
        let mut rng = rand::thread_rng();
        for _ in 0..20 {
            let suffix: String = (&mut rng)
                .sample_iter(Alphanumeric)
                .take(6)
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

    /// 替换下一处图片占位符；异常情况下把图片描述追加到消息末尾。
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
    /// 渲染给主聊天 AI 看的图片文本。
    fn context_text(&self) -> String {
        format!(
            "[图片消息 图片ID={} 内容={}]",
            self.image_id, self.description,
        )
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
