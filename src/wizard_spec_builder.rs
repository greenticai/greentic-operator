use serde_json::{Value, json};

use crate::wizard::{QaQuestion, QaSpec, WizardMode};

pub fn build_spec(mode: WizardMode) -> QaSpec {
    QaSpec {
        mode: mode.as_str().to_string(),
        questions: vec![
            QaQuestion {
                id: "operator.bundle.path".to_string(),
                title: "Bundle output path".to_string(),
                required: true,
            },
            QaQuestion {
                id: "operator.packs.refs".to_string(),
                title: "Pack refs (catalog + custom)".to_string(),
                required: false,
            },
            QaQuestion {
                id: "operator.tenants".to_string(),
                title: "Tenants and optional teams".to_string(),
                required: true,
            },
            QaQuestion {
                id: "operator.allow.paths".to_string(),
                title: "Allow rules as PACK[/FLOW[/NODE]]".to_string(),
                required: false,
            },
        ],
    }
}

pub fn build_validation_form(mode: WizardMode) -> Value {
    build_validation_form_with_providers(mode, &[])
}

pub fn build_validation_form_with_providers(mode: WizardMode, provider_ids: &[String]) -> Value {
    match mode {
        WizardMode::Create => create_validation_form(provider_ids),
        WizardMode::Update => update_validation_form(provider_ids),
        WizardMode::Remove => remove_validation_form(),
    }
}

fn create_validation_form(provider_ids: &[String]) -> Value {
    let provider_field = if provider_ids.is_empty() {
        json!({ "id": "provider_id", "type": "string", "title": "Provider id", "required": true })
    } else {
        json!({
            "id": "provider_id",
            "type": "enum",
            "title": "Provider id",
            "required": true,
            "choices": provider_ids
        })
    };
    json!({
        "id": "operator.wizard.create",
        "title": "Create bundle",
        "version": "1.0.0",
        "presentation": { "default_locale": "en-GB" },
        "questions": [
            {
                "id": "bundle_path",
                "type": "string",
                "title": "Bundle output path",
                "title_i18n": { "key": "wizard.create.bundle_path" },
                "required": true
            },
            {
                "id": "bundle_name",
                "type": "string",
                "title": "Bundle name",
                "title_i18n": { "key": "wizard.create.bundle_name" },
                "required": true
            },
            {
                "id": "locale",
                "type": "string",
                "title": "Locale",
                "title_i18n": { "key": "wizard.create.locale" },
                "required": false
            },
            {
                "id": "pack_refs",
                "type": "list",
                "title": "Pack references",
                "title_i18n": { "key": "wizard.create.pack_refs" },
                "required": false,
                "list": {
                    "fields": [
                        { "id": "pack_ref", "type": "string", "title": "Pack reference (e.g. /path/to/app.gtpack, file://..., oci://ghcr.io/..., repo://..., store://...)", "required": true },
                        {
                            "id": "access_scope",
                            "type": "enum",
                            "title": "Who can access this application?",
                            "required": false,
                            "choices": ["all_tenants", "tenant_all_teams", "specific_team"]
                        },
                        { "id": "tenant_id", "type": "string", "title": "What is the tenant id who can access this application?", "required": false },
                        { "id": "team_id", "type": "string", "title": "What is the team id who can access this application?", "required": false },
                        { "id": "make_default_pack", "type": "string", "title": "Is this pack the default pack when no pack is specified [y, N]?", "required": false }
                    ]
                }
            },
            {
                "id": "providers",
                "type": "list",
                "title": "Providers",
                "title_i18n": { "key": "wizard.create.providers" },
                "required": false,
                "list": {
                    "fields": [provider_field]
                }
            },
            {
                "id": "custom_provider_refs",
                "type": "list",
                "title": "Non-well-known provider references",
                "required": false,
                "list": {
                    "fields": [
                        { "id": "pack_ref", "type": "string", "title": "Provider pack reference (e.g. /path/to/provider.gtpack, file://..., oci://ghcr.io/..., repo://..., store://...)", "required": true }
                    ]
                }
            },
            {
                "id": "deployment_targets",
                "type": "list",
                "title": "Deployment targets",
                "required": false,
                "list": {
                    "fields": [
                        {
                            "id": "target",
                            "type": "enum",
                            "title": "Deployment target",
                            "required": true,
                            "choices": ["aws", "gcp", "azure", "single-vm", "runtime"]
                        },
                        { "id": "pack_ref", "type": "string", "title": "Deployer pack reference to bind to this target", "required": false },
                        { "id": "provider_pack", "type": "string", "title": "Bundle-relative provider pack path (advanced)", "required": false },
                        {
                            "id": "default",
                            "type": "enum",
                            "title": "Make this the default deployment target?",
                            "required": false,
                            "choices": ["true", "false"]
                        }
                    ]
                }
            },
            {
                "id": "execution_mode",
                "type": "enum",
                "title": "Execution mode",
                "title_i18n": { "key": "wizard.create.execution_mode" },
                "required": true,
                "choices": ["dry run", "execute"]
            }
        ],
        "validations": []
    })
}

