use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    convert::TryFrom,
    env, fs,
    io::{self, IsTerminal, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, anyhow};
use chrono::{TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue, json};

use base64::Engine as _;
use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use tokio::runtime::Runtime;

use crate::bin_resolver::{self, ResolveCtx};
use crate::capabilities::ResolveScope;
use crate::config;
use crate::config_gate::{self, ConfigGateItem, ConfigValueSource};
use crate::demo::{
    self, BuildOptions, DemoRepl, DemoRunner,
    card::{detect_adaptive_card_view, print_card_summary},
    http_ingress::{HttpIngressConfig, HttpIngressServer},
    input as demo_input, pack_resolve,
    runner_host::{DemoRunnerHost, FlowOutcome, OperatorContext, primary_provider_type},
    setup::{ProvidersInput, discover_tenants},
    timer_scheduler::{TimerScheduler, TimerSchedulerConfig, discover_timer_handlers},
};
use crate::dev_store_path;
use crate::discovery;
use crate::domains::{self, Domain, DomainAction};
use crate::gmap::{self, Policy};
use crate::messaging_universal::{
    dto::{EncodeInV1, EncodeOutV1, RenderPlanOutV1, SendPayloadOutV1},
    egress,
};
use crate::operator_i18n;
use crate::operator_log;
use crate::project;
use crate::provider_registry;
use crate::qa_setup_wizard;
use crate::runner_exec;
use crate::runner_integration;
use crate::runtime_state::RuntimePaths;
use crate::secrets_gate::{self, DynSecretsManager, SecretsManagerHandle};
use crate::secrets_manager;
use crate::secrets_setup::resolve_env;
use crate::setup_input::{SetupInputAnswers, collect_setup_answers, load_setup_input};
use crate::state_layout;
use crate::subscriptions_universal::{
    build_runner,
    scheduler::Scheduler,
    service::{SubscriptionEnsureRequest, SubscriptionService},
    state_root,
    store::{AuthUserRefV1, SubscriptionStore},
};
use crate::wizard;
use crate::wizard_executor;
use crate::wizard_i18n;
use crate::wizard_plan_builder;
use crate::wizard_spec_builder;
use greentic_qa_lib::{
    I18nConfig, QaLibError, QaRunner, ResolvedI18nMap, WizardDriver, WizardFrontend,
    WizardRunConfig,
};
use greentic_runner_host::secrets::default_manager;
use greentic_types::{ChannelMessageEnvelope, Destination, EnvId, TeamId, TenantCtx, TenantId};
use std::time::Duration;
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "greentic-operator")]
#[command(about = "Greentic operator tooling", version)]
pub struct Cli {
    #[arg(long, global = true, help = "CLI locale (for translated output).")]
    locale: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Demo(Box<DemoCommand>),
    #[command(
        about = "Alias of demo wizard. Plan/create a demo bundle with pack refs and allow rules."
    )]
    Wizard(Box<DemoWizardArgs>),
}

#[derive(Parser)]
struct DemoCommand {
    #[arg(long, global = true)]
    debug: bool,
    #[command(subcommand)]
    command: DemoSubcommand,
}

#[derive(Subcommand)]
enum DemoSubcommand {
    Build(DemoBuildArgs),
    #[command(hide = true)]
    Up(DemoUpArgs),
    Start(DemoUpArgs),
    Setup(DemoSetupArgs),
    Send(DemoSendArgs),
    #[command(about = "Send a synthetic HTTP request through the messaging ingress pipeline")]
    Ingress(DemoIngressArgs),
    New(DemoNewArgs),
    Status(DemoStatusArgs),
    Logs(DemoLogsArgs),
    Doctor(DemoDoctorArgs),
    #[command(about = "Allow a tenant/team access to a pack/flow/node")]
    Allow(DemoPolicyArgs),
    #[command(about = "Forbid a tenant/team access to a pack/flow/node")]
    Forbid(DemoPolicyArgs),
    #[command(about = "Manage demo subscriptions via provider components")]
    Subscriptions(DemoSubscriptionsCommand),
    #[command(about = "Manage capability resolution/invocation in demo bundles")]
    Capability(DemoCapabilityCommand),
    #[command(about = "Run a pack/flow with inline input")]
    Run(DemoRunArgs),
    #[command(about = "List resolved packs from a bundle")]
    ListPacks(DemoListPacksArgs),
    #[command(about = "List flows declared by a pack")]
    ListFlows(DemoListFlowsArgs),
    #[command(
        about = "Alias of wizard. Plan or create a demo bundle from pack refs and allow rules"
    )]
    Wizard(DemoWizardArgs),
    #[command(about = "Run interactive card-based setup wizard for a provider pack")]
    SetupWizard(DemoSetupWizardArgs),
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum DomainArg {
    Messaging,
    Events,
    Secrets,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PlanFormat {
    Text,
    Json,
    Yaml,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CloudflaredModeArg {
    On,
    Off,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum NgrokModeArg {
    On,
    Off,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum RestartTarget {
    All,
    Cloudflared,
    Ngrok,
    Nats,
    Gateway,
    Egress,
    Subscriptions,
}

#[derive(Parser)]
#[command(
    about = "Build a portable demo bundle.",
    long_about = "Copies packs/providers/tenants and writes resolved manifests under the output directory.",
    after_help = "Main options:\n  --out <DIR>\n\nOptional options:\n  --tenant <TENANT>\n  --team <TEAM>\n  --allow-pack-dirs\n  --only-used-providers\n  --doctor\n  --skip-doctor\n  --project-root <PATH> (default: current directory)"
)]
struct DemoBuildArgs {
    #[arg(long)]
    out: PathBuf,
    #[arg(long)]
    tenant: Option<String>,
    #[arg(long)]
    team: Option<String>,
    #[arg(long)]
    allow_pack_dirs: bool,
    #[arg(long)]
    only_used_providers: bool,
    #[arg(long)]
    doctor: bool,
    #[arg(long)]
    skip_doctor: bool,
    #[arg(long)]
    project_root: Option<PathBuf>,
}

#[derive(Parser)]
#[command(
    about = "Start demo services from a bundle.",
    long_about = "Uses resolved manifests inside the bundle to start services and optional NATS."
)]
struct DemoUpArgs {
    #[arg(
        long,
        help_heading = "Main options",
        help = "Path to the bundle directory to run in bundle mode."
    )]
    bundle: Option<PathBuf>,
    #[arg(
        long = "domains",
        alias = "domain",
        value_enum,
        value_delimiter = ',',
        default_value = "all",
        help_heading = "Optional options",
        help = "Domain(s) to operate on (messaging, events, secrets, all); defaults to auto-detect from the bundle."
    )]
    domain: DemoSetupDomainArg,
    #[arg(
        long,
        help_heading = "Main options",
        help = "JSON/YAML file describing provider setup inputs."
    )]
    setup_input: Option<PathBuf>,
    #[arg(
        long,
        help_heading = "Main options",
        help = "Optional override for the public base URL injected into every setup input."
    )]
    public_base_url: Option<String>,
    #[arg(
        long,
        help_heading = "Optional options",
        help = "Tenant to target when running the bundle (defaults to demo)."
    )]
    tenant: Option<String>,
    #[arg(
        long,
        help_heading = "Optional options",
        help = "Team to assign when running demo services."
    )]
    team: Option<String>,
    #[arg(
        long,
        help_heading = "Optional options",
        help = "Legacy flag (sets --nats=external) still honored for compatibility.",
        hide = true,
        conflicts_with = "nats"
    )]
    no_nats: bool,
    #[arg(
        long = "nats",
        value_enum,
        default_value_t = NatsModeArg::Off,
        help_heading = "Optional options",
        help = "Selects the NATS mode: off (default), on (legacy local NATS), or external (explicit URL)."
    )]
    nats: NatsModeArg,
    #[arg(
        long,
        help_heading = "Optional options",
        help = "URL of an existing NATS server to use instead of spawning one (default: nats://127.0.0.1:4222)."
    )]
    nats_url: Option<String>,
    #[arg(
        long,
        default_value = "demo",
        help_heading = "Optional options",
        help = "Environment used for secrets lookups."
    )]
    env: String,
    #[arg(
        long,
        help_heading = "Optional options",
        help = "Path to a prebuilt config file to use instead of auto-discovery."
    )]
    config: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = CloudflaredModeArg::On, help_heading = "Optional options", help = "Whether to start cloudflared for webhook tunneling.")]
    cloudflared: CloudflaredModeArg,
    #[arg(
        long,
        help_heading = "Optional options",
        help = "Explicit path to the cloudflared binary used when cloudflared mode is on."
    )]
    cloudflared_binary: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = NgrokModeArg::Off, help_heading = "Optional options", help = "Whether to start ngrok for webhook tunneling (alternative to cloudflared).")]
    ngrok: NgrokModeArg,
    #[arg(
        long,
        help_heading = "Optional options",
        help = "Explicit path to the ngrok binary used when ngrok mode is on."
    )]
    ngrok_binary: Option<PathBuf>,
    #[arg(
        long,
        value_enum,
        value_delimiter = ',',
        help_heading = "Optional options",
        help = "Comma-separated list of services to restart before running demo (e.g. gateway)."
    )]
    restart: Vec<RestartTarget>,
    #[arg(
        long,
        value_delimiter = ',',
        help_heading = "Optional options",
        help = "CSV list of provider pack IDs to restrict setup to."
    )]
    providers: Vec<String>,
    #[arg(
        long,
        help_heading = "Optional options",
        help = "Avoid running provider setup flows."
    )]
    skip_setup: bool,
    #[arg(
        long,
        help_heading = "Optional options",
        help = "Skip greentic-secrets init during setup."
    )]
    skip_secrets_init: bool,
    #[arg(
        long,
        help_heading = "Optional options",
        help = "Run webhook verification flows after setup completes."
    )]
    verify_webhooks: bool,
    #[arg(
        long,
        help_heading = "Optional options",
        help = "Force re-run of setup flows even if records already exist."
    )]
    force_setup: bool,
    #[arg(
        long,
        help_heading = "Optional options",
        help = "Allow stored-vs-resolved contract hash changes when writing provider config."
    )]
    allow_contract_change: bool,
    #[arg(
        long,
        help_heading = "Optional options",
        help = "Write a single .bak backup before replacing provider config envelopes."
    )]
    backup: bool,
    #[arg(
        long,
        help_heading = "Optional options",
        help = "Path to a greentic-runner binary override."
    )]
    runner_binary: Option<PathBuf>,
    #[arg(
        long,
        value_name = "DIR",
        help_heading = "Optional options",
        help = "Directory to write operator.log, cloudflared.log, and nats.log (default: ./logs or bundle/logs)."
    )]
    log_dir: Option<PathBuf>,
    #[arg(
        long,
        help_heading = "Optional options",
        help = "Enable verbose operator logging (debug level).",
        conflicts_with = "quiet"
    )]
    verbose: bool,
    #[arg(
        long,
        help_heading = "Optional options",
        help = "Suppress operator logging below warnings.",
        conflicts_with = "verbose"
    )]
    quiet: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum DemoSetupDomainArg {
    Messaging,
    Events,
    Secrets,
    #[value(alias = "auto")]
    All,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum NatsModeArg {
    Off,
    On,
    External,
}

impl DemoSetupDomainArg {
    fn resolve_domains(self, discovery: Option<&discovery::DiscoveryResult>) -> Vec<Domain> {
        match self {
            DemoSetupDomainArg::Messaging => vec![Domain::Messaging],
            DemoSetupDomainArg::Events => vec![Domain::Events],
            DemoSetupDomainArg::Secrets => vec![Domain::Secrets],
            DemoSetupDomainArg::All => {
                let mut enabled = Vec::new();
                let has_messaging = discovery
                    .map(|value| value.domains.messaging)
                    .unwrap_or(true);
                let has_events = discovery.map(|value| value.domains.events).unwrap_or(true);
                if has_messaging {
                    enabled.push(Domain::Messaging);
                }
                if has_events {
                    enabled.push(Domain::Events);
                }
                enabled.push(Domain::Secrets);
                enabled
            }
        }
    }
}

impl From<NatsModeArg> for demo::NatsMode {
    fn from(value: NatsModeArg) -> Self {
        match value {
            NatsModeArg::Off => demo::NatsMode::Off,
            NatsModeArg::On => demo::NatsMode::On,
            NatsModeArg::External => demo::NatsMode::External,
        }
    }
}

#[derive(Parser)]
#[command(
    about = "Run provider setup flows against a demo bundle.",
    long_about = "Executes setup flows for provider packs included in the bundle.",
    after_help = "Main options:\n  --bundle <DIR>\n  --tenant <TENANT>\n\nOptional options:\n  --team <TEAM>\n  --domain <messaging|events|secrets|all> (default: all)\n  --provider <FILTER>\n  --dry-run\n  --format <text|json|yaml> (default: text)\n  --parallel <N> (default: 1)\n  --allow-missing-setup\n  --allow-contract-change\n  --backup\n  --online\n  --secrets-env <ENV>\n  --skip-secrets-init\n  --setup-input <PATH>\n  --runner-binary <PATH>\n  --best-effort"
)]
struct DemoSetupArgs {
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long)]
    tenant: String,
    #[arg(long)]
    team: Option<String>,
    #[arg(long, value_enum, default_value_t = DemoSetupDomainArg::All)]
    domain: DemoSetupDomainArg,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    dry_run: bool,
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
    #[arg(long, default_value_t = 1)]
    parallel: usize,
    #[arg(long)]
    allow_missing_setup: bool,
    #[arg(long)]
    allow_contract_change: bool,
    #[arg(long)]
    backup: bool,
    #[arg(long)]
    online: bool,
    #[arg(long)]
    secrets_env: Option<String>,
    #[arg(long)]
    skip_secrets_init: bool,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    runner_binary: Option<PathBuf>,
    #[arg(long)]
    setup_input: Option<PathBuf>,
    #[arg(long)]
    best_effort: bool,
}

#[derive(Parser)]
#[command(
    long_about = "Updates the demo bundle's gmap, reruns the resolver, and copies the updated manifest so demo start sees the change immediately.",
    after_help = "Main options:\n  --bundle <DIR>\n  --tenant <TENANT>\n  --path <PACK[/FLOW[/NODE]] (up to 3 segments)\n\nOptional options:\n  --team <TEAM>\n\nPaths use the same PACK[/FLOW[/NODE]] syntax as the dev allow/forbid commands (max 3 segments). The command modifies tenants/<tenant>[/teams/<team>]/(tenant|team).gmap, resolves state/resolved/<tenant>[.<team>].yaml, and overwrites resolved/<tenant>[.<team>].yaml so demo start picks it up without a rebuild."
)]
struct DemoPolicyArgs {
    #[arg(long, help = "Path to the demo bundle directory.")]
    bundle: PathBuf,
    #[arg(long, help = "Tenant owning the gmap rule.")]
    tenant: String,
    #[arg(long, help = "Team owning the gmap rule.")]
    team: Option<String>,
    #[arg(long, help = "Gmap path to allow or forbid.")]
    path: String,
}

#[derive(Parser)]
#[command(
    about = "Plan/create a demo bundle with pack refs and allow rules.",
    long_about = "Builds a deterministic wizard plan first. Execution reuses the same gmap + resolver + resolved-copy lifecycle as demo allow.",
    after_help = "Main options:\n  --mode <create|update|remove>\n  --bundle <DIR> (or provide in --qa-answers)\n\nOptional options:\n  --qa-answers <PATH>\n  --catalog-pack <ID> (repeatable)\n  --pack-ref <REF> (repeatable, oci://|repo://|store://)\n  --provider-registry <REF>\n  --locale <TAG> (default: detected from system locale)\n  --tenant <TENANT> (default: demo)\n  --team <TEAM>\n  --target <tenant[:team]> (repeatable)\n  --allow <PACK[/FLOW[/NODE]]> (repeatable)\n  --execute\n  --dry-run\n  --offline\n  --verbose\n  --run-setup"
)]
struct DemoWizardArgs {
    #[arg(long, value_enum, default_value_t = WizardModeArg::Create)]
    mode: WizardModeArg,
    #[arg(long, help = "Path to the demo bundle to create.")]
    bundle: Option<PathBuf>,
    #[arg(
        long = "qa-answers",
        help = "Optional JSON/YAML answers emitted by greentic-qa."
    )]
    qa_answers: Option<PathBuf>,
    #[arg(
        long = "catalog-pack",
        help = "Catalog pack id to include (repeatable)."
    )]
    catalog_packs: Vec<String>,
    #[arg(long = "catalog-file", help = "Optional catalog JSON/YAML file.")]
    catalog_file: Option<PathBuf>,
    #[arg(
        long = "pack-ref",
        help = "Custom pack ref (oci://, repo://, store://); repeatable."
    )]
    pack_refs: Vec<String>,
    #[arg(
        long = "provider-registry",
        help = "Provider registry override (file://<path> or local path)."
    )]
    provider_registry: Option<String>,
    #[arg(long, default_value = "demo", help = "Tenant for allow rules.")]
    tenant: String,
    #[arg(long, help = "Optional team for allow rules.")]
    team: Option<String>,
    #[arg(
        long = "target",
        help = "Tenant target in tenant[:team] form; repeatable."
    )]
    targets: Vec<String>,
    #[arg(
        long = "allow",
        help = "Allow path PACK[/FLOW[/NODE]] for tenant/team; repeatable."
    )]
    allow_paths: Vec<String>,
    #[arg(
        long,
        conflicts_with = "dry_run",
        help = "Execute the plan. Without this, only prints plan."
    )]
    execute: bool,
    #[arg(
        long,
        conflicts_with = "execute",
        help = "Force plan-only mode (dry-run)."
    )]
    dry_run: bool,
    #[arg(long, help = "Resolve packs in offline mode (cache-only).")]
    offline: bool,
    #[arg(long, help = "Locale tag for wizard QA rendering.")]
    locale: Option<String>,
    #[arg(long, help = "Print detailed plan step fields.")]
    verbose: bool,
    #[arg(long, help = "Run existing provider setup flows after execution.")]
    run_setup: bool,
    #[arg(long, help = "Optional JSON/YAML setup-input passed to setup runner.")]
    setup_input: Option<PathBuf>,
}

#[derive(Parser)]
#[command(about = "Run interactive card-based setup wizard for a provider pack.")]
struct DemoSetupWizardArgs {
    #[arg(long, help = "Path to the .gtpack file.")]
    pack: PathBuf,
    #[arg(long, help = "Provider ID (default: derived from pack manifest).")]
    provider: Option<String>,
    #[arg(long, default_value = "demo", help = "Tenant ID.")]
    tenant: String,
    #[arg(long, help = "Team ID.")]
    team: Option<String>,
    #[arg(long, help = "Setup flow to run (default: setup_default).")]
    flow: Option<String>,
    #[arg(long, help = "Path to demo bundle (for secrets resolution).")]
    bundle: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum WizardModeArg {
    Create,
    Update,
    Remove,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct WizardQaAnswers {
    #[serde(alias = "bundle_path")]
    bundle: Option<PathBuf>,
    bundle_name: Option<String>,
    #[serde(default)]
    catalog_packs: Vec<WizardCatalogPackAnswer>,
    #[serde(default)]
    pack_refs: Vec<WizardPackRefAnswer>,
    tenant: Option<String>,
    team: Option<String>,
    #[serde(default)]
    targets: Vec<WizardTargetAnswer>,
    #[serde(default)]
    allow_paths: Vec<String>,
    #[serde(default)]
    providers: Vec<WizardProviderAnswer>,
    #[serde(default)]
    update_ops: Vec<WizardUpdateOpAnswer>,
    #[serde(default)]
    packs_remove: Vec<WizardPackRemoveAnswer>,
    #[serde(default)]
    providers_remove: Vec<WizardProviderAnswer>,
    #[serde(default)]
    tenants_remove: Vec<WizardTargetAnswer>,
    #[serde(default)]
    access_change: Vec<WizardAccessChangeAnswer>,
    access_mode: Option<String>,
    #[serde(default)]
    remove_targets: Vec<WizardRemoveTargetAnswer>,
    locale: Option<String>,
    execution_mode: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum WizardCatalogPackAnswer {
    Id(String),
    Item { id: String },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum WizardPackRefAnswer {
    Ref(String),
    Item {
        pack_ref: String,
        #[serde(default)]
        access_scope: Option<String>,
        #[serde(default)]
        #[serde(alias = "make_default_scope")]
        make_default_pack: Option<String>,
        #[serde(default)]
        tenant_id: Option<String>,
        #[serde(default)]
        team_id: Option<String>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum WizardTargetAnswer {
    Target(String),
    Item {
        tenant_id: String,
        #[serde(default)]
        team_id: Option<String>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum WizardProviderAnswer {
    Id(String),
    Item {
        provider_id: Option<String>,
        id: Option<String>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum WizardUpdateOpAnswer {
    Op(String),
    Item { op: String },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum WizardRemoveTargetAnswer {
    Target(String),
    Item {
        target_type: Option<String>,
        target: Option<String>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum WizardPackRemoveAnswer {
    Pack(String),
    Item {
        pack_identifier: Option<String>,
        pack_id: Option<String>,
        pack_ref: Option<String>,
        scope: Option<String>,
        tenant_id: Option<String>,
        team_id: Option<String>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum WizardAccessChangeAnswer {
    Item {
        pack_id: Option<String>,
        pack_ref: Option<String>,
        operation: Option<String>,
        tenant_id: String,
        #[serde(default)]
        team_id: Option<String>,
    },
}

const DEFAULT_PROVIDER_REGISTRY_REF: &str = "oci://ghcr.io/greenticai/registries/providers:latest";
#[derive(Parser)]
#[command(
    about = "Show demo service status using runtime state.",
    long_about = "Lists pidfiles under state/pids for the selected tenant/team.",
    after_help = "Main options:\n  (none)\n\nOptional options:\n  --tenant <TENANT> (default: demo)\n  --team <TEAM> (default: default)\n  --state-dir <PATH> (default: ./state or <bundle>/state)\n  --bundle <DIR> (legacy mode if --state-dir omitted)\n  --verbose\n  --no-nats"
)]
struct DemoStatusArgs {
    #[arg(long)]
    bundle: Option<PathBuf>,
    #[arg(long, default_value = "demo")]
    tenant: String,
    #[arg(long, default_value = "default")]
    team: String,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    verbose: bool,
    #[arg(long)]
    no_nats: bool,
}

#[derive(Parser)]
#[command(
    about = "Show demo logs produced by the operator and services.",
    long_about = "Prints or tails logs under logs/operator.log or tenant/service logs in the log directory.",
    after_help = "Main options:\n  <SERVICE> (operator|messaging|nats|cloudflared)\n\nOptional options:\n  --tail\n  --tenant <TENANT> (default: demo)\n  --team <TEAM> (default: default)\n  --log-dir <PATH> (default: ./logs or <bundle>/logs)\n  --bundle <DIR>\n  --verbose\n  --no-nats"
)]
struct DemoLogsArgs {
    #[arg(default_value = "operator")]
    service: String,
    #[arg(long)]
    tail: bool,
    #[arg(long)]
    bundle: Option<PathBuf>,
    #[arg(long, default_value = "demo")]
    tenant: String,
    #[arg(long, default_value = "default")]
    team: String,
    #[arg(long)]
    log_dir: Option<PathBuf>,
    #[arg(long)]
    verbose: bool,
    #[arg(long)]
    no_nats: bool,
}

#[derive(Parser)]
#[command(
    about = "Run demo doctor validation from a bundle.",
    long_about = "Runs greentic-pack doctor against packs in the demo bundle.",
    after_help = "Main options:\n  --bundle <DIR>"
)]
struct DemoDoctorArgs {
    #[arg(long)]
    bundle: PathBuf,
}

#[derive(Parser)]
#[command(
    about = "Send a demo message via a provider pack.",
    long_about = "Runs provider requirements or sends a generic message payload.",
    after_help = "Main options:\n  --bundle <DIR>\n  --provider <PROVIDER>\n\nOptional options:\n  --text <TEXT>\n  --card <FILE>\n  --arg <k=v>...\n  --args-json <JSON>\n  --env <ENV> (default: demo)\n  --tenant <TENANT> (default: demo)\n  --team <TEAM> (default: default)\n  --print-required-args"
)]
struct DemoSendArgs {
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long)]
    provider: String,
    #[arg(long)]
    text: Option<String>,
    #[arg(long = "arg")]
    args: Vec<String>,
    #[arg(long)]
    args_json: Option<String>,
    #[arg(long, default_value = "demo")]
    tenant: String,
    #[arg(long, default_value = "default")]
    team: String,
    #[arg(long)]
    print_required_args: bool,
    #[arg(long)]
    runner_binary: Option<PathBuf>,
    #[arg(long, default_value = "demo")]
    env: String,
    #[arg(long, help = "Destination identifier (repeatable).")]
    to: Vec<String>,
    #[arg(
        long = "to-kind",
        help = "Optional destination kind (chat, channel, room, email, etc.)."
    )]
    to_kind: Option<String>,
    #[arg(
        long,
        value_name = "FILE",
        help = "JSON file containing the adaptive card to include in the message."
    )]
    card: Option<PathBuf>,
}

#[derive(Parser)]
#[command(
    about = "Manage demo subscriptions via provider components.",
    long_about = "Ensure, renew, or delete provider-managed subscriptions from a demo bundle."
)]
struct DemoSubscriptionsCommand {
    #[command(subcommand)]
    command: DemoSubscriptionsSubcommand,
}

#[derive(Parser)]
#[command(
    about = "Manage capabilities in a demo bundle.",
    long_about = "Resolve, invoke, and mark setup status for capability offers."
)]
struct DemoCapabilityCommand {
    #[command(subcommand)]
    command: DemoCapabilitySubcommand,
}

#[derive(Subcommand)]
enum DemoCapabilitySubcommand {
    Invoke(DemoCapabilityInvokeArgs),
    SetupPlan(DemoCapabilitySetupPlanArgs),
    MarkReady(DemoCapabilityMarkReadyArgs),
    MarkFailed(DemoCapabilityMarkFailedArgs),
}

#[derive(Parser)]
#[command(
    about = "Resolve and invoke a capability provider op.",
    long_about = "Uses capability registry resolution and routes to the selected provider op."
)]
struct DemoCapabilityInvokeArgs {
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long)]
    cap_id: String,
    #[arg(long, default_value = "")]
    op: String,
    #[arg(long)]
    payload_json: Option<String>,
    #[arg(long, default_value = "demo")]
    tenant: String,
    #[arg(long, default_value = "default")]
    team: String,
    #[arg(long)]
    env: Option<String>,
}

#[derive(Parser)]
#[command(
    about = "Print capabilities that require setup.",
    long_about = "Builds capability setup plan for current tenant/team scope."
)]
struct DemoCapabilitySetupPlanArgs {
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long, default_value = "demo")]
    tenant: String,
    #[arg(long, default_value = "default")]
    team: String,
}

