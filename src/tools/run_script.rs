use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::process::Command;

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"运行项目中一个已存在且允许执行的脚本，并返回退出状态、标准输出和错误输出。
type 必须是受支持的脚本类型，script 必须是上下文中提供的确定路径，不得猜测或遍历路径。"#;
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ARGUMENTS: usize = 64;
const MAX_ARGUMENT_CHARS: usize = 4_096;

struct ScriptRuntime {
    name: &'static str,
    program: &'static str,
    extensions: &'static [&'static str],
    removed_environment: &'static [&'static str],
}

// 运行类型白名单只负责固定解释器和脚本扩展名。即使脚本可信，它接受的参数仍可能触发文件、
// 网络或子进程操作，因此新增运行类型或脚本能力时仍需检查脚本自身的参数处理逻辑。
const SCRIPT_RUNTIMES: &[ScriptRuntime] = &[ScriptRuntime {
    name: "node",
    program: "node",
    extensions: &["js", "mjs", "cjs"],
    removed_environment: &["NODE_OPTIONS", "NODE_PATH"],
}];

// 脚本路径权限与运行类型分离；当前只允许执行项目 skills 目录中的脚本。
const ALLOWED_SCRIPT_DIRECTORIES: &[&str] = &[crate::skills::DIRECTORY_NAME];

#[derive(Debug, Deserialize)]
pub struct RunScriptArgs {
    #[serde(rename = "type")]
    pub script_type: String,
    pub script: String,
    pub args: Vec<String>,
}

#[derive(Serialize)]
struct RunScriptResult {
    success: bool,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

pub struct RunScriptTool;

#[async_trait]
impl Tool for RunScriptTool {
    fn name(&self) -> &'static str {
        "run_script"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        let supported_types = SCRIPT_RUNTIMES
            .iter()
            .map(|runtime| runtime.name)
            .collect::<Vec<_>>();
        json!({
            "type": "object",
            "properties": {
                "type": {
                    "type": "string",
                    "description": "脚本运行类型",
                    "enum": supported_types
                },
                "script": {
                    "type": "string",
                    "description": "在上下文中提供的脚本绝对路径，例如 /skills/example/main.js",
                    "minLength": 1
                },
                "args": {
                    "type": "array",
                    "description": "按原始顺序传给脚本的参数，不包含解释器运行参数",
                    "items": { "type": "string" },
                    "maxItems": MAX_ARGUMENTS
                }
            },
            "required": ["type", "script", "args"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, _context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let RunScriptArgs {
            script_type,
            script,
            args,
        } = parse_arguments(self.name(), arguments)?;
        let runtime = find_runtime(&script_type)?;
        validate_arguments(&args)?;
        let script_path = resolve_script_path(&script, runtime).await?;
        let working_directory = script_path.parent().context("脚本路径缺少父目录")?;

        // 解释器、脚本路径和工作目录均由后端决定；AI 参数只能出现在脚本路径之后。
        let mut command = Command::new(runtime.program);
        command
            .arg(&script_path)
            .args(&args)
            .current_dir(working_directory)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        for variable in runtime.removed_environment {
            // 清除可向解释器预加载代码或改变模块来源的环境变量。
            command.env_remove(variable);
        }

        let output = tokio::time::timeout(EXECUTION_TIMEOUT, command.output())
            .await
            .with_context(|| format!("脚本执行超过 {} 秒，已终止", EXECUTION_TIMEOUT.as_secs()))?
            .with_context(|| format!("无法启动 {} 脚本运行时", runtime.name))?;

        let result = RunScriptResult {
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout: crate::text_utils::truncate_long_text(&String::from_utf8_lossy(&output.stdout)),
            stderr: crate::text_utils::truncate_long_text(&String::from_utf8_lossy(&output.stderr)),
        };
        Ok(ToolOutput::text(serde_json::to_string(&result)?))
    }
}

fn find_runtime(script_type: &str) -> Result<&'static ScriptRuntime> {
    let script_type = script_type.trim();
    SCRIPT_RUNTIMES
        .iter()
        .find(|runtime| runtime.name == script_type)
        .ok_or_else(|| anyhow::anyhow!("不支持的脚本运行类型：{script_type}"))
}

fn validate_arguments(arguments: &[String]) -> Result<()> {
    if arguments.len() > MAX_ARGUMENTS {
        anyhow::bail!("脚本参数不能超过 {MAX_ARGUMENTS} 个");
    }
    for (index, argument) in arguments.iter().enumerate() {
        if argument.contains('\0') {
            anyhow::bail!("第 {} 个脚本参数包含空字节", index + 1);
        }
        if argument.chars().count() > MAX_ARGUMENT_CHARS {
            anyhow::bail!(
                "第 {} 个脚本参数不能超过 {} 个字符",
                index + 1,
                MAX_ARGUMENT_CHARS
            );
        }
    }
    Ok(())
}

async fn resolve_script_path(raw_path: &str, runtime: &ScriptRuntime) -> Result<PathBuf> {
    let relative_path = validate_virtual_path(raw_path)?;
    let project_root = tokio::fs::canonicalize(std::env::current_dir()?)
        .await
        .context("解析项目根目录失败")?;
    let requested_path = project_root.join(&relative_path);

    // 直接拒绝符号链接脚本，避免校验后执行的是另一个文件。
    if tokio::fs::symlink_metadata(&requested_path)
        .await
        .with_context(|| format!("脚本不存在：{raw_path}"))?
        .file_type()
        .is_symlink()
    {
        anyhow::bail!("不允许执行符号链接脚本：{raw_path}");
    }
    let script_path = tokio::fs::canonicalize(&requested_path)
        .await
        .with_context(|| format!("脚本不存在：{raw_path}"))?;

    // 使用规范化后的真实路径校验目录边界，阻止脚本路径通过链接或跳转越界。
    let mut allowed = false;
    for directory in ALLOWED_SCRIPT_DIRECTORIES {
        let allowed_root = tokio::fs::canonicalize(project_root.join(directory))
            .await
            .with_context(|| format!("允许执行脚本的目录不存在：{directory}"))?;
        if script_path.starts_with(allowed_root) {
            allowed = true;
            break;
        }
    }
    if !allowed {
        anyhow::bail!("没有权限执行该脚本：{raw_path}");
    }
    if !tokio::fs::metadata(&script_path).await?.is_file() {
        anyhow::bail!("脚本路径不是文件：{raw_path}");
    }

    let extension = script_path
        .extension()
        .and_then(|extension| extension.to_str())
        .context("脚本缺少有效扩展名")?;
    if !runtime.extensions.contains(&extension) {
        anyhow::bail!("{} 类型不允许执行 .{} 脚本", runtime.name, extension);
    }
    Ok(script_path)
}

fn validate_virtual_path(raw_path: &str) -> Result<PathBuf> {
    let path = raw_path.trim();
    if path.contains('\\') {
        anyhow::bail!("script 必须使用正斜杠");
    }
    let relative_path = path
        .strip_prefix('/')
        .filter(|path| !path.is_empty())
        .ok_or_else(|| anyhow::anyhow!("script 必须是以 / 开头的虚拟绝对路径"))?;
    let path = Path::new(relative_path);
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!("script 不能包含路径跳转");
    }
    if !ALLOWED_SCRIPT_DIRECTORIES
        .iter()
        .any(|directory| path.starts_with(directory))
    {
        anyhow::bail!("script 不在允许执行的路径白名单中");
    }
    Ok(path.to_path_buf())
}
