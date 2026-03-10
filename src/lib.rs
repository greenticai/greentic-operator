#![allow(clippy::collapsible_if)]
#![allow(clippy::too_many_arguments)]

pub mod admin_api;
pub mod bin_resolver;
pub mod capabilities;
pub mod capability_bootstrap;
pub mod cards;
pub mod cli;
pub mod cloudflared;
pub mod component_qa_ops;
pub mod config;
pub mod config_gate;
pub mod demo;
pub mod doctor;
pub mod domains;
pub mod gmap;
pub mod hooks;
pub mod ingress;
pub mod messaging_universal;
pub mod ngrok;
pub mod offers;
pub mod onboard;
pub mod operator_i18n;
pub mod operator_log;
pub mod project;
pub mod provider_config_envelope;
pub mod provider_registry;
pub mod providers;
pub mod qa_flow_handler;
pub mod qa_persist;
pub mod qa_setup_wizard;
pub mod runner_exec;
pub mod runner_integration;
pub mod runtime_state;
pub mod secret_requirements;
pub mod secret_value;
pub mod secrets_backend;
pub mod secrets_client;
pub mod secrets_gate;
pub mod secrets_manager;
pub mod secrets_setup;
pub mod services;
pub mod state_layout;
pub mod static_routes;
pub mod subscriptions_universal;
pub mod supervisor;
pub mod wizard;
pub mod wizard_executor;
pub mod wizard_i18n;
pub mod wizard_plan_builder;
pub mod wizard_spec_builder;

// ── Re-exports from greentic-setup ──────────────────────────────────────
// These modules were extracted from this crate into greentic-setup.
// Re-exporting them keeps `use crate::<module>::*` imports working.
pub use greentic_setup::secrets as dev_store_path;
pub use greentic_setup::discovery;
pub use greentic_setup::secret_name;
pub use greentic_setup::setup_input;
pub use greentic_setup::setup_to_formspec;

#[cfg(test)]
pub(crate) fn test_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}
