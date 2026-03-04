#[cfg(test)]
mod suite {
    use crate::messaging_universal::dlq;
    use crate::messaging_universal::dto::{HttpInV1, ProviderPayloadV1};
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn http_in_serializes_body_as_base64() {
        let payload = HttpInV1 {
            v: 1,
            provider: "dummy".to_string(),
            route: Some("events".to_string()),
            binding_id: None,
            tenant_hint: None,
            team_hint: None,
            method: "POST".to_string(),
            path: "/ingress/dummy".to_string(),
            query: vec![("k".to_string(), "v".to_string())],
            headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
            body_b64: STANDARD.encode("hello".as_bytes()),
        };
        let serialized = serde_json::to_string(&payload).unwrap();
        assert!(serialized.contains("aGVsbG8="));
    }

    #[test]
    fn provider_payload_round_trips() {
        let payload = ProviderPayloadV1 {
            content_type: "application/json".to_string(),
            body_b64: STANDARD.encode(b"{}"),
            metadata_json: Some(json!({"foo": "bar"}).to_string()),
            metadata: None,
        };
        let round_trip = serde_json::to_string(&payload).unwrap();
        let parsed: ProviderPayloadV1 = serde_json::from_str(&round_trip).unwrap();
        assert_eq!(parsed.content_type, "application/json");
        assert!(parsed.metadata_json.unwrap().contains("foo"));
    }

    #[test]
    fn dlq_entry_contains_expected_fields() {
        let node_error = json!({
            "code": "node-error",
            "message": "boom",
            "retryable": true,
            "backoff_ms": 100,
        });
        let entry = dlq::build_dlq_entry(
            "job-123",
            "dummy",
            "demo",
            Some("default"),
            None,
            Some("corr-1"),
            2,
            5,
            node_error.clone(),
            json!({
                "id": "env-1",
                "channel": "team",
                "text": "hi",
            }),
        );
        assert_eq!(entry["provider"], "dummy");
        assert_eq!(entry["tenant"], "demo");
        assert_eq!(entry["team"], "default");
        assert_eq!(entry["attempt"], 2);
        assert_eq!(entry["max_attempts"], 5);
        assert_eq!(entry["node_error"], node_error);
        assert_eq!(entry["message_summary"]["text"], json!("hi"));
        assert!(entry.get("ts").is_some());
    }

    #[test]
    fn append_dlq_entry_creates_jsonl_file() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let log_path = dir.path().join("logs").join("dlq.log");
        let entry = dlq::build_dlq_entry(
            "job-xyz",
            "dummy",
            "demo",
            None,
            Some("sess-1"),
            Some("corr-1"),
            1,
            3,
            json!({"code": "node-error"}),
            json!({"id": "env-2"}),
        );
        dlq::append_dlq_entry(&log_path, &entry)?;
        let contents = std::fs::read_to_string(&log_path)?;
        assert!(contents.ends_with('\n'));
        let trimmed = contents.trim_end();
        let parsed: serde_json::Value = serde_json::from_str(trimmed)?;
        assert_eq!(parsed["job_id"], "job-xyz");
        Ok(())
    }
}
