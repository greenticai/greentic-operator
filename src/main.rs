use clap::{Parser, error::ErrorKind};
use greentic_operator::cli;
use greentic_operator::operator_i18n;
use std::env;

fn main() -> anyhow::Result<()> {
    // Tracing subscriber is NOT initialized here.
    // For demo commands, capability_bootstrap will set up OTel or fmt.
    // For non-demo commands, fmt is set up lazily below.
    let raw_args = env::args().skip(1).collect::<Vec<_>>();
    if env::var("GREENTIC_PROVIDER_CORE_ONLY").is_err() {
        // set_var is unsafe in this codebase, so wrap it accordingly.
        unsafe {
            env::set_var("GREENTIC_PROVIDER_CORE_ONLY", "false");
        }
    }
    let selected_locale = operator_i18n::select_locale(cli_locale_arg(&raw_args).as_deref());
    operator_i18n::set_locale(&selected_locale);
    if should_print_top_level_help(&raw_args) {
        print_top_level_help();
        return Ok(());
    }
    if should_print_demo_help(&raw_args) {
        print_demo_help();
        return Ok(());
    }
    let cli = match cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            if err.kind() == ErrorKind::DisplayHelp {
                let rendered = err.to_string();
                print!("{}", localize_help_text(&rendered));
                std::process::exit(0);
            }
            if err.kind() == ErrorKind::MissingSubcommand {
                print_missing_subcommand_help();
                std::process::exit(2);
            }
            err.exit();
        }
    };
    let result = cli.run();
    greentic_telemetry::shutdown();
    result
}

fn cli_locale_arg(args: &[String]) -> Option<String> {
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = &args[idx];
        if let Some(value) = arg.strip_prefix("--locale=") {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if arg == "--locale" && idx + 1 < args.len() {
            let value = &args[idx + 1];
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        idx += 1;
    }
    None
}

fn should_print_top_level_help(args: &[String]) -> bool {
    let mut has_help_flag = false;
    let mut has_subcommand = false;
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--help" || arg == "-h" {
            has_help_flag = true;
            idx += 1;
            continue;
        }
        if arg == "--locale" {
            idx += 2;
            continue;
        }
        if arg.starts_with("--locale=") || arg.starts_with('-') {
            idx += 1;
            continue;
        }
        has_subcommand = true;
        idx += 1;
    }
    has_help_flag && !has_subcommand
}

fn should_print_demo_help(args: &[String]) -> bool {
    let (has_help_flag, positionals) = help_context(args);
    has_help_flag && positionals.first() == Some(&"demo") && positionals.len() == 1
}

fn help_context(args: &[String]) -> (bool, Vec<&str>) {
    let mut has_help_flag = false;
    let mut positionals: Vec<&str> = Vec::new();
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--help" || arg == "-h" {
            has_help_flag = true;
            idx += 1;
            continue;
        }
        if arg == "--locale" {
            idx += 2;
            continue;
        }
        if arg.starts_with("--locale=") || arg.starts_with('-') {
            idx += 1;
            continue;
        }
        positionals.push(arg);
        idx += 1;
    }
    (has_help_flag, positionals)
}

fn print_top_level_help() {
    println!(
        "{}",
        operator_i18n::tr("cli.main.help.tagline", "Greentic operator tooling")
    );
    println!();
    println!(
        "{} greentic-operator [OPTIONS] <COMMAND>",
        operator_i18n::tr("cli.main.help.usage_label", "Usage:")
    );
    println!();
    println!(
        "{}",
        operator_i18n::tr("cli.main.help.commands_header", "Commands:")
    );
    println!(
        "  demo    {}",
        operator_i18n::tr("cli.main.help.command_demo", "")
    );
    println!(
        "  help    {}",
        operator_i18n::tr(
            "cli.main.help.command_help",
            "Print this message or the help of the given subcommand(s)"
        )
    );
    println!();
    println!(
        "{}",
        operator_i18n::tr("cli.main.help.options_header", "Options:")
    );
    println!(
        "      --locale <LOCALE>  {}",
        operator_i18n::tr(
            "cli.main.help.option_locale",
            "CLI locale (for translated output)."
        )
    );
    println!(
        "  -h, --help             {}",
        operator_i18n::tr("cli.main.help.option_help", "Print help")
    );
    println!(
        "  -V, --version          {}",
        operator_i18n::tr("cli.main.help.option_version", "Print version")
    );
}

