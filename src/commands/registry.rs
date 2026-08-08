use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{bail, Result};

use super::Command;

/// 已注册命令的稳定有序集合，负责名称唯一性和命令查找。
#[derive(Default)]
pub struct CommandRegistry {
    commands: BTreeMap<&'static str, Arc<dyn Command>>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<T>(&mut self, command: T) -> Result<()>
    where
        T: Command + 'static,
    {
        let name = command.name();
        let description = command.description();
        if name.is_empty() || name.contains(char::is_whitespace) || name.contains('/') {
            bail!("命令名称无效：{}", name);
        }
        if description.trim().is_empty() {
            bail!("命令 {} 缺少说明", name);
        }
        if self.commands.contains_key(name) {
            bail!("命令 {} 重复注册", name);
        }
        self.commands.insert(name, Arc::new(command));
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Command>> {
        self.commands.get(name).cloned()
    }
}
