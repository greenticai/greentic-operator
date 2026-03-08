use std::path::{Path, PathBuf};

use crate::bundle_access::{BundleAccessHandle, operator_bundle_access_config};
use crate::domains::{self, Domain};

#[derive(Clone, Debug)]
pub struct DoctorOptions {
    pub tenant: Option<String>,
    pub team: Option<String>,
    pub strict: bool,
    pub validator_packs: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug)]
pub enum DoctorScope {
    One(Domain),
    All,
}

#[derive(Clone, Debug)]
pub struct DoctorRun {
    pub pack_path: PathBuf,
    pub status: std::process::ExitStatus,
}

pub fn run_doctor(
    root: &Path,
    scope: DoctorScope,
    options: DoctorOptions,
    pack_command: &Path,
) -> anyhow::Result<Vec<DoctorRun>> {
    let bundle_access = BundleAccessHandle::open(root, &operator_bundle_access_config(root))?;
    let bundle_read_root = bundle_access.active_root();
    let base_dir = doctor_root(root)?;
    std::fs::create_dir_all(&base_dir)?;

    let domains = match scope {
        DoctorScope::One(domain) => vec![domain],
        DoctorScope::All => vec![
            Domain::Messaging,
            Domain::Events,
            Domain::Secrets,
            Domain::OAuth,
        ],
    };

    let mut runs = Vec::new();

    for domain in domains {
        let provider_packs = domains::discover_provider_packs(bundle_read_root, domain)?;
        let validators = if !options.validator_packs.is_empty() {
            options.validator_packs.clone()
        } else {
            domains::validator_pack_path(bundle_read_root, domain)
                .map(|path| vec![path])
                .unwrap_or_default()
        };

        for pack in provider_packs {
            let run = run_doctor_for_pack(
                root,
                &base_dir,
                domain,
                &pack.path,
                &pack.pack_id,
                &validators,
                options.strict,
                pack_command,
            )?;
            runs.push(run);
        }
    }

    if let Some(selection) = demo_packs_with_roots(root, bundle_read_root, &options)? {
        for pack in selection.packs {
            let run = run_doctor_for_pack(
                root,
                &base_dir,
                Domain::Messaging,
                &pack,
                pack.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("pack"),
                &options.validator_packs,
                options.strict,
                pack_command,
            )?;
            if !run.status.success() {
                let _ = write_summary(
                    &base_dir,
                    "demo",
                    &format!("doctor failed for demo pack {:?}", pack.display()),
                );
            }
        }
        let summary = format!(
            "demo packs validated for tenant={} team={}\n",
            selection.tenant,
            selection.team.unwrap_or_else(|| "none".to_string())
        );
        write_summary(&base_dir, "demo", &summary)?;
    }

    Ok(runs)
}

pub fn build_doctor_args(
    pack_path: &Path,
    validator_packs: &[PathBuf],
    strict: bool,
) -> Vec<String> {
    let mut args = vec!["doctor".to_string(), pack_path.display().to_string()];
    if strict {
        args.push("--strict".to_string());
    }
    for validator in validator_packs {
        args.push("--validator-pack".to_string());
        args.push(validator.display().to_string());
    }
    args
}

#[allow(clippy::too_many_arguments)]
fn run_doctor_for_pack(
    _root: &Path,
    base_dir: &Path,
    domain: Domain,
    pack_path: &Path,
    pack_label: &str,
    validator_packs: &[PathBuf],
    strict: bool,
    pack_command: &Path,
) -> anyhow::Result<DoctorRun> {
    let run_dir = base_dir.join(domain_name(domain)).join(pack_label);
    std::fs::create_dir_all(&run_dir)?;
    let stdout_path = run_dir.join("stdout.txt");
    let stderr_path = run_dir.join("stderr.txt");

    let stdout = std::fs::File::create(&stdout_path)?;
    let stderr = std::fs::File::create(&stderr_path)?;

    let args = build_doctor_args(pack_path, validator_packs, strict);
    let status = std::process::Command::new(pack_command)
        .args(&args)
        .stdout(stdout)
        .stderr(stderr)
        .status()?;

    let summary = format!("pack: {}\nstatus: {}\n", pack_path.display(), status);
    write_summary(
        base_dir,
        &format!("{}-{}", domain_name(domain), pack_label),
        &summary,
    )?;

    if !status.success() {
        return Err(anyhow::anyhow!(
            "greentic-pack doctor failed for {}",
            pack_path.display()
        ));
    }

    Ok(DoctorRun {
        pack_path: pack_path.to_path_buf(),
        status,
    })
}

struct DemoPackSelection {
    packs: Vec<PathBuf>,
    tenant: String,
    team: Option<String>,
}

fn demo_packs_with_roots(
    root: &Path,
    bundle_read_root: &Path,
    options: &DoctorOptions,
) -> anyhow::Result<Option<DemoPackSelection>> {
    let Some(tenant) = options.tenant.clone() else {
        return Ok(None);
    };
    let manifest = resolved_manifest_path(root, &tenant, options.team.as_deref());
    if !manifest.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(manifest)?;
    let manifest: ResolvedManifest = serde_yaml_bw::from_str(&contents)?;
    let mut packs = Vec::new();
    for pack in manifest.packs {
        if pack.ends_with(".gtpack") {
            let pack_path = Path::new(&pack);
            if pack_path.is_absolute() {
                packs.push(pack_path.to_path_buf());
            } else {
                packs.push(bundle_read_root.join(pack_path));
            }
        } else {
            eprintln!("Warning: skipping non-gtpack demo pack {}", pack);
        }
    }
    Ok(Some(DemoPackSelection {
        packs,
        tenant,
        team: options.team.clone(),
    }))
}

fn resolved_manifest_path(root: &Path, tenant: &str, team: Option<&str>) -> PathBuf {
    let filename = match team {
        Some(team) => format!("{tenant}.{team}.yaml"),
        None => format!("{tenant}.yaml"),
    };
    root.join("state").join("resolved").join(filename)
}

fn doctor_root(root: &Path) -> anyhow::Result<PathBuf> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|err| anyhow::anyhow!("timestamp error: {err}"))?
        .as_secs();
    Ok(root
        .join("state")
        .join("doctor")
        .join(format!("{timestamp}")))
}

fn write_summary(base_dir: &Path, name: &str, contents: &str) -> anyhow::Result<()> {
    let summary_path = base_dir.join(format!("{name}-summary.txt"));
    std::fs::write(summary_path, contents)?;
    Ok(())
}

fn domain_name(domain: Domain) -> &'static str {
    match domain {
        Domain::Messaging => "messaging",
        Domain::Events => "events",
        Domain::Secrets => "secrets",
        Domain::OAuth => "oauth",
    }
}

#[derive(Debug, serde::Deserialize)]
struct ResolvedManifest {
    packs: Vec<String>,
}

#[cfg(test)]
mod tests {
    use crate::bundle_access::operator_bundle_access_config;

    #[test]
    fn doctor_bundle_access_config_uses_bundle_state_for_directory_bundles() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = operator_bundle_access_config(tmp.path());
        assert_eq!(
            config.runtime_dir,
            tmp.path().join("state").join("runtime").join("bundle_fs")
        );
    }

    #[test]
    fn doctor_bundle_access_config_uses_temp_root_for_image_bundles() {
        let sqsh = std::path::Path::new("/tmp/demo-bundle.sqsh");
        let config = operator_bundle_access_config(sqsh);
        assert!(config.runtime_dir.starts_with(std::env::temp_dir()));
        assert!(config.runtime_dir.ends_with("bundle_fs"));
    }
}
