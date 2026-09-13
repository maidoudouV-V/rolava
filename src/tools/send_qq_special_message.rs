use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};
use crate::transport::{QqExpression, QqImage, SendOptions};

const DESCRIPTION: &str = r#"向当前聊天窗口发送一个表情或一张、多张图片，按 type 选择：
- face：通过 face_name 指定准确的 QQ 表情名称，可用经典表情（如撇嘴、大哭、尴尬）或群友之前发送的表情名称。
- dice / rps：随机掷骰子 / 包剪锤，无需 face_name。
- image：通过 image_paths 数组指定图片，不接受 face_name；表情类型不接受 image_paths。

image_paths 必须原样使用系统提供的 /data/images/ 下的绝对路径，如 /data/images/example.png，不得使用 URL 或编造路径。
常用 face_name：流泪、打call、变形、仔细分析、菜汪、崇拜、比心、庆祝、惊吓、花朵脸、打招呼、大怨种、贴贴、蛋糕、鞭炮、烟花、求放过、偷感、给你一拳、散味儿、热化了、比爱心。"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendQqSpecialMessageArgs {
    #[serde(rename = "type")]
    pub message_type: String,
    #[serde(default)]
    pub face_name: Option<String>,
    pub image_paths: Option<Vec<String>>,
}

pub struct SendQqSpecialMessageTool;

impl SendQqSpecialMessageTool {
    fn resolve_expression(
        arguments: SendQqSpecialMessageArgs,
        face_id_map: &HashMap<String, String>,
    ) -> Result<QqExpression> {
        if arguments.image_paths.is_some() {
            anyhow::bail!("表情类型不接受 image_paths");
        }
        match arguments.message_type.as_str() {
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
impl Tool for SendQqSpecialMessageTool {
    fn name(&self) -> &'static str {
        "send_qq_special_message"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "type": {
                    "type": "string",
                    "enum": ["face", "dice", "rps", "image"],
                    "description": "特殊消息类型"
                },
                "face_name": {
                    "type": "string",
                    "description": "type 为 face 时必填，必须是准确的 QQ 表情名称"
                },
                "image_paths": {
                    "type": "array",
                    "minItems": 1,
                    "items": { "type": "string", "minLength": 1 },
                    "description": "type 为 image 时必填；系统提供的图片绝对路径数组，例如 /data/images/example.png，不接受 URL"
                }
            },
            "required": ["type"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let arguments: SendQqSpecialMessageArgs = parse_arguments(self.name(), arguments)?;
        if arguments.message_type == "image" {
            if arguments.face_name.is_some() {
                anyhow::bail!("image 不接受 face_name");
            }
            let paths = arguments
                .image_paths
                .filter(|paths| !paths.is_empty())
                .ok_or_else(|| anyhow::anyhow!("image 必须提供非空 image_paths 数组"))?;
            let project_root =
                std::env::current_dir().map_err(|_| anyhow::anyhow!("无法定位图片目录"))?;
            let images = load_images(&project_root, &paths).await?;
            let sent = context
                .services
                .message_sender
                .send_qq_images(&context.conversation.target, images, SendOptions::default())
                .await?;
            return Ok(ToolOutput::text(format!("图片发送成功：{}", sent.text)));
        }
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

fn image_relative_path(path: &str) -> Result<PathBuf> {
    let suffix = path
        .strip_prefix("/data/images/")
        .ok_or_else(|| anyhow::anyhow!("图片路径必须以 /data/images/ 开头"))?;
    if suffix.contains(['\\', ':', '\0'])
        || suffix
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        anyhow::bail!("图片路径必须使用正斜杠，且不能包含路径跳转");
    }
    Ok(Path::new("data/images").join(suffix))
}

async fn load_images(project_root: &Path, paths: &[String]) -> Result<Vec<QqImage>> {
    let relative_paths = paths
        .iter()
        .map(|path| image_relative_path(path))
        .collect::<Result<Vec<_>>>()?;
    let project_root = tokio::fs::canonicalize(project_root)
        .await
        .map_err(|_| anyhow::anyhow!("无法定位图片目录"))?;
    let image_root = tokio::fs::canonicalize(project_root.join("data/images"))
        .await
        .map_err(|_| anyhow::anyhow!("图片目录 /data/images/ 不存在或无法访问"))?;
    if !image_root.starts_with(&project_root) {
        anyhow::bail!("图片目录不能指向项目外部");
    }
    let mut images = Vec::with_capacity(paths.len());
    for (path, relative_path) in paths.iter().zip(relative_paths) {
        let resolved = tokio::fs::canonicalize(project_root.join(relative_path))
            .await
            .map_err(|_| anyhow::anyhow!("图片不存在或无法访问：{}", path))?;
        if !resolved.starts_with(&image_root) {
            anyhow::bail!("图片路径不能指向 /data/images/ 外部：{}", path);
        }
        let metadata = tokio::fs::metadata(&resolved)
            .await
            .map_err(|_| anyhow::anyhow!("无法读取图片信息：{}", path))?;
        if !metadata.is_file() {
            anyhow::bail!("图片路径必须指向文件：{}", path);
        }
        let bytes = tokio::fs::read(&resolved)
            .await
            .map_err(|_| anyhow::anyhow!("读取图片失败：{}", path))?;
        if image::guess_format(&bytes).is_err() {
            anyhow::bail!("文件不是可识别的图片：{}", path);
        }
        images.push(QqImage {
            path: path.clone(),
            bytes,
        });
    }
    Ok(images)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_outside_image_directory() {
        for path in [
            "data/images/a.png",
            "https://example.com/a.png",
            "/data/images/",
            "/data/images/../a.png",
            "/data/images/./a.png",
            "/data/images//a.png",
            "/data/images/a\\b.png",
            "/data/images/C:/a.png",
            "/data/images/a.png:secret",
            "/data/images-other/a.png",
        ] {
            assert!(image_relative_path(path).is_err(), "{path}");
        }
    }

    #[tokio::test]
    async fn loads_images_in_order_and_rejects_invalid_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data/images");
        std::fs::create_dir_all(&root).unwrap();
        image::RgbImage::new(1, 1).save(root.join("a.png")).unwrap();
        image::RgbImage::new(2, 1).save(root.join("b.png")).unwrap();
        let paths = vec!["/data/images/b.png".into(), "/data/images/a.png".into()];
        let loaded = load_images(dir.path(), &paths).await.unwrap();
        assert_eq!(
            loaded.iter().map(|image| &image.path).collect::<Vec<_>>(),
            paths.iter().collect::<Vec<_>>()
        );
        assert_eq!(loaded[0].bytes, std::fs::read(root.join("b.png")).unwrap());
        std::fs::write(root.join("text.png"), "not an image").unwrap();
        for invalid in ["text.png", "missing.png"] {
            let paths = vec![
                "/data/images/a.png".into(),
                format!("/data/images/{invalid}"),
            ];
            let error = load_images(dir.path(), &paths)
                .await
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains(&paths[1]));
            assert!(!error.contains(&dir.path().display().to_string()));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_image_symlinks_outside_allowed_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("data/images")).unwrap();
        image::RgbImage::new(1, 1)
            .save(dir.path().join("outside.png"))
            .unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("outside.png"),
            dir.path().join("data/images/link.png"),
        )
        .unwrap();
        assert!(load_images(dir.path(), &["/data/images/link.png".into()])
            .await
            .is_err());
    }
}
