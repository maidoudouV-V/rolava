use anyhow::Result;
use async_trait::async_trait;

use super::{Command, CommandContext, CommandOutput};

pub struct ResetCommand;

#[async_trait]
impl Command for ResetCommand {
    fn name(&self) -> &'static str {
        "reset"
    }

    fn description(&self) -> &'static str {
        "删除当前会话聊天记录并重置临时会话状态"
    }

    async fn execute(
        &self,
        context: &CommandContext<'_>,
        arguments: &str,
    ) -> Result<CommandOutput> {
        if !arguments.is_empty() {
            anyhow::bail!("/reset 不接受参数");
        }

        let result = context.services.db_manager.reset_conversation_history(
            &context.message.source,
            &context.message.conversation.id,
        )?;
        Ok(CommandOutput::conversation_reset(format!(
            "当前会话已重置，已删除 {} 条聊天记录。",
            result.deleted_messages
        )))
    }
}