#[derive(Parser)]
#[command(
    about = "Mark resolved capability as setup-ready.",
    long_about = "Writes capability install record with ready status for the selected capability."
)]
struct DemoCapabilityMarkReadyArgs {
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long)]
    cap_id: String,
    #[arg(long, default_value = "demo")]
    tenant: String,
    #[arg(long, default_value = "default")]
    team: String,
}

#[derive(Parser)]
#[command(
    about = "Mark resolved capability as setup-failed.",
    long_about = "Writes capability install record with failed status for the selected capability."
)]
struct DemoCapabilityMarkFailedArgs {
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long)]
    cap_id: String,
    #[arg(long, default_value = "setup_failed")]
    key: String,
    #[arg(long, default_value = "demo")]
    tenant: String,
    #[arg(long, default_value = "default")]
    team: String,
}

#[derive(Parser)]
#[command(
    about = "Run a pack/flow with inline input.",
    long_about = "Resolves the selected pack, picks the requested or default flow, parses any provided input, and prints a run summary."
)]
struct DemoRunArgs {
    #[arg(long, default_value = "./packs")]
    packs_dir: PathBuf,
    #[arg(long)]
    bundle: Option<PathBuf>,
    #[arg(long)]
    pack: String,
    #[arg(long)]
    tenant: String,
    #[arg(long)]
    team: Option<String>,
    #[arg(long)]
    flow: Option<String>,
    #[arg(long)]
    input: Option<String>,
}

#[derive(Parser)]
#[command(
    about = "List provider packs for a domain",
    long_about = "Prints each pack_id and how many entry flows it declares for the selected domain."
)]
struct DemoListPacksArgs {
    #[arg(long, default_value = ".")]
    bundle: PathBuf,
    #[arg(long, value_enum, default_value_t = DomainArg::Messaging)]
    domain: DomainArg,
}

#[derive(Parser)]
#[command(
    about = "List flows exposed by a provider pack",
    long_about = "Shows the entry flows declared by the matching pack so you can pass --flow to demo run."
)]
struct DemoListFlowsArgs {
    #[arg(long, default_value = ".")]
    bundle: PathBuf,
    #[arg(long)]
    pack: String,
    #[arg(long, value_enum, default_value_t = DomainArg::Messaging)]
    domain: DomainArg,
}

#[derive(Subcommand)]
enum DemoSubscriptionsSubcommand {
    Ensure(DemoSubscriptionsEnsureArgs),
    Status(DemoSubscriptionsStatusArgs),
    Renew(DemoSubscriptionsRenewArgs),
    Delete(DemoSubscriptionsDeleteArgs),
}

#[derive(Parser)]
#[command(
    about = "Ensure a subscription binding via a demo provider.",
    long_about = "Invokes the provider's subscription_ensure flow, persists the binding state, and returns the binding_id."
)]
struct DemoSubscriptionsEnsureArgs {
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long)]
    provider: String,
    #[arg(long, default_value = "demo")]
    tenant: String,
    #[arg(long, default_value = "default")]
    team: String,
    #[arg(long)]
    binding_id: Option<String>,
    #[arg(long)]
    resource: Option<String>,
    #[arg(long = "change-type", action = ArgAction::Append)]
    change_types: Vec<String>,
    #[arg(long)]
    notification_url: Option<String>,
    #[arg(long)]
    client_state: Option<String>,
    #[arg(long)]
    user_id: Option<String>,
    #[arg(long)]
    user_token_key: Option<String>,
}

#[derive(Parser)]
#[command(
    about = "List demo subscription bindings persisted by the operator.",
    long_about = "Prints provider/tenant/team/binding info for demo-managed subscriptions."
)]
struct DemoSubscriptionsStatusArgs {
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    binding_id: Option<String>,
    #[arg(long, default_value = "demo")]
    tenant: String,
    #[arg(long, default_value = "default")]
    team: String,
}

#[derive(Parser)]
#[command(
    about = "Renew stored subscriptions that are near expiry.",
    long_about = "Runs the scheduler to renew eligible bindings or a single binding if --binding-id is provided."
)]
struct DemoSubscriptionsRenewArgs {
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long)]
    binding_id: Option<String>,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long, default_value = "demo")]
    tenant: String,
    #[arg(long, default_value = "default")]
    team: String,
    #[arg(long, default_value = "10")]
    skew_minutes: u64,
}

#[derive(Parser)]
#[command(
    about = "Delete a persisted demo subscription binding through the provider.",
    long_about = "Invokes subscription_delete for the binding and removes the stored state file."
)]
struct DemoSubscriptionsDeleteArgs {
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long)]
    binding_id: String,
    #[arg(long)]
    provider: String,
    #[arg(long, default_value = "demo")]
    tenant: String,
    #[arg(long, default_value = "default")]
    team: String,
}

impl DemoSubscriptionsCommand {
    fn run(self) -> anyhow::Result<()> {
        match self.command {
            DemoSubscriptionsSubcommand::Ensure(args) => args.run(),
            DemoSubscriptionsSubcommand::Status(args) => args.run(),
            DemoSubscriptionsSubcommand::Renew(args) => args.run(),
            DemoSubscriptionsSubcommand::Delete(args) => args.run(),
        }
    }
}

impl DemoCapabilityCommand {
    fn run(self) -> anyhow::Result<()> {
        match self.command {
            DemoCapabilitySubcommand::Invoke(args) => args.run(),
            DemoCapabilitySubcommand::SetupPlan(args) => args.run(),
            DemoCapabilitySubcommand::MarkReady(args) => args.run(),
            DemoCapabilitySubcommand::MarkFailed(args) => args.run(),
        }
    }
}

impl DemoRunArgs {
    fn run(self, _ctx: &AppCtx) -> anyhow::Result<()> {
        let packs_dir = self
            .bundle
            .clone()
            .map(|bundle| bundle.join("packs"))
            .unwrap_or(self.packs_dir);
        let pack = pack_resolve::resolve_pack(&packs_dir, &self.pack)?;
        let pack_path = ensure_pack_within_root(&packs_dir, &pack.pack_path)?;
        let flow_id = pack.select_flow(self.flow.as_deref())?;
        let parsed_input = match self.input {
            Some(value) => Some(demo_input::parse_input(&value)?),
            None => None,
        };
        let team_display = self.team.as_deref().unwrap_or("default");
        let input_desc = match &parsed_input {
            None => "none".to_string(),
            Some(parsed) => match &parsed.source {
                demo_input::InputSource::Inline(encoding) => {
                    format!("inline ({})", encoding.label())
                }
                demo_input::InputSource::File { path, encoding } => {
                    format!("file {} ({})", path.display(), encoding.label())
                }
            },
        };
        println!(
            "{}",
            operator_i18n::tr("cli.run.summary_header", "Run summary:")
        );
        println!(
            "{}",
            operator_i18n::trf(
                "cli.run.summary_pack",
                "  pack: {} ({})",
                &[&pack.pack_id, &pack_path.display().to_string()]
            )
        );
        println!(
            "{}",
            operator_i18n::trf(
                "cli.run.summary_tenant_team",
                "  tenant: {} team: {}",
                &[&self.tenant, team_display]
            )
        );
        println!(
            "{}",
            operator_i18n::trf("cli.run.summary_flow", "  flow: {}", &[&flow_id])
        );
        println!(
            "{}",
            operator_i18n::trf("cli.run.summary_input", "  input: {}", &[&input_desc])
        );

        let initial_input = parsed_input
            .as_ref()
            .map(|parsed| parsed.value.clone())
            .unwrap_or_else(|| json!({}));
        let secrets_manager = if let Some(bundle) = &self.bundle {
            let secrets_handle =
                secrets_gate::resolve_secrets_manager(bundle, &self.tenant, self.team.as_deref())?;
            secrets_handle.runtime_manager(Some(&pack.pack_id))
        } else {
            default_manager()?
        };
        let runner = DemoRunner::with_entry_flow(
            pack_path,
            &self.tenant,
            self.team.clone(),
            flow_id.clone(),
            pack.pack_id.clone(),
            initial_input,
            secrets_manager,
        )?;
        let mut repl = DemoRepl::new(runner);
        println!(
            "{}",
            operator_i18n::tr(
                "cli.run.enter_interactive",
                "Entering interactive mode (type @help for commands)."
            )
        );
        repl.run()?;
        Ok(())
    }
}

fn ensure_pack_within_root(root: &Path, pack_path: &Path) -> anyhow::Result<PathBuf> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let resolved = std::fs::canonicalize(pack_path).unwrap_or_else(|_| pack_path.to_path_buf());
    if resolved.starts_with(&root) {
        return Ok(pack_path.to_path_buf());
    }
    let file_name = pack_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("pack path missing file name"))?;
    let cache_dir = root.join(".resolved");
    std::fs::create_dir_all(&cache_dir)?;
    let dest = cache_dir.join(file_name);
    std::fs::copy(&resolved, &dest)?;
    Ok(dest)
}

impl DemoListPacksArgs {
    fn run(self, _ctx: &AppCtx) -> anyhow::Result<()> {
        let domain = Domain::from(self.domain);
        let cfg = domains::config(domain);
        let packs = demo_provider_packs(&self.bundle, domain)?;
        let providers_root = self.bundle.join(cfg.providers_dir);
        let apps_root = self.bundle.join("packs");
        let mut provider_packs = Vec::new();
        let mut app_packs = Vec::new();
        for pack in packs {
            if pack.path.starts_with(&providers_root) {
                provider_packs.push(pack);
            } else if pack.path.starts_with(&apps_root) {
                app_packs.push(pack);
            } else {
                provider_packs.push(pack);
            }
        }

        if provider_packs.is_empty() {
            println!(
                "{}",
                operator_i18n::trf(
                    "cli.list_packs.none_for_domain",
                    "no packs found for domain {}",
                    &[domains::domain_name(domain)]
                )
            );
        } else {
            println!(
                "{}",
                operator_i18n::trf(
                    "cli.list_packs.for_domain",
                    "packs for {}:",
                    &[domains::domain_name(domain)]
                )
            );
            for pack in &provider_packs {
                println!(
                    "  {} ({} entry flows) {}",
                    pack.pack_id,
                    pack.entry_flows.len(),
                    pack.file_name
                );
            }
        }

        if !app_packs.is_empty() {
            if !provider_packs.is_empty() {
                println!();
            }
            println!(
                "{}",
                operator_i18n::tr("cli.list_packs.for_applications", "packs for applications:")
            );
            for pack in app_packs {
                let relative = pack
                    .path
                    .strip_prefix(&apps_root)
                    .unwrap_or_else(|_| Path::new(&pack.file_name));
                let mut trimmed = relative.to_string_lossy().to_string();
                if let Some(stripped) = trimmed.strip_suffix(".gtpack") {
                    trimmed = stripped.to_string();
                }
                let has_parent = relative
                    .parent()
                    .map(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or(false);
                let display_name = if has_parent {
                    format!("/{trimmed}")
                } else {
                    trimmed
                };
                let depth = relative.components().count().saturating_sub(1);
                let indent = " ".repeat(depth);
                println!(
                    "  {indent}{display_name} ({} entry flows) {}",
                    pack.entry_flows.len(),
                    pack.file_name
                );
            }
        }
        Ok(())
    }
}

impl DemoListFlowsArgs {
    fn run(self, _ctx: &AppCtx) -> anyhow::Result<()> {
        let domain = Domain::from(self.domain);
        let pack = demo_provider_pack_by_filter(&self.bundle, domain, &self.pack)?;
        println!(
            "{}",
            operator_i18n::trf(
                "cli.list_flows.header",
                "flows declared by pack {} ({}):",
                &[&pack.pack_id, &pack.file_name]
            )
        );
        for flow_id in pack.entry_flows {
            println!(
                "{}",
                operator_i18n::trf("cli.list_flows.item", "  - {}", &[&flow_id])
            );
        }
        Ok(())
    }
}

impl DemoSubscriptionsEnsureArgs {
    fn run(self) -> anyhow::Result<()> {
        let DemoSubscriptionsEnsureArgs {
            bundle,
            provider,
            tenant,
            team,
            binding_id,
            resource,
            change_types,
            notification_url,
            client_state,
            user_id,
            user_token_key,
        } = self;

        let team_override = if team.trim().is_empty() {
            None
        } else {
            Some(team)
        };

        domains::ensure_cbor_packs(&bundle)?;
        let pack = resolve_demo_provider_pack(
            &bundle,
            &tenant,
            team_override.as_deref(),
            &provider,
            Domain::Messaging,
        )?;
        let discovery = discovery::discover_with_options(
            &bundle,
            discovery::DiscoveryOptions { cbor_only: true },
        )?;
        let provider_map = discovery_map(&discovery.providers);
        let provider_id = provider_id_for_pack(&pack.path, &pack.pack_id, Some(&provider_map));

        let secrets_handle =
            secrets_gate::resolve_secrets_manager(&bundle, &tenant, team_override.as_deref())?;
        let runner_host = DemoRunnerHost::new(
            bundle.clone(),
            &discovery,
            None,
            secrets_handle.clone(),
            false,
        )?;
        let context = OperatorContext {
            tenant: tenant.clone(),
            team: team_override.clone(),
            correlation_id: None,
        };
        let service = SubscriptionService::new(runner_host, context);

        let binding_id = binding_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let request = build_subscription_request(
            &binding_id,
            resource,
            change_types,
            notification_url,
            client_state,
            user_id,
            user_token_key,
        );
        let state = service.ensure_once(&provider_id, &request)?;

        let store = SubscriptionStore::new(state_root(&bundle));
        store.write_state(&state)?;
        let state_path = store.state_path(
            &state.provider,
            &state.tenant,
            state.team.as_deref(),
            &state.binding_id,
        );
        println!(
            "subscription binding {} persisted to {}",
            state.binding_id,
            state_path.display()
        );
        Ok(())
    }
}

fn build_subscription_request(
    binding_id: &str,
    resource: Option<String>,
    change_types: Vec<String>,
    notification_url: Option<String>,
    client_state: Option<String>,
    user_id: Option<String>,
    user_token_key: Option<String>,
) -> SubscriptionEnsureRequest {
    let change_types = if change_types.is_empty() {
        vec!["created".to_string()]
    } else {
        change_types
    };
    let user = match (user_id, user_token_key) {
        (Some(user_id), Some(token_key)) => Some(AuthUserRefV1 {
            user_id,
            token_key,
            tenant_id: None,
            email: None,
            display_name: None,
        }),
        _ => None,
    };
    SubscriptionEnsureRequest {
        binding_id: binding_id.to_string(),
        resource,
        change_types,
        notification_url,
        client_state,
        user,
        expiration_target_unix_ms: None,
    }
}

impl DemoSubscriptionsStatusArgs {
    fn run(self) -> anyhow::Result<()> {
        let DemoSubscriptionsStatusArgs {
            bundle,
            provider,
            binding_id,
            tenant,
            team,
        } = self;
        let team = if team.trim().is_empty() {
            None
        } else {
            Some(team.clone())
        };
        let store = SubscriptionStore::new(state_root(&bundle));
        let states = store.list_states()?;
        let filtered = states
            .into_iter()
            .filter(|state| state.tenant == tenant)
            .filter(|state| match team.as_deref() {
                Some(team) => state.team.as_deref().unwrap_or("default") == team,
                None => true,
            })
            .filter(|state| {
                provider
                    .as_deref()
                    .map(|value| state.provider == value)
                    .unwrap_or(true)
            })
            .filter(|state| {
                binding_id
                    .as_deref()
                    .map(|value| state.binding_id == value)
                    .unwrap_or(true)
            })
            .collect::<Vec<_>>();
        if filtered.is_empty() {
            println!(
                "{}",
                operator_i18n::tr("cli.subscriptions.none", "no subscriptions found")
            );
            return Ok(());
        }
        for state in filtered {
            let team_label = state.team.as_deref().unwrap_or("default");
            let expiry = state.expiration_unix_ms.and_then(|ms| {
                Utc.timestamp_millis_opt(ms)
                    .single()
                    .map(|value| value.to_rfc3339())
            });
            println!(
                "{} {} {} binding={} tenant={} team={} expires={}",
                state.provider,
                state.subscription_id.as_deref().unwrap_or("<unknown>"),
                state.change_types.join(","),
                state.binding_id,
                state.tenant,
                team_label,
                expiry.unwrap_or_else(|| "<unknown>".to_string())
            );
        }
        Ok(())
    }
}

impl DemoSubscriptionsRenewArgs {
    fn run(self) -> anyhow::Result<()> {
        let DemoSubscriptionsRenewArgs {
            bundle,
            binding_id,
            provider,
            tenant,
            team,
            skew_minutes,
        } = self;
        let team_override = if team.trim().is_empty() {
            None
        } else {
            Some(team)
        };
        let (runner_host, context) = build_runner(&bundle, &tenant, team_override.clone())?;
        let store = SubscriptionStore::new(state_root(&bundle));
        let scheduler = Scheduler::new(
            SubscriptionService::new(runner_host, context),
            store.clone(),
        );

        if let Some(binding) = binding_id {
            let provider = provider
                .ok_or_else(|| anyhow!("--provider is required when renewing a single binding"))?;
            let state = store
                .read_state(&provider, &tenant, team_override.as_deref(), &binding)?
                .ok_or_else(|| {
                    anyhow!("subscription {binding} not found for provider {provider}")
                })?;
            scheduler.renew_binding(&state)?;
            println!(
                "{}",
                operator_i18n::trf("cli.subscriptions.renewed", "renewed {}", &[&binding])
            );
            return Ok(());
        }

        let skew = Duration::from_secs(skew_minutes * 60);
        scheduler.renew_due(skew)?;
        println!(
            "{}",
            operator_i18n::tr(
                "cli.subscriptions.renewed_eligible",
                "renewed eligible subscriptions"
            )
        );
        Ok(())
    }
}

impl DemoSubscriptionsDeleteArgs {
    fn run(self) -> anyhow::Result<()> {
        let DemoSubscriptionsDeleteArgs {
            bundle,
            binding_id,
            provider,
            tenant,
            team,
        } = self;
        let team_override = if team.trim().is_empty() {
            None
        } else {
            Some(team)
        };
        let (runner_host, context) = build_runner(&bundle, &tenant, team_override.clone())?;
        let store = SubscriptionStore::new(state_root(&bundle));
        let scheduler = Scheduler::new(
            SubscriptionService::new(runner_host, context),
            store.clone(),
        );
        let state = store
            .read_state(&provider, &tenant, team_override.as_deref(), &binding_id)?
            .ok_or_else(|| {
                anyhow!("subscription {binding_id} not found for provider {provider}")
            })?;
        scheduler.delete_binding(&state)?;
        println!(
            "{}",
            operator_i18n::trf("cli.subscriptions.deleted", "deleted {}", &[&binding_id])
        );
        Ok(())
    }
}

impl DemoCapabilityInvokeArgs {
    fn run(self) -> anyhow::Result<()> {
        if let Some(env_value) = self.env.as_ref() {
            // set_var is unsafe in this codebase, so wrap it accordingly.
            unsafe {
                env::set_var("GREENTIC_ENV", env_value);
            }
        }
        domains::ensure_cbor_packs(&self.bundle)?;
        let discovery = discovery::discover_with_options(
            &self.bundle,
            discovery::DiscoveryOptions { cbor_only: true },
        )?;
        let secrets_handle =
            secrets_gate::resolve_secrets_manager(&self.bundle, &self.tenant, Some(&self.team))?;
        let runner_host =
            DemoRunnerHost::new(self.bundle.clone(), &discovery, None, secrets_handle, false)?;
        let ctx = OperatorContext {
            tenant: self.tenant.clone(),
            team: Some(self.team.clone()),
            correlation_id: None,
        };
        let payload_value = if let Some(raw) = self.payload_json.as_ref() {
            serde_json::from_str::<JsonValue>(raw)
                .map_err(|err| anyhow!("invalid --payload-json: {err}"))?
        } else {
            json!({})
        };
        let payload_bytes = serde_json::to_vec(&payload_value)?;
        let outcome =
            runner_host.invoke_capability(&self.cap_id, &self.op, &payload_bytes, &ctx)?;
        print_capability_outcome(&outcome)?;
        if !outcome.success {
            anyhow::bail!(
                "capability invoke failed cap_id={} op={}",
                self.cap_id,
                if self.op.is_empty() {
                    "<binding-default>"
                } else {
                    self.op.as_str()
                }
            );
        }
        Ok(())
    }
}

impl DemoCapabilitySetupPlanArgs {
    fn run(self) -> anyhow::Result<()> {
        domains::ensure_cbor_packs(&self.bundle)?;
        let discovery = discovery::discover_with_options(
            &self.bundle,
            discovery::DiscoveryOptions { cbor_only: true },
        )?;
        let secrets_handle =
            secrets_gate::resolve_secrets_manager(&self.bundle, &self.tenant, Some(&self.team))?;
        let runner_host =
            DemoRunnerHost::new(self.bundle.clone(), &discovery, None, secrets_handle, false)?;
        let ctx = OperatorContext {
            tenant: self.tenant,
            team: Some(self.team),
            correlation_id: None,
        };
        let plan = runner_host.capability_setup_plan(&ctx);
        if plan.is_empty() {
            println!(
                "{}",
                operator_i18n::tr(
                    "cli.capabilities.none_requiring_setup",
                    "no capabilities requiring setup found"
                )
            );
            return Ok(());
        }
        for item in plan {
            println!(
                "{} | cap={} | pack={} | op={} | qa_ref={}",
                item.stable_id,
                item.cap_id,
                item.pack_id,
                item.provider_op,
                item.setup_qa_ref.as_deref().unwrap_or("<none>")
            );
        }
        Ok(())
    }
}

impl DemoCapabilityMarkReadyArgs {
    fn run(self) -> anyhow::Result<()> {
        domains::ensure_cbor_packs(&self.bundle)?;
        let discovery = discovery::discover_with_options(
            &self.bundle,
            discovery::DiscoveryOptions { cbor_only: true },
        )?;
        let secrets_handle =
            secrets_gate::resolve_secrets_manager(&self.bundle, &self.tenant, Some(&self.team))?;
        let runner_host =
            DemoRunnerHost::new(self.bundle.clone(), &discovery, None, secrets_handle, false)?;
        let scope = ResolveScope {
            env: env::var("GREENTIC_ENV").ok(),
            tenant: Some(self.tenant.clone()),
            team: Some(self.team.clone()),
        };
        let Some(binding) = runner_host.resolve_capability(&self.cap_id, None, scope) else {
            anyhow::bail!(
                "capability {} is not offered in current pack set",
                self.cap_id
            );
        };
        let ctx = OperatorContext {
            tenant: self.tenant,
            team: Some(self.team),
            correlation_id: None,
        };
        let path = runner_host.mark_capability_ready(&ctx, &binding)?;
        println!(
            "{}",
            operator_i18n::trf(
                "cli.capabilities.marked_ready",
                "capability marked ready: {}",
                &[&path.display().to_string()]
            )
        );
        Ok(())
    }
}

impl DemoCapabilityMarkFailedArgs {
    fn run(self) -> anyhow::Result<()> {
        domains::ensure_cbor_packs(&self.bundle)?;
        let discovery = discovery::discover_with_options(
            &self.bundle,
            discovery::DiscoveryOptions { cbor_only: true },
        )?;
        let secrets_handle =
            secrets_gate::resolve_secrets_manager(&self.bundle, &self.tenant, Some(&self.team))?;
        let runner_host =
            DemoRunnerHost::new(self.bundle.clone(), &discovery, None, secrets_handle, false)?;
        let scope = ResolveScope {
            env: env::var("GREENTIC_ENV").ok(),
            tenant: Some(self.tenant.clone()),
            team: Some(self.team.clone()),
        };
        let Some(binding) = runner_host.resolve_capability(&self.cap_id, None, scope) else {
            anyhow::bail!(
                "capability {} is not offered in current pack set",
                self.cap_id
            );
        };
        let ctx = OperatorContext {
            tenant: self.tenant,
            team: Some(self.team),
            correlation_id: None,
        };
        let path = runner_host.mark_capability_failed(&ctx, &binding, &self.key)?;
        println!(
            "{}",
            operator_i18n::trf(
                "cli.capabilities.marked_failed",
                "capability marked failed: {}",
                &[&path.display().to_string()]
            )
        );
        Ok(())
    }
}

#[derive(Parser)]
#[command(
    about = "Create a new demo bundle scaffold.",
    long_about = "Initializes the directory layout and metadata files that the demo commands expect.",
    after_help = "Main options:\n  <BUNDLE-NAME>\n\nOptional options:\n  --out <DIR> (default: current working directory)"
)]
struct DemoNewArgs {
    #[arg(value_name = "BUNDLE-NAME")]
    bundle: String,
    #[arg(long)]
    out: Option<PathBuf>,
}
#[derive(Parser)]
#[command(
    about = "Allow/forbid a gmap rule for tenant or team.",
    long_about = "Updates the appropriate gmap file with a deterministic ordering.",
    after_help = "Main options:\n  --tenant <TENANT>\n  --path <PACK[/FLOW[/NODE]]>\n\nOptional options:\n  --team <TEAM>\n  --project-root <PATH> (default: current directory)"
)]
struct DevPolicyArgs {
    #[arg(long)]
    tenant: String,
    #[arg(long)]
    team: Option<String>,
    #[arg(long)]
    path: String,
    #[arg(long)]
    project_root: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Format {
    Text,
    Json,
    Yaml,
}

impl Cli {
    pub fn run(self) -> anyhow::Result<()> {
        let selected_locale = operator_i18n::select_locale(self.locale.as_deref());
        operator_i18n::set_locale(&selected_locale);
        let ctx = AppCtx {};
        match self.command {
            Command::Demo(demo) => demo.run(&ctx),
            Command::Wizard(args) => args.run(),
        }
    }
}

struct AppCtx {}

impl DemoCommand {
    fn run(self, ctx: &AppCtx) -> anyhow::Result<()> {
        if env::var("GREENTIC_ENV").is_err() {
            // set_var is unsafe in this codebase, so wrap it accordingly.
            unsafe {
                std::env::set_var("GREENTIC_ENV", "demo");
            }
        }
        if self.debug {
            unsafe {
                std::env::set_var("GREENTIC_OPERATOR_DEMO_DEBUG", "1");
            }
        }
        match self.command {
            DemoSubcommand::Build(args) => args.run(ctx),
            DemoSubcommand::Up(args) => args.run_start(ctx),
            DemoSubcommand::Start(args) => args.run_start(ctx),
            DemoSubcommand::Setup(args) => args.run(),
            DemoSubcommand::Send(args) => args.run(),
            DemoSubcommand::Ingress(args) => args.run(),
            DemoSubcommand::New(args) => args.run(),
            DemoSubcommand::Status(args) => args.run(),
            DemoSubcommand::Logs(args) => args.run(),
            DemoSubcommand::Doctor(args) => args.run(ctx),
            DemoSubcommand::ListPacks(args) => args.run(ctx),
            DemoSubcommand::ListFlows(args) => args.run(ctx),
            DemoSubcommand::Allow(args) => args.run(Policy::Public),
            DemoSubcommand::Forbid(args) => args.run(Policy::Forbidden),
            DemoSubcommand::Subscriptions(args) => args.run(),
            DemoSubcommand::Capability(args) => args.run(),
            DemoSubcommand::Run(args) => args.run(ctx),
            DemoSubcommand::Wizard(args) => args.run(),
            DemoSubcommand::SetupWizard(args) => args.run(),
        }
    }
}

impl DemoBuildArgs {
    fn run(self, _ctx: &AppCtx) -> anyhow::Result<()> {
        let root = project_root(self.project_root)?;
        if demo_debug_enabled() {
            println!(
                "[demo] build root={} out={} tenant={:?} team={:?} doctor={}",
                root.display(),
                self.out.display(),
                self.tenant,
                self.team,
                self.doctor
            );
        }
        let env_skip_doctor = std::env::var("GREENTIC_OPERATOR_SKIP_DOCTOR").is_ok();
        let skip_doctor = self.skip_doctor || env_skip_doctor;
        let run_doctor = self.doctor || !skip_doctor;
        if demo_debug_enabled() && skip_doctor {
            println!(
                "[demo] skipping doctor gate (skip_doctor flag or GREENTIC_OPERATOR_SKIP_DOCTOR set)"
            );
        }
        let options = BuildOptions {
            out_dir: self.out,
            tenant: self.tenant,
            team: self.team,
            allow_pack_dirs: self.allow_pack_dirs,
            only_used_providers: self.only_used_providers,
            run_doctor,
        };
        let config = config::load_operator_config(&root)?;
        let pack_command = if options.run_doctor {
            let explicit = config::binary_override(config.as_ref(), "greentic-pack", &root);
            Some(bin_resolver::resolve_binary(
                "greentic-pack",
                &ResolveCtx {
                    config_dir: root.clone(),
                    explicit_path: explicit,
                },
            )?)
        } else {
            None
        };
        demo::build_bundle(&root, options, pack_command.as_deref())
    }
}

impl DemoUpArgs {
    fn run_start(self, _ctx: &AppCtx) -> anyhow::Result<()> {
        self.run_with_shutdown()
    }

