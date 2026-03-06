use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use anyhow::{Context, anyhow};
use serde_json::Value;

use crate::domains::{self, Domain};

/// Providers input describes the domain + provider configuration shipped in `--setup-input`.
#[derive(Debug)]
pub struct ProvidersInput {
    domain_providers: BTreeMap<Domain, BTreeMap<String, Value>>,
}

impl ProvidersInput {
    /// Load providers input from JSON or YAML.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)?;
        let value: Value = serde_json::from_str(&raw)
            .or_else(|_| serde_yaml_bw::from_str(&raw))
            .with_context(|| format!("parse providers input {}", path.display()))?;
        let map = parse_providers_value(&value)?;
        Ok(Self {
            domain_providers: map,
        })
    }

    /// Returns the configured providers for the selected domain.
    pub fn providers_for_domain(&self, domain: Domain) -> Option<&BTreeMap<String, Value>> {
        self.domain_providers.get(&domain)
    }
}

fn parse_providers_value(
    value: &Value,
) -> anyhow::Result<BTreeMap<Domain, BTreeMap<String, Value>>> {
    let map = match value.as_object() {
        Some(map) => map,
        None => {
            return Err(anyhow!(
                "providers input must be an object keyed by domain names"
            ));
        }
    };
    let mut result = BTreeMap::new();
    for (domain_key, entry) in map {
        let domain = match domain_from_str(domain_key) {
            Some(domain) => domain,
            None => {
                return Err(anyhow!(
                    "unknown domain '{domain_key}' in providers input (expected messaging|events|secrets|oauth)"
                ));
            }
        };
        let providers = match entry.as_object() {
            Some(map) => map,
            None => {
                return Err(anyhow!(
                    "providers for domain '{domain_key}' must be an object"
                ));
            }
        };
        let mut provider_map = BTreeMap::new();
        for (name, value) in providers {
            provider_map.insert(name.clone(), value.clone());
        }
        result.insert(domain, provider_map);
    }
    Ok(result)
}

fn domain_from_str(value: &str) -> Option<Domain> {
    match value.to_lowercase().as_str() {
        "messaging" => Some(Domain::Messaging),
        "events" => Some(Domain::Events),
        "secrets" => Some(Domain::Secrets),
        "oauth" => Some(Domain::OAuth),
        _ => None,
    }
}

/// Discover tenants inside the bundle for the requested domain.
pub fn discover_tenants(bundle: &Path, domain: Domain) -> anyhow::Result<Vec<String>> {
    let domain_dir = bundle.join(domains::domain_name(domain)).join("tenants");
    let general_dir = bundle.join("tenants");
    if let Some(tenants) = read_tenants(&domain_dir)? {
        return Ok(tenants);
    }
    if let Some(tenants) = read_tenants(&general_dir)? {
        return Ok(tenants);
    }
    Ok(Vec::new())
}

fn read_tenants(dir: &Path) -> anyhow::Result<Option<Vec<String>>> {
    if !dir.exists() {
        return Ok(None);
    }
    let mut tenants = BTreeSet::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
                tenants.insert(name.to_string());
            }
            continue;
        }
        if path.is_file()
            && let Some(stem) = path.file_stem().and_then(|value| value.to_str())
        {
            tenants.insert(stem.to_string());
        }
    }
    let tenants = tenants.into_iter().collect();
    Ok(Some(tenants))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn parse_providers_input() -> anyhow::Result<()> {
        let yaml = r#"
messaging:
  messaging-telegram:
    config: value
"#;
        let dir = TempDir::new()?;
        let path = dir.path().join("providers.json");
        std::fs::write(&path, yaml)?;
        let input = ProvidersInput::load(&path)?;
        let providers = input
            .providers_for_domain(Domain::Messaging)
            .expect("expected messaging providers");
        assert_eq!(
            providers.get("messaging-telegram"),
            Some(&json!({"config":"value"}))
        );
        Ok(())
    }

    #[test]
    fn discover_tenants_reads_dirs_and_files() -> anyhow::Result<()> {
        let bundle = TempDir::new()?;
        let domain_dir = bundle.path().join("messaging").join("tenants");
        fs::create_dir_all(&domain_dir)?;
        fs::create_dir_all(domain_dir.join("alpha"))?;
        std::fs::write(domain_dir.join("beta.json"), "{}")?;
        let tenants = discover_tenants(bundle.path(), Domain::Messaging)?;
        assert!(tenants.contains(&"alpha".to_string()));
        assert!(tenants.contains(&"beta".to_string()));
        Ok(())
    }

    #[test]
    fn discover_tenants_falls_back_to_general_dir() -> anyhow::Result<()> {
        let bundle = TempDir::new()?;
        let tenants_dir = bundle.path().join("tenants");
        fs::create_dir_all(tenants_dir.join("gamma"))?;
        let tenants = discover_tenants(bundle.path(), Domain::Events)?;
        assert_eq!(tenants, vec!["gamma".to_string()]);
        Ok(())
    }
}
