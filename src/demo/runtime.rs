use std::path::{Path, PathBuf};

use crate::runtime_state::{RuntimePaths, read_service_manifest};
use crate::services;
use crate::supervisor;

pub fn demo_status_runtime(
    state_dir: &Path,
    tenant: &str,
    team: &str,
    verbose: bool,
) -> anyhow::Result<()> {
    let paths = RuntimePaths::new(state_dir, tenant, team);
    let statuses = supervisor::read_status(&paths)?;
    if statuses.is_empty() {
        println!(
            "{}",
            crate::operator_i18n::tr("demo.runtime.none_running", "none running")
        );
        return Ok(());
    }
    for status in statuses {
        let state = if status.running {
            crate::operator_i18n::tr("demo.runtime.status_running", "running")
        } else {
            crate::operator_i18n::tr("demo.runtime.status_stopped", "stopped")
        };
        let pid = status
            .pid
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string());
        if verbose {
            println!(
                "{}: {} (pid={}, log={})",
                status.id.as_str(),
                &state,
                pid,
                status.log_path.display()
            );
        } else {
            println!("{}: {} (pid={})", status.id.as_str(), &state, pid);
        }
    }
    Ok(())
}

pub fn demo_logs_runtime(
    state_dir: &Path,
    log_dir: &Path,
    tenant: &str,
    team: &str,
    service: &str,
    tail: bool,
) -> anyhow::Result<()> {
    let log_dir = resolve_manifest_log_dir(state_dir, tenant, team, log_dir)?;
    let log_path = if service == "operator" {
        log_dir.join("operator.log")
    } else {
        let tenant_log_path = tenant_log_path(&log_dir, service, tenant, team)?;
        select_log_path(&log_dir, service, tenant, &tenant_log_path)
    };
    if tail {
        return services::tail_log(&log_path);
    }
    let lines = read_last_lines(&log_path, 200)?;
    if !lines.is_empty() {
        println!("{lines}");
    }
    Ok(())
}

fn select_log_path(log_dir: &Path, service: &str, tenant: &str, tenant_log: &Path) -> PathBuf {
    let candidates = [
        log_dir.join(format!("{service}.log")),
        log_dir.join(format!("{service}-{tenant}.log")),
        log_dir.join(format!("{service}.{tenant}.log")),
    ];
    for candidate in &candidates {
        if candidate.exists() {
            return candidate.clone();
        }
    }
    if tenant_log.exists() {
        return tenant_log.to_path_buf();
    }
    let _ = ensure_log_file(tenant_log);
    tenant_log.to_path_buf()
}

fn tenant_log_path(
    log_dir: &Path,
    service: &str,
    tenant: &str,
    team: &str,
) -> anyhow::Result<PathBuf> {
    let tenant_dir = log_dir.join(format!("{tenant}.{team}"));
    let path = tenant_dir.join(format!("{service}.log"));
    ensure_log_file(&path)?;
    Ok(path)
}

fn ensure_log_file(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        std::fs::File::create(path)?;
    }
    Ok(())
}

fn resolve_manifest_log_dir(
    state_dir: &Path,
    tenant: &str,
    team: &str,
    default: &Path,
) -> anyhow::Result<PathBuf> {
    let paths = RuntimePaths::new(state_dir, tenant, team);
    if let Some(manifest) = read_service_manifest(&paths)?
        && let Some(dir) = manifest.log_dir
    {
        return Ok(PathBuf::from(dir));
    }
    Ok(default.to_path_buf())
}

fn read_last_lines(path: &Path, count: usize) -> anyhow::Result<String> {
    if !path.exists() {
        anyhow::bail!("Log file does not exist: {}", path.display());
    }
    let contents = std::fs::read_to_string(path)?;
    let mut lines: Vec<&str> = contents.lines().collect();
    if lines.len() > count {
        lines = lines.split_off(lines.len() - count);
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn tenant_log_path_creates_file() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = tenant_log_path(dir.path(), "messaging", "demo", "default")?;
        assert!(path.exists());
        Ok(())
    }

    #[test]
    fn select_log_path_prefers_service_log_when_present() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let tenant_path = tenant_log_path(dir.path(), "messaging", "demo", "default")?;
        let service_path = dir.path().join("messaging.log");
        fs::write(&service_path, "other")?;
        let selected = select_log_path(dir.path(), "messaging", "demo", &tenant_path);
        assert_eq!(selected, service_path);
        Ok(())
    }

    #[test]
    fn demo_logs_runtime_reads_operator_log() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let log = dir.path().join("operator.log");
        fs::write(&log, "operator ready")?;
        demo_logs_runtime(dir.path(), dir.path(), "demo", "default", "operator", false)?;
        Ok(())
    }
}