    fn run_with_shutdown(self) -> anyhow::Result<()> {
        let restart: std::collections::BTreeSet<String> =
            self.restart.iter().map(restart_name).collect();
        let log_level = if self.quiet {
            operator_log::Level::Warn
        } else if self.verbose {
            operator_log::Level::Debug
        } else {
            operator_log::Level::Info
        };
        let command_label = "demo start";
        let debug_enabled = self.verbose;
        if let Some(bundle) = self.bundle.clone() {
            let state_dir = bundle.join("state");
            std::fs::create_dir_all(&state_dir)?;
            let log_dir = self.log_dir.clone().unwrap_or_else(|| bundle.join("logs"));
            let log_dir = operator_log::init(log_dir.clone(), log_level)?;
            let run_targets =
                select_bundle_run_targets(&bundle, self.tenant.as_deref(), self.team.as_deref())?;
            let target_summary = format_bundle_targets(&run_targets);
            operator_log::info(
                module_path!(),
                format!(
                    "{command_label} (bundle={} targets=[{}]) log_dir={}",
                    bundle.display(),
                    &target_summary,
                    log_dir.display()
                ),
            );
            let mut nats_mode_arg = self.nats;
            if self.no_nats {
                nats_mode_arg = NatsModeArg::External;
            }
            let nats_mode = demo::NatsMode::from(nats_mode_arg);
            if matches!(nats_mode, demo::NatsMode::On) {
                eprintln!(
                    "{}",
                    operator_i18n::tr(
                        "cli.start.warn_legacy_nats",
                        "Warning: '--nats=on' uses the legacy GSM NATS stack; switch to embedded mode when possible."
                    )
                );
            }
            if demo_debug_enabled() {
                println!(
                    "[demo] start bundle={} tenant={:?} team={:?} nats_mode={:?} nats_url={:?} cloudflared={:?}",
                    bundle.display(),
                    self.tenant,
                    self.team,
                    nats_mode,
                    self.nats_url,
                    self.cloudflared
                );
            }
            let tenant = self
                .tenant
                .clone()
                .unwrap_or_else(|| DEMO_DEFAULT_TENANT.to_string());
            let config = config::load_operator_config(&bundle)?;
            domains::ensure_cbor_packs(&bundle)?;
            let discovery = discovery::discover_with_options(
                &bundle,
                discovery::DiscoveryOptions { cbor_only: true },
            )?;
            discovery::persist(&bundle, &tenant, &discovery)?;
            operator_log::info(
                module_path!(),
                format!(
                    "bundle discovery targets=[{}] messaging={} events={} providers={}",
                    &target_summary,
                    discovery.domains.messaging,
                    discovery.domains.events,
                    discovery.providers.len()
                ),
            );
            let demo_config_path = bundle.join("greentic.demo.yaml");
            let demo_config = load_demo_config_or_default(&demo_config_path);
            let services = config
                .as_ref()
                .and_then(|config| config.services.clone())
                .unwrap_or_default();
            let messaging_enabled = services
                .messaging
                .enabled
                .is_enabled(discovery.domains.messaging);
            let explicit_nats_url = self.nats_url.clone();
            let domains_to_setup = self.domain.resolve_domains(Some(&discovery));

            let mut cloudflared_config = match self.cloudflared {
                CloudflaredModeArg::Off => None,
                CloudflaredModeArg::On => {
                    let explicit = self.cloudflared_binary.clone();
                    let binary = bin_resolver::resolve_binary(
                        "cloudflared",
                        &ResolveCtx {
                            config_dir: bundle.clone(),
                            explicit_path: explicit,
                        },
                    )?;
                    Some(crate::cloudflared::CloudflaredConfig {
                        binary,
                        local_port: 8080,
                        extra_args: Vec::new(),
                        restart: restart.contains("cloudflared"),
                    })
                }
            };

            let mut ngrok_config = match self.ngrok {
                NgrokModeArg::Off => None,
                NgrokModeArg::On => {
                    let explicit = self.ngrok_binary.clone();
                    let binary = bin_resolver::resolve_binary(
                        "ngrok",
                        &ResolveCtx {
                            config_dir: bundle.clone(),
                            explicit_path: explicit,
                        },
                    )?;
                    Some(crate::ngrok::NgrokConfig {
                        binary,
                        local_port: 8080,
                        extra_args: Vec::new(),
                        restart: restart.contains("ngrok"),
                    })
                }
            };

            let mut public_base_url = self.public_base_url.clone();
            let team_id = self
                .team
                .clone()
                .unwrap_or_else(|| DEMO_DEFAULT_TEAM.to_string());
            let mut started_tunnel_early = false;
            if public_base_url.is_none()
                && self.setup_input.is_some()
                && let Some(cfg) = cloudflared_config.as_mut()
            {
                let paths = RuntimePaths::new(&state_dir, &tenant, &team_id);
                let setup_log = operator_log::reserve_service_log(&log_dir, "cloudflared")
                    .with_context(|| "unable to open cloudflared.log")?;
                operator_log::info(
                    module_path!(),
                    format!(
                        "starting setup-mode cloudflared log={}",
                        setup_log.display()
                    ),
                );
                let handle = crate::cloudflared::start_quick_tunnel(&paths, cfg, &setup_log)?;
                operator_log::info(
                    module_path!(),
                    format!(
                        "cloudflared setup mode ready url={} log={}",
                        handle.url,
                        setup_log.display()
                    ),
                );
                let domain_labels = domains_to_setup
                    .iter()
                    .map(|domain| domains::domain_name(*domain))
                    .collect::<Vec<_>>()
                    .join(",");
                println!(
                    "{}",
                    operator_i18n::trf(
                        "cli.start.public_url_setup_domains",
                        "Public URL (cloudflared setup domains={}): {}",
                        &[&domain_labels, &handle.url]
                    )
                );
                public_base_url = Some(handle.url.clone());
                started_tunnel_early = true;
            }

            if public_base_url.is_none()
                && self.setup_input.is_some()
                && let Some(cfg) = ngrok_config.as_mut()
            {
                let paths = RuntimePaths::new(&state_dir, &tenant, &team_id);
                let setup_log = operator_log::reserve_service_log(&log_dir, "ngrok")
                    .with_context(|| "unable to open ngrok.log")?;
                operator_log::info(
                    module_path!(),
                    format!("starting setup-mode ngrok log={}", setup_log.display()),
                );
                let handle = crate::ngrok::start_tunnel(&paths, cfg, &setup_log)?;
                operator_log::info(
                    module_path!(),
                    format!(
                        "ngrok setup mode ready url={} log={}",
                        handle.url,
                        setup_log.display()
                    ),
                );
                let domain_labels = domains_to_setup
                    .iter()
                    .map(|domain| domains::domain_name(*domain))
                    .collect::<Vec<_>>()
                    .join(",");
                println!(
                    "{}",
                    operator_i18n::trf(
                        "cli.start.public_url_setup_domains",
                        "Public URL (ngrok setup domains={}): {}",
                        &[&domain_labels, &handle.url]
                    )
                );
                public_base_url = Some(handle.url.clone());
                started_tunnel_early = true;
            }

            if started_tunnel_early && let Some(cfg) = cloudflared_config.as_mut() {
                cfg.restart = false;
            }
            if started_tunnel_early && let Some(cfg) = ngrok_config.as_mut() {
                cfg.restart = false;
            }

            if let Some(setup_input) = self.setup_input.as_ref() {
                let tenant_ref = self.tenant.as_deref().unwrap_or(DEMO_DEFAULT_TENANT);
                let secrets_handle = secrets_gate::resolve_secrets_manager(
                    &bundle,
                    tenant_ref,
                    self.team.as_deref(),
                )?;
                run_demo_up_setup(
                    &bundle,
                    &domains_to_setup,
                    setup_input,
                    self.tenant.clone(),
                    self.team.clone(),
                    &self.env,
                    self.runner_binary.clone(),
                    public_base_url.clone(),
                    Some(secrets_handle.manager()),
                )?;
            }

            let start_result = {
                let mut started = 0;
                let guard = (|| -> anyhow::Result<()> {
                    for target in &run_targets {
                        demo::demo_up(
                            &bundle,
                            &target.tenant,
                            target.team.as_deref(),
                            explicit_nats_url.as_deref(),
                            nats_mode,
                            messaging_enabled,
                            cloudflared_config.clone(),
                            ngrok_config.clone(),
                            &log_dir,
                            debug_enabled,
                        )
                        .with_context(|| {
                            format!("target tenant={} team={}", target.tenant, target.team_id())
                        })?;
                        started += 1;
                    }
                    Ok(())
                })();
                if guard.is_err() {
                    for target in &run_targets[..started] {
                        if let Err(cleanup_err) = demo::demo_down_runtime(
                            &state_dir,
                            &target.tenant,
                            target.team_id(),
                            false,
                        ) {
                            eprintln!(
                                "{}",
                                operator_i18n::trf(
                                    "cli.start.warn_failed_stop_earlier_target",
                                    "Warning: failed to stop earlier target tenant={} team={} : {}",
                                    &[&target.tenant, target.team_id(), &cleanup_err.to_string()]
                                )
                            );
                        }
                    }
                }
                guard
            };
            let mut ingress_server = None;
            let mut timer_scheduler = None;
            if start_result.is_ok() {
                let ingress_secrets_handle =
                    secrets_gate::resolve_secrets_manager(&bundle, &tenant, self.team.as_deref())?;
                match start_demo_ingress_server(
                    &bundle,
                    &discovery,
                    &demo_config,
                    &domains_to_setup,
                    self.runner_binary.clone(),
                    debug_enabled,
                    ingress_secrets_handle.clone(),
                ) {
                    Ok(server) => {
                        println!(
                            "{}",
                            operator_i18n::trf(
                                "cli.start.http_ingress_ready",
                                "HTTP ingress ready at http://{}:{}",
                                &[
                                    &demo_config.services.gateway.listen_addr,
                                    &demo_config.services.gateway.port.to_string()
                                ]
                            )
                        );
                        ingress_server = Some(server);
                    }
                    Err(err) => {
                        eprintln!(
                            "{}",
                            operator_i18n::trf(
                                "cli.start.warn_http_ingress_disabled",
                                "Warning: HTTP ingress disabled: {}",
                                &[&err.to_string()]
                            )
                        );
                        operator_log::warn(
                            module_path!(),
                            format!("demo ingress server unavailable: {err}"),
                        );
                    }
                }
                match start_demo_timer_scheduler(
                    &bundle,
                    &discovery,
                    &domains_to_setup,
                    self.runner_binary.clone(),
                    debug_enabled,
                    ingress_secrets_handle.clone(),
                    &tenant,
                    self.team.as_deref().unwrap_or(DEMO_DEFAULT_TEAM),
                ) {
                    Ok(Some(scheduler)) => {
                        println!(
                            "{}",
                            operator_i18n::tr(
                                "cli.start.events_timer_scheduler_ready",
                                "events timer scheduler ready"
                            )
                        );
                        timer_scheduler = Some(scheduler);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        eprintln!(
                            "{}",
                            operator_i18n::trf(
                                "cli.start.warn_events_timer_scheduler_disabled",
                                "Warning: events timer scheduler disabled: {}",
                                &[&err.to_string()]
                            )
                        );
                        operator_log::warn(
                            module_path!(),
                            format!("demo timer scheduler unavailable: {err}"),
                        );
                    }
                }
            }
            if let Err(ref err) = start_result {
                operator_log::error(
                    module_path!(),
                    format!(
                        "{command_label} bundle {} failed for targets=[{}]: {err}",
                        bundle.display(),
                        &target_summary
                    ),
                );
            } else {
                operator_log::info(
                    module_path!(),
                    format!(
                        "{command_label} bundle {} completed for targets=[{}]",
                        bundle.display(),
                        &target_summary
                    ),
                );
            }
            if start_result.is_ok() {
                println!(
                    "{command_label} running (bundle={} targets=[{}]); press Ctrl+C to stop",
                    bundle.display(),
                    &target_summary
                );
                wait_for_ctrlc()?;
                if let Some(server) = ingress_server.take() {
                    server.stop()?;
                }
                if let Some(scheduler) = timer_scheduler.take() {
                    scheduler.stop()?;
                }
                for target in run_targets.iter().rev() {
                    demo::demo_down_runtime(&state_dir, &target.tenant, target.team_id(), false)?;
                }
            }
            return start_result;
        }

        let config_path = resolve_demo_config_path(self.config.clone())?;
        let config_dir = config_path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let state_dir = config_dir.join("state");
        let initial_log_dir = self
            .log_dir
            .clone()
            .unwrap_or_else(|| config_dir.join("logs"));
        let log_dir = operator_log::init(initial_log_dir.clone(), log_level)?;
        operator_log::info(
            module_path!(),
            format!(
                "{command_label} (config={}) tenant={:?} team={:?} log_dir={}",
                config_path.display(),
                self.tenant,
                self.team,
                log_dir.display()
            ),
        );
        let demo_config = config::load_demo_config(&config_path)?;
        let tenant = demo_config.tenant.clone();
        let team = demo_config.team.clone();
        let cloudflared = match self.cloudflared {
            CloudflaredModeArg::Off => None,
            CloudflaredModeArg::On => {
                let explicit = self.cloudflared_binary.clone();
                let binary = bin_resolver::resolve_binary(
                    "cloudflared",
                    &ResolveCtx {
                        config_dir: config_dir.clone(),
                        explicit_path: explicit,
                    },
                )?;
                Some(crate::cloudflared::CloudflaredConfig {
                    binary,
                    local_port: demo_config.services.gateway.port,
                    extra_args: Vec::new(),
                    restart: restart.contains("cloudflared"),
                })
            }
        };
        let ngrok = match self.ngrok {
            NgrokModeArg::Off => None,
            NgrokModeArg::On => {
                let explicit = self.ngrok_binary.clone();
                let binary = bin_resolver::resolve_binary(
                    "ngrok",
                    &ResolveCtx {
                        config_dir: config_dir.clone(),
                        explicit_path: explicit,
                    },
                )?;
                Some(crate::ngrok::NgrokConfig {
                    binary,
                    local_port: demo_config.services.gateway.port,
                    extra_args: Vec::new(),
                    restart: restart.contains("ngrok"),
                })
            }
        };

        let provider_setup_input = self.setup_input.clone();
        let timer_runner_binary = self.runner_binary.clone();
        let provider_options = crate::providers::ProviderSetupOptions {
            providers: if self.providers.is_empty() {
                None
            } else {
                Some(self.providers)
            },
            verify_webhooks: self.verify_webhooks,
            force_setup: self.force_setup,
            skip_setup: self.skip_setup,
            skip_secrets_init: self.skip_secrets_init,
            allow_contract_change: self.allow_contract_change,
            backup: self.backup,
            setup_input: provider_setup_input.clone(),
            runner_binary: self.runner_binary,
            continue_on_error: provider_setup_input.is_none(),
        };

        let result = demo::demo_up_services(
            &config_path,
            &demo_config,
            cloudflared,
            ngrok,
            &restart,
            provider_options,
            &log_dir,
            debug_enabled,
        );
        if let Err(ref err) = result {
            operator_log::error(
                module_path!(),
                format!("{command_label} services failed: {err}"),
            );
        } else {
            operator_log::info(module_path!(), "{command_label} services completed");
        }
        if result.is_ok() {
            let is_demo_bundle = config_dir.join("greentic.demo.yaml").exists();
            let discovery = discovery::discover_with_options(
                &config_dir,
                discovery::DiscoveryOptions {
                    cbor_only: is_demo_bundle,
                },
            )?;
            let domains = if discovery.domains.events {
                vec![Domain::Events]
            } else {
                Vec::new()
            };
            let timer_secrets_handle =
                secrets_gate::resolve_secrets_manager(&config_dir, &tenant, Some(&team))?;
            let timer_scheduler = start_demo_timer_scheduler(
                &config_dir,
                &discovery,
                &domains,
                timer_runner_binary.clone(),
                debug_enabled,
                timer_secrets_handle,
                &tenant,
                &team,
            )?;
            println!(
                "{command_label} running (config={} tenant={} team={}); press Ctrl+C to stop",
                config_path.display(),
                tenant,
                team
            );
            wait_for_ctrlc()?;
            if let Some(scheduler) = timer_scheduler {
                scheduler.stop()?;
            }
            demo::demo_down_runtime(&state_dir, &tenant, &team, false)?;
        }
        result
    }
}

const DEMO_DEFAULT_TENANT: &str = "demo";
const DEMO_DEFAULT_TEAM: &str = "default";

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct DemoBundleTarget {
    tenant: String,
    team: Option<String>,
}

impl DemoBundleTarget {
    fn label(&self) -> String {
        match &self.team {
            Some(team) if !team.is_empty() => format!("{}.{}", self.tenant, team),
            _ => self.tenant.clone(),
        }
    }

    fn matches_filters(&self, tenant_filter: Option<&str>, team_filter: Option<&str>) -> bool {
        if let Some(filter) = tenant_filter
            && filter != self.tenant
        {
            return false;
        }
        if let Some(filter) = team_filter
            && filter != self.team_id()
        {
            return false;
        }
        true
    }

    fn team_id(&self) -> &str {
        self.team.as_deref().unwrap_or(DEMO_DEFAULT_TEAM)
    }
}

fn format_bundle_targets(targets: &[DemoBundleTarget]) -> String {
    targets
        .iter()
        .map(|target| target.label())
        .collect::<Vec<_>>()
        .join(", ")
}

fn select_bundle_run_targets(
    bundle: &Path,
    tenant_filter: Option<&str>,
    team_filter: Option<&str>,
) -> anyhow::Result<Vec<DemoBundleTarget>> {
    let resolved_targets = discover_bundle_run_targets(bundle)?;
    let filtered = resolved_targets
        .iter()
        .filter(|target| target.matches_filters(tenant_filter, team_filter))
        .cloned()
        .collect::<Vec<_>>();
    if !filtered.is_empty() {
        return Ok(filtered);
    }
    if resolved_targets.is_empty() {
        let tenant = tenant_filter.unwrap_or(DEMO_DEFAULT_TENANT).to_string();
        let team = team_filter.map(|value| value.to_string());
        return Ok(vec![DemoBundleTarget { tenant, team }]);
    }
    anyhow::bail!(
        "no resolved targets matched tenant={:?} team={:?}",
        tenant_filter,
        team_filter
    );
}

fn discover_bundle_run_targets(bundle: &Path) -> anyhow::Result<Vec<DemoBundleTarget>> {
    let resolved_dir = bundle.join("state").join("resolved");
    if !resolved_dir.exists() {
        return Ok(Vec::new());
    }
    let mut seen = BTreeSet::new();
    for entry in fs::read_dir(resolved_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Some(ext) = path.extension().and_then(|value| value.to_str()) {
            let ext = ext.to_ascii_lowercase();
            if ext != "yaml" && ext != "yml" {
                continue;
            }
        } else {
            continue;
        }
        let stem = match path.file_stem().and_then(|value| value.to_str()) {
            Some(value) if !value.is_empty() => value,
            _ => continue,
        };
        let mut parts = stem.splitn(2, '.');
        let tenant = match parts.next() {
            Some(value) if !value.is_empty() => value.to_string(),
            _ => continue,
        };
        let team = parts
            .next()
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string());
        seen.insert(DemoBundleTarget { tenant, team });
    }
    Ok(seen.into_iter().collect())
}

impl DemoSetupArgs {
    fn run(self) -> anyhow::Result<()> {
        domains::ensure_cbor_packs(&self.bundle)?;
        let discovery = discovery::discover_with_options(
            &self.bundle,
            discovery::DiscoveryOptions { cbor_only: true },
        )?;
        discovery::persist(&self.bundle, &self.tenant, &discovery)?;
        let domains = self.domain.resolve_domains(Some(&discovery));
        if demo_debug_enabled() {
            println!(
                "[demo] setup bundle={} tenant={} team={:?} domains={:?} provider_filter={:?} dry_run={} parallel={} skip_secrets_init={}",
                self.bundle.display(),
                self.tenant,
                self.team,
                domains,
                self.provider,
                self.dry_run,
                self.parallel,
                self.skip_secrets_init
            );
        }
        let format = match self.format {
            Format::Text => PlanFormat::Text,
            Format::Json => PlanFormat::Json,
            Format::Yaml => PlanFormat::Yaml,
        };
        for domain in domains {
            let discovered_providers = match domain {
                Domain::Messaging | Domain::Events => Some(
                    discovery
                        .providers
                        .iter()
                        .filter(|provider| provider.domain == domains::domain_name(domain))
                        .cloned()
                        .collect(),
                ),
                Domain::Secrets => None,
            };
            run_domain_command(DomainRunArgs {
                root: self.bundle.clone(),
                state_root: self.state_dir.clone(),
                domain,
                action: DomainAction::Setup,
                tenant: self.tenant.clone(),
                team: self.team.clone(),
                provider_filter: self.provider.clone(),
                dry_run: self.dry_run,
                format,
                parallel: self.parallel,
                allow_missing_setup: self.allow_missing_setup,
                allow_contract_change: self.allow_contract_change,
                backup: self.backup,
                online: self.online,
                secrets_env: if self.skip_secrets_init {
                    None
                } else {
                    self.secrets_env.clone()
                },
                runner_binary: self.runner_binary.clone(),
                best_effort: self.best_effort,
                setup_input: self.setup_input.clone(),
                allowed_providers: None,
                preloaded_setup_answers: None,
                public_base_url: None,
                secrets_manager: None,
                discovered_providers,
            })?;
        }
        Ok(())
    }
}

impl DemoPolicyArgs {
    fn run(self, policy: Policy) -> anyhow::Result<()> {
        let effective_team = if let Some(team) = self.team.clone() {
            Some(team)
        } else if self
            .bundle
            .join("tenants")
            .join(&self.tenant)
            .join("teams")
            .join("default")
            .exists()
        {
            Some("default".to_string())
        } else {
            None
        };
        let gmap_path =
            demo_bundle_gmap_path(&self.bundle, &self.tenant, effective_team.as_deref());
        gmap::upsert_policy(&gmap_path, &self.path, policy)?;
        project::sync_project(&self.bundle)?;
        copy_resolved_manifest(&self.bundle, &self.tenant, effective_team.as_deref())?;
        Ok(())
    }
}

impl DemoSetupWizardArgs {
    fn run(self) -> anyhow::Result<()> {
        let meta = domains::read_pack_meta(&self.pack)
            .with_context(|| format!("failed to read pack {}", self.pack.display()))?;
        let provider_id = self.provider.unwrap_or(meta.pack_id);
        let setup_flow = self.flow.unwrap_or_else(|| "setup_default".to_string());

        // 1. Collect answers via card wizard
        let answers = qa_setup_wizard::run_interactive_card_wizard(&self.pack, &provider_id)?;

        // 2. Build input payload with collected answers
        let input = json!({
            "tenant": &self.tenant,
            "team": self.team.as_deref().unwrap_or("default"),
            "id": &provider_id,
            "setup_answers": &answers,
            "config": { "id": &provider_id },
            "msg": {
                "id": format!("{provider_id}.setup"),
                "tenant": { "env": "dev", "tenant": &self.tenant },
                "channel": "setup",
                "session_id": "setup",
            },
            "payload": {},
        });

        println!("\nRunning flow '{setup_flow}' with collected answers...");

        // 3. Resolve secrets manager
        let secrets_manager = if let Some(bundle) = &self.bundle {
            secrets_gate::resolve_secrets_manager(bundle, &self.tenant, self.team.as_deref())?
                .runtime_manager(Some(&provider_id))
        } else {
            default_manager()?
        };

        // 4. Run the setup flow via DemoRunner
        let mut runner = DemoRunner::with_entry_flow(
            self.pack.clone(),
            &self.tenant,
            self.team.clone(),
            setup_flow.clone(),
            provider_id.clone(),
            input,
            secrets_manager,
        )?;

        match runner.run_until_blocked() {
            demo::DemoBlockedOn::Finished(output) => {
                println!("\nFlow '{setup_flow}' completed:");
                println!(
                    "{}",
                    serde_json::to_string_pretty(&output)
                        .unwrap_or_else(|_| "<invalid>".into())
                );
            }
            demo::DemoBlockedOn::Waiting { reason, output, .. } => {
                println!(
                    "\nFlow '{setup_flow}' is waiting for input: {}",
                    reason.as_deref().unwrap_or("unknown")
                );
                println!(
                    "Output so far: {}",
                    serde_json::to_string_pretty(&output)
                        .unwrap_or_else(|_| "<invalid>".into())
                );
            }
            demo::DemoBlockedOn::Error(err) => {
                return Err(err.context(format!("flow '{setup_flow}' failed")));
            }
        }

        Ok(())
    }
}

