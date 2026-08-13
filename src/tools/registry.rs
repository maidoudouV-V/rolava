use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{bail, Result};

use super::{
    AgentWebSearchTool, ContinueConversationTool, CreateScheduledTaskTool, CreateUserMemoryTool,
    DeleteCharacterMemoryTool, DeleteScheduledTaskTool, DeleteUserMemoryTool, EndConversationTool,
    GetScheduledTaskTool, SendQqExpressionTool, SetCharacterMemoryTool, Tool, ToolCall,
    ToolContext, ToolDefinition, ToolResult, UpdateScheduledTaskTool, UpdateUserMemoryTool,
    WaitForReplyTool,
};

/// 管理后台可配置的工具信息；固定启用的内部工具不会出现在这里。
#[derive(Debug, Clone, serde::Serialize)]
pub struct OptionalToolDefinition {
    pub name: &'static str,
    pub display_name: &'static str,
    pub description: &'static str,
}

const WEB_SEARCH_MODULE: &str = "agent_web_search";
const MEMORY_MODULE: &str = "memory";
const SCHEDULED_TASKS_MODULE: &str = "scheduled_tasks";

/// 已注册工具的稳定有序集合。
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<&'static str, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册固定工具以及配置中明确启用的可选工具。
    pub fn built_in(enabled_optional_tools: &[String]) -> Self {
        let mut registry = Self::new();
        // registry.register(super::SendMessageTool).unwrap();
        if Self::is_enabled(enabled_optional_tools, WEB_SEARCH_MODULE) {
            registry.register(AgentWebSearchTool).unwrap();
        }
        registry.register(SendQqExpressionTool).unwrap();
        if Self::is_enabled(enabled_optional_tools, MEMORY_MODULE) {
            // 一个模块开关统一控制角色记忆和用户记忆的全部维护工具。
            registry.register(SetCharacterMemoryTool).unwrap();
            registry.register(DeleteCharacterMemoryTool).unwrap();
            registry.register(CreateUserMemoryTool).unwrap();
            registry.register(UpdateUserMemoryTool).unwrap();
            registry.register(DeleteUserMemoryTool).unwrap();
        }
        registry.register(WaitForReplyTool::new()).unwrap();
        if Self::is_enabled(enabled_optional_tools, SCHEDULED_TASKS_MODULE) {
            registry.register(CreateScheduledTaskTool).unwrap();
            registry.register(GetScheduledTaskTool).unwrap();
            registry.register(UpdateScheduledTaskTool).unwrap();
            registry.register(DeleteScheduledTaskTool).unwrap();
        }
        registry.register(ContinueConversationTool).unwrap();
        registry.register(EndConversationTool).unwrap();
        registry
    }

    /// 返回允许用户开关的功能模块，供配置校验和管理页面共同使用。
    pub fn optional_definitions() -> Vec<OptionalToolDefinition> {
        vec![
            OptionalToolDefinition {
                name: WEB_SEARCH_MODULE,
                display_name: "网络搜索",
                description: "让主模型在需要实时信息时查询互联网。",
            },
            OptionalToolDefinition {
                name: MEMORY_MODULE,
                display_name: "记忆",
                description: "允许主模型维护角色记忆和用户记忆。",
            },
            OptionalToolDefinition {
                name: SCHEDULED_TASKS_MODULE,
                display_name: "定时任务",
                description: "允许主模型创建和管理当前会话的定时任务。",
            },
        ]
    }

    pub fn is_optional_tool(name: &str) -> bool {
        Self::optional_definitions()
            .iter()
            .any(|definition| definition.name == name)
    }

    fn is_enabled(enabled_optional_tools: &[String], module: &str) -> bool {
        enabled_optional_tools
            .iter()
            .any(|name| name.trim() == module)
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
                conversation_effect: output.conversation_effect,
            },
            Err(error) => ToolResult {
                tool_call_id: call.id,
                tool_name: call.name,
                // 保留完整错误链，既方便模型修正参数，也便于日志定位底层原因。
                content: format!("{:#}", error),
                requires_ai_response: true,
                is_error: true,
                conversation_effect: super::ConversationEffect::None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ToolRegistry;

    #[test]
    fn optional_modules_register_only_their_own_tools() {
        let disabled = ToolRegistry::built_in(&[]);
        assert!(disabled.get("agent_web_search").is_none());
        assert!(disabled.get("set_character_memory").is_none());
        assert!(disabled.get("create_scheduled_task").is_none());
        assert!(disabled.get("continue_conversation").is_some());

        let enabled = ToolRegistry::built_in(&[
            "agent_web_search".to_string(),
            "memory".to_string(),
            "scheduled_tasks".to_string(),
        ]);
        assert!(enabled.get("agent_web_search").is_some());
        for name in [
            "set_character_memory",
            "delete_character_memory",
            "create_user_memory",
            "update_user_memory",
            "delete_user_memory",
        ] {
            assert!(enabled.get(name).is_some(), "未注册记忆工具 {name}");
        }
        for name in [
            "create_scheduled_task",
            "get_scheduled_task",
            "update_scheduled_task",
            "delete_scheduled_task",
        ] {
            assert!(enabled.get(name).is_some(), "未注册定时任务工具 {name}");
        }
    }
}
