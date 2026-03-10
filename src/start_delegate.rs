use std::process::{Command, ExitStatus, Stdio};

use anyhow::Context;

pub fn should_delegate_demo_lifecycle(args: &[String]) -> bool {
    if std::env::var("GREENTIC_OPERATOR_START_INTERNAL").is_ok() {
        return false;
    }

    let mut positionals = Vec::new();
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--locale" {
            idx += 2;
            continue;
        }
        if arg.starts_with("--locale=") || arg.starts_with('-') {
            idx += 1;
            continue;
        }
        positionals.push(arg.as_str());
        idx += 1;
    }

    if positionals.len() < 2 {
        return false;
    }

    positionals[0] == "demo" && matches!(positionals[1], "start" | "up" | "stop" | "restart")
}

pub fn delegate_to_greentic_start(raw_args: &[String]) -> anyhow::Result<ExitStatus> {
    Command::new("greentic-start")
        .args(raw_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| "failed to launch greentic-start")
}

#[cfg(test)]
mod tests {
    use super::should_delegate_demo_lifecycle;

    #[test]
    fn delegates_demo_start() {
        assert!(should_delegate_demo_lifecycle(&[
            "demo".into(),
            "start".into(),
            "--bundle".into(),
            "./bundle".into(),
        ]));
    }

    #[test]
    fn delegates_demo_stop_with_locale() {
        assert!(should_delegate_demo_lifecycle(&[
            "--locale".into(),
            "en".into(),
            "demo".into(),
            "stop".into(),
        ]));
    }

    #[test]
    fn ignores_non_lifecycle_commands() {
        assert!(!should_delegate_demo_lifecycle(&[
            "demo".into(),
            "status".into(),
        ]));
    }
}