impl DemoWizardArgs {
    fn run(self) -> anyhow::Result<()> {
        let mode: wizard::WizardMode = self.mode.into();
        let effective_locale = self.locale.clone().unwrap_or_else(detect_system_locale_tag);
        let provider_registry_ref = self
            .provider_registry
            .clone()
            .or_else(|| std::env::var("GTC_PROVIDER_REGISTRY_REF").ok())
            .unwrap_or_else(|| DEFAULT_PROVIDER_REGISTRY_REF.to_string());
        let qa_catalog_bundle_hint = self.bundle.clone().unwrap_or_else(|| PathBuf::from("."));
        let qa_catalog_path = provider_registry::resolve_catalog_path(
            self.catalog_file.clone().or_else(|| {
                std::env::var("GREENTIC_OPERATOR_WIZARD_CATALOG")
                    .ok()
                    .map(PathBuf::from)
            }),
            Some(provider_registry_ref.as_str()),
            self.offline,
            &qa_catalog_bundle_hint,
        )?;
        let qa_catalog_entries = {
            let path = qa_catalog_path.ok_or_else(|| {
                anyhow!(
                    "provider registry is required; set --provider-registry <ref> or GTC_PROVIDER_REGISTRY_REF"
                )
            })?;
            wizard::load_catalog_from_file(&path)?
        };
        let qa_provider_ids = qa_catalog_entries
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        let prefilled_answers = build_prefilled_wizard_answers_from_cli(&self, &effective_locale);
        let mut answers = if let Some(path) = self.qa_answers.as_ref() {
            load_wizard_qa_answers(path)?
        } else {
            run_wizard_via_qa(
                mode,
                &effective_locale,
                prefilled_answers,
                &qa_provider_ids,
                self.verbose,
            )?
        };
        merge_cli_overrides_into_wizard_answers(&mut answers, &self, &effective_locale);

        let bundle = self
            .bundle
            .clone()
            .or(answers.bundle.clone())
            .ok_or_else(|| anyhow!("bundle path is required via --bundle or wizard answers"))?;

        let catalog_path = provider_registry::resolve_catalog_path(
            self.catalog_file.clone().or_else(|| {
                std::env::var("GREENTIC_OPERATOR_WIZARD_CATALOG")
                    .ok()
                    .map(PathBuf::from)
            }),
            Some(provider_registry_ref.as_str()),
            self.offline,
            &bundle,
        )?;

        let catalog_path = catalog_path.ok_or_else(|| {
            anyhow!(
                "provider registry is required; set --provider-registry <ref> or GTC_PROVIDER_REGISTRY_REF"
            )
        })?;
        let catalog_entries = wizard::load_catalog_from_file(&catalog_path)?;
        if mode != wizard::WizardMode::Create || bundle.exists() {
            if let Some(local_path) = parse_local_registry_ref(provider_registry_ref.as_str()) {
                if local_path.exists() {
                    let _ = provider_registry::cache_registry_file(
                        &bundle,
                        provider_registry_ref.as_str(),
                        &local_path,
                    );
                }
            } else if catalog_path.exists() {
                let _ = provider_registry::cache_registry_file(
                    &bundle,
                    provider_registry_ref.as_str(),
                    &catalog_path,
                );
            }
        }
        let by_id = catalog_entries
            .into_iter()
            .map(|entry| (entry.id.clone(), entry))
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut refs = normalize_pack_refs(&answers.pack_refs);
        refs.extend(self.pack_refs.clone());
        let provider_ids = normalize_provider_ids(&answers.providers);
        for provider_id in &provider_ids {
            if let Some(item) = by_id.get(provider_id) {
                refs.push(item.reference.clone());
            }
        }

        let mut catalog_ids = normalize_catalog_packs(&answers.catalog_packs);
        catalog_ids.extend(self.catalog_packs.clone());
        for id in &catalog_ids {
            let item = by_id.get(id).ok_or_else(|| {
                anyhow!(
                    "unknown --catalog-pack {}; available: {}",
                    id,
                    by_id.keys().cloned().collect::<Vec<_>>().join(", ")
                )
            })?;
            refs.push(item.reference.clone());
        }

        let mut tenants = Vec::new();
        let merged_allow_paths = if self.allow_paths.is_empty() {
            answers.allow_paths.clone()
        } else {
            self.allow_paths.clone()
        };
        let merged_targets = if self.targets.is_empty() {
            normalize_targets(&answers.targets)
        } else {
            self.targets.clone()
        };
        if merged_targets.is_empty() {
            tenants.push(wizard::TenantSelection {
                tenant: if self.tenant == "demo" {
                    answers.tenant.clone().unwrap_or(self.tenant.clone())
                } else {
                    self.tenant.clone()
                },
                team: self.team.clone().or(answers.team.clone()),
                allow_paths: merged_allow_paths.clone(),
            });
        } else {
            for target in &merged_targets {
                let (tenant, team) = parse_wizard_target(target)?;
                tenants.push(wizard::TenantSelection {
                    tenant,
                    team,
                    allow_paths: merged_allow_paths.clone(),
                });
            }
        }

        let update_ops = normalize_update_ops(&answers.update_ops);
        let remove_targets = normalize_remove_targets(&answers.remove_targets);
        let packs_remove = normalize_pack_removes(&answers.packs_remove)?;
        let providers_remove = normalize_provider_ids(&answers.providers_remove);
        let tenants_remove = normalize_target_selections(&answers.tenants_remove);
        let access_changes = if mode == wizard::WizardMode::Create {
            normalize_access_changes_from_pack_refs(&answers.pack_refs, &tenants)?
        } else {
            build_access_changes(
                mode,
                answers.access_mode.as_deref(),
                &tenants,
                &refs,
                normalize_access_changes(&answers.access_change),
            )?
        };
        let default_assignments = normalize_default_assignments_from_pack_refs(&answers.pack_refs)?;

        let request = wizard::WizardCreateRequest {
            bundle: bundle.clone(),
            bundle_name: answers.bundle_name.clone(),
            pack_refs: refs,
            tenants,
            default_assignments,
            providers: provider_ids,
            update_ops,
            remove_targets,
            packs_remove,
            providers_remove,
            tenants_remove,
            access_changes,
        };
        let qa_execute = matches!(answers.execution_mode.as_deref(), Some("execute"));
        let execute_requested = if self.execute || self.dry_run {
            self.execute
        } else {
            qa_execute
        };
        let dry_run = if self.execute || self.dry_run {
            self.dry_run || !self.execute
        } else {
            !qa_execute
        };
        let plan = wizard_plan_builder::build_plan(mode, &request, dry_run)?;
        wizard::print_plan_summary(&plan);
        if self.verbose {
            for step in &plan.steps {
                if step.details.is_empty() {
                    println!(
                        "{}",
                        operator_i18n::trf(
                            "cli.wizard.step_details_none",
                            "step details {:?}: <none>",
                            &[&format!("{:?}", step.kind)]
                        )
                    );
                    continue;
                }
                println!(
                    "{}",
                    operator_i18n::trf(
                        "cli.wizard.step_details_header",
                        "step details {:?}:",
                        &[&format!("{:?}", step.kind)]
                    )
                );
                for (key, value) in &step.details {
                    println!(
                        "{}",
                        operator_i18n::trf(
                            "cli.wizard.step_details_item",
                            "  {}={}",
                            &[key, value]
                        )
                    );
                }
            }
        }

        if !execute_requested {
            let output_path = prompt_output_answers_path()?;
            let payload =
                serde_json::to_string_pretty(&answers).context("serialize wizard answers")?;
            std::fs::write(&output_path, payload)
                .with_context(|| format!("write wizard answers {}", output_path.display()))?;
            println!(
                "{} {}",
                operator_i18n::tr("cli.wizard.saved_answers", "saved wizard answers:"),
                output_path.display()
            );
            return Ok(());
        }

        if mode == wizard::WizardMode::Create
            && bundle.exists()
            && !prompt_yes_no(
                &format!(
                    "Bundle path {} already exists. Overwrite bundle? [y, N]",
                    bundle.display()
                ),
                false,
            )?
        {
            println!(
                "{}",
                operator_i18n::tr(
                    "cli.wizard.execution_aborted",
                    "wizard execution aborted by user"
                )
            );
            return Ok(());
        }
        if mode == wizard::WizardMode::Create && bundle.exists() {
            std::fs::remove_dir_all(&bundle)
                .with_context(|| format!("remove existing bundle {}", bundle.display()))?;
        }

        let report = wizard_executor::execute(mode, &plan, self.offline)?;
        let no_op_count = plan
            .steps
            .iter()
            .filter(|step| step.kind == wizard::WizardStepKind::NoOp)
            .count();
        println!(
            "{}",
            operator_i18n::trf(
                "cli.wizard.execute_complete",
                "wizard execute complete bundle={} packs={} manifests={} providers={} no_ops={}",
                &[
                    &report.bundle.display().to_string(),
                    &report.resolved_packs.len().to_string(),
                    &report.resolved_manifests.len().to_string(),
                    &report.provider_updates.to_string(),
                    &no_op_count.to_string()
                ]
            )
        );
        for manifest in &report.resolved_manifests {
            println!(
                "{}",
                operator_i18n::trf(
                    "cli.wizard.resolved_manifest",
                    "resolved manifest: {}",
                    &[&manifest.display().to_string()]
                )
            );
        }
        for warning in &report.warnings {
            println!(
                "{}",
                operator_i18n::trf("cli.wizard.warning", "warning: {}", &[warning])
            );
        }

        if self.run_setup && mode != wizard::WizardMode::Remove {
            let setup_provider_ids = report
                .resolved_packs
                .iter()
                .filter(|pack| pack.entry_flows.iter().any(|flow| flow == "setup_default"))
                .map(|pack| pack.pack_id.clone())
                .collect::<BTreeSet<_>>();
            let allowed_providers = if setup_provider_ids.is_empty() {
                None
            } else {
                Some(setup_provider_ids)
            };
            let preloaded_setup_answers = if let Some(allowed) = allowed_providers.as_ref() {
                Some(build_wizard_setup_answers(
                    &plan.bundle,
                    &report.resolved_packs,
                    allowed,
                    self.setup_input.as_ref(),
                )?)
            } else {
                None
            };
            for tenant in &plan.metadata.tenants {
                run_wizard_setup_for_target(
                    &plan.bundle,
                    &tenant.tenant,
                    tenant.team.as_deref(),
                    self.setup_input.as_ref(),
                    allowed_providers.clone(),
                    preloaded_setup_answers.clone(),
                )?;
            }
        } else if self.run_setup && mode == wizard::WizardMode::Remove {
            println!(
                "{}",
                operator_i18n::tr("cli.wizard.skip_setup_remove", "skip setup for remove mode")
            );
        }
        Ok(())
    }
}

fn parse_wizard_target(input: &str) -> anyhow::Result<(String, Option<String>)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("target must not be empty"));
    }
    let mut parts = trimmed.splitn(2, ':');
    let tenant = parts.next().unwrap_or_default().trim().to_string();
    if tenant.is_empty() {
        return Err(anyhow!("target tenant must not be empty"));
    }
    let team = parts
        .next()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    Ok((tenant, team))
}

fn prompt_output_answers_path() -> anyhow::Result<PathBuf> {
    print!(
        "{} ",
        operator_i18n::tr(
            "cli.wizard.answers_output_prompt",
            "Answers output file [answers.json]:"
        )
    );
    io::stdout().flush().context("flush stdout")?;
    let mut input = String::new();
    let read = io::stdin()
        .read_line(&mut input)
        .context("read answers output file path")?;
    if read == 0 {
        return Err(anyhow!("stdin closed"));
    }
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(PathBuf::from("answers.json"));
    }
    Ok(PathBuf::from(trimmed))
}

fn prompt_yes_no(prompt: &str, default_yes: bool) -> anyhow::Result<bool> {
    loop {
        print!("{prompt} ");
        io::stdout().flush().context("flush stdout")?;
        let mut input = String::new();
        let read = io::stdin()
            .read_line(&mut input)
            .context("read yes/no input")?;
        if read == 0 {
            return Err(anyhow!("stdin closed"));
        }
        let normalized = input.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return Ok(default_yes);
        }
        if let Some(value) = parse_yes_no_token(&normalized) {
            return Ok(value);
        }
        println!(
            "{}",
            operator_i18n::tr("cli.common.answer_yes_no", "please answer y or n")
        );
    }
}

fn parse_yes_no_token(token: &str) -> Option<bool> {
    match token {
        "y" | "yes" | "j" | "ja" => Some(true),
        "n" | "no" | "nee" | "nein" => Some(false),
        _ => None,
    }
}

fn load_wizard_qa_answers(path: &Path) -> anyhow::Result<WizardQaAnswers> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read qa answers {}", path.display()))?;
    let value: JsonValue = serde_json::from_str(&raw)
        .or_else(|_| serde_yaml_bw::from_str(&raw))
        .with_context(|| format!("parse qa answers {}", path.display()))?;
    parse_wizard_qa_answers_value(value)
}

fn parse_wizard_qa_answers_value(value: JsonValue) -> anyhow::Result<WizardQaAnswers> {
    serde_json::from_value(value).context("parse wizard answers object")
}

fn run_wizard_via_qa(
    mode: wizard::WizardMode,
    locale: &str,
    initial_answers: JsonValue,
    provider_ids: &[String],
    verbose: bool,
) -> anyhow::Result<WizardQaAnswers> {
    let spec = wizard_spec_builder::build_validation_form_with_providers(mode, provider_ids);
    let prefilled_answers = initial_answers.clone();
    let config = WizardRunConfig {
        spec_json: spec.to_string(),
        initial_answers_json: Some(initial_answers.to_string()),
        frontend: WizardFrontend::Text,
        i18n: I18nConfig {
            locale: Some(locale.to_string()),
            resolved: Some(load_wizard_i18n(locale)?),
            debug: false,
        },
        verbose,
    };
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let result = if interactive {
        let mut driver = WizardDriver::new(config)
            .map_err(|err| anyhow!("wizard QA flow failed (greentic-qa-lib): {err}"))?;
        loop {
            let _ = driver
                .next_payload_json()
                .map_err(|err| anyhow!("wizard QA flow failed (greentic-qa-lib): {err}"))?;
            if driver.is_complete() {
                break;
            }
            let ui_raw = driver.last_ui_json().ok_or_else(|| {
                anyhow!("wizard QA flow failed (greentic-qa-lib): missing ui payload")
            })?;
            let ui: JsonValue = serde_json::from_str(ui_raw)
                .with_context(|| "wizard QA flow failed (greentic-qa-lib): parse ui payload")?;
            let question_id = ui
                .get("next_question_id")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| {
                    anyhow!("wizard QA flow failed (greentic-qa-lib): missing next_question_id")
                })?
                .to_string();
            let answer = answer_for_question(&prefilled_answers, &ui, &question_id)?;
            if verbose {
                eprintln!(
                    "{}",
                    operator_i18n::trf(
                        "cli.wizard.qa.submitting_answer",
                        "wizard qa: submitting answer [{}]",
                        &[&question_id]
                    )
                );
                let _ = io::stderr().flush();
            }
            let submit = driver
                .submit_patch_json(&json!({ question_id: answer }).to_string())
                .map_err(|err| anyhow!("wizard QA flow failed (greentic-qa-lib): {err}"))?;
            if submit.status == "error" {
                if verbose {
                    eprintln!(
                        "{}",
                        operator_i18n::tr(
                            "cli.wizard.qa.submit_validation_error",
                            "wizard qa: submit returned validation error"
                        )
                    );
                    let _ = io::stderr().flush();
                }
            } else if verbose {
                eprintln!(
                    "{}",
                    operator_i18n::trf(
                        "cli.wizard.qa.submit_accepted",
                        "wizard qa: submit accepted (status={})",
                        &[&submit.status]
                    )
                );
                let _ = io::stderr().flush();
            }
        }
        driver
            .finish()
            .map_err(|err| anyhow!("wizard QA flow failed (greentic-qa-lib): {err}"))?
    } else {
        match QaRunner::run_wizard_non_interactive(config) {
            Ok(result) => result,
            Err(QaLibError::NeedsInteraction) => {
                return Err(anyhow!(
                    "wizard requires additional answers. Re-run with --qa-answers <PATH> generated by greentic-qa."
                ));
            }
            Err(err) => return Err(anyhow!("wizard QA flow failed (greentic-qa-lib): {err}")),
        }
    };
    parse_wizard_qa_answers_value(result.answer_set.answers)
}

fn prefilled_answer_for_question(
    prefilled_answers: &JsonValue,
    question_id: &str,
) -> Option<JsonValue> {
    let value = prefilled_answers.get(question_id)?;
    if value.is_null() {
        return None;
    }
    Some(value.clone())
}

fn answer_for_question(
    prefilled_answers: &JsonValue,
    ui: &JsonValue,
    question_id: &str,
) -> anyhow::Result<JsonValue> {
    if let Some(value) = prefilled_answer_for_question(prefilled_answers, question_id) {
        return Ok(value);
    }
    let question = question_for_id(ui, question_id)?;
    prompt_for_wizard_answer(question_id, question)
        .map_err(|err| anyhow!("wizard QA flow failed (greentic-qa-lib): {err}"))
}

fn question_for_id<'a>(ui: &'a JsonValue, question_id: &str) -> anyhow::Result<&'a JsonValue> {
    ui.get("questions")
        .and_then(JsonValue::as_array)
        .and_then(|questions| {
            questions.iter().find(|question| {
                question.get("id").and_then(JsonValue::as_str) == Some(question_id)
            })
        })
        .ok_or_else(|| {
            anyhow!(
                "wizard QA flow failed (greentic-qa-lib): missing question {}",
                question_id
            )
        })
}

fn prompt_for_wizard_answer(
    question_id: &str,
    question: &JsonValue,
) -> Result<JsonValue, QaLibError> {
    let title = question
        .get("title")
        .and_then(JsonValue::as_str)
        .unwrap_or(question_id);
    let required = question
        .get("required")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);
    let kind = question
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or("string");

    match kind {
        "string" => prompt_string_value(title, required),
        "enum" => prompt_enum_value(question_id, title, required, question),
        "list" => prompt_list_value(question_id, title, required, question),
        _ => prompt_string_value(title, required),
    }
}

fn prompt_string_value(title: &str, required: bool) -> Result<JsonValue, QaLibError> {
    loop {
        print!("{title}: ");
        io::stdout()
            .flush()
            .map_err(|err| QaLibError::Component(err.to_string()))?;
        let mut input = String::new();
        let read = io::stdin()
            .read_line(&mut input)
            .map_err(|err| QaLibError::Component(err.to_string()))?;
        if read == 0 {
            return Err(QaLibError::Component("stdin closed".to_string()));
        }
        let trimmed = input.trim();
        if trimmed.is_empty() {
            if required {
                println!(
                    "{}",
                    operator_i18n::tr("cli.qa.value_required", "value is required")
                );
                continue;
            }
            return Ok(JsonValue::Null);
        }
        return Ok(JsonValue::String(trimmed.to_string()));
    }
}

fn prompt_enum_value(
    question_id: &str,
    title: &str,
    required: bool,
    question: &JsonValue,
) -> Result<JsonValue, QaLibError> {
    let choices = question
        .get("choices")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| QaLibError::MissingField("choices".to_string()))?
        .iter()
        .filter_map(JsonValue::as_str)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if choices.is_empty() {
        return Err(QaLibError::MissingField("choices".to_string()));
    }
    loop {
        println!("{title}:");
        for (idx, choice) in choices.iter().enumerate() {
            println!("  {}. {}", idx + 1, enum_choice_label(question_id, choice));
        }
        print!(
            "{} ",
            operator_i18n::tr("cli.qa.select_number_or_value", "Select number or value:")
        );
        io::stdout()
            .flush()
            .map_err(|err| QaLibError::Component(err.to_string()))?;
        let mut input = String::new();
        let read = io::stdin()
            .read_line(&mut input)
            .map_err(|err| QaLibError::Component(err.to_string()))?;
        if read == 0 {
            return Err(QaLibError::Component("stdin closed".to_string()));
        }
        let trimmed = input.trim();
        if trimmed.is_empty() {
            if required {
                println!(
                    "{}",
                    operator_i18n::tr("cli.qa.value_required", "value is required")
                );
                continue;
            }
            return Ok(JsonValue::Null);
        }
        if let Ok(n) = trimmed.parse::<usize>()
            && n > 0
            && n <= choices.len()
        {
            return Ok(JsonValue::String(choices[n - 1].clone()));
        }
        if choices.iter().any(|choice| choice == trimmed) {
            return Ok(JsonValue::String(trimmed.to_string()));
        }
        println!(
            "{}",
            operator_i18n::tr("cli.qa.invalid_choice", "invalid choice")
        );
    }
}

fn prompt_list_value(
    question_id: &str,
    title: &str,
    required: bool,
    question: &JsonValue,
) -> Result<JsonValue, QaLibError> {
    let fields = question
        .get("list")
        .and_then(|value| value.get("fields"))
        .and_then(JsonValue::as_array)
        .ok_or_else(|| QaLibError::MissingField("list.fields".to_string()))?;

    let custom_prompt = custom_list_add_prompt(question_id);
    println!("{title}:");
    if custom_prompt.is_none() {
        println!(
            "{}",
            operator_i18n::tr(
                "cli.qa.list_finish_hint",
                "Press Enter on 'Add item?' to finish."
            )
        );
    }
    let mut items = Vec::new();
    loop {
        if let Some((prompt, _default_yes)) = custom_prompt.as_ref() {
            print!("{prompt} ");
        } else {
            print!(
                "{} ",
                operator_i18n::trf(
                    "cli.qa.add_item_prompt",
                    "Add item #{}? [y/N]:",
                    &[&(items.len() + 1).to_string()]
                )
            );
        }
        io::stdout()
            .flush()
            .map_err(|err| QaLibError::Component(err.to_string()))?;
        let mut add = String::new();
        let read = io::stdin()
            .read_line(&mut add)
            .map_err(|err| QaLibError::Component(err.to_string()))?;
        if read == 0 {
            return Err(QaLibError::Component("stdin closed".to_string()));
        }
        let add = add.trim().to_ascii_lowercase();
        if let Some((_, default_yes)) = custom_prompt.as_ref() {
            if add.is_empty() {
                if !*default_yes {
                    break;
                }
            } else if let Some(value) = parse_yes_no_token(&add) {
                if !value {
                    break;
                }
            } else {
                println!(
                    "{}",
                    operator_i18n::tr("cli.common.answer_yes_no", "please answer y or n")
                );
                continue;
            }
        } else {
            if add.is_empty() {
                break;
            }
            let Some(value) = parse_yes_no_token(&add) else {
                println!(
                    "{}",
                    operator_i18n::tr("cli.common.answer_yes_no", "please answer y or n")
                );
                continue;
            };
            if !value {
                break;
            }
        }

        let mut item = JsonMap::new();
        for field in fields {
            let field_id = field
                .get("id")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| QaLibError::MissingField("id".to_string()))?;
            if should_skip_pack_ref_field(field_id, &item) {
                continue;
            }
            let field_title_fallback = field
                .get("title")
                .and_then(JsonValue::as_str)
                .unwrap_or(field_id);
            let field_title_owned =
                localized_list_field_title(question_id, field_id, field_title_fallback);
            let field_title = field_title_owned.as_str();
            let field_kind = field
                .get("type")
                .and_then(JsonValue::as_str)
                .unwrap_or("string");
            let field_required = field
                .get("required")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            let value = if field_id == "make_default_pack" {
                prompt_yes_no_value(field_title, false)?
            } else {
                match field_kind {
                    "enum" => prompt_enum_value(field_id, field_title, field_required, field)?,
                    _ => prompt_string_value(field_title, field_required)?,
                }
            };
            if !value.is_null() {
                item.insert(field_id.to_string(), value);
            }
        }
        items.push(JsonValue::Object(item));
    }

    if required && items.is_empty() {
        println!(
            "{}",
            operator_i18n::tr("cli.qa.at_least_one_item", "at least one item is required")
        );
        return prompt_list_value(question_id, title, required, question);
    }
    Ok(JsonValue::Array(items))
}

fn enum_choice_label<'a>(question_id: &str, choice: &'a str) -> Cow<'a, str> {
    match (question_id, choice) {
        ("access_mode", "all_selected_get_all_packs") => Cow::Owned(operator_i18n::tr(
            "cli.qa.choice.access_mode.all_selected_get_all_packs",
            "All tenants and teams get access to all packs",
        )),
        ("access_mode", "per_pack_matrix") => Cow::Owned(operator_i18n::tr(
            "cli.qa.choice.access_mode.per_pack_matrix",
            "Fine-grained access control",
        )),
        ("access_scope", "all_tenants") => Cow::Owned(operator_i18n::tr(
            "cli.qa.choice.access_scope.all_tenants",
            "all tenant",
        )),
        ("access_scope", "tenant_all_teams") => Cow::Owned(operator_i18n::tr(
            "cli.qa.choice.access_scope.tenant_all_teams",
            "all teams from a specific tenant",
        )),
        ("access_scope", "specific_team") => Cow::Owned(operator_i18n::tr(
            "cli.qa.choice.access_scope.specific_team",
            "specific team for a specific tenant",
        )),
        _ => Cow::Borrowed(choice),
    }
}

fn should_skip_pack_ref_field(field_id: &str, item: &JsonMap<String, JsonValue>) -> bool {
    let scope = item
        .get("access_scope")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    match field_id {
        "tenant_id" => scope != "tenant_all_teams" && scope != "specific_team",
        "team_id" => scope != "specific_team",
        _ => false,
    }
}

fn localized_list_field_title(question_id: &str, field_id: &str, fallback: &str) -> String {
    match (question_id, field_id) {
        ("pack_refs", "pack_ref") => operator_i18n::tr(
            "cli.qa.pack_ref_field_title",
            "Pack reference (e.g. /path/to/app.gtpack, file://..., oci://ghcr.io/..., repo://..., store://...)",
        ),
        ("pack_refs", "access_scope") => operator_i18n::tr(
            "cli.qa.pack_ref.access_scope_title",
            "Who can access this application?",
        ),
        ("pack_refs", "tenant_id") => operator_i18n::tr(
            "cli.qa.pack_ref.tenant_id_title",
            "What is the tenant id who can access this application?",
        ),
        ("pack_refs", "team_id") => operator_i18n::tr(
            "cli.qa.pack_ref.team_id_title",
            "What is the team id who can access this application?",
        ),
        ("pack_refs", "make_default_pack") => operator_i18n::tr(
            "cli.qa.pack_ref.make_default_pack_title",
            "Is this pack the default pack when no pack is specified?",
        ),
        _ => fallback.to_string(),
    }
}

