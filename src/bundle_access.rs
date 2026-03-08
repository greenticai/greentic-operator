use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleAccessMode {
    Directory,
    Mounted,
    Userspace,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BundleAccessDiagnostics {
    pub mode: BundleAccessMode,
    pub bundle_ref: PathBuf,
    pub active_root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_digest_sha256: Option<String>,
    pub warm_status: String,
}

#[derive(Clone, Debug)]
pub struct BundleAccessConfig {
    pub prefer_mount: bool,
    pub runtime_dir: PathBuf,
}

impl BundleAccessConfig {
    pub fn new(runtime_dir: PathBuf) -> Self {
        Self {
            prefer_mount: true,
            runtime_dir,
        }
    }
}

pub fn operator_bundle_access_config(bundle_ref: &Path) -> BundleAccessConfig {
    let runtime_dir = if bundle_ref.is_dir() {
        bundle_ref.join("state").join("runtime").join("bundle_fs")
    } else {
        env::temp_dir().join("greentic-operator").join("bundle_fs")
    };
    BundleAccessConfig::new(runtime_dir)
}

pub fn with_operator_bundle_read_root<T>(
    bundle_ref: &Path,
    f: impl FnOnce(&Path) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    if !bundle_ref.is_dir() || bundle_ref.join("greentic.demo.yaml").exists() {
        let bundle_access =
            BundleAccessHandle::open(bundle_ref, &operator_bundle_access_config(bundle_ref))?;
        f(bundle_access.active_root())
    } else {
        f(bundle_ref)
    }
}

pub fn operator_bundle_cbor_only(bundle_ref: &Path) -> anyhow::Result<bool> {
    with_operator_bundle_read_root(bundle_ref, |bundle_read_root| {
        Ok(bundle_ref.join("greentic.demo.yaml").exists()
            || bundle_read_root.join("greentic.demo.yaml").exists())
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BundleAccessSupport {
    mount: bool,
    userspace: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DesiredMode {
    Mounted,
    Userspace,
}

#[derive(Debug)]
enum BundleCleanup {
    RemoveDir(PathBuf),
    Unmount(PathBuf),
}

#[derive(Debug)]
struct BundleAccessState {
    diagnostics: BundleAccessDiagnostics,
    cleanup: Option<BundleCleanup>,
}

impl Drop for BundleAccessState {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            match cleanup {
                BundleCleanup::RemoveDir(path) => {
                    let _ = fs::remove_dir_all(path);
                }
                BundleCleanup::Unmount(path) => {
                    let _ = Command::new("umount").arg(&path).status();
                    let _ = fs::remove_dir_all(path);
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct BundleAccessHandle {
    state: Arc<BundleAccessState>,
}

impl BundleAccessHandle {
    pub fn open(bundle_ref: impl AsRef<Path>, config: &BundleAccessConfig) -> anyhow::Result<Self> {
        let bundle_ref = bundle_ref.as_ref().to_path_buf();
        if bundle_ref.is_dir() {
            return Ok(Self::directory(bundle_ref));
        }
        if bundle_ref.extension().and_then(|value| value.to_str()) != Some("sqsh") {
            anyhow::bail!(
                "bundle access currently supports directory bundles or .sqsh images, got {}",
                bundle_ref.display()
            );
        }

        let support = detect_support();
        let digest = digest_file(&bundle_ref).ok();
        let desired = select_desired_mode(&bundle_ref, config.prefer_mount, support)?;
        fs::create_dir_all(&config.runtime_dir).with_context(|| {
            format!("create bundle runtime dir {}", config.runtime_dir.display())
        })?;

        let bundle_key = stable_bundle_key(&bundle_ref);
        match desired {
            DesiredMode::Mounted => {
                let mount_point = config.runtime_dir.join(format!("{bundle_key}-mounted"));
                match mount_sqsh(&bundle_ref, &mount_point) {
                    Ok(()) => Ok(Self {
                        state: Arc::new(BundleAccessState {
                            diagnostics: BundleAccessDiagnostics {
                                mode: BundleAccessMode::Mounted,
                                bundle_ref,
                                active_root: mount_point.clone(),
                                fallback_reason: None,
                                bundle_digest_sha256: digest,
                                warm_status: "mounted".to_string(),
                            },
                            cleanup: Some(BundleCleanup::Unmount(mount_point)),
                        }),
                    }),
                    Err(mount_err) if support.userspace => {
                        let extract_root =
                            config.runtime_dir.join(format!("{bundle_key}-userspace"));
                        extract_sqsh(&bundle_ref, &extract_root)?;
                        Ok(Self {
                            state: Arc::new(BundleAccessState {
                                diagnostics: BundleAccessDiagnostics {
                                    mode: BundleAccessMode::Userspace,
                                    bundle_ref,
                                    active_root: extract_root.clone(),
                                    fallback_reason: Some(format!("mount failed: {mount_err}")),
                                    bundle_digest_sha256: digest,
                                    warm_status: "extracted".to_string(),
                                },
                                cleanup: Some(BundleCleanup::RemoveDir(extract_root)),
                            }),
                        })
                    }
                    Err(mount_err) => Err(mount_err),
                }
            }
            DesiredMode::Userspace => {
                let extract_root = config.runtime_dir.join(format!("{bundle_key}-userspace"));
                extract_sqsh(&bundle_ref, &extract_root)?;
                Ok(Self {
                    state: Arc::new(BundleAccessState {
                        diagnostics: BundleAccessDiagnostics {
                            mode: BundleAccessMode::Userspace,
                            bundle_ref,
                            active_root: extract_root.clone(),
                            fallback_reason: config.prefer_mount.then(|| {
                                "mount support unavailable; used userspace fallback".to_string()
                            }),
                            bundle_digest_sha256: digest,
                            warm_status: "extracted".to_string(),
                        },
                        cleanup: Some(BundleCleanup::RemoveDir(extract_root)),
                    }),
                })
            }
        }
    }

    pub fn active_root(&self) -> &Path {
        &self.state.diagnostics.active_root
    }

    pub fn diagnostics(&self) -> &BundleAccessDiagnostics {
        &self.state.diagnostics
    }

    fn directory(bundle_root: PathBuf) -> Self {
        Self {
            state: Arc::new(BundleAccessState {
                diagnostics: BundleAccessDiagnostics {
                    mode: BundleAccessMode::Directory,
                    bundle_ref: bundle_root.clone(),
                    active_root: bundle_root,
                    fallback_reason: None,
                    bundle_digest_sha256: None,
                    warm_status: "not_applicable".to_string(),
                },
                cleanup: None,
            }),
        }
    }
}

fn select_desired_mode(
    bundle_ref: &Path,
    prefer_mount: bool,
    support: BundleAccessSupport,
) -> anyhow::Result<DesiredMode> {
    if bundle_ref.is_dir() {
        anyhow::bail!("directory bundles do not require staged access mode selection");
    }
    if prefer_mount {
        if support.mount {
            return Ok(DesiredMode::Mounted);
        }
        if support.userspace {
            return Ok(DesiredMode::Userspace);
        }
    } else if support.userspace {
        return Ok(DesiredMode::Userspace);
    } else if support.mount {
        return Ok(DesiredMode::Mounted);
    }
    Err(anyhow!(
        "no supported SquashFS access path available for {}",
        bundle_ref.display()
    ))
}

fn detect_support() -> BundleAccessSupport {
    BundleAccessSupport {
        mount: command_in_path("mount") && command_in_path("umount"),
        userspace: command_in_path("unsquashfs"),
    }
}

fn command_in_path(name: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file()
    })
}

fn stable_bundle_key(bundle_ref: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bundle_ref.display().to_string().as_bytes());
    let digest = hasher.finalize();
    hex_string(&digest[..8])
}

fn digest_file(path: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(path)
        .with_context(|| format!("read bundle image for digest {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex_string(&hasher.finalize()))
}

fn mount_sqsh(bundle_ref: &Path, mount_point: &Path) -> anyhow::Result<()> {
    if mount_point.exists() {
        fs::remove_dir_all(mount_point)
            .with_context(|| format!("remove stale mount dir {}", mount_point.display()))?;
    }
    fs::create_dir_all(mount_point)
        .with_context(|| format!("create mount dir {}", mount_point.display()))?;
    let status = Command::new("mount")
        .args(["-t", "squashfs", "-o", "loop,ro"])
        .arg(bundle_ref)
        .arg(mount_point)
        .status()
        .with_context(|| format!("mount squashfs {}", bundle_ref.display()))?;
    if status.success() {
        return Ok(());
    }
    let _ = fs::remove_dir_all(mount_point);
    Err(anyhow!(
        "mount command failed for {} with status {}",
        bundle_ref.display(),
        status
    ))
}

fn extract_sqsh(bundle_ref: &Path, extract_root: &Path) -> anyhow::Result<()> {
    if extract_root.exists() {
        fs::remove_dir_all(extract_root)
            .with_context(|| format!("remove stale userspace dir {}", extract_root.display()))?;
    }
    let parent = extract_root
        .parent()
        .ok_or_else(|| anyhow!("userspace extract path has no parent"))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create bundle runtime parent {}", parent.display()))?;
    let status = Command::new("unsquashfs")
        .args(["-no-progress", "-dest"])
        .arg(extract_root)
        .arg(bundle_ref)
        .status()
        .with_context(|| format!("extract squashfs {}", bundle_ref.display()))?;
    if status.success() {
        return Ok(());
    }
    let _ = fs::remove_dir_all(extract_root);
    Err(anyhow!(
        "unsquashfs failed for {} with status {}",
        bundle_ref.display(),
        status
    ))
}

fn hex_string(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::{
        BundleAccessConfig, BundleAccessHandle, BundleAccessMode, BundleAccessSupport, DesiredMode,
        operator_bundle_cbor_only, select_desired_mode, with_operator_bundle_read_root,
    };
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard, OnceLock};

    static TEST_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct EnvVarGuard {
        key: &'static str,
        original: Option<std::ffi::OsString>,
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvVarGuard {
        fn set_path(path: &Path) -> Self {
            let lock = TEST_ENV_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .expect("env lock");
            let original = env::var_os("PATH");
            // SAFETY: tests serialize their PATH override via a local guard and restore it on drop.
            unsafe {
                env::set_var("PATH", path);
            }
            Self {
                key: "PATH",
                original,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: restoring the prior process env value for the same test-scoped override.
            unsafe {
                match self.original.as_ref() {
                    Some(value) => env::set_var(self.key, value),
                    None => env::remove_var(self.key),
                }
            }
        }
    }

    fn write_executable(path: &Path, body: &str) {
        fs::write(path, body).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path).expect("metadata").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).expect("chmod");
        }
    }

    fn write_fake_sqsh(path: &Path) {
        fs::write(path, b"fake sqsh image").expect("fake sqsh");
    }

    fn temp_runtime_dir(tmp: &tempfile::TempDir) -> PathBuf {
        tmp.path().join("runtime")
    }

    #[test]
    fn directory_bundles_skip_staging() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let handle =
            BundleAccessHandle::open(tmp.path(), &BundleAccessConfig::new(tmp.path().join("rt")))
                .expect("open bundle access");
        assert_eq!(handle.diagnostics().mode, BundleAccessMode::Directory);
        assert_eq!(handle.active_root(), tmp.path());
    }

    #[test]
    fn selection_prefers_mount_when_supported() {
        let selected = select_desired_mode(
            std::path::Path::new("bundle.sqsh"),
            true,
            BundleAccessSupport {
                mount: true,
                userspace: true,
            },
        )
        .expect("selection");
        assert_eq!(selected, DesiredMode::Mounted);
    }

    #[test]
    fn selection_falls_back_to_userspace_when_mount_missing() {
        let selected = select_desired_mode(
            std::path::Path::new("bundle.sqsh"),
            true,
            BundleAccessSupport {
                mount: false,
                userspace: true,
            },
        )
        .expect("selection");
        assert_eq!(selected, DesiredMode::Userspace);
    }

    #[test]
    fn selection_rejects_sqsh_when_no_access_path_exists() {
        let err = select_desired_mode(
            std::path::Path::new("bundle.sqsh"),
            true,
            BundleAccessSupport {
                mount: false,
                userspace: false,
            },
        )
        .expect_err("selection should fail");
        assert!(
            err.to_string()
                .contains("no supported SquashFS access path available")
        );
    }

    #[test]
    fn operator_bundle_helpers_use_directory_root_without_marker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let seen = with_operator_bundle_read_root(tmp.path(), |root| Ok(root.to_path_buf()))
            .expect("read root");
        assert_eq!(seen, tmp.path());
        assert!(
            !operator_bundle_cbor_only(tmp.path()).expect("bundle cbor mode"),
            "plain directory bundle should not be cbor-only"
        );
    }

    #[test]
    fn operator_bundle_cbor_only_detects_demo_marker_in_directory_bundle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::write(tmp.path().join("greentic.demo.yaml"), "demo: true\n").expect("demo marker");
        let seen = with_operator_bundle_read_root(tmp.path(), |root| Ok(root.to_path_buf()))
            .expect("read root");
        assert_eq!(seen, tmp.path());
        assert!(
            operator_bundle_cbor_only(tmp.path()).expect("bundle cbor mode"),
            "demo directory bundle should be cbor-only"
        );
    }

    #[test]
    fn open_sqsh_uses_userspace_extractor_when_mount_tools_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).expect("bin dir");
        let unsquashfs = bin_dir.join("unsquashfs");
        write_executable(
            &unsquashfs,
            "#!/bin/sh\nset -eu\nout=\"$3\"\n/bin/mkdir -p \"$out/providers/messaging\"\nprintf 'demo: true\\n' > \"$out/greentic.demo.yaml\"\n",
        );
        let _path_guard = EnvVarGuard::set_path(&bin_dir);

        let sqsh = tmp.path().join("bundle.sqsh");
        write_fake_sqsh(&sqsh);
        let handle =
            BundleAccessHandle::open(&sqsh, &BundleAccessConfig::new(temp_runtime_dir(&tmp)))
                .expect("open sqsh");

        assert_eq!(handle.diagnostics().mode, BundleAccessMode::Userspace);
        assert_eq!(handle.diagnostics().warm_status, "extracted");
        assert!(
            handle
                .active_root()
                .join("providers")
                .join("messaging")
                .exists()
        );
    }

    #[test]
    fn open_sqsh_falls_back_to_userspace_when_mount_command_fails() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).expect("bin dir");
        write_executable(&bin_dir.join("mount"), "#!/bin/sh\nexit 1\n");
        write_executable(&bin_dir.join("umount"), "#!/bin/sh\nexit 0\n");
        write_executable(
            &bin_dir.join("unsquashfs"),
            "#!/bin/sh\nset -eu\nout=\"$3\"\n/bin/mkdir -p \"$out/packs\"\n: > \"$out/packs/provider.gtpack\"\n",
        );
        let _path_guard = EnvVarGuard::set_path(&bin_dir);

        let sqsh = tmp.path().join("bundle.sqsh");
        write_fake_sqsh(&sqsh);
        let handle =
            BundleAccessHandle::open(&sqsh, &BundleAccessConfig::new(temp_runtime_dir(&tmp)))
                .expect("open sqsh");

        assert_eq!(handle.diagnostics().mode, BundleAccessMode::Userspace);
        assert_eq!(handle.diagnostics().warm_status, "extracted");
        assert!(
            handle
                .diagnostics()
                .fallback_reason
                .as_deref()
                .unwrap_or_default()
                .contains("mount failed"),
            "expected mount failure fallback reason"
        );
        assert!(
            handle
                .active_root()
                .join("packs")
                .join("provider.gtpack")
                .exists()
        );
    }
}
