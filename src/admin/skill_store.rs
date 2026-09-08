use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use tempfile::NamedTempFile;

use crate::skills::{read_metadata, SkillCatalog, SkillEntry};

const MAX_SKILL_FILE_BYTES: u64 = 1024 * 1024;
const MAX_SKILL_FILES: usize = 2_000;
const MAX_DIRECTORY_DEPTH: usize = 24;

#[derive(Debug, Serialize)]
pub struct AdminSkillSummary {
    pub name: String,
    pub description: String,
    pub entry_path: String,
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
pub struct AdminSkillFile {
    pub path: String,
    pub size: u64,
}

/// 每次调用都重新扫描磁盘，使管理页面刷新能够发现新增、删除和改名的 Skill。
pub fn list_skills(root: &Path, enabled_names: &[String]) -> Result<Vec<AdminSkillSummary>> {
    let enabled_names = enabled_names
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    Ok(SkillCatalog::discover(root)?
        .iter()
        .map(|entry| AdminSkillSummary {
            name: entry.metadata.name.clone(),
            description: entry.metadata.description.clone(),
            entry_path: entry.virtual_path.clone(),
            enabled: enabled_names.contains(entry.metadata.name.as_str()),
        })
        .collect())
}

/// 删除不存在和重复的名称，只返回当前磁盘上确实存在的 Skill。
pub fn normalize_enabled_names(root: &Path, requested: Vec<String>) -> Result<Vec<String>> {
    let catalog = SkillCatalog::discover(root)?;
    let mut seen = HashSet::new();
    Ok(requested
        .into_iter()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty() && catalog.get(name).is_some())
        .filter(|name| seen.insert(name.clone()))
        .collect())
}

pub fn list_skill_files(root: &Path, skill_name: &str) -> Result<Vec<AdminSkillFile>> {
    let entry = find_skill(root, skill_name)?;
    let skill_root = skill_root(&entry)?;
    let mut files = Vec::new();
    scan_files(&skill_root, &skill_root, 0, &mut files)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

pub fn read_skill_file(root: &Path, skill_name: &str, relative_path: &str) -> Result<String> {
    let entry = find_skill(root, skill_name)?;
    let path = resolve_existing_file(&entry, relative_path)?;
    let metadata = fs::metadata(&path)?;
    if metadata.len() > MAX_SKILL_FILE_BYTES {
        anyhow::bail!("文件超过管理页面读取上限 1 MiB");
    }
    fs::read_to_string(&path).with_context(|| format!("文件不是有效的 UTF-8 文本：{relative_path}"))
}

pub fn write_skill_file(
    root: &Path,
    skill_name: &str,
    relative_path: &str,
    content: &str,
) -> Result<()> {
    if content.len() as u64 > MAX_SKILL_FILE_BYTES {
        anyhow::bail!("文件超过管理页面保存上限 1 MiB");
    }
    let entry = find_skill(root, skill_name)?;
    let path = resolve_existing_file(&entry, relative_path)?;
    let mut file = NamedTempFile::new_in(path.parent().context("文件缺少父目录")?)?;
    file.write_all(content.as_bytes())?;
    file.flush()?;

    // 修改入口文件时先校验元数据，避免保存后导致整个 Skill 扫描失败。
    if Path::new(relative_path.trim()) == Path::new("SKILL.md") {
        let metadata = read_metadata(file.path()).context("SKILL.md 元数据校验失败")?;
        let catalog = SkillCatalog::discover(root)?;
        if let Some(existing) = catalog.get(&metadata.name) {
            if existing.path != entry.path {
                anyhow::bail!("Skill 名称已被其他目录使用：{}", metadata.name);
            }
        }
    }
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn find_skill(root: &Path, skill_name: &str) -> Result<SkillEntry> {
    let skill_name = skill_name.trim();
    if skill_name.is_empty() {
        anyhow::bail!("Skill 名称不能为空");
    }
    SkillCatalog::discover(root)?
        .get(skill_name)
        .cloned()
        .with_context(|| format!("找不到 Skill：{skill_name}"))
}

fn skill_root(entry: &SkillEntry) -> Result<PathBuf> {
    entry
        .path
        .parent()
        .map(Path::to_path_buf)
        .context("Skill 入口文件缺少父目录")
}

fn resolve_existing_file(entry: &SkillEntry, relative_path: &str) -> Result<PathBuf> {
    // 依次拒绝绝对路径、路径跳转、符号链接、目录目标和越出当前 Skill 的文件。
    let relative_path = validate_relative_file_path(relative_path)?;
    let root = fs::canonicalize(skill_root(entry)?)?;
    let requested = root.join(&relative_path);
    let file_type = fs::symlink_metadata(&requested)
        .with_context(|| format!("文件不存在：{}", relative_path.display()))?
        .file_type();
    if file_type.is_symlink() {
        anyhow::bail!("不允许编辑符号链接文件");
    }
    let path = fs::canonicalize(&requested)?;
    if !path.starts_with(&root) {
        anyhow::bail!("文件路径超出当前 Skill 目录");
    }
    if !fs::metadata(&path)?.is_file() {
        anyhow::bail!("目标路径不是文件");
    }
    Ok(path)
}

fn validate_relative_file_path(raw_path: &str) -> Result<PathBuf> {
    let path = raw_path.trim();
    if path.is_empty() || path.contains('\\') {
        anyhow::bail!("文件路径必须是使用正斜杠的非空相对路径");
    }
    let path = Path::new(path);
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!("文件路径不能包含根目录、盘符或路径跳转");
    }
    Ok(path.to_path_buf())
}

fn scan_files(
    root: &Path,
    directory: &Path,
    depth: usize,
    output: &mut Vec<AdminSkillFile>,
) -> Result<()> {
    if depth > MAX_DIRECTORY_DEPTH {
        anyhow::bail!("Skill 文件夹嵌套过深：{}", directory.display());
    }
    let mut entries = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            let name = entry.file_name();
            if name == "node_modules" || name == ".git" {
                continue;
            }
            scan_files(root, &path, depth + 1, output)?;
        } else if file_type.is_file() {
            if output.len() >= MAX_SKILL_FILES {
                anyhow::bail!("Skill 文件数量超过管理页面上限 {MAX_SKILL_FILES}");
            }
            let relative = path
                .strip_prefix(root)?
                .to_str()
                .context("Skill 文件路径不是有效的 UTF-8")?
                .replace('\\', "/");
            output.push(AdminSkillFile {
                path: relative,
                size: entry.metadata()?.len(),
            });
        }
    }
    Ok(())
}