fn update_validation_form(provider_ids: &[String]) -> Value {
    let provider_field = if provider_ids.is_empty() {
        json!({ "id": "provider_id", "type": "string", "title": "Provider id", "required": true })
    } else {
        json!({
            "id": "provider_id",
            "type": "enum",
            "title": "Provider id",
            "required": true,
            "choices": provider_ids
        })
    };
    json!({
        "id": "operator.wizard.update",
        "title": "Update bundle",
        "version": "1.0.0",
        "presentation": { "default_locale": "en-GB" },
        "questions": [
            {
                "id": "bundle_path",
                "type": "string",
                "title": "Bundle path",
                "title_i18n": { "key": "wizard.update.bundle_path" },
                "required": true
            },
            {
                "id": "update_ops",
                "type": "list",
                "title": "Update operations",
                "title_i18n": { "key": "wizard.update.ops" },
                "required": false,
                "list": {
                    "fields": [
                        {
                            "id": "op",
                            "type": "enum",
                            "title": "Operation",
                            "required": true,
                            "choices": [
                                "packs_add",
                                "packs_remove",
                                "providers_add",
                                "providers_remove",
                                "tenants_add",
                                "tenants_remove",
                                "access_change"
                            ]
                        }
                    ]
                }
            },
            {
                "id": "pack_refs",
                "type": "list",
                "title": "Packs to add",
                "required": false,
                "list": {
                    "fields": [
                        { "id": "pack_ref", "type": "string", "title": "Pack reference (e.g. /path/to/app.gtpack, file://..., oci://ghcr.io/..., repo://..., store://...)", "required": true }
                    ]
                }
            },
            {
                "id": "packs_remove",
                "type": "list",
                "title": "Packs to remove",
                "required": false,
                "list": {
                    "fields": [
                        { "id": "pack_identifier", "type": "string", "title": "Pack id/ref", "required": true },
                        {
                            "id": "scope",
                            "type": "enum",
                            "title": "Scope",
                            "required": false,
                            "choices": ["bundle", "global", "tenant", "team"]
                        },
                        { "id": "tenant_id", "type": "string", "title": "Tenant id", "required": false },
                        { "id": "team_id", "type": "string", "title": "Team id", "required": false }
                    ]
                }
            },
            {
                "id": "providers",
                "type": "list",
                "title": "Providers to enable",
                "required": false,
                "list": {
                    "fields": [provider_field.clone()]
                }
            },
            {
                "id": "custom_provider_refs",
                "type": "list",
                "title": "Non-well-known provider references to enable",
                "required": false,
                "list": {
                    "fields": [
                        { "id": "pack_ref", "type": "string", "title": "Provider pack reference (e.g. /path/to/provider.gtpack, file://..., oci://ghcr.io/..., repo://..., store://...)", "required": true }
                    ]
                }
            },
            {
                "id": "providers_remove",
                "type": "list",
                "title": "Providers to disable",
                "required": false,
                "list": {
                    "fields": [provider_field]
                }
            },
            {
                "id": "deployment_targets",
                "type": "list",
                "title": "Deployment targets",
                "required": false,
                "list": {
                    "fields": [
                        {
                            "id": "target",
                            "type": "enum",
                            "title": "Deployment target",
                            "required": true,
                            "choices": ["aws", "gcp", "azure", "single-vm", "runtime"]
                        },
                        { "id": "pack_ref", "type": "string", "title": "Deployer pack reference to bind to this target", "required": false },
                        { "id": "provider_pack", "type": "string", "title": "Bundle-relative provider pack path (advanced)", "required": false },
                        {
                            "id": "default",
                            "type": "enum",
                            "title": "Make this the default deployment target?",
                            "required": false,
                            "choices": ["true", "false"]
                        }
                    ]
                }
            },
            {
                "id": "targets",
                "type": "list",
                "title": "Tenants and teams to add/update",
                "required": false,
                "list": {
                    "fields": [
                        { "id": "tenant_id", "type": "string", "title": "Tenant id", "required": true },
                        { "id": "team_id", "type": "string", "title": "Team id", "required": false }
                    ]
                }
            },
            {
                "id": "tenants_remove",
                "type": "list",
                "title": "Tenants/teams to remove",
                "required": false,
                "list": {
                    "fields": [
                        { "id": "tenant_id", "type": "string", "title": "Tenant id", "required": true },
                        { "id": "team_id", "type": "string", "title": "Team id", "required": false }
                    ]
                }
            },
            {
                "id": "access_change",
                "type": "list",
                "title": "Access changes",
                "required": false,
                "list": {
                    "fields": [
                        { "id": "pack_id", "type": "string", "title": "Pack id", "required": true },
                        {
                            "id": "operation",
                            "type": "enum",
                            "title": "Operation",
                            "required": true,
                            "choices": ["allow_add", "allow_remove"]
                        },
                        { "id": "tenant_id", "type": "string", "title": "Tenant id", "required": true },
                        { "id": "team_id", "type": "string", "title": "Team id", "required": false }
                    ]
                }
            },
            {
                "id": "execution_mode",
                "type": "enum",
                "title": "Execution mode",
                "title_i18n": { "key": "wizard.update.execution_mode" },
                "required": true,
                "choices": ["dry run", "execute"]
            }
        ],
        "validations": []
    })
}

