use anyhow::{bail, Context, Result};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufReader, Write};
use std::path::Path;
use std::time::Duration;
use tempfile::NamedTempFile;
use tracing::{debug, info, warn};

const CATALOG_URL: &str = "https://models.dev/catalog.json?type=all";
const MAX_CATALOG_BYTES: usize = 50 * 1024 * 1024;

/// 参考价格，单位为美元 / 百万 tokens；缺失字段不代表免费。
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ModelPricing {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

impl ModelPricing {
    pub fn merge_missing(&mut self, other: &Self) {
        self.input = self.input.or(other.input);
        self.output = self.output.or(other.output);
        self.cache_read = self.cache_read.or(other.cache_read);
        self.cache_write = self.cache_write.or(other.cache_write);
    }

    fn sanitized(mut self) -> Self {
        for value in [
            &mut self.input,
            &mut self.output,
            &mut self.cache_read,
            &mut self.cache_write,
        ] {
            *value = value.filter(|price| price.is_finite() && *price >= 0.0);
        }
        self
    }

    fn is_empty(&self) -> bool {
        self.input.is_none()
            && self.output.is_none()
            && self.cache_read.is_none()
            && self.cache_write.is_none()
    }
}

#[derive(Clone, Debug, Default)]
pub struct ModelMetadata {
    pub vision: Option<bool>,
    pub pricing: ModelPricing,
}

/// 本地快照在启动时加载；在线更新失败时保留已有资料。
#[derive(Default)]
pub struct ModelMetadataStore {
    catalog: RwLock<Option<ModelCatalog>>,
}

impl ModelMetadataStore {
    pub fn load_local(path: &Path) -> Self {
        let catalog = match File::open(path) {
            Ok(file) => match ModelCatalog::from_reader(BufReader::new(file)) {
                Ok(catalog) => {
                    info!(models = catalog.model_count, "已加载本地模型资料");
                    Some(catalog)
                }
                Err(error) => {
                    warn!(error = %format!("{error:#}"), "读取本地模型资料失败");
                    None
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                debug!("尚无本地模型资料，启动时尝试在线下载");
                None
            }
            Err(error) => {
                warn!(error = %error, "打开本地模型资料失败");
                None
            }
        };
        Self {
            catalog: RwLock::new(catalog),
        }
    }

    pub async fn refresh(&self, path: &Path) -> Result<()> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()?;
        let mut response = client.get(CATALOG_URL).send().await?.error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len() + chunk.len() > MAX_CATALOG_BYTES {
                bail!("模型资料库超过允许的文件大小");
            }
            bytes.extend_from_slice(&chunk);
        }
        let path = path.to_path_buf();
        let (catalog, saved) = tokio::task::spawn_blocking(move || -> Result<_> {
            let catalog = ModelCatalog::from_reader(bytes.as_slice())?;
            let saved = (|| -> Result<()> {
                let parent = path.parent().unwrap_or_else(|| Path::new("."));
                fs::create_dir_all(parent)?;
                let mut file = NamedTempFile::new_in(parent)?;
                file.write_all(&bytes)?;
                file.flush()?;
                file.persist(&path).map_err(|error| error.error)?;
                Ok(())
            })();
            Ok((catalog, saved))
        })
        .await??;
        let model_count = catalog.model_count;
        *self.catalog.write() = Some(catalog);
        saved.context("在线模型资料已加载，但保存本地快照失败")?;
        info!(models = model_count, "模型资料库更新完成");
        Ok(())
    }

    pub fn lookup(&self, provider_type: &str, base_url: &str, id: &str) -> Option<ModelMetadata> {
        self.catalog
            .read()
            .as_ref()?
            .lookup(provider_type, base_url, id)
    }
}

#[derive(Deserialize)]
struct RawCatalog {
    providers: BTreeMap<String, RawProvider>,
    models: BTreeMap<String, RawModel>,
}

#[derive(Deserialize)]
struct RawProvider {
    api: Option<String>,
    models: BTreeMap<String, RawModel>,
}

#[derive(Deserialize)]
struct RawModel {
    #[serde(default)]
    name: String,
    canonical_model_id: Option<String>,
    modalities: Option<RawModalities>,
    cost: Option<ModelPricing>,
}

#[derive(Deserialize)]
struct RawModalities {
    input: Option<Vec<String>>,
}

