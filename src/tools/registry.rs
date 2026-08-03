use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{bail, Result};

use super::{
    CancelScheduledTaskTool, IgnoreMessagesTool, RecognizeImageTool, RememberTool,
    ScheduleTaskTool, Tool, ToolCall, ToolContext, ToolDefinition, ToolResult, WaitThenCheckTool,
    WebSearchTool,
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
        registry.register(RecognizeImageTool).unwrap();
        registry.register(WebSearchTool).unwrap();
        registry.register(RememberTool).unwrap();
        registry.register(WaitThenCheckTool).unwrap();
        registry.register(ScheduleTaskTool).unwrap();
        registry.register(CancelScheduledTaskTool).unwrap();
        registry.register(IgnoreMessagesTool).unwrap();
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

    /// 根据调用名称路由到对应工具。当前各工具只校验参数并返回未实现错误。
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
                is_error: false,
            },
            Err(error) => ToolResult {
                tool_call_id: call.id,
                tool_name: call.name,
                content: error.to_string(),
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
                "cancel_scheduled_task",
                "ignore_messages",
                "recognize_image",
                "remember",
                "schedule_task",
                "wait_then_check",
                "web_search",
            ]
        );
    }
}
