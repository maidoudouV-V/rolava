use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

const SCRIPT_PATH: &str = "skills/bilibili-subtitle-skill/login.mjs";
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(20);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

pub async fn run_login_action(action: &'static str) -> Result<Value> {
    if !matches!(action, "start" | "poll" | "refresh") {
        anyhow::bail!("不支持的B站凭据操作");
    }
    let script = resolve_script().await?;
    let working_directory = script.parent().context("B站登录脚本缺少父目录")?;
    let mut command = Command::new("node");
    command
        .arg(&script)
        .arg(action)
        .current_dir(working_directory)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_PATH");
    let output = tokio::time::timeout(EXECUTION_TIMEOUT, command.output())
        .await
        .context("B站凭据脚本执行超时")?
        .context("无法启动B站凭据脚本")?;
    if output.stdout.len() > MAX_OUTPUT_BYTES || output.stderr.len() > MAX_OUTPUT_BYTES {
        anyhow::bail!("B站凭据脚本返回内容过长");
    }
    let stdout = String::from_utf8(output.stdout).context("B站凭据脚本输出不是UTF-8")?;
    let stderr = compact_output(&output.stderr);
    let result: Value = serde_json::from_str(stdout.trim()).map_err(|error| {
        if stderr.is_empty() {
            anyhow::anyhow!("B站凭据脚本输出不是JSON：{error}")
        } else {
            anyhow::anyhow!("{stderr}")
        }
    })?;
    if !output.status.success() || result.get("error").is_some() {
        let message = result
            .get("error")
            .and_then(Value::as_str)
            .filter(|message| !message.trim().is_empty())
            .map(str::to_string)
            .unwrap_or(stderr);
        anyhow::bail!(
            "{}",
            if message.is_empty() {
                "B站凭据脚本执行失败"
            } else {
                &message
            }
        );
    }
    Ok(result)
}

fn compact_output(bytes: &[u8]) -> String {
    let output = String::from_utf8_lossy(bytes)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    output.chars().take(1_000).collect()
}

pub async fn maintain_credentials(shutdown: CancellationToken) {
    loop {
        match run_login_action("refresh").await {
            Ok(result) => {
                if result.get("refreshed").and_then(Value::as_bool) == Some(true) {
                    debug!("B站登录凭据已刷新");
                }
            }
            Err(error) if error.to_string().contains("登录脚本不存在") => {
                debug!("B站字幕 Skill 不存在，跳过凭据维护");
            }
            Err(error) => warn!(error = %format!("{error:#}"), "B站登录凭据维护失败"),
        }
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(MAINTENANCE_INTERVAL) => {}
        }
    }
}

async fn resolve_script() -> Result<PathBuf> {
    let project_root = tokio::fs::canonicalize(std::env::current_dir()?)
        .await
        .context("解析项目目录失败")?;
    let requested = project_root.join(SCRIPT_PATH);
    let metadata = tokio::fs::symlink_metadata(&requested)
        .await
        .with_context(|| format!("B站登录脚本不存在：{SCRIPT_PATH}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("B站登录脚本路径无效");
    }
    let script = tokio::fs::canonicalize(&requested).await?;
    let allowed =
        tokio::fs::canonicalize(project_root.join("skills/bilibili-subtitle-skill")).await?;
    if !script.starts_with(&allowed) || script.extension() != Some(std::ffi::OsStr::new("mjs")) {
        anyhow::bail!("B站登录脚本路径越界");
    }
    Ok(script)
}
