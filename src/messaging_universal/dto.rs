use serde::{Deserialize, Serialize};

/// Base namespace version for HTTP ingress payloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpInV1 {
    pub v: u32,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_hint: Option<String>,
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    pub body_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpOutV1 {
    pub v: u32,
    pub status: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_b64: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderPlanInV1 {
    pub v: u32,
    pub message: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderPlanOutPlan {
    pub plan_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderPlanOutV1 {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<RenderPlanOutPlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodeInV1 {
    pub v: u32,
    pub message: serde_json::Value,
    pub plan: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodeOutV1 {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<ProviderPayloadV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderPayloadV1 {
    pub content_type: String,
    pub body_b64: String,
    /// Accepts both `metadata` (BTreeMap from greentic-types) and legacy `metadata_json` (String).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendPayloadInV1 {
    pub v: u32,
    pub provider_type: String,
    pub payload: ProviderPayloadV1,
    pub tenant: TenantHint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_scope: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendPayloadOutV1 {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub retryable: bool,
}

/// Tenant/Team hints that accompany outbound requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantHint {
    pub tenant: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}