fn remove_validation_form() -> Value {
    json!({
        "id": "operator.wizard.remove",
        "title": "Remove from bundle",
        "version": "1.0.0",
        "presentation": { "default_locale": "en-GB" },
        "questions": [
            {
                "id": "bundle_path",
                "type": "string",
                "title": "Bundle path",
                "title_i18n": { "key": "wizard.remove.bundle_path" },
                "required": true
            },
            {
                "id": "remove_targets",
                "type": "list",
                "title": "Remove targets",
                "title_i18n": { "key": "wizard.remove.targets" },
                "required": false,
                "list": {
                    "fields": [
                        {
                            "id": "target_type",
                            "type": "enum",
                            "title": "Target type",
                            "required": true,
                            "choices": ["packs", "providers", "tenants_teams"]
                        },
                        { "id": "target_id", "type": "string", "title": "Target id", "required": true }
                    ]
                }
            },
            {
                "id": "packs_remove",
                "type": "list",
                "title": "Packs to remove",
                "required": false,
                "list": {
                    "fields": [
                        { "id": "pack_identifier", "type": "string", "title": "Pack id/ref", "required": true },
                        {
                            "id": "scope",
                            "type": "enum",
                            "title": "Scope",
                            "required": false,
                            "choices": ["bundle", "global", "tenant", "team"]
                        },
                        { "id": "tenant_id", "type": "string", "title": "Tenant id", "required": false },
                        { "id": "team_id", "type": "string", "title": "Team id", "required": false }
                    ]
                }
            },
            {
                "id": "providers_remove",
                "type": "list",
                "title": "Providers to remove",
                "required": false,
                "list": {
                    "fields": [
                        { "id": "provider_id", "type": "string", "title": "Provider id", "required": true }
                    ]
                }
            },
            {
                "id": "tenants_remove",
                "type": "list",
                "title": "Tenants/teams to remove",
                "required": false,
                "list": {
                    "fields": [
                        { "id": "tenant_id", "type": "string", "title": "Tenant id", "required": true },
                        { "id": "team_id", "type": "string", "title": "Team id", "required": false }
                    ]
                }
            },
            {
                "id": "execution_mode",
                "type": "enum",
                "title": "Execution mode",
                "title_i18n": { "key": "wizard.remove.execution_mode" },
                "required": true,
                "choices": ["dry run", "execute"]
            }
        ],
        "validations": []
    })
}
