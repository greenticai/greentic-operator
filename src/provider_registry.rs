use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use oci_distribution::Reference;
use oci_distribution::client::{Client, ClientConfig, ClientProtocol};
use oci_distribution::manifest::{
    IMAGE_MANIFEST_LIST_MEDIA_TYPE, IMAGE_MANIFEST_MEDIA_TYPE, OCI_IMAGE_INDEX_MEDIA_TYPE,
    OCI_IMAGE_MEDIA_TYPE,
};
use oci_distribution::secrets::RegistryAuth;
use serde::{Deserialize, Serialize};

pub fn resolve_catalog_path(
    catalog_file: Option<PathBuf>,
    provider_registry_ref: Option<&str>,
    offline: bool,
    refresh: bool,
    bundle: &Path,
) -> anyhow::Result<Option<PathBuf>> {
    if let Some(path) = catalog_file {
        return Ok(Some(path));
    }
    let Some(reference) = provider_registry_ref
        .map(str::trim)
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };

    if let Some(path) = parse_local_registry_ref(reference) {
        if path.exists() {
            return Ok(Some(path));
        }
        return Err(anyhow!(
            "provider registry path {} does not exist",
            path.display()
        ));
    }

    let cached = cache_path_for_ref(bundle, reference);
    let cached_by_digest = resolve_cached_by_digest(bundle, reference)?;
    if !offline && refresh {
        match fetch_remote_registry_to_cache(bundle, reference) {
            Ok(path) => return Ok(Some(path)),
            Err(err) => {
                if cached.exists() {
                    return Ok(Some(cached));
                }
                if let Some(by_digest) = cached_by_digest {
                    return Ok(Some(by_digest));
                }
                return Err(anyhow!(
                    "provider registry {} unavailable and no cached copy found at {} (cause: {}). Use --provider-registry file://<path> or local path.",
                    reference,
                    cache_path_for_ref(bundle, reference).display(),
                    err
                ));
            }
        }
    }
    if cached.exists() {
        return Ok(Some(cached));
    }
    if let Some(by_digest) = cached_by_digest {
        return Ok(Some(by_digest));
    }

    if offline {
        return Err(anyhow!(
            "Provider registry unavailable and no cached copy found. Re-run without --offline or set GTC_PROVIDER_REGISTRY_REF to a local file."
        ));
    }
    match fetch_remote_registry_to_cache(bundle, reference) {
        Ok(path) => Ok(Some(path)),
        Err(err) => Err(anyhow!(
            "provider registry {} unavailable and no cached copy found at {} (cause: {}). Use --provider-registry file://<path> or local path.",
            reference,
            cached.display(),
            err
        )),
    }
}

