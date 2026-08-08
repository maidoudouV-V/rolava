mod registry;
mod reset;

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tracing::{error, info, warn};

use crate::repository::db_manager::QQChatContextManager;
use crate::transport::message::{IncomingMessage, MessageTarget};
use crate::transport::MessageSender;

use registry::CommandRegistry;
use reset::ResetCommand;

/// 所有命令共享的应用服务；新增系统能力时统一从这里注入。
#[derive(Clone)]
pub struct CommandServices {
    pub db_manager: Arc<QQChatContextManager>,
    pub message_sender: Arc<dyn MessageSender>,
}

/// 命令执行时可访问的调用信息和共享服务。
pub struct CommandContext<'a> {
    pub message: &'a IncomingMessage,
    pub services: Arc<CommandServices>,
}

/// 命令执行结果；Actor 根据通用标记同步刷新当前会话运行态。
pub struct CommandOutput {
    pub reply: Option<String>,
    pub runtime_action: CommandRuntimeAction,
}

/// 只能由会话 Actor 应用的运行态动作，避免具体命令直接依赖 Actor 类型。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CommandRuntimeAction {
    #[default]
    None,
    ResetConversation,
}

impl CommandOutput {
    pub fn reply(text: impl Into<String>) -> Self {
        Self {
            reply: Some(text.into()),
            runtime_action: CommandRuntimeAction::None,
        }
    }

    pub fn conversation_reset(text: impl Into<String>) -> Self {
        Self {
            reply: Some(text.into()),
            runtime_action: CommandRuntimeAction::ResetConversation,
        }
    }
}

/// 单个斜杠命令。新增命令只需实现该接口并注册到内置注册表。
#[async_trait]
pub trait Command: Send + Sync {
    fn name(&self) -> &'static str;

    fn description(&self) -> &'static str;

    async fn execute(&self, context: &CommandContext<'_>, arguments: &str)
        -> Result<CommandOutput>;
}

struct CommandInvocation<'a> {
    name: &'a str,
    arguments: &'a str,
}

/// 应用命令入口；负责解析、查找和执行，平台路由仍由会话 Actor 控制。
pub struct CommandSystem {
    registry: CommandRegistry,
    services: Arc<CommandServices>,
    command_whitelist: HashSet<String>,
}

impl CommandSystem {
    pub fn built_in(
        db_manager: Arc<QQChatContextManager>,
        message_sender: Arc<dyn MessageSender>,
        command_whitelist: Vec<String>,
    ) -> Self {
        let mut registry = CommandRegistry::new();
        registry
            .register(ResetCommand)
            .expect("内置命令名称不应重复");
        Self {
            registry,
            services: Arc::new(CommandServices {
                db_manager,
                message_sender,
            }),
            command_whitelist: Self::normalize_whitelist(command_whitelist),
        }
    }

    /// 只有白名单账号发送的斜杠文本才是命令；其它调用会继续普通聊天流程。
    pub fn is_command_message(&self, message: &IncomingMessage) -> bool {
        self.sender_is_whitelisted(message)
            && Self::parse_invocation(&message.content.text).is_some()
    }

    /// 命中命令时返回执行结果；普通消息返回 None，继续进入聚合和过滤流程。
    pub async fn execute_if_command(&self, message: &IncomingMessage) -> Option<CommandOutput> {
        if !self.sender_is_whitelisted(message) {
            return None;
        }
        let invocation = Self::parse_invocation(&message.content.text)?;
        let Some(command) = self.registry.get(invocation.name) else {
            warn!(command = invocation.name, "收到未知命令");
            return Some(CommandOutput::reply(format!(
                "未知命令：/{}",
                invocation.name
            )));
        };

        let context = CommandContext {
            message,
            services: self.services.clone(),
        };
        match command.execute(&context, invocation.arguments).await {
            Ok(output) => {
                info!(command = command.name(), "命令执行完成");
                Some(output)
            }
            Err(error) => {
                warn!(command = command.name(), error = %error, "命令执行失败");
                Some(CommandOutput::reply(format!("命令执行失败：{}", error)))
            }
        }
    }

    /// 命令反馈不写入聊天记录，避免系统控制信息污染后续模型上下文。
    pub async fn send_reply(&self, target: &MessageTarget, reply: Option<&str>) {
        let Some(reply) = reply.filter(|reply| !reply.trim().is_empty()) else {
            return;
        };
        if let Err(error) = self
            .services
            .message_sender
            .send_transient_text(target, reply)
            .await
        {
            error!(error = %error, "发送命令反馈失败");
        }
    }

    fn parse_invocation(text: &str) -> Option<CommandInvocation<'_>> {
        let command_text = text.trim().strip_prefix('/')?.trim_start();
        if command_text.is_empty() {
            return None;
        }

        let name_end = command_text
            .find(char::is_whitespace)
            .unwrap_or(command_text.len());
        let name = &command_text[..name_end];
        let arguments = command_text[name_end..].trim();
        Some(CommandInvocation { name, arguments })
    }

    fn sender_is_whitelisted(&self, message: &IncomingMessage) -> bool {
        self.command_whitelist.contains(&message.sender.id)
    }

    fn normalize_whitelist(accounts: Vec<String>) -> HashSet<String> {
        accounts
            .into_iter()
            .map(|account| account.trim().to_string())
            .filter(|account| !account.is_empty())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::CommandSystem;

    #[test]
    fn parses_command_name_and_arguments() {
        let invocation = CommandSystem::parse_invocation("  /reset now  ").unwrap();

        assert_eq!(invocation.name, "reset");
        assert_eq!(invocation.arguments, "now");
        assert!(CommandSystem::parse_invocation("普通消息").is_none());
        assert!(CommandSystem::parse_invocation(" / ").is_none());
    }

    #[test]
    fn command_whitelist_ignores_empty_and_duplicate_accounts() {
        let whitelist = CommandSystem::normalize_whitelist(vec![
            " 10001 ".to_string(),
            "10001".to_string(),
            "".to_string(),
        ]);

        assert_eq!(whitelist.len(), 1);
        assert!(whitelist.contains("10001"));
    }
}
