//! Apple Containers backend — `pub(super)`. Same shape as Docker; the Apple
//! `container` CLI is a near-drop-in replacement (it shares the docker `run`
//! / `list` / `stats` / `stop` surface).

use std::process::{Command, Stdio};

use crate::data::session::{AgentHandle, Session};
use crate::engine::agent_runtime::execution::{AgentInstance, AgentStats};
use crate::engine::container::attach_socket::AttachSocketGuard;
use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::gated_launch::{
    canonical_inspection_revision, parse_lower_hex_32, run_bounded_provider_cli,
    AbsenceObservation, AppleProviderState, BoundedProviderOutput,
    CanonicalInspectionRevisionInput, CreateCliExitClass, CreateCliExitEvidence, ExactInspection,
    GatedProviderAdapter, ImmutableImageId, InspectionObservationKind, LaunchIdentityError,
    LaunchRetentionRegistry, NamePresence, ProviderCallDeadline, ProviderCliCustodyRegistry,
    ProviderCliRunOutcome, ProviderCliStartFailure, ProviderKind, ProviderLaunchInspection,
    ProviderLaunchKey, ProviderState, SanitizedProviderStateObservation,
};
use crate::engine::container::options::{ContainerName, ResolvedContainerOptions};
use crate::engine::container::process::{AttachHookCtx, ContainerCli, ContainerInstance};
use crate::engine::credential_refresh::register_container_leases;
use crate::engine::error::EngineError;

#[derive(Debug, thiserror::Error)]
enum AppleProviderError {
    #[error("invalid Apple provider inspection")]
    InvalidInspection,
}

#[derive(serde::Deserialize)]
struct AppleDescriptor {
    digest: String,
}

#[derive(serde::Deserialize)]
struct AppleImageConfiguration {
    descriptor: AppleDescriptor,
}

#[derive(serde::Deserialize)]
struct AppleImageInspection {
    configuration: AppleImageConfiguration,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum AppleImageInspectionEnvelope {
    One(AppleImageInspection),
    Many(Vec<AppleImageInspection>),
}

#[derive(serde::Deserialize)]
struct AppleContainerImage {
    descriptor: AppleDescriptor,
}

struct UniqueLabels(std::collections::BTreeMap<String, String>);

impl<'de> serde::Deserialize<'de> for UniqueLabels {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct LabelsVisitor;

        impl<'de> serde::de::Visitor<'de> for LabelsVisitor {
            type Value = UniqueLabels;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a provider label object")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                use serde::de::Error as _;

                let mut labels = std::collections::BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, String>()? {
                    if labels.insert(key, value).is_some() {
                        return Err(M::Error::custom("duplicate provider label"));
                    }
                }
                Ok(UniqueLabels(labels))
            }
        }

        deserializer.deserialize_map(LabelsVisitor)
    }
}

#[derive(serde::Deserialize)]
struct AppleContainerConfiguration {
    id: String,
    labels: UniqueLabels,
    image: AppleContainerImage,
    #[serde(rename = "creationDate")]
    creation_date: String,
}

#[derive(serde::Deserialize)]
struct AppleContainerStatus {
    state: String,
}

#[derive(serde::Deserialize)]
struct AppleContainerInspection {
    id: String,
    configuration: AppleContainerConfiguration,
    status: AppleContainerStatus,
}

#[allow(dead_code)]
fn parse_gated_image_inspection(bytes: &[u8]) -> Result<ImmutableImageId, AppleProviderError> {
    let envelope: AppleImageInspectionEnvelope =
        serde_json::from_slice(bytes).map_err(|_| AppleProviderError::InvalidInspection)?;
    let inspection = match envelope {
        AppleImageInspectionEnvelope::One(inspection) => inspection,
        AppleImageInspectionEnvelope::Many(mut inspections) if inspections.len() == 1 => {
            inspections
                .pop()
                .ok_or(AppleProviderError::InvalidInspection)?
        }
        AppleImageInspectionEnvelope::Many(_) => return Err(AppleProviderError::InvalidInspection),
    };
    ImmutableImageId::new(inspection.configuration.descriptor.digest)
        .map_err(|_| AppleProviderError::InvalidInspection)
}

