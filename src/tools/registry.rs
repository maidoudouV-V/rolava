use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{bail, Result};

use super::{
    AgentWebSearchTool, CreateScheduledTaskTool, DeleteGroupMemoryTool, DeleteScheduledTaskTool,
    DeleteUserMemoryTool, GetScheduledTaskTool, ReadContentTool, RunScriptTool,
    SendQqExpressionTool, SetConversationStateTool, SetGroupMemoryTool, SetUserMemoryTool, Tool,
    ToolCall, ToolContext, ToolDefinition, ToolResult, UpdateScheduledTaskTool, WaitForReplyTool,
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
    pub fn built_in(enabled_optional_tools: &[String], group_conversation: bool) -> Self {
        let mut registry = Self::new();
        if Self::is_enabled(enabled_optional_tools, WEB_SEARCH_MODULE) {
            registry.register(AgentWebSearchTool).unwrap();
        }
        registry.register(SendQqExpressionTool).unwrap();
        registry.register(ReadContentTool).unwrap();
        registry.register(RunScriptTool).unwrap();
        if Self::is_enabled(enabled_optional_tools, MEMORY_MODULE) {
            // 群记忆只属于群聊；用户记忆在群聊和私聊中都可维护。
            if group_conversation {
                registry.register(SetGroupMemoryTool).unwrap();
                registry.register(DeleteGroupMemoryTool).unwrap();
            }
            registry.register(SetUserMemoryTool).unwrap();
            registry.register(DeleteUserMemoryTool).unwrap();
        }
        registry.register(WaitForReplyTool::new()).unwrap();
        registry.register(CreateScheduledTaskTool).unwrap();
        registry.register(GetScheduledTaskTool).unwrap();
        registry.register(UpdateScheduledTaskTool).unwrap();
        registry.register(DeleteScheduledTaskTool).unwrap();
        if group_conversation {
            // 状态工具只控制群聊的 AI 前置过滤，私聊不需要注册。
            registry.register(SetConversationStateTool).unwrap();
        }
        registry
    }

    /// 返回允许用户开关的功能模块，供配置校验和管理页面共同使用。
    pub fn optional_definitions() -> Vec<OptionalToolDefinition> {
        vec![
            OptionalToolDefinition {
                name: WEB_SEARCH_MODULE,
                display_name: "网络搜索",
                description: "启用网络搜索工具及相关提示词，保存并重启后生效。",
            },
            OptionalToolDefinition {
                name: MEMORY_MODULE,
                display_name: "记忆",
                description: "启用记忆工具、记忆上下文与提示词及自动整理；关闭不删除已有记忆，保存并重启后生效。",
            },
        ]
    }

    pub fn is_optional_tool(name: &str) -> bool {
        Self::optional_definitions()
            .iter()
            .any(|definition| definition.name == name)
    }

    pub fn is_enabled(enabled_optional_tools: &[String], module: &str) -> bool {
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

    /// 按名称选择当前注册器中已有的工具；未注册或未启用的名称会被忽略。
    /// 返回的注册器仅能定义和执行选中的工具，不会重新启用已关闭的模块。
    /// 工具实例及其内部状态与原注册器共享，不应跨独立会话复用有状态工具。
    pub fn select(&self, names: &[&str]) -> Self {
        Self {
            tools: self
                .tools
                .iter()
                .filter(|(name, _)| names.contains(name))
                .map(|(name, tool)| (*name, tool.clone()))
                .collect(),
        }
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
