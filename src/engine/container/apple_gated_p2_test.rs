use super::*;
use crate::engine::container::gated_launch::{
    AppleProviderState, ExactInspection, ProviderKind, ProviderLaunchKey, ProviderState,
};
use crate::engine::container::ContainerName;
use chrono::{DateTime, Utc};
use std::error::Error;

const NAME: &str = "awman-altana-native-p2";
const LABEL: &str = "abababababababababababababababababababababababababababababababab";
const IMAGE: &str = "sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";

fn image_document() -> Vec<u8> {
    image_document_for(IMAGE)
}

fn image_document_for(digest: &str) -> Vec<u8> {
    format!("[{{\"configuration\":{{\"descriptor\":{{\"digest\":\"{digest}\"}}}}}}]").into_bytes()
}

fn container_document(state: &str) -> Vec<u8> {
    container_document_at(state, "2026-10-04T00:20:56Z")
}

fn container_document_at(state: &str, creation_date: &str) -> Vec<u8> {
    format!(
        "{{\"configuration\":{{\"id\":\"{NAME}\",\"labels\":{{\"dev.awman.orchestrator-launch\":\"{LABEL}\"}},\"image\":{{\"descriptor\":{{\"digest\":\"{IMAGE}\"}}}},\"creationDate\":\"{creation_date}\"}},\"id\":\"{NAME}\",\"status\":{{\"state\":\"{state}\",\"startedDate\":\"2026-10-04T00:20:57.123456Z\"}}}}"
    )
    .into_bytes()
}

fn launch_key() -> Result<ProviderLaunchKey, Box<dyn Error>> {
    let image_id = parse_gated_image_inspection(&image_document())?;
    let created_not_before: DateTime<Utc> = "2026-10-04T00:20:56Z".parse()?;
    Ok(ProviderLaunchKey {
        provider: ProviderKind::AppleContainers,
        container_name: ContainerName::new(NAME),
        token_digest: [0xab; 32],
        immutable_image_id: image_id,
        created_not_before,
    })
}

fn assert_not_matching(document: &[u8], key: &ProviderLaunchKey) {
    assert!(!matches!(
        parse_gated_launch_inspection(document, key),
        ExactInspection::Matching(_)
    ));
}

#[test]
fn apple_image_parser_uses_configuration_descriptor_digest_only() -> Result<(), Box<dyn Error>> {
    let first = parse_gated_image_inspection(&image_document())?;
    let second = parse_gated_image_inspection(&image_document_for(
        "sha256:edededededededededededededededededededededededededededededededed",
    ))?;
    assert_ne!(first, second);

    for document in [
        br#"[{"configuration":{"image":{"descriptor":{"digest":"sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"}}}}]"#.as_slice(),
        br#"[{"configuration":{"descriptor":{"digest":"CDCD"}}}]"#.as_slice(),
        br#"[{"configuration":{"descriptor":{"digest":"sha256:cdcd"}}}]"#.as_slice(),
        br#"[{"configuration":{"descriptor":{}}}]"#.as_slice(),
        br#"[]"#.as_slice(),
        br#"{}"#.as_slice(),
    ] {
        assert!(parse_gated_image_inspection(document).is_err());
    }
    Ok(())
}

#[test]
fn apple_parser_accepts_only_the_full_running_or_evidenced_stopped_tuple(
) -> Result<(), Box<dyn Error>> {
    let key = launch_key()?;
    let running = match parse_gated_launch_inspection(&container_document("running"), &key) {
        ExactInspection::Matching(value) => value,
        _ => return Err("valid running Apple tuple did not match".into()),
    };
    assert_eq!(running.provider, ProviderKind::AppleContainers);
    assert_eq!(running.runtime_id, NAME);
    assert_eq!(running.exact_name.as_str(), NAME);
    assert_eq!(running.token_digest, [0xab; 32]);
    assert_eq!(running.immutable_image_id, key.immutable_image_id);
    assert_eq!(running.created_at, key.created_not_before);
    assert_eq!(
        running.state,
        ProviderState::Apple(AppleProviderState::Running)
    );

    let stopped = match parse_gated_launch_inspection(&container_document("stopped"), &key) {
        ExactInspection::Matching(value) => value,
        _ => return Err("evidenced stopped Apple tuple did not match".into()),
    };
    assert_eq!(
        stopped.state,
        ProviderState::Apple(AppleProviderState::Stopped)
    );

    assert!(matches!(
        parse_gated_launch_inspection(
            &container_document_at("running", "2026-10-04T00:20:57Z"),
            &key,
        ),
        ExactInspection::Matching(_)
    ));
    Ok(())
}

#[test]
fn apple_parser_never_turns_missing_mismatched_duplicate_or_unknown_fields_into_authority(
) -> Result<(), Box<dyn Error>> {
    let key = launch_key()?;
    let mut value: serde_json::Value = serde_json::from_slice(&container_document("running"))?;

    value["id"] = serde_json::Value::String("foreign".into());
    assert_not_matching(&serde_json::to_vec(&value)?, &key);

    value = serde_json::from_slice(&container_document("running"))?;
    value["configuration"]["id"] = serde_json::Value::String("foreign".into());
    assert_not_matching(&serde_json::to_vec(&value)?, &key);

    value = serde_json::from_slice(&container_document("running"))?;
    value["configuration"]["labels"]["dev.awman.orchestrator-launch"] = serde_json::Value::String(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
    );
    assert_not_matching(&serde_json::to_vec(&value)?, &key);

    value = serde_json::from_slice(&container_document("running"))?;
    value["configuration"]["image"]["descriptor"]["digest"] = serde_json::Value::String(
        "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into(),
    );
    assert_not_matching(&serde_json::to_vec(&value)?, &key);

    value = serde_json::from_slice(&container_document("running"))?;
    value["configuration"]["creationDate"] =
        serde_json::Value::String("2026-10-04T00:20:55Z".into());
    assert_not_matching(&serde_json::to_vec(&value)?, &key);

    for state in ["stopping", "unknown", "RUNNING", ""] {
        assert_not_matching(&container_document(state), &key);
    }

    for pointer in [
        "/configuration/labels/dev.awman.orchestrator-launch",
        "/configuration/image/descriptor/digest",
        "/configuration/creationDate",
        "/status/state",
        "/id",
        "/configuration/id",
    ] {
        value = serde_json::from_slice(&container_document("running"))?;
        let removed = remove_pointer(&mut value, pointer);
        assert!(removed);
        assert_not_matching(&serde_json::to_vec(&value)?, &key);
    }

    let duplicate = format!(
        "{{\"configuration\":{{\"id\":\"{NAME}\",\"id\":\"{NAME}\",\"labels\":{{\"dev.awman.orchestrator-launch\":\"{LABEL}\"}},\"image\":{{\"descriptor\":{{\"digest\":\"{IMAGE}\"}}}},\"creationDate\":\"2026-10-04T00:20:56Z\"}},\"id\":\"{NAME}\",\"status\":{{\"state\":\"running\"}}}}"
    );
    assert_not_matching(duplicate.as_bytes(), &key);
    Ok(())
}

fn remove_pointer(value: &mut serde_json::Value, pointer: &str) -> bool {
    let Some((parent_pointer, key)) = pointer.rsplit_once('/') else {
        return false;
    };
    let Some(parent) = value.pointer_mut(parent_pointer) else {
        return false;
    };
    parent
        .as_object_mut()
        .and_then(|object| object.remove(key))
        .is_some()
}