#[allow(dead_code)]
fn parse_gated_launch_inspection(bytes: &[u8], key: &ProviderLaunchKey) -> ExactInspection {
    if key.provider != ProviderKind::AppleContainers {
        return ExactInspection::ForeignOrAmbiguous;
    }
    let parsed: AppleContainerInspection = match serde_json::from_slice(bytes) {
        Ok(parsed) => parsed,
        Err(_) => return ExactInspection::ForeignOrAmbiguous,
    };
    if parsed.id != key.container_name.as_str()
        || parsed.configuration.id != key.container_name.as_str()
    {
        return ExactInspection::ForeignOrAmbiguous;
    }
    let token_digest = match parsed
        .configuration
        .labels
        .0
        .get("dev.awman.orchestrator-launch")
        .and_then(|value| parse_lower_hex_32(value))
    {
        Some(digest) if digest == key.token_digest => digest,
        _ => return ExactInspection::ForeignOrAmbiguous,
    };
    let immutable_image_id =
        match ImmutableImageId::new(parsed.configuration.image.descriptor.digest) {
            Ok(image) if image == key.immutable_image_id => image,
            _ => return ExactInspection::ForeignOrAmbiguous,
        };
    let created_at = match chrono::DateTime::parse_from_rfc3339(&parsed.configuration.creation_date)
    {
        Ok(created)
            if created.offset().local_minus_utc() == 0
                && chrono::Timelike::nanosecond(&created) == 0 =>
        {
            created.with_timezone(&chrono::Utc)
        }
        _ => return ExactInspection::ForeignOrAmbiguous,
    };
    if created_at < key.created_not_before {
        return ExactInspection::ForeignOrAmbiguous;
    }
    let state = match parsed.status.state.as_str() {
        "running" => ProviderState::Apple(AppleProviderState::Running),
        "stopped" => ProviderState::Apple(AppleProviderState::Stopped),
        _ => return ExactInspection::ForeignOrAmbiguous,
    };
    let revision = canonical_inspection_revision(CanonicalInspectionRevisionInput {
        kind: InspectionObservationKind::Matching,
        provider: ProviderKind::AppleContainers,
        exact_name: &key.container_name,
        runtime_id: Some(&parsed.configuration.id),
        token_digest: Some(token_digest),
        immutable_image_id: Some(&immutable_image_id),
        created_at: Some(created_at),
        state: SanitizedProviderStateObservation::Known(state.clone()),
    });
    let inspection = ProviderLaunchInspection {
        provider: ProviderKind::AppleContainers,
        runtime_id: parsed.configuration.id,
        exact_name: key.container_name.clone(),
        token_digest,
        immutable_image_id,
        created_at,
        state,
        revision,
    };
    ExactInspection::Matching(inspection)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AppleListValueContext {
    Row,
    Configuration,
}

struct DecodedAppleListValue {
    value: serde_json::Value,
    duplicate_identity: bool,
}

fn deserialize_apple_list_value<'de, D>(
    deserializer: D,
    context: AppleListValueContext,
) -> Result<DecodedAppleListValue, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct AppleListValueVisitor {
        context: AppleListValueContext,
    }

    impl AppleListValueVisitor {
        fn decoded(value: serde_json::Value) -> DecodedAppleListValue {
            DecodedAppleListValue {
                value,
                duplicate_identity: false,
            }
        }
    }

    impl<'de> serde::de::Visitor<'de> for AppleListValueVisitor {
        type Value = DecodedAppleListValue;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an Apple container list row")
        }

        fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
            Ok(Self::decoded(serde_json::Value::Bool(value)))
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
            Ok(Self::decoded(serde_json::Value::Number(value.into())))
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
            Ok(Self::decoded(serde_json::Value::Number(value.into())))
        }

        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            let number = serde_json::Number::from_f64(value)
                .ok_or_else(|| E::custom("invalid JSON number"))?;
            Ok(Self::decoded(serde_json::Value::Number(number)))
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            self.visit_string(value.to_owned())
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
            Ok(Self::decoded(serde_json::Value::String(value)))
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(Self::decoded(serde_json::Value::Null))
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(Self::decoded(serde_json::Value::Null))
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut values = Vec::new();
            while let Some(value) = sequence.next_element()? {
                values.push(value);
            }
            Ok(Self::decoded(serde_json::Value::Array(values)))
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: serde::de::MapAccess<'de>,
        {
            let mut values = serde_json::Map::new();
            let mut duplicate_identity = false;
            while let Some(key) = map.next_key::<String>()? {
                let identity_key = match self.context {
                    AppleListValueContext::Row => {
                        matches!(key.as_str(), "configuration" | "Name" | "name" | "Names")
                    }
                    AppleListValueContext::Configuration => key == "id",
                };
                if identity_key && values.contains_key(&key) {
                    duplicate_identity = true;
                }
                let decoded =
                    if self.context == AppleListValueContext::Row && key == "configuration" {
                        map.next_value::<AppleListConfigurationValue>()?.0
                    } else {
                        Self::decoded(map.next_value()?)
                    };
                duplicate_identity |= decoded.duplicate_identity;
                values.insert(key, decoded.value);
            }
            Ok(DecodedAppleListValue {
                value: serde_json::Value::Object(values),
                duplicate_identity,
            })
        }
    }

    deserializer.deserialize_any(AppleListValueVisitor { context })
}

struct AppleListConfigurationValue(DecodedAppleListValue);

impl<'de> serde::Deserialize<'de> for AppleListConfigurationValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserialize_apple_list_value(deserializer, AppleListValueContext::Configuration).map(Self)
    }
}

struct AppleListRow(DecodedAppleListValue);

impl<'de> serde::Deserialize<'de> for AppleListRow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserialize_apple_list_value(deserializer, AppleListValueContext::Row).map(Self)
    }
}

