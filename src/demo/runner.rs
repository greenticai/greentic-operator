use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, anyhow};
use greentic_runner_host::validate::ValidationConfig;
use greentic_runner_host::{
    RunnerWasiPolicy,
    config::{
        FlowRetryConfig, HostConfig, OperatorPolicy, RateLimits, SecretsPolicy, StateStorePolicy,
        WebhookPolicy,
    },
    pack::{ComponentResolution, PackRuntime},
    runner::engine::{FlowContext, FlowEngine, FlowExecution, FlowSnapshot, FlowStatus},
    storage::{DynSessionStore, DynStateStore, new_session_store, new_state_store},
    trace::TraceConfig,
};
use greentic_types::{PackManifest, decode_pack_manifest};
use serde_json::Value;
use tokio::runtime::Runtime;
use zip::ZipArchive;

use crate::demo::types::{DemoBlockedOn, UserEvent};
use crate::secrets_gate::DynSecretsManager;

pub struct DemoRunner {
    pack_path: PathBuf,
    entry_flow: String,
    pack_id: String,
    tenant: String,
    #[allow(dead_code)]
    team: Option<String>,
    initial_input: Value,
    pending_input: Option<Value>,
    snapshot: Option<FlowSnapshot>,
    session_store: DynSessionStore,
    state_store: DynStateStore,
    secrets_manager: DynSecretsManager,
    host_config: Arc<HostConfig>,
    runtime: Runtime,
}

impl DemoRunner {
    pub fn new(
        pack_path: PathBuf,
        tenant: &str,
        team: Option<String>,
        initial_input: Value,
        secrets_manager: DynSecretsManager,
    ) -> anyhow::Result<Self> {
        let (entry_flow, pack_id) = select_entry_flow(&pack_path)?;
        Self::with_entry_flow(
            pack_path,
            tenant,
            team,
            entry_flow,
            pack_id,
            initial_input,
            secrets_manager,
        )
    }

    pub fn with_entry_flow(
        pack_path: PathBuf,
        tenant: &str,
        team: Option<String>,
        entry_flow: String,
        pack_id: String,
        initial_input: Value,
        secrets_manager: DynSecretsManager,
    ) -> anyhow::Result<Self> {
        let runtime = Runtime::new().context("build demo runner runtime")?;
        let host_config = Arc::new(build_host_config(tenant));
        Ok(Self {
            pack_path,
            entry_flow,
            pack_id,
            tenant: tenant.to_string(),
            team,
            initial_input,
            pending_input: None,
            snapshot: None,
            session_store: new_session_store(),
            state_store: new_state_store(),
            secrets_manager,
            host_config,
            runtime,
        })
    }

    pub fn pack_path(&self) -> &Path {
        &self.pack_path
    }

    pub fn pack_id(&self) -> &str {
        &self.pack_id
    }

    pub fn submit_user_event(&mut self, event: UserEvent) {
        self.pending_input = Some(event.into_value());
    }

    pub fn run_until_blocked(&mut self) -> DemoBlockedOn {
        let initial_input = self.initial_input.clone();
        let input = self.pending_input.take().unwrap_or(initial_input);
        let snapshot = self.snapshot.clone();
        let result = self.runtime.block_on(self.execute_flow(input, snapshot));
        match result {
            Ok(execution) => match execution.status {
                FlowStatus::Waiting(wait) => {
                    let snapshot = wait.snapshot.clone();
                    self.snapshot = Some(snapshot.clone());
                    DemoBlockedOn::Waiting {
                        reason: wait.reason,
                        snapshot: Box::new(snapshot),
                        output: execution.output,
                    }
                }
                FlowStatus::Completed => {
                    self.snapshot = None;
                    DemoBlockedOn::Finished(execution.output)
                }
            },
            Err(err) => DemoBlockedOn::Error(err),
        }
    }

    async fn execute_flow(
        &self,
        input: Value,
        snapshot: Option<FlowSnapshot>,
    ) -> anyhow::Result<FlowExecution> {
        let host_config = Arc::clone(&self.host_config);
        let pack_runtime = Arc::new(
            PackRuntime::load(
                &self.pack_path,
                host_config.clone(),
                None,
                Some(&self.pack_path),
                Some(self.session_store.clone()),
                Some(self.state_store.clone()),
                Arc::new(RunnerWasiPolicy::default()),
                self.secrets_manager.clone(),
                None,
                false,
                ComponentResolution::default(),
            )
            .await?,
        );
        let engine = FlowEngine::new(vec![Arc::clone(&pack_runtime)], host_config.clone()).await?;
        let make_ctx = || FlowContext {
            tenant: &self.tenant,
            pack_id: &self.pack_id,
            flow_id: &self.entry_flow,
            node_id: None,
            tool: None,
            action: None,
            session_id: None,
            provider_id: None,
            retry_config: host_config.retry_config().into(),
            attempt: 1,
            observer: None,
            mocks: None,
        };
        match snapshot {
            Some(snapshot) => engine.resume(make_ctx(), snapshot, input).await,
            None => engine.execute(make_ctx(), input).await,
        }
    }
}

fn build_host_config(tenant: &str) -> HostConfig {
    HostConfig {
        tenant: tenant.to_string(),
        bindings_path: PathBuf::from("<demo-runner>"),
        flow_type_bindings: HashMap::new(),
        rate_limits: RateLimits::default(),
        retry: FlowRetryConfig::default(),
        http_enabled: true,
        secrets_policy: SecretsPolicy::allow_all(),
        state_store_policy: StateStorePolicy::default(),
        webhook_policy: WebhookPolicy::default(),
        timers: Vec::new(),
        oauth: None,
        mocks: None,
        pack_bindings: Vec::new(),
        env_passthrough: Vec::new(),
        trace: TraceConfig::from_env(),
        validation: ValidationConfig::from_env(),
        operator_policy: OperatorPolicy::allow_all(),
    }
}

fn select_entry_flow(pack_path: &Path) -> anyhow::Result<(String, String)> {
    let manifest = read_pack_manifest(pack_path)?;
    let pack_id = manifest.pack_id.as_str().to_string();
    let entry_flow = manifest
        .flows
        .first()
        .map(|entry| entry.id.to_string())
        .ok_or_else(|| anyhow!("pack {} declares no flows", pack_id))?;
    Ok((entry_flow, pack_id))
}

fn read_pack_manifest(pack_path: &Path) -> anyhow::Result<PackManifest> {
    let file = File::open(pack_path)?;
    let mut archive = ZipArchive::new(file)?;
    let mut manifest_entry = archive
        .by_name("manifest.cbor")
        .context("manifest.cbor missing in pack")?;
    let mut bytes = Vec::new();
    manifest_entry.read_to_end(&mut bytes)?;
    decode_pack_manifest(&bytes).context("decode pack manifest")
}
