use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::Path,
};

use anyhow::{Context, Result};

use super::{SkillEntry, SkillMetadata};

const MAX_METADATA_BYTES: u64 = 64 * 1024;
const MAX_DIRECTORY_DEPTH: usize = 16;

/// 按名称稳定排序的启动时快照；不加载正文或执行任何 Skill。
#[derive(Debug, Default)]
pub struct SkillCatalog {
    entries: BTreeMap<String, SkillEntry>,
}

impl SkillCatalog {
    pub fn discover(root: &Path) -> Result<Self> {
        let mut catalog = Self::default();
        match fs::metadata(root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(catalog),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("读取 Skill 目录失败：{}", root.display()))
            }
            Ok(metadata) if !metadata.is_dir() => {
                anyhow::bail!("Skill 路径不是目录：{}", root.display())
            }
            Ok(_) => {}
        }
        let root = root.canonicalize()?;
        catalog.scan(&root, &root, 0)?;
        Ok(catalog)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn get(&self, name: &str) -> Option<&SkillEntry> {
        self.entries.get(name)
    }
    pub fn iter(&self) -> impl Iterator<Item = &SkillEntry> {
        self.entries.values()
    }

    /// 只保留配置中明确启用的 Skill；发现本地文件和启用状态彼此独立。
    pub fn retain_enabled(&mut self, enabled_names: &[String]) {
        self.entries
            .retain(|name, _| enabled_names.iter().any(|enabled| enabled == name));
    }

    /// 生成顺序稳定的 Skill 摘要；正文仍由模型按需通过读取工具获取。
    pub fn render_prompt_list(&self) -> String {
        if self.entries.is_empty() {
            return "- 暂无已启用的 Skill".to_string();
        }
        self.entries
            .values()
            .map(|entry| {
                let description = entry
                    .metadata
                    .description
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                format!(
                    "- 名称：{}；说明：{}；入口：{}",
                    entry.metadata.name, description, entry.virtual_path
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn scan(&mut self, root: &Path, directory: &Path, depth: usize) -> Result<()> {
        if depth > MAX_DIRECTORY_DEPTH {
            anyhow::bail!("Skill 目录嵌套过深：{}", directory.display());
        }
        let resolved = directory.canonicalize()?;
        if !resolved.starts_with(root) {
            anyhow::bail!("Skill 目录越界：{}", directory.display());
        }
        let mut paths = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
        paths.sort_by_key(|entry| entry.file_name());
        for entry in paths {
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                self.scan(root, &path, depth + 1)?;
            } else if kind.is_file() && entry.file_name() == "SKILL.md" {
                let path = path.canonicalize()?;
                if !path.starts_with(root) {
                    anyhow::bail!("Skill 文件越界：{}", path.display());
                }
                // 模型只看到以 /skills 为根的虚拟路径，不接触宿主机绝对路径。
                let relative_path = path
                    .strip_prefix(root)
                    .expect("规范化后的 Skill 文件应位于扫描根目录内")
                    .to_str()
                    .context("Skill 文件路径不是有效的 UTF-8")?
                    .replace('\\', "/");
                let virtual_path = format!("/{}/{relative_path}", super::DIRECTORY_NAME);
                let metadata = read_metadata(&path)
                    .with_context(|| format!("加载 Skill 失败：{}", path.display()))?;
                if let Some(previous) = self.entries.get(&metadata.name) {
                    anyhow::bail!(
                        "Skill 名称重复：{}（{} 与 {}）",
                        metadata.name,
                        previous.path.display(),
                        path.display()
                    );
                }
                self.entries.insert(
                    metadata.name.clone(),
                    SkillEntry {
                        metadata,
                        virtual_path,
                        path,
                    },
                );
            }
        }
        Ok(())
    }
}

pub(crate) fn read_metadata(path: &Path) -> Result<SkillMetadata> {
    let mut reader = BufReader::new(File::open(path)?.take(MAX_METADATA_BYTES));
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim_start_matches('\u{feff}').trim() != "---" {
        anyhow::bail!("SKILL.md 必须以 YAML 元数据分隔符 --- 开始");
    }
    let mut yaml = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            anyhow::bail!("元数据缺少结束分隔符 ---，或超过 64 KiB");
        }
        if line.trim() == "---" {
            break;
        }
        yaml.push_str(&line);
    }
    let mut metadata: SkillMetadata =
        serde_yaml::from_str(&yaml).context("Skill YAML 元数据格式错误")?;
    metadata.name = metadata.name.trim().to_string();
    metadata.description = metadata.description.trim().to_string();
    if metadata.name.is_empty()
        || !metadata
            .name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
    {
        anyhow::bail!("Skill name 仅允许小写英文字母、数字、连字符和下划线，且不能为空");
    }
    if metadata.description.is_empty() {
        anyhow::bail!("Skill description 不能为空");
    }
    Ok(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_nested_metadata_without_reading_body_and_rejects_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("bundle/review");
        fs::create_dir_all(&nested).unwrap();
        let mut data =
            b"\xef\xbb\xbf---\r\nname: review\r\ndescription: >\r\n  review memories\r\n---\r\n"
                .to_vec();
        data.extend(vec![0xff; 128 * 1024]); // 正文不是 UTF-8 也不影响仅加载元数据。
        fs::write(nested.join("SKILL.md"), data).unwrap();
        let catalog = SkillCatalog::discover(dir.path()).unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(
            catalog.get("review").unwrap().metadata.description,
            "review memories"
        );
        fs::write(
            dir.path().join("SKILL.md"),
            "---\nname: review\ndescription: duplicate\n---\n",
        )
        .unwrap();
        assert!(SkillCatalog::discover(dir.path()).is_err());
        assert!(SkillCatalog::discover(&dir.path().join("missing"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn rejects_invalid_and_unbounded_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        for content in [
            "name: a",
            "---\nname: a\n---\n",
            "---\nname: a\ndescription: [bad]\n---\n",
        ] {
            fs::write(&path, content).unwrap();
            assert!(SkillCatalog::discover(dir.path()).is_err());
        }
        fs::write(
            &path,
            format!(
                "---\nname: a\ndescription: {}\n---\n",
                "a".repeat(MAX_METADATA_BYTES as usize)
            ),
        )
        .unwrap();
        assert!(SkillCatalog::discover(dir.path()).is_err());
    }
}
