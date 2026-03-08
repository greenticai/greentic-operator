use std::path::{Path, PathBuf};

use anyhow::Result;
use anyhow::anyhow;
use uuid::Uuid;

use crate::cli::{discovery_map, provider_id_for_pack, resolve_demo_provider_pack};
use crate::config::DemoDesiredSubscription;
use crate::demo::runner_host::{DemoRunnerHost, OperatorContext};
use crate::discovery;
use crate::domains::Domain;
use crate::secrets_gate;
use crate::subscriptions_universal::scheduler::Scheduler;
use crate::subscriptions_universal::{AuthUserRefV1, SubscriptionEnsureRequest};

pub fn state_root(bundle: &Path) -> PathBuf {
    bundle.join("state").join("subscriptions")
}

pub fn build_runner(
    bundle: &Path,
    tenant: &str,
    team: Option<String>,
) -> Result<(DemoRunnerHost, OperatorContext)> {
    let discovery = discover_demo_bundle_cbor_only(bundle)?;
    let secrets_handle = secrets_gate::resolve_secrets_manager(bundle, tenant, team.as_deref())?;
    let runner_host = DemoRunnerHost::new(
        bundle.to_path_buf(),
        &discovery,
        None,
        secrets_handle.clone(),
        false,
    )?;
    let context = OperatorContext {
        tenant: tenant.to_string(),
        team,
        correlation_id: None,
    };
    Ok((runner_host, context))
}

pub fn ensure_desired_subscriptions(
    bundle: &Path,
    tenant: &str,
    team: Option<String>,
    desired: &[DemoDesiredSubscription],
    runner_host: &DemoRunnerHost,
    scheduler: &Scheduler<DemoRunnerHost>,
) -> Result<()> {
    if desired.is_empty() {
        return Ok(());
    }
    enforce_subscription_request_policy(runner_host)?;
    let team_ref = team.as_deref();
    for entry in desired {
        let pack = resolve_demo_provider_pack(
            bundle,
            tenant,
            team_ref,
            &entry.provider,
            Domain::Messaging,
        )?;
        let discovery = discover_demo_bundle_cbor_only(bundle)?;
        let provider_map = discovery_map(&discovery.providers);
        let provider_id = provider_id_for_pack(&pack.path, &pack.pack_id, Some(&provider_map));
        if !runner_host.supports_subscription_pack(&pack.pack_id, None) {
            return Err(anyhow!(
                "provider {} does not advertise any subscription offers",
                provider_id
            ));
        }
        let binding_id = entry
            .binding_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let request = to_request(entry, &binding_id);
        scheduler.ensure_once(&provider_id, &request)?;
    }
    Ok(())
}

fn to_request(entry: &DemoDesiredSubscription, binding_id: &str) -> SubscriptionEnsureRequest {
    SubscriptionEnsureRequest {
        binding_id: binding_id.to_string(),
        resource: Some(entry.resource.clone()),
        change_types: if entry.change_types.is_empty() {
            vec!["created".to_string()]
        } else {
            entry.change_types.clone()
        },
        notification_url: entry.notification_url.clone(),
        client_state: entry.client_state.clone(),
        user: entry.user.as_ref().map(|value| AuthUserRefV1 {
            user_id: value.user_id.clone(),
            token_key: value.token_key.clone(),
            tenant_id: None,
            email: None,
            display_name: None,
        }),
        expiration_target_unix_ms: None,
    }
}

fn discover_demo_bundle_cbor_only(bundle: &Path) -> anyhow::Result<discovery::DiscoveryResult> {
    discovery::discover_validated_bundle_cbor_only(bundle)
}

fn enforce_subscription_request_policy(runner_host: &DemoRunnerHost) -> Result<()> {
    runner_host
        .enforce_request_policy(
            "subscriptions.ensure",
            &[
                "session",
                "state",
                "bundle_source",
                "bundle_resolver",
                "bundle_fs",
            ],
            1,
        )
        .map_err(|refusal| anyhow!(refusal.message))
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use tempfile::tempdir;

    use crate::runtime_core::{
        RuntimeCore, RuntimeHealth, RuntimeHealthStatus, ScopedStateKey, StateProvider,
    };

    struct DegradedStateProvider;

    #[async_trait]
    impl StateProvider for DegradedStateProvider {
        async fn get(&self, _key: &ScopedStateKey) -> anyhow::Result<Option<serde_json::Value>> {
            Ok(None)
        }

        async fn put(
            &self,
            _key: &ScopedStateKey,
            _value: serde_json::Value,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn delete(&self, _key: &ScopedStateKey) -> anyhow::Result<()> {
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Degraded,
                reason: Some("state degraded".to_string()),
            })
        }
    }

    #[test]
    fn ensure_desired_subscriptions_refuses_when_safe_mode_blocks_bootstrap() -> Result<()> {
        let temp = tempdir()?;
        let discovery = discovery::DiscoveryResult {
            domains: discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            secrets_gate::resolve_secrets_manager(temp.path(), "demo", Some("team"))?;
        let host = DemoRunnerHost::new(
            temp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )?;
        let mut seams = host.runtime_core().seams().clone();
        seams.state_provider = Some(std::sync::Arc::new(DegradedStateProvider));
        host.replace_runtime_core_for_test(RuntimeCore::new(
            host.runtime_core().registry().clone(),
            seams,
            host.runtime_core().wiring_plan().clone(),
        ));

        let scheduler = Scheduler::new(
            crate::subscriptions_universal::service::SubscriptionService::new(
                host.clone(),
                OperatorContext {
                    tenant: "demo".to_string(),
                    team: Some("team".to_string()),
                    correlation_id: None,
                },
            ),
            crate::subscriptions_universal::store::SubscriptionStore::new(state_root(temp.path())),
        );
        let desired = vec![DemoDesiredSubscription {
            provider: "messaging.email".to_string(),
            resource: "/me/messages".to_string(),
            change_types: vec!["created".to_string()],
            notification_url: Some("https://example.test/hook".to_string()),
            client_state: None,
            user: None,
            binding_id: None,
        }];

        let err = ensure_desired_subscriptions(
            temp.path(),
            "demo",
            Some("team".to_string()),
            &desired,
            &host,
            &scheduler,
        )
        .expect_err("subscription bootstrap should be refused in safe mode");
        assert!(
            err.to_string()
                .contains("request class `subscriptions.ensure` refused"),
            "unexpected error: {err}"
        );
        Ok(())
    }
}
