use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{bail, Result};

use super::{
    AgentWebSearchTool, ContinueConversationTool, CreateScheduledTaskTool, DeleteScheduledTaskTool,
    EndConversationTool, GetScheduledTaskTool, RememberTool, SendQqExpressionTool, Tool, ToolCall,
    ToolContext, ToolDefinition, ToolResult, UpdateScheduledTaskTool, WaitForReplyTool,
};

/// 已注册工具的稳定有序集合。
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<&'static str, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册当前动作系统包含的全部工具，供模型请求生成 tools 定义。
    pub fn built_in() -> Self {
        let mut registry = Self::new();
        // registry.register(super::SendMessageTool).unwrap();
        registry.register(AgentWebSearchTool).unwrap();
        registry.register(RememberTool).unwrap();
        registry.register(SendQqExpressionTool).unwrap();
        registry.register(WaitForReplyTool::new()).unwrap();
        registry.register(CreateScheduledTaskTool).unwrap();
        registry.register(GetScheduledTaskTool).unwrap();
        registry.register(UpdateScheduledTaskTool).unwrap();
        registry.register(DeleteScheduledTaskTool).unwrap();
        registry.register(ContinueConversationTool).unwrap();
        registry.register(EndConversationTool).unwrap();
        registry
    }

    pub fn register<T>(&mut self, tool: T) -> Result<()>
    where
        T: Tool + 'static,
    {
        let name = tool.name();
        if self.tools.contains_key(name) {
            bail!("工具 {} 重复注册", name);
        }
        self.tools.insert(name, Arc::new(tool));
        Ok(())
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// 重置所有工具持有的当前会话临时状态。
    pub fn reset_conversation_state(&self) {
        for tool in self.tools.values() {
            tool.reset_conversation_state();
        }
    }

    /// 根据调用名称执行对应工具，并把成功或错误统一转换为工具结果。
    pub async fn execute(&self, context: &ToolContext, call: ToolCall) -> ToolResult {
        let output = match self.get(&call.name) {
            Some(tool) => tool.execute(context, &call.arguments).await,
            None => Err(anyhow::anyhow!("未知工具：{}", call.name)),
        };

        match output {
            Ok(output) => ToolResult {
                tool_call_id: call.id,
                tool_name: call.name,
                content: output.content,
                requires_ai_response: output.requires_ai_response,
                is_error: false,
            },
            Err(error) => ToolResult {
                tool_call_id: call.id,
                tool_name: call.name,
                content: error.to_string(),
                requires_ai_response: true,
                is_error: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ToolRegistry;

    #[test]
    fn built_in_registry_contains_every_current_action() {
        let names = ToolRegistry::built_in()
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "agent_web_search",
                "continue_conversation",
                "create_scheduled_task",
                "delete_scheduled_task",
                "end_conversation",
                "get_scheduled_task",
                "remember",
                "send_qq_expression",
                "update_scheduled_task",
                "wait_for_reply",
            ]
        );
    }
}
