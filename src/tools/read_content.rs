use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"读取项目目录中允许访问的文本文件并返回完整内容。
路径必须是以项目根目录为起点的虚拟绝对路径，使用正斜杠。
路径来源必须是上下文中提供的确信路径，不得猜测路径，或尝试遍历未知路径"#;
const MAX_CONTENT_BYTES: u64 = 64 * 1024;

// 文件读取权限与工具用途分离；以后开放其他项目目录时只扩展此列表。
const ALLOWED_DIRECTORIES: &[&str] = &[crate::skills::DIRECTORY_NAME];

#[derive(Debug, Deserialize)]
pub struct ReadContentArgs {
    pub path: String,
}

pub struct ReadContentTool;

#[async_trait]
impl Tool for ReadContentTool {
    fn name(&self) -> &'static str {
        "read_content"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "确信的目标绝对路径",
                    "minLength": 1
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, _context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        // 先按工具参数结构解析 JSON；随后对 path 做文件系统级校验。
        let arguments: ReadContentArgs = parse_arguments(self.name(), arguments)?;
        let relative_path = validate_relative_path(&arguments.path)?;

        // 当前工作目录是向模型展示的虚拟项目根，规范化后再参与路径拼接。
        let project_root = tokio::fs::canonicalize(std::env::current_dir()?)
            .await
            .context("解析项目根目录失败")?;
        // 规范化目标路径会确认路径存在，并解析其中可能包含的符号链接。
        let target_path = tokio::fs::canonicalize(project_root.join(&relative_path))
            .await
            .with_context(|| format!("读取路径不存在：{}", relative_path.display()))?;

        // 规范化后再次检查边界，避免通过符号链接或路径跳转访问允许目录之外的文件。
        let allowed = allowed_roots(&project_root).await?;
        if !allowed.iter().any(|root| target_path.starts_with(root)) {
            anyhow::bail!("没有权限读取该路径：{}", relative_path.display());
        }

        let metadata = tokio::fs::metadata(&target_path)
            .await
            .with_context(|| format!("读取文件信息失败：{}", relative_path.display()))?;
        // 只允许读取普通文件，目录和其他文件系统对象均不接受。
        if !metadata.is_file() {
            anyhow::bail!("读取路径不是文件：{}", relative_path.display());
        }
        // 在分配读取缓冲区前限制文件大小，避免把超大文件整个放入模型上下文。
        if metadata.len() > MAX_CONTENT_BYTES {
            anyhow::bail!(
                "文件超过读取上限 {} KiB：{}",
                MAX_CONTENT_BYTES / 1024,
                relative_path.display()
            );
        }

        let bytes = tokio::fs::read(&target_path)
            .await
            .with_context(|| format!("读取文件失败：{}", relative_path.display()))?;
        // 工具只返回文本内容，二进制或编码错误的文件不能进入模型上下文。
        let content = String::from_utf8(bytes)
            .with_context(|| format!("文件不是有效的 UTF-8 文本：{}", relative_path.display()))?;
        Ok(ToolOutput::text(crate::text_utils::truncate_long_text(
            &content,
        )))
    }
}

fn validate_relative_path(raw_path: &str) -> Result<PathBuf> {
    let path = raw_path.trim();
    // 去除参数两端空白后，拒绝空路径。
    if path.is_empty() {
        anyhow::bail!("path 不能为空");
    }
    // 统一要求正斜杠，避免同一路径在 Windows 和 Linux 下产生不同解释。
    if path.contains('\\') {
        anyhow::bail!("path 必须使用正斜杠");
    }

    // 首个斜杠表示虚拟项目根；去除后得到用于本地拼接的相对路径。
    let relative_path = path
        .strip_prefix('/')
        .filter(|path| !path.is_empty())
        .ok_or_else(|| anyhow::anyhow!("path 必须以 / 开头"))?;
    let path = Path::new(relative_path);
    // 只接受普通路径组件，拒绝 .、..、系统根目录和 Windows 盘符等跳转形式。
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!("path 不能包含路径跳转");
    }
    // 权限策略按项目根下的允许目录判断，当前仅开放 skills。
    if !ALLOWED_DIRECTORIES
        .iter()
        .any(|directory| path.starts_with(directory))
    {
        anyhow::bail!("没有权限读取该路径");
    }
    Ok(path.to_path_buf())
}

async fn allowed_roots(project_root: &Path) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::with_capacity(ALLOWED_DIRECTORIES.len());
    for directory in ALLOWED_DIRECTORIES {
        // 允许目录也必须真实存在并规范化，供目标路径进行可靠的边界比较。
        let root = tokio::fs::canonicalize(project_root.join(directory))
            .await
            .with_context(|| format!("允许读取的目录不存在：{directory}"))?;
        roots.push(root);
    }
    Ok(roots)
}
