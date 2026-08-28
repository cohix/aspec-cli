//! Part 1 AmieConfig serialization and validation tests.

use std::collections::HashMap;

use awman::data::config::env::EnvSnapshot;
use awman::data::config::global::GlobalConfig;
use awman::data::config::repo::AmieConfig;

fn load_amie_json(value: serde_json::Value) -> Result<GlobalConfig, awman::data::error::DataError> {
    let tmp = tempfile::tempdir().unwrap();
    let env = EnvSnapshot::with_overrides([(
        "AWMAN_CONFIG_HOME",
        tmp.path().to_string_lossy().to_string(),
    )]);
    let path = GlobalConfig::path_with(&env).unwrap();
    std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    GlobalConfig::load_with(&env)
}

fn assert_invalid(value: serde_json::Value, message: &str) {
    let error = load_amie_json(value).expect_err("invalid amie config must fail at load");
    assert!(
        error.to_string().contains(message),
        "expected {message:?} in validation error, got {error}"
    );
}

#[test]
fn amie_config_round_trips_through_global_config_using_amie_json_key() {
    let tmp = tempfile::tempdir().unwrap();
    let env = EnvSnapshot::with_overrides([(
        "AWMAN_CONFIG_HOME",
        tmp.path().to_string_lossy().to_string(),
    )]);
    let config = GlobalConfig {
        amie: Some(AmieConfig {
            agents_to_models: Some(HashMap::from([(
                "codex".into(),
                vec!["gpt-5".into(), "gpt-5-mini".into()],
            )])),
            max_concurrent_evaluations: Some(3),
            default_leader: Some("codex::gpt-5".into()),
            guidance: Some(vec!["Keep the patch focused.".into()]),
        }),
        ..GlobalConfig::default()
    };

    config.save_with(&env).unwrap();
    let encoded = std::fs::read_to_string(GlobalConfig::path_with(&env).unwrap()).unwrap();
    assert!(
        encoded.contains("\"amie\""),
        "global JSON must use the amie key"
    );
    assert!(encoded.contains("\"agentsToModels\""));
    assert!(encoded.contains("\"maxConcurrentEvaluations\""));
    assert_eq!(GlobalConfig::load_with(&env).unwrap(), config);
}

#[test]
fn amie_config_rejects_zero_max_concurrent_evaluations() {
    assert_invalid(
        serde_json::json!({"amie": {"maxConcurrentEvaluations": 0}}),
        "amie.maxConcurrentEvaluations must be >= 1",
    );
}

#[test]
fn amie_config_rejects_malformed_default_leader() {
    assert_invalid(
        serde_json::json!({"amie": {"defaultLeader": "codex-gpt-5"}}),
        "expected agent::model",
    );
}

#[test]
fn amie_config_rejects_empty_default_leader_component() {
    assert_invalid(
        serde_json::json!({"amie": {"defaultLeader": "::gpt-5"}}),
        "expected agent::model",
    );
}

#[test]
fn amie_config_rejects_empty_default_leader_model_component() {
    assert_invalid(
        serde_json::json!({"amie": {"defaultLeader": "codex::"}}),
        "expected agent::model",
    );
}

#[test]
fn amie_config_rejects_default_leader_component_whitespace() {
    assert_invalid(
        serde_json::json!({"amie": {"defaultLeader": "codex:: gpt-5"}}),
        "must not have leading or trailing whitespace",
    );
}

#[test]
fn amie_config_rejects_invalid_default_leader_agent_name() {
    assert_invalid(
        serde_json::json!({"amie": {"defaultLeader": "bad.agent::gpt-5"}}),
        "is not a valid agent name",
    );
}

#[test]
fn amie_config_rejects_invalid_agents_to_models_key() {
    assert_invalid(
        serde_json::json!({"amie": {"agentsToModels": {"bad.agent": ["gpt-5"]}}}),
        "agentsToModels key",
    );
}

#[test]
fn amie_config_rejects_empty_model_list() {
    assert_invalid(
        serde_json::json!({"amie": {"agentsToModels": {"codex": []}}}),
        "empty model list",
    );
}

#[test]
fn amie_config_rejects_empty_model_name() {
    assert_invalid(
        serde_json::json!({"amie": {"agentsToModels": {"codex": ["   "]}}}),
        "empty model name",
    );
}

#[test]
fn amie_config_rejects_empty_guidance_entry() {
    assert_invalid(
        serde_json::json!({"amie": {"guidance": ["  "]}}),
        "guidance[0] is empty",
    );
}

#[test]
fn amie_config_rejects_too_many_guidance_entries() {
    let entries: Vec<_> = (0..51).map(|_| "instruction").collect();
    assert_invalid(
        serde_json::json!({"amie": {"guidance": entries}}),
        "maximum is 50",
    );
}

#[test]
fn amie_config_rejects_overlong_guidance_entry() {
    assert_invalid(
        serde_json::json!({"amie": {"guidance": ["x".repeat(1001)]}}),
        "maximum is 1000",
    );
}