impl RawModel {
    fn metadata(&self) -> ModelMetadata {
        ModelMetadata {
            vision: self.modalities.as_ref().and_then(|modalities| {
                modalities.input.as_ref().map(|input| {
                    input
                        .iter()
                        .any(|value| value.eq_ignore_ascii_case("image"))
                })
            }),
            pricing: self.cost.clone().unwrap_or_default().sanitized(),
        }
    }
}

#[derive(Default)]
struct AliasIndex {
    exact: HashMap<String, BTreeSet<String>>,
    normalized: HashMap<String, BTreeSet<String>>,
}

impl AliasIndex {
    fn insert(&mut self, alias: &str, target: &str) {
        if alias.trim().is_empty() {
            return;
        }
        self.exact
            .entry(alias.to_string())
            .or_default()
            .insert(target.to_string());
        self.normalized
            .entry(normalize_alias(alias))
            .or_default()
            .insert(target.to_string());
    }

    /// 有候选但不唯一时保留歧义，不继续使用更弱的匹配规则。
    fn find(&self, alias: &str) -> Option<&BTreeSet<String>> {
        self.exact
            .get(alias)
            .or_else(|| self.normalized.get(&normalize_alias(alias)))
    }
}

#[derive(Default)]
struct ProviderIndex {
    models: BTreeMap<String, ModelMetadata>,
    ids: AliasIndex,
    names: AliasIndex,
}

#[derive(Default)]
struct ReferenceGroup {
    base: Option<ModelMetadata>,
    entries: Vec<(String, String, ModelMetadata)>,
}

struct ModelCatalog {
    providers: BTreeMap<String, ProviderIndex>,
    hosts: HashMap<String, BTreeSet<String>>,
    authors: BTreeSet<String>,
    ids: AliasIndex,
    names: AliasIndex,
    references: HashMap<String, ModelMetadata>,
    model_count: usize,
}

impl ModelCatalog {
    fn from_reader(reader: impl std::io::Read) -> Result<Self> {
        let raw: RawCatalog = serde_json::from_reader(reader).context("解析模型资料库失败")?;
        let model_count: usize = raw
            .providers
            .values()
            .map(|provider| provider.models.len())
            .sum();
        if model_count == 0 || raw.models.is_empty() {
            bail!("模型资料库缺少有效的供应商或模型信息");
        }
        let mut catalog = Self {
            providers: BTreeMap::new(),
            hosts: HashMap::new(),
            authors: BTreeSet::new(),
            ids: AliasIndex::default(),
            names: AliasIndex::default(),
            references: HashMap::new(),
            model_count,
        };
        let mut base_ids = AliasIndex::default();
        let mut groups: BTreeMap<String, ReferenceGroup> = BTreeMap::new();
        for (id, model) in &raw.models {
            let (author, slug) = id.split_once('/').unwrap_or(("", id));
            catalog.authors.insert(author.to_ascii_lowercase());
            for alias in [id.as_str(), slug] {
                base_ids.insert(alias, id);
                catalog.ids.insert(alias, id);
            }
            catalog.names.insert(&model.name, id);
            groups.entry(id.clone()).or_default().base = Some(model.metadata());
        }
        for (provider_id, provider) in raw.providers {
            if let Some(host) = provider.api.as_deref().and_then(url_host) {
                catalog
                    .hosts
                    .entry(host)
                    .or_default()
                    .insert(provider_id.clone());
            }
            let mut index = ProviderIndex::default();
            for (id, model) in provider.models {
                let qualified_id = format!("{provider_id}/{id}");
                let canonical = model.canonical_model_id.clone().unwrap_or_else(|| {
                    if raw.models.contains_key(&qualified_id) {
                        qualified_id.clone()
                    } else {
                        base_ids
                            .find(&id)
                            .filter(|targets| targets.len() == 1)
                            .and_then(|targets| targets.first())
                            .cloned()
                            .unwrap_or_else(|| qualified_id.clone())
                    }
                });
                let mut metadata = model.metadata();
                if metadata.vision.is_none() {
                    metadata.vision = raw
                        .models
                        .get(&canonical)
                        .and_then(|base| base.metadata().vision);
                }
                index.ids.insert(&id, &id);
                index.names.insert(&model.name, &id);
                index.models.insert(id.clone(), metadata.clone());
                for alias in [id.as_str(), id.rsplit('/').next().unwrap_or(&id)] {
                    catalog.ids.insert(alias, &canonical);
                }
                catalog.names.insert(&model.name, &canonical);
                groups.entry(canonical).or_default().entries.push((
                    provider_id.clone(),
                    id,
                    metadata,
                ));
            }
            catalog.providers.insert(provider_id, index);
        }
        for (host, provider) in [
            ("api.openai.com", "openai"),
            ("generativelanguage.googleapis.com", "google"),
            ("api.anthropic.com", "anthropic"),
        ] {
            catalog
                .hosts
                .entry(host.into())
                .or_default()
                .insert(provider.into());
        }
        for (id, mut group) in groups {
            let (author, slug) = id.split_once('/').unwrap_or(("", &id));
            group.entries.sort_by(|left, right| {
                let rank = |entry: &(String, String, ModelMetadata)| {
                    (
                        if entry.0 == author {
                            0
                        } else if entry.0 == "openrouter" {
                            2
                        } else {
                            1
                        },
                        if entry.1 == slug || entry.1 == id {
                            0
                        } else {
                            1
                        },
                    )
                };
                rank(left)
                    .cmp(&rank(right))
                    .then_with(|| left.0.cmp(&right.0))
                    .then_with(|| left.1.cmp(&right.1))
            });
            let mut metadata = group.base.unwrap_or_default();
            if metadata.vision.is_none() {
                let visions: BTreeSet<bool> = group
                    .entries
                    .iter()
                    .filter_map(|entry| entry.2.vision)
                    .collect();
                if visions.len() == 1 {
                    metadata.vision = visions.first().copied();
                }
            }
            if metadata.pricing.is_empty() {
                if let Some(entry) = group
                    .entries
                    .iter()
                    .find(|entry| !entry.2.pricing.is_empty())
                {
                    metadata.pricing = entry.2.pricing.clone();
                }
            }
            catalog.references.insert(id, metadata);
        }
        Ok(catalog)
    }

