mod loader;
mod model;

pub(crate) use loader::read_metadata;
pub use loader::SkillCatalog;
pub use model::{SkillEntry, SkillMetadata};
