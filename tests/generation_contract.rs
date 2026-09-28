use serde_json::Value;
use std::collections::BTreeSet;
use std::error::Error;
use std::fs;

const EXPECTED_AUTHORITY_REVISION: &str = "6d1202b1feb978343001f034df3cd03928c01b28";

#[test]
fn generation_contract_matches_shared_authority() -> Result<(), Box<dyn Error>> {
    let raw = fs::read_to_string("ores-generation-contract.json")?;
    let value: Value = serde_json::from_str(&raw)?;

    assert_eq!(
        value.get("schema").and_then(Value::as_str),
        Some("ores.desktop-generation-consumer/v1")
    );

    let lifecycle = value
        .get("lifecycle")
        .and_then(Value::as_array)
        .ok_or("lifecycle must be an array")?
        .iter()
        .map(|entry| entry.as_str().ok_or("lifecycle entry must be a string"))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        lifecycle,
        vec![
            "prepare",
            "validate",
            "compile_build_generation",
            "stage",
            "health_check",
            "atomic_activate",
            "bounded_drain",
            "commit",
        ]
    );

    let revision = value
        .pointer("/authority/revision")
        .and_then(Value::as_str)
        .ok_or("authority revision is required")?;
    assert_eq!(revision, EXPECTED_AUTHORITY_REVISION);
    assert_eq!(revision.len(), 40);
    assert!(revision.bytes().all(|byte| byte.is_ascii_hexdigit()));

    assert_eq!(
        value
            .pointer("/rollback/required_before_commit")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        value
            .pointer("/rollback/retain_previous_generation")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        value
            .pointer("/request_semantics/new_requests")
            .and_then(Value::as_str),
        Some("active_generation")
    );
    assert_eq!(
        value
            .pointer("/request_semantics/existing_requests")
            .and_then(Value::as_str),
        Some("pinned_generation")
    );
    assert_eq!(
        value
            .pointer("/request_semantics/generation_identity_required")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        value
            .pointer("/routing/edge_proxy_route_authority")
            .and_then(Value::as_bool),
        Some(false)
    );

    let stable_edges = value
        .pointer("/routing/stable_edges")
        .and_then(Value::as_array)
        .ok_or("stable_edges must be an array")?
        .iter()
        .map(|entry| entry.as_str().ok_or("stable edge must be a string"))
        .collect::<Result<BTreeSet<_>, _>>()?;
    for required in ["nginx", "haproxy", "caddy"] {
        assert!(stable_edges.contains(required));
    }

    assert_eq!(
        value
            .pointer("/middleware/beam_code_reload_requires_drain_or_otp_proof")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        value
            .pointer("/verification/shared_conformance_required")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        value
            .pointer("/verification/product_e2e_required")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        value
            .pointer("/verification/promotion_state")
            .and_then(Value::as_str),
        Some("candidate")
    );

    return Ok(());
}