fn print_demo_help() {
    println!(
        "{} greentic-operator demo [OPTIONS] <COMMAND>",
        operator_i18n::tr("cli.demo.help.usage_label", "Usage:")
    );
    println!();
    println!(
        "{}",
        operator_i18n::tr("cli.demo.help.commands_header", "Commands:")
    );
    println!(
        "  build          {}",
        operator_i18n::tr(
            "cli.demo.help.command.build",
            "Build a portable demo bundle."
        )
    );
    println!(
        "  start          {}",
        operator_i18n::tr(
            "cli.demo.help.command.start",
            "Start demo services from a bundle."
        )
    );
    println!(
        "  restart        {}",
        operator_i18n::tr(
            "cli.demo.help.command.restart",
            "Restart demo lifecycle services from a bundle."
        )
    );
    println!(
        "  stop           {}",
        operator_i18n::tr(
            "cli.demo.help.command.stop",
            "Stop demo lifecycle services from a bundle."
        )
    );
    println!(
        "  setup          {}",
        operator_i18n::tr(
            "cli.demo.help.command.setup",
            "Run provider setup flows against a demo bundle."
        )
    );
    println!(
        "  send           {}",
        operator_i18n::tr(
            "cli.demo.help.command.send",
            "Send a demo message via a provider pack."
        )
    );
    println!(
        "  ingress        {}",
        operator_i18n::tr(
            "cli.demo.help.command.ingress",
            "Send a synthetic HTTP request through the messaging ingress pipeline"
        )
    );
    println!(
        "  new            {}",
        operator_i18n::tr(
            "cli.demo.help.command.new",
            "Create a new demo bundle scaffold."
        )
    );
    println!(
        "  status         {}",
        operator_i18n::tr(
            "cli.demo.help.command.status",
            "Show demo service status using runtime state."
        )
    );
    println!(
        "  logs           {}",
        operator_i18n::tr(
            "cli.demo.help.command.logs",
            "Show demo logs produced by the operator and services."
        )
    );
    println!(
        "  doctor         {}",
        operator_i18n::tr(
            "cli.demo.help.command.doctor",
            "Run demo doctor validation from a bundle."
        )
    );
    println!(
        "  allow          {}",
        operator_i18n::tr(
            "cli.demo.help.command.allow",
            "Allow a tenant/team access to a pack/flow/node"
        )
    );
    println!(
        "  forbid         {}",
        operator_i18n::tr(
            "cli.demo.help.command.forbid",
            "Forbid a tenant/team access to a pack/flow/node"
        )
    );
    println!(
        "  subscriptions  {}",
        operator_i18n::tr(
            "cli.demo.help.command.subscriptions",
            "Manage demo subscriptions via provider components"
        )
    );
    println!(
        "  capability     {}",
        operator_i18n::tr(
            "cli.demo.help.command.capability",
            "Manage capability resolution/invocation in demo bundles"
        )
    );
    println!(
        "  run            {}",
        operator_i18n::tr(
            "cli.demo.help.command.run",
            "Run a pack/flow with inline input"
        )
    );
    println!(
        "  list-packs     {}",
        operator_i18n::tr(
            "cli.demo.help.command.list_packs",
            "List resolved packs from a bundle"
        )
    );
    println!(
        "  list-flows     {}",
        operator_i18n::tr(
            "cli.demo.help.command.list_flows",
            "List flows declared by a pack"
        )
    );
    println!(
        "  wizard         {}",
        operator_i18n::tr(
            "cli.demo.help.command.wizard",
            "Alias of wizard. Plan or create a demo bundle from pack refs and allow rules"
        )
    );
    println!(
        "  help           {}",
        operator_i18n::tr(
            "cli.demo.help.command.help",
            "Print this message or the help of the given subcommand(s)"
        )
    );
    println!();
    println!(
        "{}",
        operator_i18n::tr("cli.demo.help.options_header", "Options:")
    );
    println!(
        "      --debug            {}",
        operator_i18n::tr("cli.demo.help.option_debug", "")
    );
    println!(
        "      --locale <LOCALE>  {}",
        operator_i18n::tr(
            "cli.demo.help.option_locale",
            "CLI locale (for translated output)."
        )
    );
    println!(
        "  -h, --help             {}",
        operator_i18n::tr("cli.demo.help.option_help", "Print help")
    );
}

fn localize_help_text(rendered: &str) -> String {
    let locale = operator_i18n::current_locale();
    let Ok(en_map) = operator_i18n::load_cli("en") else {
        return rendered.to_string();
    };
    let Ok(loc_map) = operator_i18n::load_cli(&locale) else {
        return rendered.to_string();
    };

    let mut pairs = Vec::new();
    for (key, en_value) in en_map {
        if en_value.is_empty() {
            continue;
        }
        if !is_help_replacement_candidate(&en_value) {
            continue;
        }
        let Some(localized) = loc_map.get(&key) else {
            continue;
        };
        if localized == &en_value {
            continue;
        }
        pairs.push((en_value, localized.clone()));
    }
    pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    let mut out = rendered.to_string();
    for (from, to) in pairs {
        out = out.replace(&from, &to);
    }
    out
}

fn is_help_replacement_candidate(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.len() < 6 {
        return false;
    }
    trimmed.contains(char::is_whitespace)
        || trimmed.ends_with(':')
        || trimmed.starts_with("--")
        || trimmed.contains('/')
        || trimmed.contains('(')
}

fn print_missing_subcommand_help() {
    eprintln!(
        "{}",
        operator_i18n::trf(
            "cli.main.requires_subcommand",
            "error: 'greentic-operator' requires a subcommand but one was not provided",
            &[]
        )
    );
    eprintln!(
        "  [{}: demo, help]",
        operator_i18n::tr("cli.main.subcommands", "subcommands")
    );
    eprintln!();
    eprintln!(
        "{} greentic-operator [OPTIONS] <COMMAND>",
        operator_i18n::tr("cli.main.usage_label", "Usage:")
    );
    eprintln!();
    eprintln!(
        "{}",
        operator_i18n::tr("cli.main.more_info", "For more information, try '--help'.")
    );
}