struct AppleGatedProviderAdapter {
    custody: std::sync::Arc<ProviderCliCustodyRegistry>,
    #[cfg(test)]
    executable: std::path::PathBuf,
}

impl AppleGatedProviderAdapter {
    fn try_new() -> Result<std::sync::Arc<Self>, ProviderCliStartFailure> {
        Ok(std::sync::Arc::new(Self {
            custody: ProviderCliCustodyRegistry::try_new()?,
            #[cfg(test)]
            executable: std::path::PathBuf::from("container"),
        }))
    }

    #[cfg(test)]
    fn try_new_with_executable(
        executable: &std::path::Path,
    ) -> Result<std::sync::Arc<Self>, ProviderCliStartFailure> {
        Ok(std::sync::Arc::new(Self {
            custody: ProviderCliCustodyRegistry::try_new()?,
            executable: executable.to_path_buf(),
        }))
    }

    fn run(&self, args: &[&str], deadline: ProviderCallDeadline) -> Option<BoundedProviderOutput> {
        #[cfg(test)]
        let mut command = Command::new(&self.executable);
        #[cfg(not(test))]
        let mut command = Command::new("container");
        command.args(args);
        match run_bounded_provider_cli(command, deadline, &self.custody) {
            ProviderCliRunOutcome::Completed(output) => Some(output),
            ProviderCliRunOutcome::NotStarted(_)
            | ProviderCliRunOutcome::ReapedFailure(_)
            | ProviderCliRunOutcome::RetainedFailure(_) => None,
        }
    }

    fn list_rows(&self, deadline: ProviderCallDeadline) -> Option<Vec<AppleListRow>> {
        let output = self.run(&["list", "--all", "--format", "json"], deadline)?;
        if !output.status.success() {
            return None;
        }
        serde_json::from_slice::<Vec<AppleListRow>>(&output.stdout)
            .ok()
            .or_else(|| {
                let mut rows = Vec::new();
                let mut saw_row = false;
                for line in output.stdout.split(|byte| *byte == b'\n') {
                    if line.iter().all(|byte| byte.is_ascii_whitespace()) {
                        continue;
                    }
                    saw_row = true;
                    rows.push(serde_json::from_slice(line).ok()?);
                }
                saw_row.then_some(rows)
            })
    }
}

fn set_strict_apple_list_name<'a>(
    candidate: &mut Option<&'a str>,
    value: &'a serde_json::Value,
    trim_leading_slashes: bool,
) -> Option<()> {
    let raw = value.as_str()?;
    if !trim_leading_slashes && raw.starts_with('/') {
        return None;
    }
    let name = if trim_leading_slashes {
        raw.trim_start_matches('/')
    } else {
        raw
    };
    if name.is_empty() || name.chars().any(char::is_whitespace) || candidate.is_some() {
        return None;
    }
    *candidate = Some(name);
    Some(())
}

fn strict_apple_list_row_name(row: &AppleListRow) -> Option<&str> {
    if row.0.duplicate_identity {
        return None;
    }
    let row = &row.0.value;
    if !row.is_object() {
        return None;
    }
    let mut candidate = None;
    if let Some(configuration) = row.get("configuration") {
        let id = configuration.as_object()?.get("id")?;
        set_strict_apple_list_name(&mut candidate, id, false)?;
    }
    for key in ["Name", "name"] {
        if let Some(value) = row.get(key) {
            set_strict_apple_list_name(&mut candidate, value, true)?;
        }
    }
    if let Some(value) = row.get("Names") {
        let names = value.as_array()?;
        if names.len() != 1 {
            return None;
        }
        set_strict_apple_list_name(&mut candidate, &names[0], true)?;
    }
    candidate
}

impl GatedProviderAdapter for AppleGatedProviderAdapter {
    fn provider(&self) -> ProviderKind {
        ProviderKind::AppleContainers
    }

    fn resolve_image_identity(
        &self,
        image: &crate::engine::container::options::ImageRef,
        deadline: ProviderCallDeadline,
    ) -> Result<ImmutableImageId, LaunchIdentityError> {
        let output = self
            .run(&["image", "inspect", image.as_str()], deadline)
            .filter(|output| output.status.success())
            .ok_or(LaunchIdentityError::ProviderInspectionUnavailable)?;
        parse_gated_image_inspection(&output.stdout)
            .map_err(|_| LaunchIdentityError::ImageIdentityMismatch)
    }