fn prompt_yes_no_value(title: &str, default_yes: bool) -> Result<JsonValue, QaLibError> {
    loop {
        let suffix = if default_yes {
            operator_i18n::tr("cli.qa.yes_no_suffix_default_yes", "[Y,n]")
        } else {
            operator_i18n::tr("cli.qa.yes_no_suffix_default_no", "[y,N]")
        };
        if title.contains("[y, N]") || title.contains("[Y,n]") || title.contains("[y,N]") {
            print!("{title} ");
        } else {
            print!("{title} {suffix}: ");
        }
        io::stdout()
            .flush()
            .map_err(|err| QaLibError::Component(err.to_string()))?;
        let mut input = String::new();
        let read = io::stdin()
            .read_line(&mut input)
            .map_err(|err| QaLibError::Component(err.to_string()))?;
        if read == 0 {
            return Err(QaLibError::Component("stdin closed".to_string()));
        }
        let normalized = input.trim().to_ascii_lowercase();
        let yes = if normalized.is_empty() {
            default_yes
        } else if let Some(value) = parse_yes_no_token(&normalized) {
            value
        } else {
            println!(
                "{}",
                operator_i18n::tr("cli.common.answer_yes_no", "please answer y or n")
            );
            continue;
        };
        return Ok(JsonValue::String(
            if yes { "yes" } else { "no" }.to_string(),
        ));
    }
}

fn custom_list_add_prompt(question_id: &str) -> Option<(String, bool)> {
    match question_id {
        "pack_refs" => Some((
            operator_i18n::tr(
                "cli.qa.pack_refs.add_prompt",
                "Do you want to add an application pack? [Y,n]",
            ),
            true,
        )),
        "providers" => Some((
            operator_i18n::tr(
                "cli.qa.providers.add_prompt",
                "Do you want to add providers (e.g. messaging, events, etc)? [Y,n]",
            ),
            true,
        )),
        _ => None,
    }
}

fn build_prefilled_wizard_answers_from_cli(args: &DemoWizardArgs, locale: &str) -> JsonValue {
    let mut map = JsonMap::new();
    if let Some(bundle) = args.bundle.as_ref() {
        map.insert(
            "bundle_path".to_string(),
            JsonValue::String(bundle.display().to_string()),
        );
    }
    map.insert("locale".to_string(), JsonValue::String(locale.to_string()));
    if !args.pack_refs.is_empty() {
        let values = args
            .pack_refs
            .iter()
            .map(|pack_ref| json!({ "pack_ref": pack_ref }))
            .collect::<Vec<_>>();
        map.insert("pack_refs".to_string(), JsonValue::Array(values));
    }
    if !args.targets.is_empty() {
        let values = args
            .targets
            .iter()
            .filter_map(|target| parse_wizard_target(target).ok())
            .map(|(tenant, team)| {
                if let Some(team_id) = team {
                    json!({ "tenant_id": tenant, "team_id": team_id })
                } else {
                    json!({ "tenant_id": tenant })
                }
            })
            .collect::<Vec<_>>();
        if !values.is_empty() {
            map.insert("targets".to_string(), JsonValue::Array(values));
        }
    } else {
        let mut target = JsonMap::new();
        target.insert(
            "tenant_id".to_string(),
            JsonValue::String(args.tenant.clone()),
        );
        if let Some(team) = args.team.as_ref() {
            target.insert("team_id".to_string(), JsonValue::String(team.clone()));
        }
        map.insert(
            "targets".to_string(),
            JsonValue::Array(vec![JsonValue::Object(target)]),
        );
    }
    if args.execute {
        map.insert(
            "execution_mode".to_string(),
            JsonValue::String("execute".to_string()),
        );
    } else if args.dry_run {
        map.insert(
            "execution_mode".to_string(),
            JsonValue::String("dry run".to_string()),
        );
    }
    JsonValue::Object(map)
}

fn merge_cli_overrides_into_wizard_answers(
    answers: &mut WizardQaAnswers,
    args: &DemoWizardArgs,
    locale: &str,
) {
    if let Some(bundle) = args.bundle.clone() {
        answers.bundle = Some(bundle);
    }
    if !args.catalog_packs.is_empty() {
        answers.catalog_packs.extend(
            args.catalog_packs
                .iter()
                .map(|id| WizardCatalogPackAnswer::Id(id.clone())),
        );
    }
    if !args.pack_refs.is_empty() {
        answers.pack_refs.extend(
            args.pack_refs
                .iter()
                .map(|pack_ref| WizardPackRefAnswer::Ref(pack_ref.clone())),
        );
    }
    if !args.targets.is_empty() {
        answers.targets = args
            .targets
            .iter()
            .map(|target| WizardTargetAnswer::Target(target.clone()))
            .collect();
    }
    if !args.allow_paths.is_empty() {
        answers.allow_paths = args.allow_paths.clone();
    }
    if args.tenant != "demo" || answers.tenant.is_none() {
        answers.tenant = Some(args.tenant.clone());
    }
    if args.team.is_some() {
        answers.team = args.team.clone();
    }
    if let Some(value) = args.locale.as_ref() {
        answers.locale = Some(value.clone());
    } else if answers.locale.is_none() {
        answers.locale = Some(locale.to_string());
    }
}

fn detect_system_locale_tag() -> String {
    operator_i18n::select_locale(None)
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

fn normalize_pack_refs(values: &[WizardPackRefAnswer]) -> Vec<String> {
    values
        .iter()
        .map(|value| match value {
            WizardPackRefAnswer::Ref(raw) => raw.clone(),
            WizardPackRefAnswer::Item { pack_ref, .. } => pack_ref.clone(),
        })
        .filter(|value| !value.trim().is_empty())
        .collect()
}

fn normalize_default_assignments_from_pack_refs(
    values: &[WizardPackRefAnswer],
) -> anyhow::Result<Vec<wizard::PackDefaultSelection>> {
    let mut out = Vec::new();
    for value in values {
        let WizardPackRefAnswer::Item {
            pack_ref,
            access_scope,
            make_default_pack,
            tenant_id,
            team_id,
        } = value
        else {
            continue;
        };
        let make_default = make_default_pack
            .as_deref()
            .map(str::trim)
            .map(|value| value.eq_ignore_ascii_case("y") || value.eq_ignore_ascii_case("yes"))
            .unwrap_or(false);
        if !make_default {
            continue;
        }
        let scope = match access_scope.as_deref().map(str::trim) {
            None | Some("") | Some("all_tenants") => wizard::PackScope::Global,
            Some("tenant_all_teams") => {
                let tenant_id = tenant_id
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| anyhow!("access_scope=tenant_all_teams requires tenant_id"))?;
                wizard::PackScope::Tenant { tenant_id }
            }
            Some("specific_team") => {
                let tenant_id = tenant_id
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| anyhow!("access_scope=specific_team requires tenant_id"))?;
                let team_id = team_id
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| anyhow!("access_scope=specific_team requires team_id"))?;
                wizard::PackScope::Team { tenant_id, team_id }
            }
            Some(other) => return Err(anyhow!("unsupported access_scope {other}")),
        };
        out.push(wizard::PackDefaultSelection {
            pack_identifier: pack_ref.clone(),
            scope,
        });
    }
    Ok(out)
}

fn normalize_access_changes_from_pack_refs(
    values: &[WizardPackRefAnswer],
    tenants: &[wizard::TenantSelection],
) -> anyhow::Result<Vec<wizard::AccessChangeSelection>> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for value in values {
        let (pack_ref, scope, tenant_id, team_id) = match value {
            WizardPackRefAnswer::Ref(pack_ref) => (pack_ref, "all_tenants", None, None),
            WizardPackRefAnswer::Item {
                pack_ref,
                access_scope,
                tenant_id,
                team_id,
                ..
            } => (
                pack_ref,
                access_scope.as_deref().unwrap_or("all_tenants"),
                tenant_id.as_deref(),
                team_id.as_deref(),
            ),
        };
        let pack_ref = pack_ref.trim();
        if pack_ref.is_empty() {
            continue;
        }
        match scope {
            "all_tenants" | "" => {
                for target in tenants {
                    let key = (
                        pack_ref.to_string(),
                        target.tenant.clone(),
                        target.team.clone().unwrap_or_default(),
                    );
                    if !seen.insert(key) {
                        continue;
                    }
                    out.push(wizard::AccessChangeSelection {
                        pack_id: pack_ref.to_string(),
                        operation: wizard::AccessOperation::AllowAdd,
                        tenant_id: target.tenant.clone(),
                        team_id: target.team.clone(),
                    });
                }
            }
            "tenant_all_teams" => {
                let tenant_id = tenant_id
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow!("access_scope=tenant_all_teams requires tenant_id"))?;
                let key = (pack_ref.to_string(), tenant_id.to_string(), String::new());
                if seen.insert(key) {
                    out.push(wizard::AccessChangeSelection {
                        pack_id: pack_ref.to_string(),
                        operation: wizard::AccessOperation::AllowAdd,
                        tenant_id: tenant_id.to_string(),
                        team_id: None,
                    });
                }
            }
            "specific_team" => {
                let tenant_id = tenant_id
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow!("access_scope=specific_team requires tenant_id"))?;
                let team_id = team_id
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow!("access_scope=specific_team requires team_id"))?;
                let key = (
                    pack_ref.to_string(),
                    tenant_id.to_string(),
                    team_id.to_string(),
                );
                if seen.insert(key) {
                    out.push(wizard::AccessChangeSelection {
                        pack_id: pack_ref.to_string(),
                        operation: wizard::AccessOperation::AllowAdd,
                        tenant_id: tenant_id.to_string(),
                        team_id: Some(team_id.to_string()),
                    });
                }
            }
            other => return Err(anyhow!("unsupported access_scope {other}")),
        }
    }
    Ok(out)
}

fn normalize_catalog_packs(values: &[WizardCatalogPackAnswer]) -> Vec<String> {
    values
        .iter()
        .map(|value| match value {
            WizardCatalogPackAnswer::Id(raw) => raw.clone(),
            WizardCatalogPackAnswer::Item { id } => id.clone(),
        })
        .filter(|value| !value.trim().is_empty())
        .collect()
}

fn normalize_targets(values: &[WizardTargetAnswer]) -> Vec<String> {
    values
        .iter()
        .map(|value| match value {
            WizardTargetAnswer::Target(raw) => raw.clone(),
            WizardTargetAnswer::Item { tenant_id, team_id } => team_id
                .as_ref()
                .map(|team| format!("{tenant_id}:{team}"))
                .unwrap_or_else(|| tenant_id.clone()),
        })
        .filter(|value| !value.trim().is_empty())
        .collect()
}

fn normalize_target_selections(values: &[WizardTargetAnswer]) -> Vec<wizard::TenantSelection> {
    values
        .iter()
        .filter_map(|value| match value {
            WizardTargetAnswer::Target(raw) => {
                parse_wizard_target(raw)
                    .ok()
                    .map(|(tenant, team)| wizard::TenantSelection {
                        tenant,
                        team,
                        allow_paths: Vec::new(),
                    })
            }
            WizardTargetAnswer::Item { tenant_id, team_id } => {
                let tenant = tenant_id.trim();
                if tenant.is_empty() {
                    None
                } else {
                    Some(wizard::TenantSelection {
                        tenant: tenant.to_string(),
                        team: team_id.clone().filter(|value| !value.trim().is_empty()),
                        allow_paths: Vec::new(),
                    })
                }
            }
        })
        .collect()
}

fn normalize_provider_ids(values: &[WizardProviderAnswer]) -> Vec<String> {
    values
        .iter()
        .filter_map(|value| match value {
            WizardProviderAnswer::Id(raw) => Some(raw.clone()),
            WizardProviderAnswer::Item { provider_id, id } => provider_id
                .clone()
                .or_else(|| id.clone())
                .filter(|value| !value.trim().is_empty()),
        })
        .collect()
}

fn normalize_update_ops(values: &[WizardUpdateOpAnswer]) -> BTreeSet<wizard::WizardUpdateOp> {
    values
        .iter()
        .filter_map(|value| match value {
            WizardUpdateOpAnswer::Op(raw) => raw.trim().parse::<wizard::WizardUpdateOp>().ok(),
            WizardUpdateOpAnswer::Item { op } => op.trim().parse::<wizard::WizardUpdateOp>().ok(),
        })
        .collect()
}

fn normalize_remove_targets(
    values: &[WizardRemoveTargetAnswer],
) -> BTreeSet<wizard::WizardRemoveTarget> {
    values
        .iter()
        .filter_map(|value| match value {
            WizardRemoveTargetAnswer::Target(raw) => {
                raw.trim().parse::<wizard::WizardRemoveTarget>().ok()
            }
            WizardRemoveTargetAnswer::Item {
                target_type,
                target,
            } => target_type
                .as_ref()
                .or(target.as_ref())
                .and_then(|raw| raw.trim().parse::<wizard::WizardRemoveTarget>().ok()),
        })
        .collect()
}

fn normalize_pack_removes(
    values: &[WizardPackRemoveAnswer],
) -> anyhow::Result<Vec<wizard::PackRemoveSelection>> {
    let mut out = Vec::new();
    for value in values {
        match value {
            WizardPackRemoveAnswer::Pack(raw) => {
                let pack_identifier = raw.trim().to_string();
                if pack_identifier.is_empty() {
                    continue;
                }
                out.push(wizard::PackRemoveSelection {
                    pack_identifier,
                    scope: None,
                });
            }
            WizardPackRemoveAnswer::Item {
                pack_identifier,
                pack_id,
                pack_ref,
                scope,
                tenant_id,
                team_id,
            } => {
                let identifier = pack_identifier
                    .clone()
                    .or_else(|| pack_id.clone())
                    .or_else(|| pack_ref.clone())
                    .unwrap_or_default();
                let pack_identifier = identifier.trim().to_string();
                if pack_identifier.is_empty() {
                    continue;
                }
                let parsed_scope = match scope.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                    None => None,
                    Some("bundle") => Some(wizard::PackScope::Bundle),
                    Some("global") => Some(wizard::PackScope::Global),
                    Some("tenant") => {
                        let tenant_id = tenant_id
                            .clone()
                            .filter(|value| !value.trim().is_empty())
                            .ok_or_else(|| {
                                anyhow!("packs_remove scope=tenant requires tenant_id")
                            })?;
                        Some(wizard::PackScope::Tenant { tenant_id })
                    }
                    Some("team") => {
                        let tenant_id = tenant_id
                            .clone()
                            .filter(|value| !value.trim().is_empty())
                            .ok_or_else(|| anyhow!("packs_remove scope=team requires tenant_id"))?;
                        let team_id = team_id
                            .clone()
                            .filter(|value| !value.trim().is_empty())
                            .ok_or_else(|| anyhow!("packs_remove scope=team requires team_id"))?;
                        Some(wizard::PackScope::Team { tenant_id, team_id })
                    }
                    Some(other) => return Err(anyhow!("unsupported packs_remove scope {other}")),
                };
                out.push(wizard::PackRemoveSelection {
                    pack_identifier,
                    scope: parsed_scope,
                });
            }
        }
    }
    Ok(out)
}

fn normalize_access_changes(
    values: &[WizardAccessChangeAnswer],
) -> Vec<wizard::AccessChangeSelection> {
    values
        .iter()
        .filter_map(|value| match value {
            WizardAccessChangeAnswer::Item {
                pack_id,
                pack_ref,
                operation,
                tenant_id,
                team_id,
            } => {
                let pack_id = pack_id
                    .clone()
                    .or_else(|| pack_ref.clone())
                    .filter(|value| !value.trim().is_empty())?;
                let operation = match operation.as_deref().map(str::trim).unwrap_or("allow_add") {
                    "allow_add" => wizard::AccessOperation::AllowAdd,
                    "allow_remove" => wizard::AccessOperation::AllowRemove,
                    _ => wizard::AccessOperation::AllowAdd,
                };
                Some(wizard::AccessChangeSelection {
                    pack_id,
                    operation,
                    tenant_id: tenant_id.clone(),
                    team_id: team_id.clone(),
                })
            }
        })
        .collect()
}

fn build_access_changes(
    mode: wizard::WizardMode,
    access_mode: Option<&str>,
    tenants: &[wizard::TenantSelection],
    pack_refs: &[String],
    existing: Vec<wizard::AccessChangeSelection>,
) -> anyhow::Result<Vec<wizard::AccessChangeSelection>> {
    if mode != wizard::WizardMode::Create {
        return Ok(existing);
    }

    let normalized_mode = access_mode.map(str::trim).filter(|value| !value.is_empty());
    let mut changes = existing;
    match normalized_mode {
        Some("all_selected_get_all_packs") => {
            for tenant in tenants {
                for pack_ref in pack_refs {
                    changes.push(wizard::AccessChangeSelection {
                        pack_id: pack_ref.clone(),
                        operation: wizard::AccessOperation::AllowAdd,
                        tenant_id: tenant.tenant.clone(),
                        team_id: tenant.team.clone(),
                    });
                }
            }
        }
        Some("per_pack_matrix") => {
            if changes.is_empty() {
                return Err(anyhow!(
                    "access_mode=per_pack_matrix requires non-empty access_change entries"
                ));
            }
        }
        Some(other) => {
            return Err(anyhow!(
                "unsupported access_mode {}; expected all_selected_get_all_packs or per_pack_matrix",
                other
            ));
        }
        None => {
            if changes.is_empty() {
                for tenant in tenants {
                    for pack_ref in pack_refs {
                        changes.push(wizard::AccessChangeSelection {
                            pack_id: pack_ref.clone(),
                            operation: wizard::AccessOperation::AllowAdd,
                            tenant_id: tenant.tenant.clone(),
                            team_id: tenant.team.clone(),
                        });
                    }
                }
            }
        }
    }

    let mut dedup = BTreeSet::new();
    changes.retain(|change| {
        let key = (
            change.pack_id.clone(),
            match change.operation {
                wizard::AccessOperation::AllowAdd => "allow_add",
                wizard::AccessOperation::AllowRemove => "allow_remove",
            }
            .to_string(),
            change.tenant_id.clone(),
            change.team_id.clone().unwrap_or_default(),
        );
        dedup.insert(key)
    });
    Ok(changes)
}

fn load_wizard_i18n(locale: &str) -> anyhow::Result<ResolvedI18nMap> {
    wizard_i18n::load(locale)
}

fn run_wizard_setup_for_target(
    bundle: &Path,
    tenant: &str,
    team: Option<&str>,
    setup_input: Option<&PathBuf>,
    allowed_providers: Option<BTreeSet<String>>,
    preloaded_setup_answers: Option<SetupInputAnswers>,
) -> anyhow::Result<()> {
    for domain in [Domain::Messaging, Domain::Events, Domain::Secrets] {
        run_domain_command(DomainRunArgs {
            root: bundle.to_path_buf(),
            state_root: None,
            domain,
            action: DomainAction::Setup,
            tenant: tenant.to_string(),
            team: team.map(|value| value.to_string()),
            provider_filter: None,
            dry_run: false,
            format: PlanFormat::Text,
            parallel: 1,
            allow_missing_setup: true,
            allow_contract_change: false,
            backup: false,
            online: false,
            secrets_env: None,
            runner_binary: None,
            best_effort: true,
            discovered_providers: None,
            setup_input: if preloaded_setup_answers.is_some() {
                None
            } else {
                setup_input.cloned()
            },
            allowed_providers: allowed_providers.clone(),
            preloaded_setup_answers: preloaded_setup_answers.clone(),
            public_base_url: None,
            secrets_manager: None,
        })?;
    }
    Ok(())
}

fn build_wizard_setup_answers(
    bundle: &Path,
    packs: &[wizard::ResolvedPackInfo],
    allowed: &BTreeSet<String>,
    setup_input: Option<&PathBuf>,
) -> anyhow::Result<SetupInputAnswers> {
    let base_input = if let Some(path) = setup_input {
        let raw = load_setup_input(path)?;
        Some(SetupInputAnswers::new(raw, allowed.clone())?)
    } else {
        None
    };
    let mut map = serde_json::Map::new();
    for pack in packs {
        if !allowed.contains(&pack.pack_id) {
            continue;
        }
        let pack_path = bundle.join(&pack.output_path);
        let answers = collect_setup_answers(
            &pack_path,
            &pack.pack_id,
            base_input.as_ref(),
            setup_input.is_none(),
        )?;
        map.insert(pack.pack_id.clone(), answers);
    }
    SetupInputAnswers::new(serde_json::Value::Object(map), allowed.clone())
}

impl DemoSendArgs {
    fn run(self) -> anyhow::Result<()> {
        let team = if self.team.is_empty() {
            None
        } else {
            Some(self.team.as_str())
        };
        domains::ensure_cbor_packs(&self.bundle)?;
        let pack = resolve_demo_provider_pack(
            &self.bundle,
            &self.tenant,
            team,
            &self.provider,
            Domain::Messaging,
        )?;
        let provider_type = primary_provider_type(&pack.path)
            .context("failed to determine provider type for demo send")?;
        let discovery = discovery::discover_with_options(
            &self.bundle,
            discovery::DiscoveryOptions { cbor_only: true },
        )?;
        let provider_map = discovery_map(&discovery.providers);
        let provider_id = provider_id_for_pack(&pack.path, &pack.pack_id, Some(&provider_map));

        let secrets_handle =
            secrets_gate::resolve_secrets_manager(&self.bundle, &self.tenant, team)?;
        let runner_host = DemoRunnerHost::new(
            self.bundle.clone(),
            &discovery,
            self.runner_binary.clone(),
            secrets_handle.clone(),
            false,
        )?;
        let env = self.env.clone();
        let context = OperatorContext {
            tenant: self.tenant.clone(),
            team: team.map(|value| value.to_string()),
            correlation_id: None,
        };

        if self.print_required_args {
            if let Err(message) = ensure_requirements_flow(&pack) {
                eprintln!("{message}");
                std::process::exit(2);
            }
            let input = build_input_payload(
                &self.bundle,
                Domain::Messaging,
                &self.tenant,
                team,
                Some(&pack.pack_id),
                None,
                None,
                &env,
            );
            let input_bytes = serde_json::to_vec(&input)?;
            let outcome = runner_host.invoke_provider_op(
                Domain::Messaging,
                &provider_id,
                "requirements",
                &input_bytes,
                &context,
            )?;
            if !outcome.success {
                let message = outcome
                    .error
                    .unwrap_or_else(|| "requirements flow failed".to_string());
                return Err(anyhow::anyhow!(message));
            }
            if let Some(value) = outcome.output {
                if let Some(rendered) = format_requirements_output(&value) {
                    println!("{rendered}");
                } else {
                    let json = serde_json::to_string_pretty(&value)?;
                    println!("{json}");
                }
            } else if let Some(raw) = outcome.raw {
                println!("{raw}");
            }
            return Ok(());
        }

        let card_payload = if let Some(path) = &self.card {
            let contents = fs::read_to_string(path)
                .with_context(|| format!("failed to read card file {}", path.display()))?;
            Some(
                serde_json::from_str::<JsonValue>(&contents)
                    .with_context(|| format!("failed to parse card file {}", path.display()))?,
            )
        } else {
            None
        };
        let mut text_value = self.text.clone();
        if text_value.is_none() && card_payload.is_some() {
            text_value = Some("adaptive card".to_string());
        }
        let text_ref = text_value.as_deref();
        if text_ref.is_none() && card_payload.is_none() {
            return Err(anyhow::anyhow!(
                "either --text or --card is required unless --print-required-args"
            ));
        }
        let args = merge_args(self.args_json.as_deref(), &self.args)?;
        let mut config_items = Vec::new();
        config_items.push(ConfigGateItem::new(
            "env",
            Some(env.clone()),
            ConfigValueSource::Platform("GREENTIC_ENV"),
            true,
        ));
        config_items.push(ConfigGateItem::new(
            "tenant",
            Some(self.tenant.clone()),
            ConfigValueSource::Platform("tenant"),
            true,
        ));
        let team_label = team.unwrap_or("default");
        config_items.push(ConfigGateItem::new(
            "team",
            Some(team_label.to_string()),
            ConfigValueSource::Platform("team"),
            true,
        ));
        if let Some(text) = &text_value {
            config_items.push(ConfigGateItem::new(
                "text",
                Some(text.clone()),
                ConfigValueSource::Argument("--text"),
                true,
            ));
        }
        if let Some(card_path) = &self.card {
            config_items.push(ConfigGateItem::new(
                "card",
                Some(card_path.display().to_string()),
                ConfigValueSource::Argument("--card"),
                true,
            ));
        }
        let mut arg_entries = args.iter().collect::<Vec<_>>();
        arg_entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        for (key, value) in arg_entries {
            config_items.push(ConfigGateItem::new(
                key.as_str(),
                Some(config_value_display(value)),
                ConfigValueSource::Argument("--arg"),
                true,
            ));
        }
        if !self.to.is_empty() {
            config_items.push(ConfigGateItem::new(
                "to",
                Some(self.to.join(",")),
                ConfigValueSource::Argument("--to"),
                true,
            ));
        }
        if let Some(kind) = self.to_kind.as_ref() {
            config_items.push(ConfigGateItem::new(
                "to-kind",
                Some(kind.clone()),
                ConfigValueSource::Argument("--to-kind"),
                false,
            ));
        }
        config_gate::log_config_gate(Domain::Messaging, &self.tenant, team, &env, &config_items);
        let channel = provider_channel(&self.provider);
        let message = build_demo_send_message(DemoSendMessageArgs {
            text: text_ref,
            args: &args,
            tenant: &self.tenant,
            team,
            destinations: &self.to,
            to_kind: self.to_kind.as_deref(),
            provider_id: &self.provider,
            channel: &channel,
            card: card_payload.as_ref(),
        });
        debug_print_envelope("initial message", &message);

        // Compose a message plan and encode payload directly against the provider component (no flow resolution).
        let render_plan_input = egress::build_render_plan_input(message.clone());
        let render_plan_input_value = serde_json::to_value(&render_plan_input)?;
        let plan_value = run_provider_component_op_json(
            &runner_host,
            &pack,
            &provider_id,
            &context,
            "render_plan",
            render_plan_input_value.clone(),
        )
        .with_context(|| "render_plan failed")?;
        let render_plan_out: RenderPlanOutV1 =
            serde_json::from_value(plan_value.clone()).context("render_plan output invalid")?;
        debug_print_render_plan_output(&render_plan_out);
        if !render_plan_out.ok {
            let err = render_plan_out
                .error
                .unwrap_or_else(|| "render_plan returned error".to_string());
            return Err(anyhow::anyhow!(err));
        }
        let encode_input = egress::build_encode_input(message.clone(), plan_value.clone());
        debug_print_encode_input(&encode_input);
        let payload_value = run_provider_component_op_json(
            &runner_host,
            &pack,
            &provider_id,
            &context,
            "encode",
            serde_json::to_value(&encode_input)?,
        )
        .with_context(|| "encode failed")?;
        let encode_out: EncodeOutV1 =
            serde_json::from_value(payload_value).context("encode output invalid")?;
        debug_print_encode_output(&encode_out);
        if !encode_out.ok {
            let err = encode_out
                .error
                .unwrap_or_else(|| "encode returned error".to_string());
            return Err(anyhow::anyhow!(err));
        }
        let payload = encode_out
            .payload
            .ok_or_else(|| anyhow::anyhow!("encode output missing payload"))?;
        let send_input = egress::build_send_payload(
            payload,
            provider_type.clone(),
            self.tenant.clone(),
            team.map(|value| value.to_string()),
        );
        let send_value = serde_json::to_value(&send_input)?;
        let send_outcome = run_provider_component_op(
            &runner_host,
            &pack,
            &provider_id,
            &context,
            "send_payload",
            send_value,
        )
        .context("send_payload failed")?;
        println!("{}", operator_i18n::tr("cli.common.ok", "ok"));
        let status = if send_outcome.success {
            operator_i18n::tr("cli.common.success", "success")
        } else {
            operator_i18n::tr("cli.common.failed", "failed")
        };
        println!(
            "{}",
            operator_i18n::trf("cli.demo_send.flow_result", "Flow result: {}", &[&status])
        );
        if let Some(error) = &send_outcome.error {
            println!(
                "{}",
                operator_i18n::trf("cli.demo_send.flow_error", "Flow error: {}", &[error])
            );
        }
        if let Some(value) = send_outcome.output {
            if let Ok(parsed) = serde_json::from_value::<SendPayloadOutV1>(value.clone()) {
                debug_print_send_payload_output(&parsed);
            } else if demo_debug_enabled() {
                if let Ok(body) = serde_json::to_string_pretty(&value) {
                    println!(
                        "{}",
                        operator_i18n::trf(
                            "cli.demo_send.debug_parse_send_payload_failed",
                            "[demo] after send_payload output: failed to parse SendPayloadOutV1\n{}",
                            &[&body]
                        )
                    );
                } else {
                    println!(
                        "{}",
                        operator_i18n::tr(
                            "cli.demo_send.debug_invalid_json_output",
                            "[demo] after send_payload output: invalid JSON output"
                        )
                    );
                }
            }
            let missing_uris = if payload_contains_secret_error(&value) {
                gather_missing_secret_uris(
                    &secrets_handle.manager(),
                    &env,
                    &self.tenant,
                    team,
                    &pack.path,
                    &provider_id,
                    secrets_handle.dev_store_path.as_deref(),
                    secrets_handle.using_env_fallback,
                    Some(provider_type.as_str()),
                )
            } else {
                Vec::new()
            };
            if !missing_uris.is_empty() {
                println!(
                    "{}",
                    operator_i18n::trf(
                        "cli.demo_send.missing_secret_uris",
                        "missing secret URIs:\n{}",
                        &[&missing_uris
                            .iter()
                            .map(|uri| format!("  - {uri}"))
                            .collect::<Vec<_>>()
                            .join("\n")]
                    )
                );
                for uri in &missing_uris {
                    print_secret_missing_details(
                        uri,
                        secrets_handle.dev_store_path.as_deref(),
                        secrets_handle.using_env_fallback,
                        &self.bundle,
                    );
                }
            }
            let enriched = enrich_secret_error_payload(
                value,
                &context,
                &env,
                &provider_id,
                &pack.pack_id,
                &pack.path,
                &missing_uris,
                &secrets_handle.selection,
                secrets_handle.dev_store_path.as_deref(),
            );
            let json = serde_json::to_string_pretty(&enriched)?;
            println!("{json}");
        } else if let Some(raw) = send_outcome.raw {
            println!("{raw}");
        }
        Ok(())
    }
}

