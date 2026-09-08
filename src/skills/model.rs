use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 仅描述 Skill，不授予工具权限，也不决定执行模型。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub metadata: SkillMetadata,
    /// 提供给模型使用的项目虚拟绝对路径，不暴露宿主机真实路径。
    pub virtual_path: String,
    pub path: PathBuf,
}