    fn inspect_name_absence(
        &self,
        name: &ContainerName,
        deadline: ProviderCallDeadline,
    ) -> Result<AbsenceObservation, NamePresence> {
        let rows = self.list_rows(deadline).ok_or(NamePresence::Unavailable)?;
        let mut present = false;
        for row in &rows {
            let row_name = strict_apple_list_row_name(row).ok_or(NamePresence::Ambiguous)?;
            present |= row_name == name.as_str();
        }
        if present {
            return Err(NamePresence::Present);
        }
        let checked_at = chrono::Utc::now();
        Ok(AbsenceObservation {
            provider: ProviderKind::AppleContainers,
            exact_name: name.clone(),
            checked_at,
            revision: canonical_inspection_revision(CanonicalInspectionRevisionInput {
                kind: InspectionObservationKind::Absent,
                provider: ProviderKind::AppleContainers,
                exact_name: name,
                runtime_id: None,
                token_digest: None,
                immutable_image_id: None,
                created_at: None,
                state: SanitizedProviderStateObservation::Absent,
            }),
        })
    }

    fn inspect_launch(
        &self,
        key: &ProviderLaunchKey,
        deadline: ProviderCallDeadline,
    ) -> ExactInspection {
        let Some(output) = self.run(&["inspect", key.container_name.as_str()], deadline) else {
            return ExactInspection::Unavailable;
        };
        if !output.status.success() {
            return ExactInspection::Unavailable;
        }
        parse_gated_launch_inspection(&output.stdout, key)
    }

    fn classify_create_exit(&self, _exit: &CreateCliExitEvidence) -> CreateCliExitClass {
        CreateCliExitClass::Other
    }

    fn stop_inspected(
        &self,
        inspection: &ProviderLaunchInspection,
        deadline: ProviderCallDeadline,
    ) -> Result<(), EngineError> {
        let output = self
            .run(&["stop", inspection.exact_name.as_str()], deadline)
            .ok_or_else(|| EngineError::Container("Apple container stop unavailable".into()))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(EngineError::Container("Apple container stop failed".into()))
        }
    }

    fn remove_inspected(
        &self,
        inspection: &ProviderLaunchInspection,
        deadline: ProviderCallDeadline,
    ) -> Result<(), EngineError> {
        let output = self
            .run(&["rm", inspection.exact_name.as_str()], deadline)
            .ok_or_else(|| EngineError::Container("Apple container remove unavailable".into()))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(EngineError::Container(
                "Apple container remove failed".into(),
            ))
        }
    }
}

/// Extract the container name from an Apple Containers JSON row.
///
/// Apple's schema uses `configuration.id` as the container name/identifier
/// (there is no separate short hex ID). Falls back to Docker-style fields
/// for forward-compatibility.
fn extract_apple_name(row: &serde_json::Value) -> String {
    if let Some(id) = row
        .get("configuration")
        .and_then(|c| c.get("id"))
        .and_then(|v| v.as_str())
    {
        return id.to_string();
    }
    let val = row
        .get("Names")
        .or_else(|| row.get("Name"))
        .or_else(|| row.get("name"));
    match val {
        Some(v) if v.is_array() => v
            .as_array()
            .and_then(|a| a.first())
            .and_then(|s| s.as_str())
            .map(|s| s.trim_start_matches('/'))
            .unwrap_or_default()
            .to_string(),
        Some(v) => v
            .as_str()
            .map(|s| s.trim_start_matches('/'))
            .unwrap_or_default()
            .to_string(),
        None => String::new(),
    }
}