fn run_provider_component_op(
    runner_host: &DemoRunnerHost,
    pack: &domains::ProviderPack,
    provider_id: &str,
    ctx: &OperatorContext,
    op: &str,
    payload: serde_json::Value,
) -> anyhow::Result<FlowOutcome> {
    let bytes = serde_json::to_vec(&payload)?;
    let outcome = runner_host.invoke_provider_component_op_direct(
        Domain::Messaging,
        pack,
        provider_id,
        op,
        &bytes,
        ctx,
    )?;
    ensure_provider_op_success(provider_id, op, &outcome)?;
    if let Some(value) = &outcome.output
        && let Some(card) = detect_adaptive_card_view(value)
    {
        print_card_summary(&card);
    }
    Ok(outcome)
}

fn run_provider_component_op_json(
    runner_host: &DemoRunnerHost,
    pack: &domains::ProviderPack,
    provider_id: &str,
    ctx: &OperatorContext,
    op: &str,
    payload: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let outcome = run_provider_component_op(runner_host, pack, provider_id, ctx, op, payload)?;
    Ok(outcome.output.unwrap_or_else(|| json!({})))
}

#[allow(clippy::too_many_arguments)]
fn enrich_secret_error_payload(
    mut payload: serde_json::Value,
    ctx: &OperatorContext,
    env: &str,
    provider_id: &str,
    pack_id: &str,
    pack_path: &Path,
    missing_uris: &[String],
    selection: &secrets_manager::SecretsManagerSelection,
    dev_store_path: Option<&Path>,
) -> serde_json::Value {
    let team = secrets_manager::canonical_team(ctx.team.as_deref()).to_string();
    let selection_desc = selection.description();
    let dev_store_desc = dev_store_path
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<default>".to_string());
    let context_suffix = format!(
        "env={} tenant={} team={} provider={} pack_id={} pack_path={} secrets_manager={} dev_store={}",
        env,
        ctx.tenant,
        team,
        provider_id,
        pack_id,
        pack_path.display(),
        selection_desc,
        dev_store_desc
    );
    if let serde_json::Value::Object(map) = &mut payload {
        for key in ["message", "error"] {
            if let Some(entry) = map.get_mut(key)
                && let Some(text) = entry.as_str()
                && text_contains_secret_error(text)
            {
                let suffix = secret_error_suffix(&context_suffix, missing_uris);
                let enriched = format!("{text} ({suffix})");
                *entry = serde_json::Value::String(enriched);
            }
        }
    }
    payload
}

fn print_secret_missing_details(
    uri: &str,
    store_path: Option<&Path>,
    using_env_fallback: bool,
    bundle_root: &Path,
) {
    let key = secrets_gate::canonical_secret_store_key(uri)
        .unwrap_or_else(|| "<invalid secret uri>".to_string());
    let default_store = dev_store_path::default_path(bundle_root);
    let store_desc = match (store_path, using_env_fallback) {
        (Some(path), _) => path.display().to_string(),
        (None, true) => "<env secrets store>".to_string(),
        (None, false) => default_store.display().to_string(),
    };
    println!(
        "{}",
        operator_i18n::tr("cli.secrets.not_found", "Secret not found:")
    );
    println!(
        "{}",
        operator_i18n::trf("cli.secrets.uri", "  uri: {}", &[uri])
    );
    println!(
        "{}",
        operator_i18n::trf("cli.secrets.key", "  key: {}", &[&key])
    );
    println!(
        "{}",
        operator_i18n::trf("cli.secrets.store", "  store: {}", &[&store_desc])
    );
    println!(
        "{}",
        operator_i18n::trf(
            "cli.secrets.hint_setup_or_add_key",
            "hint: run `greentic-operator setup` or add the key to {}",
            &[&default_store.display().to_string()]
        )
    );
}

fn payload_contains_secret_error(value: &JsonValue) -> bool {
    for key in ["message", "error"] {
        if let Some(text) = value.get(key).and_then(JsonValue::as_str)
            && text_contains_secret_error(text)
        {
            return true;
        }
    }
    false
}

fn text_contains_secret_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("secret store error") || text.contains("SecretsError")
}

fn secret_error_suffix(context_suffix: &str, missing_uris: &[String]) -> String {
    if missing_uris.is_empty() {
        context_suffix.to_string()
    } else {
        let missing = missing_uris.join(", ");
        format!("{context_suffix}; missing secrets: {missing}")
    }
}

#[allow(clippy::too_many_arguments)]
fn gather_missing_secret_uris(
    manager: &DynSecretsManager,
    env: &str,
    tenant: &str,
    team: Option<&str>,
    pack_path: &Path,
    provider_id: &str,
    store_path: Option<&Path>,
    using_env_fallback: bool,
    provider_type: Option<&str>,
) -> Vec<String> {
    match secrets_gate::check_provider_secrets(
        manager,
        env,
        tenant,
        team,
        pack_path,
        provider_id,
        provider_type,
        store_path,
        using_env_fallback,
    ) {
        Ok(Some(missing)) => missing,
        Ok(None) => Vec::new(),
        Err(err) => {
            operator_log::warn(
                module_path!(),
                format!(
                    "failed to check missing secrets for provider {}: {}",
                    provider_id, err
                ),
            );
            Vec::new()
        }
    }
}

fn ensure_provider_op_success(
    provider_id: &str,
    op: &str,
    outcome: &FlowOutcome,
) -> anyhow::Result<()> {
    if outcome.success {
        return Ok(());
    }
    let message = outcome
        .error
        .clone()
        .or_else(|| outcome.raw.clone())
        .unwrap_or_else(|| "unknown error".to_string());
    Err(anyhow::anyhow!("{provider_id}.{op} failed: {message}"))
}

fn print_capability_outcome(outcome: &FlowOutcome) -> anyhow::Result<()> {
    println!(
        "{}",
        operator_i18n::trf(
            "cli.capabilities.outcome.success",
            "success: {}",
            &[&outcome.success.to_string()]
        )
    );
    if let Some(error) = outcome.error.as_ref() {
        println!(
            "{}",
            operator_i18n::trf("cli.capabilities.outcome.error", "error: {}", &[error])
        );
    }
    if let Some(raw) = outcome.raw.as_ref()
        && !raw.trim().is_empty()
    {
        println!(
            "{}",
            operator_i18n::trf("cli.capabilities.outcome.raw", "raw:\n{}", &[raw])
        );
    }
    if let Some(value) = outcome.output.as_ref() {
        println!("{}", serde_json::to_string_pretty(value)?);
    }
    Ok(())
}

#[derive(Parser)]
#[command(
    about = "Send a synthetic HTTP request through the messaging ingress pipeline.",
    long_about = "Constructs an HttpInV1 payload, invokes the provider's ingest_http op, and optionally runs the resulting events through the app/outbound flow."
)]
struct DemoIngressArgs {
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long)]
    provider: String,
    #[arg(long)]
    path: Option<String>,
    #[arg(long, value_enum, default_value_t = DemoIngressMethod::Post)]
    method: DemoIngressMethod,
    #[arg(long = "header")]
    headers: Vec<String>,
    #[arg(long = "query")]
    queries: Vec<String>,
    #[arg(long)]
    body: Option<String>,
    #[arg(long)]
    body_json: Option<String>,
    #[arg(long)]
    body_raw: Option<String>,
    #[arg(long)]
    binding_id: Option<String>,
    #[arg(long, default_value = "demo")]
    tenant: String,
    #[arg(long, default_value = "default")]
    team: String,
    #[arg(long)]
    runner_binary: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "all")]
    print: DemoIngressPrintMode,
    #[arg(long)]
    end_to_end: bool,
    #[arg(long)]
    app_pack: Option<String>,
    #[arg(long, action = ArgAction::SetTrue)]
    send: bool,
    #[arg(long)]
    retries: Option<u32>,
    #[arg(long, action = ArgAction::SetTrue)]
    dlq_tail: bool,
    #[arg(long, default_value_t = true)]
    dry_run: bool,
    #[arg(long)]
    correlation_id: Option<String>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum DemoIngressMethod {
    Get,
    Post,
}

impl DemoIngressMethod {
    fn as_str(&self) -> &'static str {
        match self {
            DemoIngressMethod::Get => "GET",
            DemoIngressMethod::Post => "POST",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum DemoIngressPrintMode {
    Http,
    Events,
    #[default]
    All,
}

impl DemoIngressPrintMode {
    fn should_print_http(&self) -> bool {
        matches!(self, DemoIngressPrintMode::Http | DemoIngressPrintMode::All)
    }

    fn should_print_events(&self) -> bool {
        matches!(
            self,
            DemoIngressPrintMode::Events | DemoIngressPrintMode::All
        )
    }
}

impl DemoIngressArgs {
    fn run(self) -> anyhow::Result<()> {
        ensure_single_body_field(&self)?;
        let body_bytes = resolve_ingress_body(
            self.body.as_deref(),
            self.body_json.as_deref(),
            self.body_raw.as_deref(),
        )?;
        let path = self
            .path
            .clone()
            .unwrap_or_else(|| default_ingress_path(&self.provider, self.binding_id.as_deref()));
        let headers = parse_header_pairs(&self.headers)?;
        let queries = parse_query_pairs(&self.queries)?;
        let route = derive_route_from_path(&path);
        let full_path = if path.starts_with('/') {
            path.clone()
        } else {
            format!("/{path}")
        };

        let request = crate::messaging_universal::ingress::build_ingress_request(
            &self.provider,
            route,
            self.method.as_str(),
            &full_path,
            headers,
            queries,
            &body_bytes,
            self.binding_id.clone(),
            Some(self.tenant.clone()),
            Some(self.team.clone()),
        );

        let team_context = if self.team.is_empty() {
            None
        } else {
            Some(self.team.clone())
        };
        let context = OperatorContext {
            tenant: self.tenant.clone(),
            team: team_context,
            correlation_id: self.correlation_id.clone(),
        };
        let secrets_handle = secrets_gate::resolve_secrets_manager(
            &self.bundle,
            &self.tenant,
            context.team.as_deref(),
        )?;

        let (response, events) = crate::messaging_universal::ingress::run_ingress(
            &self.bundle,
            &self.provider,
            &request,
            &context,
            self.runner_binary.clone(),
            secrets_handle.clone(),
        )?;

        if self.print.should_print_http() {
            print_http_response(&response)?;
        }
        if self.print.should_print_events() {
            print_envelopes(&events)?;
        }

        if self.end_to_end {
            crate::messaging_universal::egress::run_end_to_end(
                events,
                &self.provider,
                &self.bundle,
                &context,
                self.runner_binary.clone(),
                self.app_pack.clone(),
                self.send,
                self.dry_run,
                self.retries.unwrap_or(0),
                secrets_handle.clone(),
            )?;
        }

        if self.dlq_tail {
            let paths = RuntimePaths::new(self.bundle.join("state"), &self.tenant, &self.team);
            println!(
                "{}",
                operator_i18n::trf(
                    "cli.ingress.dlq_log_location",
                    "DLQ log location: {}",
                    &[&paths.dlq_log_path().display().to_string()]
                )
            );
        }
        Ok(())
    }
}

fn ensure_single_body_field(args: &DemoIngressArgs) -> anyhow::Result<()> {
    let count =
        args.body.is_some() as u8 + args.body_json.is_some() as u8 + args.body_raw.is_some() as u8;
    if count > 1 {
        Err(anyhow::anyhow!(
            "only one of --body, --body-json, or --body-raw can be provided"
        ))
    } else {
        Ok(())
    }
}

fn resolve_ingress_body(
    body: Option<&str>,
    body_json: Option<&str>,
    body_raw: Option<&str>,
) -> anyhow::Result<Vec<u8>> {
    if let Some(raw) = body_raw {
        return Ok(raw.as_bytes().to_vec());
    }
    if let Some(json) = body_json {
        let _ = serde_json::from_str::<serde_json::Value>(json)
            .with_context(|| "invalid JSON provided to --body-json")?;
        return Ok(json.as_bytes().to_vec());
    }
    if let Some(path) = body {
        let path = path.strip_prefix('@').unwrap_or(path);
        let bytes =
            std::fs::read(path).with_context(|| format!("failed to read body file at {}", path))?;
        return Ok(bytes);
    }
    Ok(Vec::new())
}

fn parse_header_pairs(values: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    for raw in values {
        let score = raw.splitn(2, ':').collect::<Vec<_>>();
        if score.len() != 2 {
            return Err(anyhow::anyhow!(
                "invalid header '{}'; expected 'Name: value'",
                raw
            ));
        }
        headers.push((score[0].trim().to_string(), score[1].trim().to_string()));
    }
    Ok(headers)
}

fn parse_query_pairs(values: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    let mut queries = Vec::new();
    for raw in values {
        let mut parts = raw.splitn(2, '=');
        let key = parts
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("invalid query '{}'; expected 'k=v'", raw))?;
        let value = parts
            .next()
            .map(str::trim)
            .ok_or_else(|| anyhow::anyhow!("invalid query '{}'; expected 'k=v'", raw))?;
        queries.push((key.to_string(), value.to_string()));
    }
    Ok(queries)
}

fn default_ingress_path(provider: &str, binding_id: Option<&str>) -> String {
    if let Some(binding) = binding_id {
        format!("/ingress/{}/{}", provider, binding)
    } else {
        format!("/ingress/{}/webhook", provider)
    }
}

fn derive_route_from_path(path: &str) -> Option<String> {
    let segments = path
        .trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.len() >= 3 && segments[1].eq_ignore_ascii_case("ingress") {
        Some(segments[2].to_string())
    } else {
        None
    }
}

fn print_http_response(
    response: &crate::messaging_universal::dto::HttpOutV1,
) -> anyhow::Result<()> {
    println!(
        "{}",
        operator_i18n::trf(
            "cli.ingress.http_out_status",
            "HTTP OUT: status {}",
            &[&response.status.to_string()]
        )
    );
    for (name, value) in &response.headers {
        println!(
            "{}",
            operator_i18n::trf("cli.ingress.http_header", "  {}: {}", &[name, value])
        );
    }
    if let Some(body_b64) = &response.body_b64 {
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(body_b64) {
            if let Ok(text) = std::str::from_utf8(&bytes) {
                println!(
                    "{}",
                    operator_i18n::trf("cli.ingress.http_body", "  body: {}", &[text])
                );
            } else {
                println!(
                    "{}",
                    operator_i18n::trf(
                        "cli.ingress.http_body_base64",
                        "  body (base64): {}",
                        &[body_b64]
                    )
                );
            }
        } else {
            println!(
                "{}",
                operator_i18n::trf(
                    "cli.ingress.http_body_base64",
                    "  body (base64): {}",
                    &[body_b64]
                )
            );
        }
    }
    Ok(())
}

fn print_envelopes(envelopes: &[greentic_types::ChannelMessageEnvelope]) -> anyhow::Result<()> {
    for envelope in envelopes {
        let formatted = serde_json::to_string_pretty(envelope)?;
        println!("{formatted}");
    }
    Ok(())
}

impl DemoNewArgs {
    fn run(self) -> anyhow::Result<()> {
        let base = self
            .out
            .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let bundle_path = PathBuf::from(&self.bundle);
        let target = if bundle_path.is_absolute() {
            bundle_path
        } else {
            base.join(bundle_path)
        };
        if target.exists() {
            return Err(anyhow::anyhow!(
                "bundle path {} already exists",
                target.display()
            ));
        }
        create_demo_bundle_structure(&target)?;
        println!(
            "{}",
            operator_i18n::trf(
                "cli.demo_new.created_scaffold",
                "created demo bundle scaffold at {}",
                &[&target.display().to_string()]
            )
        );
        Ok(())
    }
}

#[cfg(test)]
#[derive(Clone)]
struct DemoProviderInfo {
    pack: domains::ProviderPack,
}

#[cfg(test)]
fn select_demo_providers(
    providers: &[DemoProviderInfo],
    provider_filter: Option<&str>,
) -> anyhow::Result<Vec<DemoProviderInfo>> {
    if let Some(filter) = provider_filter {
        let matches: Vec<_> = providers
            .iter()
            .filter(|info| provider_filter_matches(&info.pack, filter))
            .cloned()
            .collect();
        match matches.len() {
            0 => Err(anyhow::anyhow!(
                "No provider packs matched '{}'; try a more specific identifier.",
                filter
            )),
            1 => Ok(matches),
            _ => Err(anyhow::anyhow!(
                "Multiple provider packs matched '{}'; provide a more specific identifier.",
                filter
            )),
        }
    } else {
        Ok(providers.to_vec())
    }
}

const DEMO_CONFIG_CONTENT: &str = "version: \"1\"\nproject_root: \"./\"\n";
const DEFAULT_DEMO_GMAP: &str = "_ = forbidden\n";

fn create_demo_bundle_structure(root: &Path) -> anyhow::Result<()> {
    let directories = [
        "",
        "providers",
        "providers/messaging",
        "providers/events",
        "providers/secrets",
        "packs",
        "resolved",
        "state",
        "state/resolved",
        "state/runs",
        "state/pids",
        "state/logs",
        "state/runtime",
        "state/doctor",
        "tenants",
        "tenants/default",
        "tenants/default/teams",
        "tenants/demo",
        "tenants/demo/teams",
        "tenants/demo/teams/default",
        "logs",
    ];
    for directory in directories {
        ensure_dir(&root.join(directory))?;
    }
    write_if_missing(&root.join("greentic.demo.yaml"), DEMO_CONFIG_CONTENT)?;
    write_if_missing(
        &root.join("tenants").join("default").join("tenant.gmap"),
        DEFAULT_DEMO_GMAP,
    )?;
    write_if_missing(
        &root.join("tenants").join("demo").join("tenant.gmap"),
        DEFAULT_DEMO_GMAP,
    )?;
    write_if_missing(
        &root
            .join("tenants")
            .join("demo")
            .join("teams")
            .join("default")
            .join("team.gmap"),
        DEFAULT_DEMO_GMAP,
    )?;
    Ok(())
}

fn load_demo_config_or_default(path: &Path) -> config::DemoConfig {
    match config::load_demo_config(path) {
        Ok(value) => value,
        Err(err) => {
            operator_log::warn(
                module_path!(),
                format!(
                    "failed to load {}: {err}; using default values",
                    path.display()
                ),
            );
            config::DemoConfig::default()
        }
    }
}

fn start_demo_ingress_server(
    bundle: &Path,
    discovery: &discovery::DiscoveryResult,
    demo_config: &config::DemoConfig,
    domains: &[Domain],
    runner_binary: Option<PathBuf>,
    debug_enabled: bool,
    secrets_handle: SecretsManagerHandle,
) -> anyhow::Result<HttpIngressServer> {
    let addr = format!(
        "{}:{}",
        demo_config.services.gateway.listen_addr, demo_config.services.gateway.port
    );
    let bind_addr: SocketAddr = addr
        .parse()
        .with_context(|| format!("invalid gateway listen address {addr}"))?;
    let runner_host = Arc::new(DemoRunnerHost::new(
        bundle.to_path_buf(),
        discovery,
        runner_binary,
        secrets_handle.clone(),
        debug_enabled,
    )?);
    HttpIngressServer::start(HttpIngressConfig {
        bind_addr,
        domains: domains.to_vec(),
        runner_host,
    })
}

#[allow(clippy::too_many_arguments)]
fn start_demo_timer_scheduler(
    bundle: &Path,
    discovery: &discovery::DiscoveryResult,
    domains: &[Domain],
    runner_binary: Option<PathBuf>,
    debug_enabled: bool,
    secrets_handle: SecretsManagerHandle,
    tenant: &str,
    team: &str,
) -> anyhow::Result<Option<TimerScheduler>> {
    if !domains.contains(&Domain::Events) {
        return Ok(None);
    }
    let default_interval_seconds = std::env::var("GREENTIC_OPERATOR_TIMER_INTERVAL_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(60)
        .max(1);
    let handlers = discover_timer_handlers(discovery, default_interval_seconds)?;
    if handlers.is_empty() {
        return Ok(None);
    }
    let runner_host = Arc::new(DemoRunnerHost::new(
        bundle.to_path_buf(),
        discovery,
        runner_binary,
        secrets_handle,
        debug_enabled,
    )?);
    let scheduler = TimerScheduler::start(TimerSchedulerConfig {
        runner_host,
        tenant: tenant.to_string(),
        team: Some(team.to_string()),
        handlers,
        debug_enabled,
    })?;
    Ok(Some(scheduler))
}

fn ensure_dir(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path)?;
    Ok(())
}

fn write_if_missing(path: &Path, contents: &str) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    fs::write(path, contents)?;
    Ok(())
}
fn restart_name(target: &RestartTarget) -> String {
    match target {
        RestartTarget::All => "all",
        RestartTarget::Cloudflared => "cloudflared",
        RestartTarget::Ngrok => "ngrok",
        RestartTarget::Nats => "nats",
        RestartTarget::Gateway => "gateway",
        RestartTarget::Egress => "egress",
        RestartTarget::Subscriptions => "subscriptions",
    }
    .to_string()
}

fn resolve_demo_config_path(explicit: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    let cwd = std::env::current_dir()?;
    let demo_path = cwd.join("demo").join("demo.yaml");
    if demo_path.exists() {
        return Ok(demo_path);
    }
    let fallback = cwd.join("greentic.operator.yaml");
    if fallback.exists() {
        return Ok(fallback);
    }
    Err(anyhow::anyhow!(
        "no demo config found; pass --config or create ./demo/demo.yaml"
    ))
}

fn wait_for_ctrlc() -> anyhow::Result<()> {
    let runtime = Runtime::new().context("failed to spawn runtime for Ctrl+C listener")?;
    runtime.block_on(async {
        tokio::signal::ctrl_c()
            .await
            .map_err(|err| anyhow::anyhow!("failed to wait for Ctrl+C: {err}"))
    })
}

impl DemoStatusArgs {
    fn run(self) -> anyhow::Result<()> {
        let state_dir = resolve_state_dir(self.state_dir, self.bundle.as_ref());
        if demo_debug_enabled() {
            println!(
                "[demo] status state_dir={} tenant={} team={} verbose={}",
                state_dir.display(),
                self.tenant,
                self.team,
                self.verbose
            );
        }
        demo::demo_status_runtime(&state_dir, &self.tenant, &self.team, self.verbose)
    }
}

impl DemoLogsArgs {
    fn run(self) -> anyhow::Result<()> {
        let log_dir = resolve_log_dir(self.log_dir.clone(), self.bundle.as_ref());
        let state_dir = resolve_state_dir(None, self.bundle.as_ref());
        if demo_debug_enabled() {
            println!(
                "[demo] logs log_dir={} tenant={} team={} service={} tail={}",
                log_dir.display(),
                self.tenant,
                self.team,
                self.service,
                self.tail
            );
        }
        demo::demo_logs_runtime(
            &state_dir,
            &log_dir,
            &self.tenant,
            &self.team,
            &self.service,
            self.tail,
        )
    }
}