pub fn cache_registry_file(
    bundle: &Path,
    reference: &str,
    source: &Path,
) -> anyhow::Result<PathBuf> {
    let destination = cache_path_for_ref(bundle, reference);
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("invalid cache destination {}", destination.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create registry cache dir {}", parent.display()))?;
    std::fs::copy(source, &destination).with_context(|| {
        format!(
            "copy provider registry cache from {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(destination)
}

fn parse_local_registry_ref(reference: &str) -> Option<PathBuf> {
    if let Some(path) = reference.strip_prefix("file://") {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return None;
        }
        return Some(PathBuf::from(trimmed));
    }
    if reference.contains("://") {
        return None;
    }
    Some(PathBuf::from(reference))
}

fn cache_path_for_ref(bundle: &Path, reference: &str) -> PathBuf {
    bundle
        .join(".greentic")
        .join("cache")
        .join("provider-registry")
        .join(format!("{}.json", slug(reference)))
}

fn cache_path_for_digest(bundle: &Path, digest: &str) -> PathBuf {
    bundle
        .join(".greentic")
        .join("cache")
        .join("provider-registry")
        .join("by-digest")
        .join(format!("{}.json", slug(digest)))
}

fn cache_index_path(bundle: &Path) -> PathBuf {
    bundle
        .join(".greentic")
        .join("cache")
        .join("provider-registry")
        .join("index.json")
}

fn resolve_cached_by_digest(bundle: &Path, reference: &str) -> anyhow::Result<Option<PathBuf>> {
    let index = load_cache_index(bundle)?;
    let Some(digest) = index.refs.get(reference) else {
        return Ok(None);
    };
    let path = cache_path_for_digest(bundle, digest);
    if path.exists() {
        return Ok(Some(path));
    }
    Ok(None)
}

fn cached_digest_for_reference(bundle: &Path, reference: &str) -> anyhow::Result<Option<String>> {
    let index = load_cache_index(bundle)?;
    Ok(index.refs.get(reference).cloned())
}

fn resolve_existing_cache_for_digest(
    bundle: &Path,
    reference: &str,
    digest: &str,
) -> anyhow::Result<Option<PathBuf>> {
    let by_digest = cache_path_for_digest(bundle, digest);
    if by_digest.exists() {
        return Ok(Some(by_digest));
    }
    let by_ref = cache_path_for_ref(bundle, reference);
    if by_ref.exists() {
        return Ok(Some(by_ref));
    }
    Ok(None)
}

fn fetch_remote_registry_to_cache(bundle: &Path, reference: &str) -> anyhow::Result<PathBuf> {
    use greentic_distributor_client::{
        OciPackFetcher, PackFetchOptions, oci_packs::DefaultRegistryClient,
    };

    let mapped = map_remote_registry_ref(reference)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for provider registry fetch")?;

    let fetcher: OciPackFetcher<DefaultRegistryClient> = OciPackFetcher::new(PackFetchOptions {
        allow_tags: true,
        offline: false,
        ..PackFetchOptions::default()
    });
    let prior_digest = cached_digest_for_reference(bundle, reference)?;
    match rt.block_on(fetcher.fetch_pack_to_cache(&mapped)) {
        Ok(fetched) => {
            if prior_digest.as_deref() == Some(fetched.resolved_digest.as_str())
                && let Some(existing) =
                    resolve_existing_cache_for_digest(bundle, reference, &fetched.resolved_digest)?
            {
                return Ok(existing);
            }
            cache_remote_registry_file(
                bundle,
                reference,
                fetched.resolved_digest.as_str(),
                std::fs::read(&fetched.path)
                    .with_context(|| format!("read fetched registry {}", fetched.path.display()))?,
            )
        }
        Err(primary_err) => {
            let (bytes, digest) = rt
                .block_on(fetch_registry_bytes_via_oci(&mapped))
                .with_context(|| format!("fetch provider registry {reference}"))
                .with_context(|| format!("primary fetch error: {primary_err}"))?;
            if prior_digest.as_deref() == Some(digest.as_str())
                && let Some(existing) =
                    resolve_existing_cache_for_digest(bundle, reference, digest.as_str())?
            {
                return Ok(existing);
            }
            cache_remote_registry_file(bundle, reference, &digest, bytes)
        }
    }
}

fn map_remote_registry_ref(reference: &str) -> anyhow::Result<String> {
    let trimmed = reference.trim();
    if let Some(rest) = trimmed.strip_prefix("oci://") {
        return Ok(rest.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("repo://") {
        return map_registry_target(rest, std::env::var("GREENTIC_REPO_REGISTRY_BASE").ok())
            .ok_or_else(|| {
                anyhow!(
                    "repo:// reference {trimmed} requires GREENTIC_REPO_REGISTRY_BASE to map to OCI"
                )
            });
    }
    if let Some(rest) = trimmed.strip_prefix("store://") {
        return map_registry_target(rest, std::env::var("GREENTIC_STORE_REGISTRY_BASE").ok())
            .ok_or_else(|| {
                anyhow!(
                    "store:// reference {trimmed} requires GREENTIC_STORE_REGISTRY_BASE to map to OCI"
                )
            });
    }
    if trimmed.contains("://") {
        return Err(anyhow!(
            "unsupported provider registry scheme for {}; expected oci://, repo://, store://, file://, or local path",
            reference
        ));
    }
    Ok(trimmed.to_string())
}

fn map_registry_target(target: &str, base: Option<String>) -> Option<String> {
    if looks_like_explicit_oci_ref(target) {
        return Some(target.to_string());
    }
    let base = base?;
    let normalized_base = base.trim_end_matches('/');
    let normalized_target = target.trim_start_matches('/');
    Some(format!("{normalized_base}/{normalized_target}"))
}

fn looks_like_explicit_oci_ref(target: &str) -> bool {
    if !(target.contains('/') && (target.contains('@') || target.contains(':'))) {
        return false;
    }
    let registry = target.split('/').next().unwrap_or_default();
    registry.contains('.') || registry.contains(':') || registry == "localhost"
}

fn cache_remote_registry_file(
    bundle: &Path,
    reference: &str,
    digest: &str,
    bytes: Vec<u8>,
) -> anyhow::Result<PathBuf> {
    let digest_path = cache_path_for_digest(bundle, digest);
    if let Some(parent) = digest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&digest_path, &bytes)
        .with_context(|| format!("write digest cache {}", digest_path.display()))?;

    let ref_path = cache_path_for_ref(bundle, reference);
    if let Some(parent) = ref_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&ref_path, &bytes).with_context(|| format!("write {}", ref_path.display()))?;

    let mut index = load_cache_index(bundle)?;
    index.refs.insert(reference.to_string(), digest.to_string());
    write_cache_index(bundle, &index)?;
    Ok(ref_path)
}

async fn fetch_registry_bytes_via_oci(mapped_ref: &str) -> anyhow::Result<(Vec<u8>, String)> {
    let reference = Reference::try_from(mapped_ref)
        .map_err(|err| anyhow!("invalid OCI reference {}: {}", mapped_ref, err))?;
    let client = Client::new(ClientConfig {
        protocol: ClientProtocol::Https,
        ..Default::default()
    });
    let accepted = vec![
        "application/vnd.oci.artifact.manifest.v1+json",
        OCI_IMAGE_MEDIA_TYPE,
        OCI_IMAGE_INDEX_MEDIA_TYPE,
        IMAGE_MANIFEST_MEDIA_TYPE,
        IMAGE_MANIFEST_LIST_MEDIA_TYPE,
        "application/vnd.docker.distribution.manifest.v2+json",
        "application/vnd.docker.distribution.manifest.list.v2+json",
    ];
    let image = client
        .pull(&reference, &RegistryAuth::Anonymous, accepted)
        .await
        .with_context(|| format!("pull OCI reference {}", mapped_ref))?;
    let layer = image
        .layers
        .iter()
        .find(|layer| {
            layer.media_type == "application/json"
                || layer.media_type == "application/octet-stream"
                || layer.media_type == "application/vnd.greentic.pack+json"
        })
        .or_else(|| image.layers.first())
        .ok_or_else(|| anyhow!("OCI reference {} returned no layers", mapped_ref))?;
    let digest = image
        .digest
        .unwrap_or_else(|| format!("sha256:{}", layer.sha256_digest()));
    Ok((layer.data.clone(), digest))
}

fn slug(value: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "registry".to_string()
    } else {
        out
    }
}

fn load_cache_index(bundle: &Path) -> anyhow::Result<ProviderRegistryCacheIndex> {
    let path = cache_index_path(bundle);
    if !path.exists() {
        return Ok(ProviderRegistryCacheIndex::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read provider registry cache index {}", path.display()))?;
    serde_json::from_str::<ProviderRegistryCacheIndex>(&raw)
        .with_context(|| format!("parse provider registry cache index {}", path.display()))
}

fn write_cache_index(bundle: &Path, index: &ProviderRegistryCacheIndex) -> anyhow::Result<()> {
    let path = cache_index_path(bundle);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = serde_json::to_string_pretty(index)
        .with_context(|| format!("serialize provider registry cache index {}", path.display()))?;
    std::fs::write(&path, payload)
        .with_context(|| format!("write provider registry cache index {}", path.display()))?;
    Ok(())
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ProviderRegistryCacheIndex {
    #[serde(default)]
    refs: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_file_ref_resolves() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        let catalog = temp.path().join("catalog.json");
        std::fs::write(&catalog, "[]").unwrap();
        let path = resolve_catalog_path(
            None,
            Some(&format!("file://{}", catalog.display())),
            false,
            false,
            &bundle,
        )
        .unwrap()
        .unwrap();
        assert_eq!(path, catalog);
    }

    #[test]
    fn remote_ref_uses_cache() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        let source = temp.path().join("registry.json");
        std::fs::write(&source, "[]").unwrap();
        let reference = "oci://ghcr.io/greenticai/registries/providers:latest";
        let cached = cache_registry_file(&bundle, reference, &source).unwrap();
        assert!(cached.exists());

        let resolved = resolve_catalog_path(None, Some(reference), true, false, &bundle)
            .unwrap()
            .unwrap();
        assert_eq!(resolved, cached);
    }

    #[test]
    fn offline_uses_digest_index_when_ref_cache_missing() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        let digest = "sha256:abc123";
        let digest_path = cache_path_for_digest(&bundle, digest);
        std::fs::create_dir_all(digest_path.parent().unwrap()).unwrap();
        std::fs::write(&digest_path, "[]").unwrap();
        let mut index = ProviderRegistryCacheIndex::default();
        index.refs.insert(
            "oci://ghcr.io/greenticai/registries/providers:latest".to_string(),
            digest.to_string(),
        );
        write_cache_index(&bundle, &index).unwrap();

        let resolved = resolve_catalog_path(
            None,
            Some("oci://ghcr.io/greenticai/registries/providers:latest"),
            true,
            false,
            &bundle,
        )
        .unwrap()
        .unwrap();
        assert_eq!(resolved, digest_path);
    }

    #[test]
    fn map_remote_registry_ref_supports_repo_and_store_via_env_base() {
        unsafe {
            std::env::set_var("GREENTIC_REPO_REGISTRY_BASE", "ghcr.io/org");
            std::env::set_var("GREENTIC_STORE_REGISTRY_BASE", "ghcr.io/store");
        }
        assert_eq!(
            map_remote_registry_ref("repo://providers/catalog@latest").unwrap(),
            "ghcr.io/org/providers/catalog@latest"
        );
        assert_eq!(
            map_remote_registry_ref("store://providers/catalog:latest").unwrap(),
            "ghcr.io/store/providers/catalog:latest"
        );
        unsafe {
            std::env::remove_var("GREENTIC_REPO_REGISTRY_BASE");
            std::env::remove_var("GREENTIC_STORE_REGISTRY_BASE");
        }
    }
}