/// Extract the image reference from an Apple Containers JSON row.
///
/// Apple stores the image as `configuration.image` (an object); we serialize
/// it for display. Falls back to Docker-style string `Image`/`image` fields.
fn extract_apple_image(row: &serde_json::Value) -> String {
    if let Some(img_obj) = row.get("configuration").and_then(|c| c.get("image")) {
        if let Some(s) = img_obj.as_str() {
            return s.to_string();
        }
        // Apple Containers stores the image name in descriptor.annotations.
        if let Some(s) = img_obj
            .get("descriptor")
            .and_then(|d| d.get("annotations"))
            .and_then(|a| a.get("com.apple.containerization.image.name"))
            .and_then(|v| v.as_str())
        {
            return s.to_string();
        }
        // Apple Containers uses "reference" for a full OCI image reference string.
        if let Some(s) = img_obj.get("reference").and_then(|v| v.as_str()) {
            return s.to_string();
        }
        if let Some(repo) = img_obj.get("repository").and_then(|v| v.as_str()) {
            return match img_obj.get("tag").and_then(|v| v.as_str()) {
                Some(tag) if !tag.is_empty() => format!("{repo}:{tag}"),
                _ => repo.to_string(),
            };
        }
        return serde_json::to_string(img_obj).unwrap_or_default();
    }
    row.get("Image")
        .or_else(|| row.get("image"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Extract the started-at timestamp from an Apple Containers JSON row.
///
/// Apple uses `startedDate` (float epoch seconds). Falls back to
/// Docker-style `CreatedAt`/`Created` RFC3339 strings.
fn extract_apple_started_at(row: &serde_json::Value) -> chrono::DateTime<chrono::Utc> {
    if let Some(ts) = row.get("startedDate").and_then(|v| v.as_f64()) {
        let secs = ts as i64;
        let nanos = ((ts - secs as f64) * 1_000_000_000.0) as u32;
        if let Some(dt) = chrono::DateTime::from_timestamp(secs, nanos) {
            return dt;
        }
    }
    row.get("CreatedAt")
        .or_else(|| row.get("Created"))
        .or_else(|| row.get("created"))
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(chrono::Utc::now)
}

/// Check whether the row represents a running container.
///
/// Apple uses a `status` field: "running" | "stopped" | "stopping" | "unknown".
/// If absent (Docker-style output from `ps`), assume running.
fn is_apple_running(row: &serde_json::Value) -> bool {
    match row.get("status").and_then(|v| v.as_str()) {
        Some(s) => s == "running",
        None => true,
    }
}

/// Check whether the row's name matches awman container patterns.
fn is_awman_container(name: &str) -> bool {
    name.starts_with("awman-") || name.contains("nanoclaw")
}

/// Parse the JSON output of `container list --format json` into container
/// handles, filtering for running awman containers. When `name_prefix` is
/// `Some`, additionally require the container name to start with it — Apple
/// has no server-side name filter, so name-prefix discovery is done here,
/// client-side.
fn parse_apple_list_output(stdout: &str, name_prefix: Option<&str>) -> Vec<AgentHandle> {
    let arr: Result<Vec<serde_json::Value>, _> = serde_json::from_str(stdout);
    let rows: Vec<serde_json::Value> = match arr {
        Ok(v) => v,
        Err(_) => stdout
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect(),
    };
    let mut handles = Vec::new();
    for row in rows {
        if !is_apple_running(&row) {
            continue;
        }
        let name = extract_apple_name(&row);
        if !is_awman_container(&name) {
            continue;
        }
        if let Some(prefix) = name_prefix {
            if !name.starts_with(prefix) {
                continue;
            }
        }
        let id = name.clone();
        let image_tag = extract_apple_image(&row);
        let started_at = extract_apple_started_at(&row);
        if id.is_empty() && name.is_empty() {
            continue;
        }
        handles.push(AgentHandle {
            id,
            image_tag,
            name,
            started_at,
        });
    }
    handles
}

pub(super) struct AppleBackend {
    gated_provider: std::sync::OnceLock<
        Result<std::sync::Arc<dyn GatedProviderAdapter>, ProviderCliStartFailure>,
    >,
    #[cfg(test)]
    test_boundary:
        Option<std::sync::Arc<dyn super::orchestrated_apple::OrchestratedAppleTestBoundary>>,
}

impl Default for AppleBackend {
    fn default() -> Self {
        Self {
            gated_provider: std::sync::OnceLock::new(),
            #[cfg(test)]
            test_boundary: None,
        }
    }
}

impl AppleBackend {
    #[cfg(test)]
    pub(super) fn with_test_boundary(
        boundary: std::sync::Arc<dyn super::orchestrated_apple::OrchestratedAppleTestBoundary>,
    ) -> Self {
        Self {
            gated_provider: std::sync::OnceLock::from(Ok(boundary.provider())),
            test_boundary: Some(boundary),
        }
    }

    pub(super) fn build_native_codex_auth(
        &self,
        options: ResolvedContainerOptions,
        registry: Option<std::sync::Arc<LaunchRetentionRegistry>>,
        custody: std::sync::Arc<crate::engine::agent::native_codex_preparation::AuthCustody>,
    ) -> Result<Box<dyn AgentInstance>, EngineError> {
        self.build_common(options, registry, Some(custody))
    }

    fn build_common(
        &self,
        options: ResolvedContainerOptions,
        launch_retention: Option<std::sync::Arc<LaunchRetentionRegistry>>,
        native_auth: Option<
            std::sync::Arc<crate::engine::agent::native_codex_preparation::AuthCustody>,
        >,
    ) -> Result<Box<dyn AgentInstance>, EngineError> {
        let orchestrated = options
            .startup_gate
            .as_ref()
            .is_some_and(|gate| gate.control.orchestrated_parts().is_some());
        if orchestrated && launch_retention.is_none() {
            return Err(EngineError::Config(
                "orchestrated launch retention is unavailable".into(),
            ));
        }
        let gated_provider = if orchestrated {
            Some(
                self.gated_provider
                    .get_or_init(|| {
                        AppleGatedProviderAdapter::try_new()
                            .map(|adapter| adapter as std::sync::Arc<dyn GatedProviderAdapter>)
                    })
                    .clone()
                    .map_err(|_| {
                        EngineError::Config("Apple provider custody is unavailable".into())
                    })?,
            )
        } else {
            None
        };
        let image = options.image.clone().ok_or_else(|| {
            EngineError::ConflictingOptions("missing required Image option".into())
        })?;
        let name = options.name.clone().unwrap_or_else(|| {
            ContainerName::new(crate::engine::container::naming::generate_container_name())
        });
        let leases = register_container_leases(&options, &name.0);
        let cli = ContainerCli::APPLE;
        #[cfg(test)]
        let cli = self
            .test_boundary
            .as_ref()
            .map_or(cli, |boundary| cli.with_bin(boundary.executable()));
        let mut instance = ContainerInstance::new_with_orchestrated_launch(
            cli,
            image,
            name,
            options,
            leases,
            Some(serve_attach_socket),
            (launch_retention, gated_provider),
        );
        instance.native_auth = native_auth;
        #[cfg(test)]
        if let Some(boundary) = &self.test_boundary {
            instance.set_test_boundary(std::sync::Arc::clone(boundary));
        }
        Ok(Box::new(instance))
    }
}

#[cfg(test)]
#[allow(non_upper_case_globals)]
#[allow(clippy::declare_interior_mutable_const)]
const AppleBackend: AppleBackend = AppleBackend {
    gated_provider: std::sync::OnceLock::new(),
    test_boundary: None,
};

impl ContainerBackend for AppleBackend {
    fn build(
        &self,
        options: ResolvedContainerOptions,
    ) -> Result<Box<dyn AgentInstance>, EngineError> {
        self.build_common(options, None, None)
    }

    fn build_with_launch_retention(
        &self,
        options: ResolvedContainerOptions,
        launch_retention: Option<std::sync::Arc<LaunchRetentionRegistry>>,
    ) -> Result<Box<dyn AgentInstance>, EngineError> {
        self.build_common(options, launch_retention, None)
    }

    fn list_running(&self, _session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        let output = Command::new("container")
            .args(["list", "--format", "json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return Ok(Vec::new()),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_apple_list_output(&stdout, None))
    }

    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        let output = Command::new("container")
            .args(["list", "--format", "json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return Ok(Vec::new()),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_apple_list_output(&stdout, None))
    }

    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        let take_sample = |name: &str| -> Result<(u64, u64), EngineError> {
            let out = Command::new("container")
                .args(["stats", "--no-stream", "--format", "json", name])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        EngineError::ContainerRuntimeUnavailable {
                            binary: "container".into(),
                        }
                    } else {
                        EngineError::Container(format!("container stats: {e}"))
                    }
                })?;
            if !out.status.success() {
                return Err(EngineError::Container(format!(
                    "container stats failed for {}",
                    name
                )));
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            let value: serde_json::Value = serde_json::from_str(stdout.trim())
                .or_else(|_| {
                    stdout
                        .lines()
                        .next()
                        .ok_or_else(|| serde_json::Error::io(std::io::Error::other("empty")))
                        .and_then(serde_json::from_str)
                })
                .map_err(|e| {
                    EngineError::Container(format!("unparseable container stats output: {e}"))
                })?;
            let entry = match &value {
                serde_json::Value::Array(arr) => {
                    arr.first().cloned().unwrap_or(serde_json::Value::Null)
                }
                _ => value,
            };
            let cpu = entry
                .get("cpuUsageUsec")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let mem = entry
                .get("memoryUsageBytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Ok((cpu, mem))
        };

        let (cpu1, _) = take_sample(&handle.name)?;
        let t0 = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let (cpu2, mem) = take_sample(&handle.name)?;
        let elapsed_usec = t0.elapsed().as_micros() as u64;

        let cpu_delta = cpu2.saturating_sub(cpu1);
        let cpu_percent = if elapsed_usec > 0 {
            (cpu_delta as f64 / elapsed_usec as f64) * 100.0
        } else {
            0.0
        };
        let memory_mb = (mem as f64) / (1024.0 * 1024.0);

        Ok(AgentStats {
            name: handle.name.clone(),
            cpu_percent,
            memory_mb,
        })
    }

    fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        // Apple's `container` CLI has no `attach` subcommand, so reattach
        // goes through the awman-owned rendezvous instead: the process that
        // launched the container serves its live PTY on a per-container unix
        // socket (see `attach_socket.rs`), and this instance connects to it.
        // Docker keeps its native `docker attach`; the semantics are the same
        // either way — the real agent TTY, never a sibling shell.
        let path = crate::engine::container::attach_socket::attach_socket_path(&handle.name)
            .ok_or_else(|| {
                EngineError::Container(
                    "cannot resolve the attach socket directory (no home directory)".into(),
                )
            })?;
        Ok(Box::new(
            crate::engine::container::attach_socket::SocketAttachInstance {
                handle: handle.clone(),
                path,
            },
        ))
    }

    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError> {
        // Apple has no server-side name filter; list everything and filter by
        // the prefix client-side.
        let output = Command::new("container")
            .args(["list", "--format", "json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return Ok(Vec::new()),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_apple_list_output(&stdout, Some(prefix)))
    }

    fn name(&self) -> &'static str {
        "apple-containers"
    }

    fn image_home_dir(&self, tag: &str) -> Option<String> {
        // `container image inspect` emits a JSON array of variants; the env
        // list lives at `[0].variants[*].config.config.Env`. We pick the
        // first variant whose env contains a non-empty `HOME=…` entry, which
        // matches the runtime selection for single-platform images.
        let output = Command::new("container")
            .args(["image", "inspect", tag])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let arr: serde_json::Value = serde_json::from_str(&stdout).ok()?;
        let variants = arr.get(0)?.get("variants")?.as_array()?;
        for variant in variants {
            let env = variant
                .get("config")
                .and_then(|c| c.get("config"))
                .and_then(|c| c.get("Env"))
                .and_then(|v| v.as_array());
            let Some(env) = env else { continue };
            for entry in env {
                if let Some(rest) = entry.as_str().and_then(|s| s.strip_prefix("HOME=")) {
                    let v = rest.trim();
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
        None
    }
}

/// Resize the PTY behind an attach client's resize request, guaranteeing the
/// agent receives a SIGWINCH even when the requested size equals the PTY's
/// current size.
///
/// The attach rendezvous relies on the client's initial resize as its repaint
/// trigger (see `attach_socket.rs`): the agent gets WINCH, redraws, and the
/// fresh client's screen fills. But the kernel only delivers SIGWINCH when the
/// size actually *changes* — so a client reattaching at the same terminal
/// dimensions as a previous session (the ordinary detach → reattach flow)
/// would otherwise get a silent no-op resize, no repaint, and a blank screen
/// until the agent spontaneously produced output. Bounce through an
/// off-by-one-row size first so every attach-client resize repaints.
fn resize_pty_forcing_winch(master: &dyn portable_pty::MasterPty, cols: u16, rows: u16) {
    let unchanged = master
        .get_size()
        .map(|size| size.cols == cols && size.rows == rows)
        .unwrap_or(false);
    if unchanged {
        let _ = master.resize(portable_pty::PtySize {
            rows: if rows > 1 { rows - 1 } else { rows + 1 },
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }
    let _ = master.resize(portable_pty::PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    });
}

/// The Apple backend's `process::AttachHook`, run once the PTY bridge is up.
///
/// Apple's `container` CLI has no attach verb, so this process — the sole
/// holder of the container's PTY — is the attach rendezvous: it serves the
/// PTY over a per-container unix socket that `AppleBackend::attach` clients
/// connect to. The shared spawn path installs the output tap on the bridge
/// (`AttachHookCtx::output_broadcast`) and hands us the stdin injector and a
/// weak PTY master; everything below this line is Apple's alone.
///
/// Returns `None` — after a warning — when the socket cannot be created. The
/// container still runs; it simply cannot be attached to.
fn serve_attach_socket(ctx: AttachHookCtx<'_>) -> Option<AttachSocketGuard> {
    let AttachHookCtx {
        container_name,
        output_broadcast,
        stdin_injector,
        pty_master,
    } = ctx;

    // The master reference is weak: the attach server must never keep the PTY
    // alive past the execution backend that owns it.
    let resize: std::sync::Arc<dyn Fn(u16, u16) + Send + Sync> =
        std::sync::Arc::new(move |cols, rows| {
            if let Some(master) = pty_master.upgrade() {
                if let Ok(master) = master.lock() {
                    resize_pty_forcing_winch(master.as_ref(), cols, rows);
                }
            }
        });

    let path = match crate::engine::container::attach_socket::attach_socket_path(container_name) {
        Some(path) => path,
        None => {
            tracing::warn!(
                container = %container_name,
                "no home directory to place the attach socket in; the container \
                 runs but cannot be attached to"
            );
            return None;
        }
    };

    crate::engine::container::attach_socket::spawn_attach_socket_server(
        &path,
        crate::engine::container::attach_socket::AttachHooks {
            output: output_broadcast,
            stdin: stdin_injector,
            resize,
        },
    )
    .map_err(|error| {
        tracing::warn!(
            container = %container_name,
            error = %error,
            "attach socket unavailable; the container runs but cannot be attached to"
        );
    })
    .ok()
}

#[cfg(all(test, unix))]
#[path = "apple_cleanup_authority_test.rs"]
mod cleanup_authority_test;

#[cfg(test)]
#[path = "apple_gated_p2_test.rs"]
mod gated_p2;

#[cfg(test)]
mod apple_tests {
    use super::*;

    #[test]
    fn parse_apple_list_picks_up_running_awman_containers() {
        let json = r#"[
            {
                "status": "running",
                "configuration": {
                    "id": "awman-12345-999",
                    "image": {"repository": "awman/dev", "tag": "latest"}
                },
                "startedDate": 1715000000.0
            },
            {
                "status": "running",
                "configuration": {
                    "id": "awman-claws-controller",
                    "image": {"repository": "awman/dev", "tag": "latest"}
                },
                "startedDate": 1715000100.5
            },
            {
                "status": "stopped",
                "configuration": {
                    "id": "awman-old-stopped",
                    "image": {"repository": "awman/dev", "tag": "latest"}
                },
                "startedDate": 1714000000.0
            },
            {
                "status": "running",
                "configuration": {
                    "id": "unrelated-container",
                    "image": {"repository": "nginx", "tag": "latest"}
                },
                "startedDate": 1715000200.0
            }
        ]"#;
        let handles = parse_apple_list_output(json, None);
        assert_eq!(handles.len(), 2);
        assert_eq!(handles[0].name, "awman-12345-999");
        assert_eq!(handles[0].id, "awman-12345-999");
        assert_eq!(handles[1].name, "awman-claws-controller");
    }

    #[test]
    fn parse_apple_list_handles_nanoclaw_containers() {
        let json = r#"[{
            "status": "running",
            "configuration": {
                "id": "nanoclaw-worker-1",
                "image": {"repository": "awman/dev"}
            },
            "startedDate": 1715000000.0
        }]"#;
        let handles = parse_apple_list_output(json, None);
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].name, "nanoclaw-worker-1");
    }

    #[test]
    fn parse_apple_list_empty_array() {
        let handles = parse_apple_list_output("[]", None);
        assert!(handles.is_empty());
    }

    #[test]
    fn parse_apple_list_skips_non_running() {
        let json = r#"[{
            "status": "stopping",
            "configuration": { "id": "awman-dying" },
            "startedDate": 1715000000.0
        }]"#;
        let handles = parse_apple_list_output(json, None);
        assert!(handles.is_empty());
    }

    #[test]
    fn extract_apple_image_formats_repo_and_tag() {
        let row: serde_json::Value = serde_json::from_str(
            r#"{"configuration": {"image": {"repository": "awman/dev", "tag": "latest"}}}"#,
        )
        .unwrap();
        assert_eq!(extract_apple_image(&row), "awman/dev:latest");
    }

    #[test]
    fn extract_apple_image_repo_only_without_tag() {
        let row: serde_json::Value =
            serde_json::from_str(r#"{"configuration": {"image": {"repository": "awman/dev"}}}"#)
                .unwrap();
        assert_eq!(extract_apple_image(&row), "awman/dev");
    }

    #[test]
    fn extract_apple_image_plain_string() {
        let row: serde_json::Value =
            serde_json::from_str(r#"{"configuration": {"image": "awman/dev:latest"}}"#).unwrap();
        assert_eq!(extract_apple_image(&row), "awman/dev:latest");
    }

    #[test]
    fn extract_apple_image_reference_field() {
        let row: serde_json::Value = serde_json::from_str(
            r#"{"configuration": {"image": {"reference": "ghcr.io/awman/dev:latest"}}}"#,
        )
        .unwrap();
        assert_eq!(extract_apple_image(&row), "ghcr.io/awman/dev:latest");
    }

    #[test]
    fn extract_apple_image_descriptor_annotations() {
        let row: serde_json::Value = serde_json::from_str(
            r#"{
            "configuration": {
                "image": {
                    "descriptor": {
                        "annotations": {
                            "com.apple.containerization.image.name": "awman-myproj-claude:latest"
                        }
                    }
                }
            }
        }"#,
        )
        .unwrap();
        assert_eq!(extract_apple_image(&row), "awman-myproj-claude:latest");
    }

    #[test]
    fn parse_apple_list_formats_image_correctly() {
        let json = r#"[{
            "status": "running",
            "configuration": {
                "id": "awman-test",
                "image": {
                    "descriptor": {
                        "annotations": {
                            "com.apple.containerization.image.name": "awman-myproj-claude:latest"
                        }
                    }
                }
            },
            "startedDate": 1715000000.0
        }]"#;
        let handles = parse_apple_list_output(json, None);
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].image_tag, "awman-myproj-claude:latest");
    }

    #[test]
    fn image_home_dir_returns_none_for_unknown_image() {
        // `container image inspect` exits non-zero for an unknown tag, and
        // the helper must collapse that to `None` rather than panic — also
        // covers the case where the `container` CLI itself isn't installed.
        let backend = AppleBackend;
        let bogus = "awman-test-image-that-does-not-exist:tag-xyz123";
        assert!(backend.image_home_dir(bogus).is_none());
    }

    /// A same-size attach resize must still land on the requested size after
    /// its WINCH-forcing bounce, and a changed size must apply directly.
    #[test]
    #[cfg(unix)]
    fn resize_forcing_winch_always_lands_on_the_requested_size() {
        use portable_pty::{native_pty_system, PtySize};
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        // Unchanged size: the reattach case. The bounce must be invisible in
        // the final state.
        resize_pty_forcing_winch(pair.master.as_ref(), 80, 24);
        let size = pair.master.get_size().expect("get_size");
        assert_eq!((size.cols, size.rows), (80, 24));

        // Changed size: the ordinary case.
        resize_pty_forcing_winch(pair.master.as_ref(), 132, 50);
        let size = pair.master.get_size().expect("get_size");
        assert_eq!((size.cols, size.rows), (132, 50));
    }

    #[test]
    fn extract_apple_started_at_from_float() {
        let row: serde_json::Value =
            serde_json::from_str(r#"{"startedDate": 1715000000.5}"#).unwrap();
        let dt = extract_apple_started_at(&row);
        assert_eq!(dt.timestamp(), 1715000000);
    }
}
