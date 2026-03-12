use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

use greentic_start::config::{DemoConfig, load_demo_config};
use greentic_start::runtime::demo_up_services;
use greentic_start::runtime_state::RuntimePaths;
use greentic_start::supervisor;

#[test]
fn demo_up_starts_services() {
    let temp = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("demo.yaml");
    let config_text = format!(
        r#"tenant: demo
team: default
services:
  nats:
    enabled: true
    url: "nats://127.0.0.1:4222"
    spawn:
      enabled: true
      binary: "{nats}"
      args: []
  gateway:
    binary: "{gateway}"
    listen_addr: "127.0.0.1"
    port: 8081
    args: []
  egress:
    binary: "{egress}"
    args: []
  subscriptions:
    msgraph:
      enabled: true
      binary: "{subs}"
      mode: "poll"
      args: []
"#,
        nats = fake_bin("fake_nats_server").display(),
        gateway = fake_bin("fake_gsm_gateway").display(),
        egress = fake_bin("fake_gsm_egress").display(),
        subs = fake_bin("fake_gsm_msgraph_subscriptions").display(),
    );
    std::fs::write(&config_path, config_text).unwrap();

    let config: DemoConfig = load_demo_config(&config_path).unwrap();
    let log_dir = temp.path().join("logs");
    demo_up_services(
        &config_path,
        &config,
        &Default::default(),
        None,
        None,
        None,
        &BTreeSet::new(),
        None,
        &log_dir,
        true,
    )
    .unwrap();

    let paths = RuntimePaths::new(temp.path().join("state"), "demo", "default");
    let status = supervisor::read_status(&paths).unwrap();
    assert!(!status.is_empty());

    let _ = supervisor::stop_pidfile(&paths.pid_path("gateway"), 1_000);
    let _ = supervisor::stop_pidfile(&paths.pid_path("egress"), 1_000);
    let _ = supervisor::stop_pidfile(&paths.pid_path("subscriptions"), 1_000);
    let _ = supervisor::stop_pidfile(&paths.pid_path("nats"), 1_000);
}

fn fake_bin(name: &str) -> PathBuf {
    if name == "greentic-operator" {
        return PathBuf::from(env!("CARGO_BIN_EXE_greentic-operator"));
    }
    example_bin(name)
}

fn binary_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn example_bin(name: &str) -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.file_name().and_then(|name| name.to_str()) == Some("deps") {
        path.pop();
    }
    let candidate = path.join("examples").join(binary_name(name));
    if candidate.exists() {
        return candidate;
    }
    let status = Command::new("cargo")
        .args(["build", "--example", name])
        .status()
        .expect("failed to build example binary");
    assert!(status.success(), "failed to build example binary");
    candidate
}
