mod loader;
mod model;

/// Skill 在项目工作目录中的固定文件夹名称。
pub(crate) const DIRECTORY_NAME: &str = "skills";

pub(crate) use loader::read_metadata;
pub use loader::SkillCatalog;
pub use model::{SkillEntry, SkillMetadata};