    fn lookup(&self, provider_type: &str, base_url: &str, id: &str) -> Option<ModelMetadata> {
        if provider_type == "openrouter" {
            return None;
        }
        let base_url = if base_url.trim().is_empty() {
            if provider_type == "gemini" {
                "https://generativelanguage.googleapis.com/v1beta"
            } else {
                "https://api.openai.com/v1"
            }
        } else {
            base_url.trim()
        };
        if let Some(provider) = url_host(base_url)
            .and_then(|host| self.hosts.get(&host))
            .filter(|providers| providers.len() == 1)
            .and_then(|providers| providers.first())
            .and_then(|provider| self.providers.get(provider))
        {
            if let Some(targets) = provider.ids.find(id).or_else(|| provider.names.find(id)) {
                if targets.len() != 1 {
                    return None;
                }
                return provider.models.get(targets.first()?).cloned();
            }
        }
        if let Some(targets) = self.ids.find(id) {
            return self.reference(targets, None);
        }
        if let Some((namespace, slug)) = id.rsplit_once('/') {
            let author = namespace
                .rsplit('/')
                .next()
                .unwrap_or(namespace)
                .to_ascii_lowercase();
            let author = self.authors.contains(&author).then_some(author);
            if let Some(targets) = self.ids.find(slug) {
                return self.reference(targets, author.as_deref());
            }
        }
        self.names
            .find(id)
            .and_then(|targets| self.reference(targets, None))
    }

    fn reference(&self, targets: &BTreeSet<String>, author: Option<&str>) -> Option<ModelMetadata> {
        if targets.len() != 1 {
            return None;
        }
        let id = targets.first()?;
        if author.is_some_and(|author| {
            id.split_once('/')
                .is_none_or(|(actual, _)| actual != author)
        }) {
            return None;
        }
        self.references.get(id).cloned()
    }
}

fn url_host(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()?
        .host_str()
        .map(str::to_ascii_lowercase)
}

/// 仅规范化大小写、空白及 -_. 分隔符，保留数字段、顺序和其它符号。
/// 不删除日期、规格、预览标记或路由后缀，也不使用编辑距离猜测型号。
fn normalize_alias(alias: &str) -> String {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut previous_kind = 0;
    for character in alias.trim().chars().flat_map(char::to_lowercase) {
        let kind = if character.is_ascii_digit() {
            1
        } else if character.is_alphabetic() {
            2
        } else {
            3
        };
        let separator = character.is_whitespace() || matches!(character, '-' | '_' | '.');
        if separator || kind != previous_kind || kind == 3 {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        }
        if !separator {
            token.push(character);
        }
        previous_kind = if separator { 0 } else { kind };
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens.join("|")
}