impl DemoDoctorArgs {
    fn run(self, _ctx: &AppCtx) -> anyhow::Result<()> {
        let config = config::load_operator_config(&self.bundle)?;
        let explicit = config::binary_override(config.as_ref(), "greentic-pack", &self.bundle);
        let pack_command = bin_resolver::resolve_binary(
            "greentic-pack",
            &ResolveCtx {
                config_dir: self.bundle.clone(),
                explicit_path: explicit,
            },
        )?;
        if demo_debug_enabled() {
            println!(
                "[demo] doctor bundle={} greentic-pack={}",
                self.bundle.display(),
                pack_command.display()
            );
        }
        demo::demo_doctor(&self.bundle, &pack_command)
    }
}

fn project_root(arg: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    Ok(arg.unwrap_or(env::current_dir()?))
}

fn resolve_state_dir(state_dir: Option<PathBuf>, bundle: Option<&PathBuf>) -> PathBuf {
    if let Some(state_dir) = state_dir {
        return state_dir;
    }
    if let Some(bundle) = bundle {
        return bundle.join("state");
    }
    PathBuf::from("state")
}

fn resolve_log_dir(log_dir: Option<PathBuf>, bundle: Option<&PathBuf>) -> PathBuf {
    if let Some(path) = log_dir {
        return path;
    }
    if let Some(bundle) = bundle {
        return bundle.join("logs");
    }
    PathBuf::from("logs")
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn resolves_explicit_log_dir() {
        let dir = PathBuf::from("/tmp/logs");
        assert_eq!(resolve_log_dir(Some(dir.clone()), None), dir);
    }

    #[test]
    fn resolves_bundle_log_dir() {
        let bundle = PathBuf::from("/tmp/bundle");
        assert_eq!(resolve_log_dir(None, Some(&bundle)), bundle.join("logs"));
    }

    #[test]
    fn resolves_default_log_dir() {
        assert_eq!(resolve_log_dir(None, None), PathBuf::from("logs"));
    }
}

fn demo_debug_enabled() -> bool {
    matches!(
        std::env::var("GREENTIC_OPERATOR_DEMO_DEBUG").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

#[allow(clippy::too_many_arguments)]
fn run_demo_up_setup(
    bundle: &Path,
    domains: &[Domain],
    setup_input: &Path,
    tenant_override: Option<String>,
    team_override: Option<String>,
    env: &str,
    runner_binary: Option<PathBuf>,
    public_base_url: Option<String>,
    secrets_manager: Option<DynSecretsManager>,
) -> anyhow::Result<()> {
    let providers_input = ProvidersInput::load(setup_input)?;
    for domain in domains {
        let provider_map = match providers_input.providers_for_domain(*domain) {
            Some(map) if !map.is_empty() => map,
            _ => {
                println!(
                    "[demo] no providers configured for domain {}; skipping provider setup",
                    domains::domain_name(*domain)
                );
                continue;
            }
        };
        let tenants = if let Some(tenant) = tenant_override.as_ref() {
            vec![tenant.clone()]
        } else {
            let discovered = discover_tenants(bundle, *domain)?;
            if discovered.is_empty() {
                println!(
                    "[demo] no tenants discovered for domain {}; skipping",
                    domains::domain_name(*domain)
                );
                operator_log::warn(
                    module_path!(),
                    format!(
                        "no tenants discovered for domain {}; skipping setup",
                        domains::domain_name(*domain)
                    ),
                );
                continue;
            }
            discovered
        };
        let provider_keys: BTreeSet<String> = provider_map.keys().cloned().collect();
        let mut map = serde_json::Map::new();
        for (provider, value) in provider_map {
            map.insert(provider.clone(), value.clone());
        }
        let setup_answers =
            SetupInputAnswers::new(serde_json::Value::Object(map), provider_keys.clone())?;
        for tenant in tenants {
            run_domain_command(DomainRunArgs {
                root: bundle.to_path_buf(),
                state_root: None,
                domain: *domain,
                action: DomainAction::Setup,
                tenant,
                team: team_override.clone(),
                provider_filter: None,
                dry_run: false,
                format: PlanFormat::Text,
                parallel: 1,
                allow_missing_setup: true,
                allow_contract_change: false,
                backup: false,
                online: false,
                secrets_env: Some(env.to_string()),
                runner_binary: runner_binary.clone(),
                best_effort: false,
                discovered_providers: None,
                setup_input: None,
                allowed_providers: Some(provider_keys.clone()),
                preloaded_setup_answers: Some(setup_answers.clone()),
                public_base_url: public_base_url.clone(),
                secrets_manager: secrets_manager.clone(),
            })?;
        }
    }
    Ok(())
}

fn demo_provider_files(
    root: &Path,
    tenant: &str,
    team: Option<&str>,
    domain: Domain,
) -> anyhow::Result<Option<std::collections::BTreeSet<String>>> {
    let resolved = demo_resolved_manifest_path(root, tenant, team);
    if !resolved.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(resolved)?;
    let manifest: DemoResolvedManifest = serde_yaml_bw::from_str(&contents)?;
    let key = match domain {
        Domain::Messaging => "messaging",
        Domain::Events => "events",
        Domain::Secrets => "secrets",
    };
    let Some(list) = manifest.providers.get(key) else {
        return Ok(Some(std::collections::BTreeSet::new()));
    };
    let mut files = std::collections::BTreeSet::new();
    for path in list {
        if let Some(name) = Path::new(path).file_name().and_then(|value| value.to_str()) {
            files.insert(name.to_string());
        }
    }
    Ok(Some(files))
}

fn demo_resolved_manifest_path(root: &Path, tenant: &str, team: Option<&str>) -> PathBuf {
    root.join("resolved")
        .join(resolved_manifest_filename(tenant, team))
}

fn demo_state_resolved_manifest_path(root: &Path, tenant: &str, team: Option<&str>) -> PathBuf {
    root.join("state")
        .join("resolved")
        .join(resolved_manifest_filename(tenant, team))
}

fn resolved_manifest_filename(tenant: &str, team: Option<&str>) -> String {
    match team {
        Some(team) => format!("{tenant}.{team}.yaml"),
        None => format!("{tenant}.yaml"),
    }
}

fn demo_bundle_gmap_path(bundle: &Path, tenant: &str, team: Option<&str>) -> PathBuf {
    let mut path = bundle.join("tenants").join(tenant);
    if let Some(team) = team {
        path = path.join("teams").join(team).join("team.gmap");
    } else {
        path = path.join("tenant.gmap");
    }
    path
}

fn copy_resolved_manifest(bundle: &Path, tenant: &str, team: Option<&str>) -> anyhow::Result<()> {
    let src = demo_state_resolved_manifest_path(bundle, tenant, team);
    if !src.exists() {
        return Err(anyhow::anyhow!(
            "resolved manifest not found at {}",
            src.display()
        ));
    }
    let dst = demo_resolved_manifest_path(bundle, tenant, team);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dst)?;
    Ok(())
}

pub(crate) fn discovery_map(
    providers: &[discovery::DetectedProvider],
) -> std::collections::BTreeMap<PathBuf, discovery::DetectedProvider> {
    let mut map = std::collections::BTreeMap::new();
    for provider in providers {
        map.insert(provider.pack_path.clone(), provider.clone());
    }
    map
}

fn provider_filter_matches(pack: &domains::ProviderPack, filter: &str) -> bool {
    let file_stem = pack
        .file_name
        .strip_suffix(".gtpack")
        .unwrap_or(&pack.file_name);
    pack.pack_id == filter
        || pack.file_name == filter
        || file_stem == filter
        || pack.pack_id.contains(filter)
        || pack.file_name.contains(filter)
        || file_stem.contains(filter)
}

pub fn demo_provider_packs(
    bundle: &Path,
    domain: Domain,
) -> anyhow::Result<Vec<domains::ProviderPack>> {
    let is_demo_bundle = bundle.join("greentic.demo.yaml").exists();
    if is_demo_bundle {
        domains::discover_provider_packs_cbor_only(bundle, domain)
    } else {
        domains::discover_provider_packs(bundle, domain)
    }
}

pub fn demo_provider_pack_by_filter(
    bundle: &Path,
    domain: Domain,
    filter: &str,
) -> anyhow::Result<domains::ProviderPack> {
    let mut packs = demo_provider_packs(bundle, domain)?;
    packs.retain(|pack| provider_filter_matches(pack, filter));
    if packs.is_empty() {
        return Err(anyhow::anyhow!(
            "no provider pack matched {} in {}",
            filter,
            domains::domain_name(domain)
        ));
    }
    packs.sort_by(|a, b| a.path.cmp(&b.path));
    if packs.len() > 1 {
        let names = packs
            .iter()
            .map(|pack| pack.file_name.clone())
            .collect::<Vec<_>>();
        return Err(anyhow::anyhow!(
            "multiple provider packs matched {}; specify a more precise --pack: {}",
            filter,
            names.join(", ")
        ));
    }
    Ok(packs.remove(0))
}

pub(crate) fn resolve_demo_provider_pack(
    root: &Path,
    tenant: &str,
    team: Option<&str>,
    provider: &str,
    domain: Domain,
) -> anyhow::Result<domains::ProviderPack> {
    let is_demo_bundle = root.join("greentic.demo.yaml").exists();
    let mut packs = if is_demo_bundle {
        domains::discover_provider_packs_cbor_only(root, domain)?
    } else {
        domains::discover_provider_packs(root, domain)?
    };
    if is_demo_bundle && let Some(allowed) = demo_provider_files(root, tenant, team, domain)? {
        packs.retain(|pack| allowed.contains(&pack.file_name));
    }
    packs.retain(|pack| provider_filter_matches(pack, provider));
    if packs.is_empty() {
        return Err(anyhow::anyhow!(
            "No provider packs matched. Try --provider <pack_id>."
        ));
    }
    packs.sort_by(|a, b| a.path.cmp(&b.path));
    if packs.len() > 1 {
        let names = packs
            .iter()
            .map(|pack| pack.file_name.clone())
            .collect::<Vec<_>>();
        return Err(anyhow::anyhow!(
            "Multiple provider packs matched: {}. Use a more specific --provider.",
            names.join(", ")
        ));
    }
    Ok(packs.remove(0))
}

fn ensure_requirements_flow(pack: &domains::ProviderPack) -> Result<(), String> {
    if pack.entry_flows.iter().any(|flow| flow == "requirements") {
        return Ok(());
    }
    Err(
        "requirements flow not found in provider pack; ask the provider pack to include an entry flow named 'requirements'."
            .to_string(),
    )
}

#[derive(serde::Deserialize)]
struct DemoResolvedManifest {
    #[serde(default)]
    providers: std::collections::BTreeMap<String, Vec<String>>,
}

impl From<WizardModeArg> for wizard::WizardMode {
    fn from(value: WizardModeArg) -> Self {
        match value {
            WizardModeArg::Create => wizard::WizardMode::Create,
            WizardModeArg::Update => wizard::WizardMode::Update,
            WizardModeArg::Remove => wizard::WizardMode::Remove,
        }
    }
}

struct DomainRunArgs {
    root: PathBuf,
    state_root: Option<PathBuf>,
    domain: Domain,
    action: DomainAction,
    tenant: String,
    team: Option<String>,
    provider_filter: Option<String>,
    dry_run: bool,
    format: PlanFormat,
    parallel: usize,
    allow_missing_setup: bool,
    allow_contract_change: bool,
    backup: bool,
    online: bool,
    secrets_env: Option<String>,
    runner_binary: Option<PathBuf>,
    best_effort: bool,
    discovered_providers: Option<Vec<discovery::DetectedProvider>>,
    setup_input: Option<PathBuf>,
    allowed_providers: Option<BTreeSet<String>>,
    preloaded_setup_answers: Option<SetupInputAnswers>,
    public_base_url: Option<String>,
    secrets_manager: Option<DynSecretsManager>,
}

fn run_domain_command(args: DomainRunArgs) -> anyhow::Result<()> {
    let is_demo_bundle = args.root.join("greentic.demo.yaml").exists();
    let mut packs = if is_demo_bundle {
        domains::discover_provider_packs_cbor_only(&args.root, args.domain)?
    } else {
        domains::discover_provider_packs(&args.root, args.domain)?
    };
    let provider_map = args.discovered_providers.as_ref().map(|providers| {
        let mut map = std::collections::BTreeMap::new();
        for provider in providers {
            map.insert(provider.pack_path.clone(), provider.clone());
        }
        map
    });
    if let Some(provider_map) = provider_map.as_ref() {
        packs.retain(|pack| provider_map.contains_key(&pack.path));
        packs.sort_by(|a, b| a.path.cmp(&b.path));
    }
    if is_demo_bundle
        && let Some(allowed) =
            demo_provider_files(&args.root, &args.tenant, args.team.as_deref(), args.domain)?
    {
        packs.retain(|pack| allowed.contains(&pack.file_name));
    }
    if args.action == DomainAction::Setup {
        let setup_flow = domains::config(args.domain).setup_flow;
        let missing: Vec<String> = packs
            .iter()
            .filter(|pack| !pack.entry_flows.iter().any(|flow| flow == setup_flow))
            .map(|pack| pack.file_name.clone())
            .collect();
        if !missing.is_empty() && !args.allow_missing_setup {
            if args.best_effort {
                println!(
                    "{}",
                    operator_i18n::trf(
                        "cli.domain.best_effort_skipped_missing_setup",
                        "Best-effort: skipped {} pack(s) missing {}.",
                        &[&missing.len().to_string(), setup_flow]
                    )
                );
                packs.retain(|pack| pack.entry_flows.iter().any(|flow| flow == setup_flow));
            } else {
                return Err(anyhow::anyhow!(
                    "missing {setup_flow} in packs: {}",
                    missing.join(", ")
                ));
            }
        }
    }
    if packs.is_empty() {
        return Ok(());
    }
    if let Some(allowed) = args.allowed_providers.as_ref() {
        let missing = filter_packs_by_allowed(&mut packs, allowed);
        if !missing.is_empty() {
            println!(
                "{}",
                operator_i18n::trf(
                    "cli.domain.warn_skip_missing_packs",
                    "[warn] skip setup domain={} missing packs: {}",
                    &[domains::domain_name(args.domain), &missing.join(", ")]
                )
            );
            operator_log::warn(
                module_path!(),
                format!(
                    "provider filter domain={} removed packs: {}",
                    domains::domain_name(args.domain),
                    missing.join(", ")
                ),
            );
        }
    }
    operator_log::info(
        module_path!(),
        format!(
            "provider selection domain={} packs={}",
            domains::domain_name(args.domain),
            packs.len()
        ),
    );
    let setup_answers = if let Some(preloaded) = args.preloaded_setup_answers.clone() {
        Some(preloaded)
    } else if let Some(path) = args.setup_input.as_ref() {
        let provider_keys: BTreeSet<String> =
            packs.iter().map(|pack| pack.pack_id.clone()).collect();
        Some(SetupInputAnswers::new(
            load_setup_input(path)?,
            provider_keys,
        )?)
    } else {
        None
    };
    let interactive = args.setup_input.is_none();
    let plan = domains::plan_runs(
        args.domain,
        args.action,
        &packs,
        args.provider_filter.as_deref(),
        args.allow_missing_setup,
    )?;

    operator_log::info(
        module_path!(),
        format!(
            "plan domain={} action={:?} items={}",
            domains::domain_name(args.domain),
            args.action,
            plan.len()
        ),
    );
    for item in &plan {
        operator_log::debug(
            module_path!(),
            format!(
                "plan item domain={} pack={} flow={}",
                domains::domain_name(args.domain),
                item.pack.file_name,
                item.flow_id
            ),
        );
    }

    if plan.is_empty() {
        if is_demo_bundle {
            println!(
                "{}",
                operator_i18n::tr(
                    "cli.domain.no_provider_packs_matched",
                    "No provider packs matched. Try --provider <pack_id>."
                )
            );
        } else {
            println!(
                "{}",
                operator_i18n::tr(
                    "cli.domain.no_provider_packs_matched_or_project_root",
                    "No provider packs matched. Try --provider <pack_id> or --project-root."
                )
            );
        }
        operator_log::warn(
            module_path!(),
            format!(
                "no provider packs matched domain={} action={:?}",
                domains::domain_name(args.domain),
                args.action
            ),
        );
        return Ok(());
    }

    if args.dry_run {
        render_plan(&plan, args.format)?;
        return Ok(());
    }

    let runner_binary = resolve_demo_runner_binary(&args.root, args.runner_binary)?;
    let dist_offline = !args.online;
    let state_root = args.state_root.as_ref().unwrap_or(&args.root);
    run_plan(
        &args.root,
        state_root,
        args.domain,
        args.action,
        &args.tenant,
        args.team.as_deref(),
        plan,
        args.parallel,
        dist_offline,
        args.allow_contract_change,
        args.backup,
        args.secrets_env.as_deref(),
        runner_binary,
        args.best_effort,
        provider_map,
        setup_answers,
        interactive,
        args.public_base_url.clone(),
        args.secrets_manager.clone(),
    )
}

fn filter_packs_by_allowed(
    packs: &mut Vec<domains::ProviderPack>,
    allowed: &BTreeSet<String>,
) -> Vec<String> {
    let mut seen = BTreeSet::new();
    packs.retain(|pack| {
        if allowed.contains(&pack.pack_id) {
            seen.insert(pack.pack_id.clone());
            true
        } else {
            false
        }
    });
    allowed
        .iter()
        .filter(|value| !seen.contains(*value))
        .cloned()
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn run_plan(
    root: &Path,
    state_root: &Path,
    domain: Domain,
    action: DomainAction,
    tenant: &str,
    team: Option<&str>,
    plan: Vec<domains::PlannedRun>,
    parallel: usize,
    dist_offline: bool,
    allow_contract_change: bool,
    backup: bool,
    secrets_env: Option<&str>,
    runner_binary: Option<PathBuf>,
    best_effort: bool,
    provider_map: Option<std::collections::BTreeMap<PathBuf, discovery::DetectedProvider>>,
    setup_answers: Option<SetupInputAnswers>,
    interactive: bool,
    public_base_url: Option<String>,
    secrets_manager: Option<DynSecretsManager>,
) -> anyhow::Result<()> {
    let setup_answers = setup_answers.map(Arc::new);
    let plan_public_base_url = public_base_url.map(Arc::new);
    let plan_secrets_manager = secrets_manager;
    if parallel <= 1 {
        let mut errors = Vec::new();
        for item in plan {
            let result = run_plan_item(
                root,
                state_root,
                domain,
                action,
                tenant,
                team,
                &item,
                dist_offline,
                allow_contract_change,
                backup,
                secrets_env,
                runner_binary.as_deref(),
                setup_answers.as_deref(),
                provider_map.as_ref(),
                interactive,
                plan_public_base_url.clone(),
                plan_secrets_manager.clone(),
            );
            if let Err(err) = result {
                if best_effort {
                    errors.push(err);
                } else {
                    return Err(err);
                }
            }
        }
        if best_effort && !errors.is_empty() {
            println!(
                "{}",
                operator_i18n::trf(
                    "cli.domain.best_effort_flows_failed",
                    "Best-effort: {} flow(s) failed.",
                    &[&errors.len().to_string()]
                )
            );
            return Ok(());
        }
        return Ok(());
    }

    let mut handles = Vec::new();
    let plan = std::sync::Arc::new(std::sync::Mutex::new(plan));
    let errors = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    for _ in 0..parallel {
        let plan = plan.clone();
        let errors = errors.clone();
        let root = root.to_path_buf();
        let state_root = state_root.to_path_buf();
        let tenant = tenant.to_string();
        let team = team.map(|value| value.to_string());
        let secrets_env = secrets_env.map(|value| value.to_string());
        let runner_binary = runner_binary.clone();
        let provider_map = provider_map.clone();
        let setup_answers = setup_answers.clone();
        let interactive_flag = interactive;
        let thread_public_base_url = plan_public_base_url.clone();
        let thread_secrets_manager = plan_secrets_manager.clone();
        handles.push(std::thread::spawn(move || {
            loop {
                let next = {
                    let mut queue = plan.lock().unwrap();
                    queue.pop()
                };
                let Some(item) = next else {
                    break;
                };
                let result = run_plan_item(
                    &root,
                    &state_root,
                    domain,
                    action,
                    &tenant,
                    team.as_deref(),
                    &item,
                    dist_offline,
                    allow_contract_change,
                    backup,
                    secrets_env.as_deref(),
                    runner_binary.as_deref(),
                    setup_answers.as_deref(),
                    provider_map.as_ref(),
                    interactive_flag,
                    thread_public_base_url.clone(),
                    thread_secrets_manager.clone(),
                );
                if let Err(err) = result {
                    errors.lock().unwrap().push(err);
                }
            }
        }));
    }

    for handle in handles {
        let _ = handle.join();
    }

    let errors = errors.lock().unwrap();
    if !errors.is_empty() {
        if best_effort {
            println!(
                "{}",
                operator_i18n::trf(
                    "cli.domain.best_effort_flows_failed",
                    "Best-effort: {} flow(s) failed.",
                    &[&errors.len().to_string()]
                )
            );
            return Ok(());
        }
        return Err(anyhow::anyhow!("{} flow(s) failed.", errors.len()));
    }
    Ok(())
}

fn render_plan(plan: &[domains::PlannedRun], format: PlanFormat) -> anyhow::Result<()> {
    match format {
        PlanFormat::Text => {
            println!("{}", operator_i18n::tr("cli.domain.plan_header", "Plan:"));
            for item in plan {
                println!(
                    "{}",
                    operator_i18n::trf(
                        "cli.domain.plan_item",
                        "  {} -> {}",
                        &[&item.pack.file_name, &item.flow_id]
                    )
                );
            }
            Ok(())
        }
        PlanFormat::Json => {
            let json = serde_json::to_string_pretty(plan)?;
            println!("{json}");
            Ok(())
        }
        PlanFormat::Yaml => {
            let yaml = serde_yaml_bw::to_string(plan)?;
            print!("{yaml}");
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_plan_item(
    root: &Path,
    state_root: &Path,
    domain: Domain,
    action: DomainAction,
    tenant: &str,
    team: Option<&str>,
    item: &domains::PlannedRun,
    dist_offline: bool,
    allow_contract_change: bool,
    backup: bool,
    secrets_env: Option<&str>,
    runner_binary: Option<&Path>,
    setup_answers: Option<&SetupInputAnswers>,
    provider_map: Option<&std::collections::BTreeMap<PathBuf, discovery::DetectedProvider>>,
    interactive: bool,
    public_base_url: Option<Arc<String>>,
    secrets_manager: Option<DynSecretsManager>,
) -> anyhow::Result<()> {
    let provider_id = provider_id_for_pack(&item.pack.path, &item.pack.pack_id, provider_map);
    let env_value = resolve_env(secrets_env);

    if domain == Domain::Messaging
        && action == DomainAction::Setup
        && let Some(manager) = secrets_manager.as_ref()
    {
        match secrets_gate::check_provider_secrets(
            manager,
            &env_value,
            tenant,
            team,
            &item.pack.path,
            &provider_id,
            None,
            None,
            false,
        ) {
            Ok(Some(missing)) => {
                let formatted = missing
                    .iter()
                    .map(|entry| format!("  - {entry}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                println!(
                    "{}",
                    operator_i18n::trf(
                        "cli.plan.warn_skip_missing_secrets",
                        "[warn] skip setup domain={} tenant={} provider={}: missing secrets:\n{}",
                        &[
                            domains::domain_name(domain),
                            tenant,
                            &provider_id,
                            &formatted
                        ]
                    )
                );
                return Ok(());
            }
            Ok(None) => {}
            Err(err) => {
                println!(
                    "{}",
                    operator_i18n::trf(
                        "cli.plan.warn_skip_secrets_check_failed",
                        "[warn] skip setup domain={} tenant={} provider={}: secrets check failed: {}",
                        &[
                            domains::domain_name(domain),
                            tenant,
                            &provider_id,
                            &err.to_string()
                        ]
                    )
                );
                return Ok(());
            }
        }
    }

    let (setup_values, qa_form_spec) = if action == DomainAction::Setup {
        let (answers, form_spec) = qa_setup_wizard::run_qa_setup(
            &item.pack.path,
            &item.pack.pack_id,
            setup_answers,
            interactive,
            None, // no pre-built FormSpec; will try setup.yaml fallback
        )?;
        (Some(answers), form_spec)
    } else {
        (None, None)
    };
    let providers_root = state_root
        .join("state")
        .join("runtime")
        .join(tenant)
        .join("providers");
    if let Err(err) = crate::provider_config_envelope::ensure_contract_compatible(
        &providers_root,
        &provider_id,
        &item.flow_id,
        &item.pack.path,
        allow_contract_change,
    ) {
        operator_log::error(module_path!(), err.to_string());
        return Err(err);
    }
    let current_config = crate::provider_config_envelope::read_provider_config_envelope(
        &providers_root,
        &provider_id,
    )?
    .map(|envelope| envelope.config);
    let qa_mode = if action == DomainAction::Setup {
        Some(crate::component_qa_ops::QaMode::Setup)
    } else {
        crate::component_qa_ops::qa_mode_for_flow(&item.flow_id)
    };
    let qa_answers = if action == DomainAction::Setup {
        setup_values.clone().unwrap_or_else(|| json!({}))
    } else {
        json!({})
    };
    let qa_config_override = if let Some(mode) = qa_mode {
        if let Err(err) = crate::component_qa_ops::persist_answers_artifacts(
            &providers_root,
            &provider_id,
            mode,
            &qa_answers,
        ) {
            operator_log::warn(
                module_path!(),
                format!(
                    "failed to persist qa answers provider={} mode={} flow={}: {err}",
                    provider_id,
                    mode.as_str(),
                    item.flow_id
                ),
            );
        }
        match crate::component_qa_ops::apply_answers_via_component_qa(
            root,
            domain,
            tenant,
            team,
            &item.pack,
            &provider_id,
            mode,
            current_config.as_ref(),
            &qa_answers,
        ) {
            Ok(value) => value,
            Err(diag) => {
                operator_log::error(
                    module_path!(),
                    format!(
                        "component qa failed provider={} flow={} code={} message={}",
                        provider_id,
                        item.flow_id,
                        diag.code.as_str(),
                        diag.message
                    ),
                );
                return Err(anyhow::anyhow!("{diag}"));
            }
        }
    } else {
        None
    };

    // Persist secrets and config from QA results when FormSpec is available
    if let Some(ref config) = qa_config_override
        && let Some(ref form_spec) = qa_form_spec
        && action == DomainAction::Setup
        && let Err(err) = crate::qa_persist::persist_qa_config(
            &providers_root,
            &provider_id,
            config,
            &item.pack.path,
            form_spec,
            backup,
        )
    {
        operator_log::warn(
            module_path!(),
            format!(
                "failed to persist qa config provider={}: {err}",
                provider_id
            ),
        );
    }

    let public_base_url_ref = public_base_url.as_deref().map(|value| value.as_str());
    let mut input = build_input_payload(
        state_root,
        domain,
        tenant,
        team,
        Some(&item.pack.pack_id),
        setup_values.as_ref(),
        public_base_url_ref,
        &env_value,
    );
    if let Some(config) = qa_config_override.as_ref() {
        input["config"] = config.clone();
    }
    if demo_debug_enabled() {
        println!(
            "[demo] setup input pack={} flow={} input={}",
            item.pack.file_name,
            item.flow_id,
            serde_json::to_string(&input).unwrap_or_else(|_| "<invalid-json>".to_string())
        );
    }
    if action == DomainAction::Setup
        && let Some(config_value) = qa_config_override.as_ref()
    {
        let setup_path = providers_root.join(format!("{provider_id}.setup.json"));
        crate::providers::write_qa_setup_success_record(
            &setup_path,
            &provider_id,
            &item.flow_id,
            Some(config_value),
        )?;
        if let Err(err) = crate::provider_config_envelope::write_provider_config_envelope(
            &providers_root,
            &provider_id,
            &item.flow_id,
            config_value,
            &item.pack.path,
            backup,
        ) {
            operator_log::warn(
                module_path!(),
                format!(
                    "failed to write provider config envelope provider={} flow={}: {err}",
                    provider_id, item.flow_id
                ),
            );
        }
        println!(
            "{} {} -> Success (component-qa)",
            item.pack.file_name, item.flow_id
        );
        return Ok(());
    }
    if let Some(runner_binary) = runner_binary {
        let run_dir = state_layout::run_dir(state_root, domain, &item.pack.pack_id, &item.flow_id)?;
        std::fs::create_dir_all(&run_dir)?;
        let input_path = run_dir.join("input.json");
        let input_json = serde_json::to_string_pretty(&input)?;
        std::fs::write(&input_path, input_json)?;

        let runner_flavor = runner_integration::detect_runner_flavor(runner_binary);
        let output = runner_integration::run_flow_with_options(
            runner_binary,
            &item.pack.path,
            &item.flow_id,
            &input,
            runner_integration::RunFlowOptions {
                dist_offline,
                tenant: Some(tenant),
                team,
                artifacts_dir: Some(&run_dir),
                runner_flavor,
            },
        )?;
        write_runner_cli_artifacts(&run_dir, &output)?;
        if action == DomainAction::Setup {
            let setup_path = providers_root.join(format!("{provider_id}.setup.json"));
            crate::providers::write_run_output(&setup_path, &provider_id, &item.flow_id, &output)?;
            if let Some(config_value) = qa_config_override
                .clone()
                .or_else(|| extract_config_for_envelope(output.parsed.as_ref()))
                && let Err(err) = crate::provider_config_envelope::write_provider_config_envelope(
                    &providers_root,
                    &provider_id,
                    &item.flow_id,
                    &config_value,
                    &item.pack.path,
                    backup,
                )
            {
                operator_log::warn(
                    module_path!(),
                    format!(
                        "failed to write provider config envelope provider={} flow={}: {err}",
                        provider_id, item.flow_id
                    ),
                );
            }
        }
        let exit = format_runner_exit(&output);
        if output.status.success() {
            println!(
                "{}",
                operator_i18n::trf(
                    "cli.plan.item_result_ok",
                    "{} {} -> {}",
                    &[&item.pack.file_name, &item.flow_id, &exit]
                )
            );
        } else if let Some(summary) = summarize_runner_error(&output) {
            println!(
                "{}",
                operator_i18n::trf(
                    "cli.plan.item_result_error_with_summary",
                    "{} {} -> {} ({})",
                    &[&item.pack.file_name, &item.flow_id, &exit, &summary]
                )
            );
        } else {
            println!(
                "{}",
                operator_i18n::trf(
                    "cli.plan.item_result_error",
                    "{} {} -> {}",
                    &[&item.pack.file_name, &item.flow_id, &exit]
                )
            );
        }
    } else {
        let output = runner_exec::run_provider_pack_flow(runner_exec::RunRequest {
            root: state_root.to_path_buf(),
            domain,
            pack_path: item.pack.path.clone(),
            pack_label: item.pack.pack_id.clone(),
            flow_id: item.flow_id.clone(),
            tenant: tenant.to_string(),
            team: team.map(|value| value.to_string()),
            input,
            dist_offline,
        })
        .map_err(|err| {
            let message = err.to_string();
            if message.contains("manifest.cbor is invalid") {
                if let Ok(Some(detail)) = domains::manifest_cbor_issue_detail(&item.pack.path) {
                    return anyhow::anyhow!(
                        "pack verification failed for {}: {}",
                        item.pack.path.display(),
                        detail
                    );
                }
                return anyhow::anyhow!(
                    "pack verification failed for {}: {message}",
                    item.pack.path.display()
                );
            }
            err
        })?;
        if action == DomainAction::Setup {
            let setup_path = providers_root.join(format!("{provider_id}.setup.json"));
            crate::providers::write_run_result(
                &setup_path,
                &provider_id,
                &item.flow_id,
                &output.result,
            )?;
            if let Some(config_value) = qa_config_override.clone().or_else(|| {
                extract_config_for_envelope(serde_json::to_value(&output.result).ok().as_ref())
            }) && let Err(err) = crate::provider_config_envelope::write_provider_config_envelope(
                &providers_root,
                &provider_id,
                &item.flow_id,
                &config_value,
                &item.pack.path,
                backup,
            ) {
                operator_log::warn(
                    module_path!(),
                    format!(
                        "failed to write provider config envelope provider={} flow={}: {err}",
                        provider_id, item.flow_id
                    ),
                );
            }
        }
        println!(
            "{} {} -> {:?}",
            item.pack.file_name, item.flow_id, output.result.status
        );
    }

    Ok(())
}

fn resolve_demo_runner_binary(
    config_dir: &Path,
    runner_binary: Option<PathBuf>,
) -> anyhow::Result<Option<PathBuf>> {
    let Some(runner_binary) = runner_binary else {
        return Ok(None);
    };
    let runner_str = runner_binary.to_string_lossy();
    let (name, explicit) = if looks_like_path_str(&runner_str) {
        let name = runner_binary
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("greentic-runner")
            .to_string();
        (name, Some(runner_binary))
    } else {
        (runner_str.to_string(), None)
    };
    let resolved = bin_resolver::resolve_binary(
        &name,
        &ResolveCtx {
            config_dir: config_dir.to_path_buf(),
            explicit_path: explicit,
        },
    )?;
    Ok(Some(resolved))
}

fn write_runner_cli_artifacts(
    run_dir: &Path,
    output: &runner_integration::RunnerOutput,
) -> anyhow::Result<()> {
    let run_json = run_dir.join("run.json");
    let summary_path = run_dir.join("summary.txt");
    let stdout_path = run_dir.join("stdout.txt");
    let stderr_path = run_dir.join("stderr.txt");

    let json = serde_json::json!({
        "status": {
            "success": output.status.success(),
            "code": output.status.code(),
        },
        "stdout": output.stdout,
        "stderr": output.stderr,
        "parsed": output.parsed,
    });
    let json = serde_json::to_string_pretty(&json)?;
    std::fs::write(run_json, json)?;
    std::fs::write(stdout_path, &output.stdout)?;
    std::fs::write(stderr_path, &output.stderr)?;

    let summary = format!(
        "success: {}\nexit_code: {}\n",
        output.status.success(),
        output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string())
    );
    std::fs::write(summary_path, summary)?;
    Ok(())
}

fn format_runner_exit(output: &runner_integration::RunnerOutput) -> String {
    if let Some(code) = output.status.code() {
        return format!("exit={code}");
    }
    if output.status.success() {
        return "exit=0".to_string();
    }
    "exit=signal".to_string()
}

fn summarize_runner_error(output: &runner_integration::RunnerOutput) -> Option<String> {
    output
        .stderr
        .lines()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
        .map(|line| line.to_string())
}

fn extract_config_for_envelope(parsed: Option<&JsonValue>) -> Option<JsonValue> {
    let value = parsed?;
    if let Some(config) = value.get("config") {
        return Some(config.clone());
    }
    Some(value.clone())
}

pub(crate) fn provider_id_for_pack(
    pack_path: &Path,
    fallback: &str,
    provider_map: Option<&std::collections::BTreeMap<PathBuf, discovery::DetectedProvider>>,
) -> String {
    provider_map
        .and_then(|map| map.get(pack_path))
        .map(|provider| provider.provider_id.clone())
        .unwrap_or_else(|| fallback.to_string())
}

fn looks_like_path_str(value: &str) -> bool {
    value.contains('/') || value.contains('\\') || Path::new(value).is_absolute()
}

#[allow(clippy::too_many_arguments)]
fn build_input_payload(
    root: &Path,
    domain: Domain,
    tenant: &str,
    team: Option<&str>,
    pack_id: Option<&str>,
    setup_answers: Option<&serde_json::Value>,
    public_base_url: Option<&str>,
    env: &str,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "tenant": tenant,
    });
    if let Some(team) = team {
        payload["team"] = serde_json::Value::String(team.to_string());
    }

    let resolved_public_base_url = public_base_url.map(|value| value.to_string()).or_else(|| {
        if matches!(domain, Domain::Messaging | Domain::Events) {
            read_public_base_url(root, tenant, team)
        } else {
            None
        }
    });

    if matches!(domain, Domain::Messaging | Domain::Events) {
        let mut config = serde_json::json!({});
        if let Some(url) = resolved_public_base_url.as_ref() {
            payload["public_base_url"] = serde_json::Value::String(url.clone());
            config["public_base_url"] = serde_json::Value::String(url.clone());
        }
        payload["config"] = config;
    }

    if let Some(pack_id) = pack_id
        && let Some(config_map) = payload
            .get_mut("config")
            .and_then(|value| value.as_object_mut())
    {
        config_map.insert(
            "id".to_string(),
            serde_json::Value::String(pack_id.to_string()),
        );
    }
    if let Some(pack_id) = pack_id {
        payload["id"] = serde_json::Value::String(pack_id.to_string());
    }
    if let Some(answers) = setup_answers {
        payload["setup_answers"] = answers.clone();
        if let Ok(json) = serde_json::to_string(answers) {
            payload["answers_json"] = serde_json::Value::String(json);
        }
    }
    let mut tenant_ctx = serde_json::json!({
        "env": env,
        "tenant": tenant,
        "tenant_id": tenant,
    });
    if let Some(team) = team {
        tenant_ctx["team"] = serde_json::Value::String(team.to_string());
        tenant_ctx["team_id"] = serde_json::Value::String(team.to_string());
    }
    let msg_id = pack_id
        .map(|value| format!("{value}.setup"))
        .unwrap_or_else(|| "setup".to_string());
    let mut metadata = serde_json::json!({});
    if let Some(url) = resolved_public_base_url {
        metadata["public_base_url"] = serde_json::Value::String(url);
    }
    let msg = serde_json::json!({
        "id": msg_id,
        "tenant": tenant_ctx,
        "channel": "setup",
        "message": {
            "id": pack_id
                .map(|value| format!("{value}.setup_default__collect"))
                .unwrap_or_else(|| "setup_default__collect".to_string()),
            "text": "Collect inputs for setup_default."
        },
        "session_id": "setup",
        "metadata": metadata,
        "reply_scope": "",
        "text": "Collect inputs for setup_default.",
        "user_id": "operator",
    });
    payload["msg"] = msg;
    let payload_id = pack_id
        .map(|value| format!("{value}-setup_default"))
        .unwrap_or_else(|| "setup_default".to_string());
    payload["payload"] = serde_json::json!({
        "id": payload_id,
        "spec_ref": "assets/setup.yaml"
    });
    payload
}

fn read_public_base_url(root: &Path, tenant: &str, team: Option<&str>) -> Option<String> {
    let team_id = team.unwrap_or("default");
    let paths = crate::runtime_state::RuntimePaths::new(root.join("state"), tenant, team_id);
    let path = crate::cloudflared::public_url_path(&paths);
    let contents = std::fs::read_to_string(path).ok()?;
    crate::cloudflared::parse_public_url(&contents)
        .or_else(|| crate::ngrok::parse_public_url(&contents))
}

fn parse_kv(input: &str) -> anyhow::Result<(String, JsonValue)> {
    let mut parts = input.splitn(2, '=');
    let key = parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("expected key=value"))?;
    let value = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("expected key=value"))?
        .trim();
    if value.eq_ignore_ascii_case("true") {
        return Ok((key.to_string(), JsonValue::Bool(true)));
    }
    if value.eq_ignore_ascii_case("false") {
        return Ok((key.to_string(), JsonValue::Bool(false)));
    }
    if let Ok(int_value) = value.parse::<i64>() {
        return Ok((key.to_string(), JsonValue::Number(int_value.into())));
    }
    Ok((key.to_string(), JsonValue::String(value.to_string())))
}

fn merge_args(
    args_json: Option<&str>,
    args: &[String],
) -> anyhow::Result<JsonMap<String, JsonValue>> {
    let mut merged = JsonMap::new();
    if let Some(raw) = args_json {
        let parsed: JsonValue = serde_json::from_str(raw)?;
        let JsonValue::Object(map) = parsed else {
            return Err(anyhow::anyhow!("--args-json must be a JSON object"));
        };
        merged.extend(map);
    }
    for item in args {
        let (key, value) = parse_kv(item)?;
        merged.insert(key, value);
    }
    Ok(merged)
}

struct DemoSendMessageArgs<'a> {
    text: Option<&'a str>,
    args: &'a JsonMap<String, JsonValue>,
    tenant: &'a str,
    team: Option<&'a str>,
    destinations: &'a [String],
    to_kind: Option<&'a str>,
    provider_id: &'a str,
    channel: &'a str,
    card: Option<&'a JsonValue>,
}

fn build_demo_send_message(args: DemoSendMessageArgs<'_>) -> JsonValue {
    let mut metadata = BTreeMap::new();
    if let Some(card_value) = args.card
        && let Ok(card_str) = serde_json::to_string(card_value)
    {
        metadata.insert("adaptive_card".to_string(), card_str);
    }
    for (key, value) in args.args {
        metadata.insert(key.clone(), value.to_string());
    }
    let env_value = std::env::var("GREENTIC_ENV").unwrap_or_else(|_| "local".to_string());
    let env = EnvId::try_from(env_value.clone())
        .unwrap_or_else(|_| EnvId::try_from("local").expect("local env invalid"));
    let tenant_id = TenantId::try_from(args.tenant.to_string())
        .unwrap_or_else(|_| TenantId::try_from("demo").expect("demo tenant invalid"));
    let mut tenant_ctx = TenantCtx::new(env, tenant_id.clone());
    if let Some(team_value) = args.team
        && let Ok(team_id) = TeamId::try_from(team_value.to_string())
    {
        tenant_ctx = tenant_ctx.with_team(Some(team_id));
    }
    tenant_ctx = tenant_ctx
        .with_session(Uuid::new_v4().to_string())
        .with_flow(Uuid::new_v4().to_string())
        .with_node("demo".to_string())
        .with_provider(args.provider_id.to_string())
        .with_attempt(1);

    let to_kind_owned = args.to_kind.map(|value| value.to_string());
    let to = args
        .destinations
        .iter()
        .map(|value| Destination {
            id: value.clone(),
            kind: to_kind_owned.clone(),
        })
        .collect::<Vec<_>>();
    let envelope = ChannelMessageEnvelope {
        id: Uuid::new_v4().to_string(),
        tenant: tenant_ctx,
        channel: args.channel.to_string(),
        session_id: Uuid::new_v4().to_string(),
        reply_scope: None,
        from: None,
        to,
        correlation_id: None,
        text: args.text.map(|value| value.to_string()),
        attachments: Vec::new(),
        metadata,
    };
    serde_json::to_value(envelope).unwrap_or(JsonValue::Null)
}

fn debug_print_envelope(op_label: &str, envelope: &JsonValue) {
    if !demo_debug_enabled() {
        return;
    }
    match serde_json::to_string_pretty(envelope) {
        Ok(body) => println!(
            "{}",
            operator_i18n::trf(
                "cli.demo.debug.before_envelope",
                "[demo] before {} envelope:\n{}",
                &[op_label, &body]
            )
        ),
        Err(err) => println!(
            "{}",
            operator_i18n::trf(
                "cli.demo.debug.before_envelope_serialize_failed",
                "[demo] before {} envelope: failed to serialize envelope: {}",
                &[op_label, &err.to_string()]
            )
        ),
    }
}

fn debug_print_render_plan_output(output: &RenderPlanOutV1) {
    if !demo_debug_enabled() {
        return;
    }
    match serde_json::to_string_pretty(&output) {
        Ok(body) => println!(
            "{}",
            operator_i18n::trf(
                "cli.demo.debug.after_render_plan",
                "[demo] after render_plan output:\n{}",
                &[&body]
            )
        ),
        Err(err) => println!(
            "{}",
            operator_i18n::trf(
                "cli.demo.debug.after_render_plan_serialize_failed",
                "[demo] after render_plan output: failed to serialize output: {}",
                &[&err.to_string()]
            )
        ),
    }
}

fn debug_print_encode_input(input: &EncodeInV1) {
    if !demo_debug_enabled() {
        return;
    }
    match serde_json::to_string_pretty(&input) {
        Ok(body) => println!(
            "{}",
            operator_i18n::trf(
                "cli.demo.debug.encode_input",
                "[demo] encode input:\n{}",
                &[&body]
            )
        ),
        Err(err) => println!(
            "{}",
            operator_i18n::trf(
                "cli.demo.debug.encode_input_serialize_failed",
                "[demo] encode input: failed to serialize input: {}",
                &[&err.to_string()]
            )
        ),
    }
}

fn debug_print_encode_output(output: &EncodeOutV1) {
    if !demo_debug_enabled() {
        return;
    }
    match serde_json::to_string_pretty(&output) {
        Ok(body) => println!(
            "{}",
            operator_i18n::trf(
                "cli.demo.debug.after_encode",
                "[demo] after encode output:\n{}",
                &[&body]
            )
        ),
        Err(err) => println!(
            "{}",
            operator_i18n::trf(
                "cli.demo.debug.after_encode_serialize_failed",
                "[demo] after encode output: failed to serialize output: {}",
                &[&err.to_string()]
            )
        ),
    }
}

fn debug_print_send_payload_output(output: &SendPayloadOutV1) {
    if !demo_debug_enabled() {
        return;
    }
    match serde_json::to_string_pretty(&output) {
        Ok(body) => println!(
            "{}",
            operator_i18n::trf(
                "cli.demo.debug.after_send_payload",
                "[demo] after send_payload output:\n{}",
                &[&body]
            )
        ),
        Err(err) => println!(
            "{}",
            operator_i18n::trf(
                "cli.demo.debug.after_send_payload_serialize_failed",
                "[demo] after send_payload output: failed to serialize output: {}",
                &[&err.to_string()]
            )
        ),
    }
}

fn provider_channel(provider: &str) -> String {
    if let Some((domain, suffix)) = provider.split_once('-') {
        format!("{domain}.{suffix}")
    } else {
        provider.replace('-', ".")
    }
}

fn config_value_display(value: &JsonValue) -> String {
    match value {
        JsonValue::String(text) => text.clone(),
        JsonValue::Number(number) => number.to_string(),
        JsonValue::Bool(flag) => flag.to_string(),
        JsonValue::Null => "<null>".to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn format_requirements_output(value: &JsonValue) -> Option<String> {
    let JsonValue::Object(map) = value else {
        return None;
    };
    let has_keys = map.contains_key("required_args")
        || map.contains_key("optional_args")
        || map.contains_key("examples")
        || map.contains_key("notes");
    if !has_keys {
        return None;
    }
    let mut output = String::new();
    if let Some(required) = map.get("required_args").and_then(JsonValue::as_array) {
        output.push_str("Required args:\n");
        for item in required {
            output.push_str("  - ");
            output.push_str(&format_requirements_item(item));
            output.push('\n');
        }
    }
    if let Some(optional) = map.get("optional_args").and_then(JsonValue::as_array) {
        output.push_str("Optional args:\n");
        for item in optional {
            output.push_str("  - ");
            output.push_str(&format_requirements_item(item));
            output.push('\n');
        }
    }
    if let Some(examples) = map.get("examples").and_then(JsonValue::as_array) {
        output.push_str("Examples:\n");
        for item in examples {
            let pretty = serde_json::to_string_pretty(item).unwrap_or_else(|_| item.to_string());
            if pretty.contains('\n') {
                output.push_str("  -\n");
                for line in pretty.lines() {
                    output.push_str("    ");
                    output.push_str(line);
                    output.push('\n');
                }
            } else {
                output.push_str("  - ");
                output.push_str(&pretty);
                output.push('\n');
            }
        }
    }
    if let Some(notes) = map.get("notes").and_then(JsonValue::as_str) {
        output.push_str("Notes:\n");
        output.push_str(notes);
        output.push('\n');
    }
    Some(output.trim_end().to_string())
}

fn format_requirements_item(value: &JsonValue) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

impl From<DomainArg> for Domain {
    fn from(value: DomainArg) -> Self {
        match value {
            DomainArg::Messaging => Domain::Messaging,
            DomainArg::Events => Domain::Events,
            DomainArg::Secrets => Domain::Secrets,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeSet, path::PathBuf};

    #[test]
    fn parse_kv_infers_basic_types() {
        let (key, value) = parse_kv("a=1").unwrap();
        assert_eq!(key, "a");
        assert_eq!(value, JsonValue::Number(1.into()));

        let (key, value) = parse_kv("b=true").unwrap();
        assert_eq!(key, "b");
        assert_eq!(value, JsonValue::Bool(true));

        let (key, value) = parse_kv("c=hello").unwrap();
        assert_eq!(key, "c");
        assert_eq!(value, JsonValue::String("hello".to_string()));
    }

    #[test]
    fn merge_args_overrides_json() {
        let merged = merge_args(
            Some(r#"{"chat_id":1,"mode":"x"}"#),
            &["chat_id=2".to_string()],
        )
        .unwrap();
        assert_eq!(merged.get("chat_id"), Some(&JsonValue::Number(2.into())));
        assert_eq!(
            merged.get("mode"),
            Some(&JsonValue::String("x".to_string()))
        );
    }

    #[test]
    fn requirements_formatting_structured() {
        let value = serde_json::json!({
            "required_args": ["chat_id"],
            "optional_args": ["thread_id"],
            "examples": [{"chat_id": 1}],
            "notes": "Example note"
        });
        let rendered = format_requirements_output(&value).unwrap();
        assert!(rendered.contains("Required args:"));
        assert!(rendered.contains("Optional args:"));
        assert!(rendered.contains("Examples:"));
        assert!(rendered.contains("Notes:"));
    }

    #[test]
    fn requirements_missing_message() {
        let pack = domains::ProviderPack {
            pack_id: "demo".to_string(),
            file_name: "demo.gtpack".to_string(),
            path: PathBuf::from("demo.gtpack"),
            entry_flows: vec!["setup_default".to_string()],
        };
        let error = ensure_requirements_flow(&pack).unwrap_err();
        assert!(error.contains("requirements flow not found"));
    }

    #[test]
    fn filter_allowed_providers_moves_missing() {
        let mut packs = vec![
            domains::ProviderPack {
                pack_id: "messaging-telegram".to_string(),
                file_name: "telegram.gtpack".to_string(),
                path: PathBuf::from("telegram.gtpack"),
                entry_flows: vec!["setup_default".to_string()],
            },
            domains::ProviderPack {
                pack_id: "messaging-slack".to_string(),
                file_name: "slack.gtpack".to_string(),
                path: PathBuf::from("slack.gtpack"),
                entry_flows: vec!["setup_default".to_string()],
            },
        ];
        let allowed = vec![
            "messaging-telegram".to_string(),
            "messaging-email".to_string(),
        ]
        .into_iter()
        .collect::<BTreeSet<_>>();
        let missing = filter_packs_by_allowed(&mut packs, &allowed);
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].pack_id, "messaging-telegram");
        assert_eq!(missing, vec!["messaging-email".to_string()]);
    }

    #[test]
    fn select_demo_providers_respects_filter() {
        let providers = vec![
            DemoProviderInfo {
                pack: domains::ProviderPack {
                    pack_id: "messaging-telegram".to_string(),
                    file_name: "messaging-telegram.gtpack".to_string(),
                    path: PathBuf::from("messaging-telegram.gtpack"),
                    entry_flows: Vec::new(),
                },
            },
            DemoProviderInfo {
                pack: domains::ProviderPack {
                    pack_id: "messaging-slack".to_string(),
                    file_name: "messaging-slack.gtpack".to_string(),
                    path: PathBuf::from("messaging-slack.gtpack"),
                    entry_flows: Vec::new(),
                },
            },
        ];
        let all = select_demo_providers(&providers, None).unwrap();
        assert_eq!(all.len(), providers.len());
        let single = select_demo_providers(&providers, Some("messaging-slack")).unwrap();
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].pack.pack_id, "messaging-slack");
    }

    #[test]
    fn create_per_pack_matrix_requires_entries() {
        let tenants = vec![wizard::TenantSelection {
            tenant: "demo".to_string(),
            team: Some("default".to_string()),
            allow_paths: Vec::new(),
        }];
        let err = build_access_changes(
            wizard::WizardMode::Create,
            Some("per_pack_matrix"),
            &tenants,
            &["oci://ghcr.io/greentic/packs/sales@0.6.0".to_string()],
            Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("requires non-empty access_change"));
    }

    #[test]
    fn create_all_selected_expands_matrix() {
        let tenants = vec![wizard::TenantSelection {
            tenant: "demo".to_string(),
            team: Some("default".to_string()),
            allow_paths: Vec::new(),
        }];
        let changes = build_access_changes(
            wizard::WizardMode::Create,
            Some("all_selected_get_all_packs"),
            &tenants,
            &[
                "oci://ghcr.io/greentic/packs/sales@0.6.0".to_string(),
                "oci://ghcr.io/greentic/packs/hr@0.6.0".to_string(),
            ],
            Vec::new(),
        )
        .unwrap();
        assert_eq!(changes.len(), 2);
    }

    #[test]
    fn parse_yes_no_token_accepts_english_and_dutch() {
        assert_eq!(parse_yes_no_token("y"), Some(true));
        assert_eq!(parse_yes_no_token("yes"), Some(true));
        assert_eq!(parse_yes_no_token("j"), Some(true));
        assert_eq!(parse_yes_no_token("ja"), Some(true));
        assert_eq!(parse_yes_no_token("n"), Some(false));
        assert_eq!(parse_yes_no_token("no"), Some(false));
        assert_eq!(parse_yes_no_token("nee"), Some(false));
        assert_eq!(parse_yes_no_token("nein"), Some(false));
        assert_eq!(parse_yes_no_token("x"), None);
    }

    #[test]
    fn localized_pack_ref_field_title_uses_i18n_key() {
        let value = localized_list_field_title("pack_refs", "pack_ref", "fallback");
        assert!(!value.is_empty());
        assert_ne!(value, "fallback");
    }
}
