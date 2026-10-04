//! Exact provider identity and durable pre-spawn launch planning.

use crate::data::container::ContainerName;
use crate::data::startup_gate::{
    revalidate_mount_source, GatedLaunchIdentity, OrchestratedControlAuthority, PinnedRegularFile,
    PlanFileError,
};
use crate::engine::container::options::ImageRef;
use crate::engine::error::EngineError;
use chrono::{DateTime, SecondsFormat, Timelike, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub(crate) enum ProviderKind {
    Docker,
    AppleContainers,
}

impl ProviderKind {
    fn plan_name(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::AppleContainers => "apple-containers",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum DockerProviderState {
    Created,
    Running,
    Paused,
    Restarting,
    Removing,
    Exited,
    Dead,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AppleProviderState {
    Running,
    Stopped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum ProviderState {
    Docker(DockerProviderState),
    Apple(AppleProviderState),
}

#[derive(Clone, Eq, PartialEq, Hash)]
pub(crate) struct ImmutableImageId(String);

impl ImmutableImageId {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, LaunchIdentityError> {
        let value = value.into();
        if valid_sha256_id(&value) {
            Ok(Self(value))
        } else {
            Err(LaunchIdentityError::ImageIdentityMismatch)
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ImmutableImageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ImmutableImageId")
            .field(&self.0)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ProviderLaunchKey {
    pub provider: ProviderKind,
    pub container_name: ContainerName,
    pub token_digest: [u8; 32],
    pub immutable_image_id: ImmutableImageId,
    pub created_not_before: DateTime<Utc>,
}

impl fmt::Debug for ProviderLaunchKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderLaunchKey")
            .field("provider", &self.provider)
            .field("container_name", &self.container_name)
            .field("immutable_image_id", &self.immutable_image_id)
            .field("created_not_before", &self.created_not_before)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub(crate) struct InspectionRevision([u8; 32]);

impl InspectionRevision {
    #[allow(dead_code)]
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for InspectionRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("InspectionRevision([redacted])")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum SanitizedProviderStateObservation {
    Absent,
    Known(ProviderState),
    UnrecognizedDigest([u8; 32]),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum InspectionObservationKind {
    Absent,
    Matching,
    PresentUnusable,
}

pub(crate) struct CanonicalInspectionRevisionInput<'a> {
    pub kind: InspectionObservationKind,
    pub provider: ProviderKind,
    pub exact_name: &'a ContainerName,
    pub runtime_id: Option<&'a str>,
    pub token_digest: Option<[u8; 32]>,
    pub immutable_image_id: Option<&'a ImmutableImageId>,
    pub created_at: Option<DateTime<Utc>>,
    pub state: SanitizedProviderStateObservation,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ProviderLaunchInspection {
    pub provider: ProviderKind,
    pub runtime_id: String,
    pub exact_name: ContainerName,
    pub token_digest: [u8; 32],
    pub immutable_image_id: ImmutableImageId,
    pub created_at: DateTime<Utc>,
    pub state: ProviderState,
    pub revision: InspectionRevision,
}

impl fmt::Debug for ProviderLaunchInspection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderLaunchInspection")
            .field("provider", &self.provider)
            .field("runtime_id", &self.runtime_id)
            .field("exact_name", &self.exact_name)
            .field("immutable_image_id", &self.immutable_image_id)
            .field("created_at", &self.created_at)
            .field("state", &self.state)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) struct AbsenceObservation {
    pub provider: ProviderKind,
    pub exact_name: ContainerName,
    pub checked_at: DateTime<Utc>,
    pub revision: InspectionRevision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum ExactInspection {
    Absent(AbsenceObservation),
    Matching(ProviderLaunchInspection),
    ForeignOrAmbiguous,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Provider adapter constructs these after AgentEngine removes its native-launch rejection.
pub(crate) enum NamePresence {
    Present,
    Ambiguous,
    Unavailable,
}

#[derive(Clone, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) struct CreateCliExitEvidence {
    pub provider: ProviderKind,
    pub exit_code: i32,
    pub diagnostic: BoundedCreateDiagnostic,
}

#[derive(Clone, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) struct BoundedCreateDiagnostic(Vec<u8>);

impl BoundedCreateDiagnostic {
    #[allow(dead_code)]
    pub(crate) fn new(bytes: &[u8]) -> Self {
        Self(bytes[..bytes.len().min(4096)].to_vec())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum CreateCliExitClass {
    ExplicitNameCollision,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProviderCallDeadline(Instant);

impl ProviderCallDeadline {
    #[allow(dead_code)]
    pub(crate) fn instant(self) -> Instant {
        self.0
    }
}

pub(crate) const MAX_PROVIDER_CALL: Duration = Duration::from_secs(10);

pub(crate) fn provider_call_deadline(enclosing: Instant) -> ProviderCallDeadline {
    ProviderCallDeadline(std::cmp::min(enclosing, Instant::now() + MAX_PROVIDER_CALL))
}

#[allow(dead_code)]
pub(crate) trait GatedProviderAdapter: Send + Sync {
    fn provider(&self) -> ProviderKind;

    fn resolve_image_identity(
        &self,
        image: &ImageRef,
        deadline: ProviderCallDeadline,
    ) -> Result<ImmutableImageId, LaunchIdentityError>;

    fn inspect_name_absence(
        &self,
        name: &ContainerName,
        deadline: ProviderCallDeadline,
    ) -> Result<AbsenceObservation, NamePresence>;

    fn inspect_launch(
        &self,
        key: &ProviderLaunchKey,
        deadline: ProviderCallDeadline,
    ) -> ExactInspection;

    fn classify_create_exit(&self, exit: &CreateCliExitEvidence) -> CreateCliExitClass;

    fn stop_inspected(
        &self,
        inspection: &ProviderLaunchInspection,
        deadline: ProviderCallDeadline,
    ) -> Result<(), EngineError>;

    fn remove_inspected(
        &self,
        inspection: &ProviderLaunchInspection,
        deadline: ProviderCallDeadline,
    ) -> Result<(), EngineError>;
}

#[derive(Clone)]
pub(crate) struct DurableLaunchPlan {
    pub key: ProviderLaunchKey,
    #[allow(dead_code)]
    // Pre-spawn barrier reads this after AgentEngine removes its native-launch rejection.
    pub request_digest: [u8; 32],
    pub controls: Arc<OrchestratedControlAuthority>,
    plan_file: Arc<PinnedRegularFile>,
}

impl fmt::Debug for DurableLaunchPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableLaunchPlan")
            .field("key", &self.key)
            .field("controls", &self.controls)
            .field("plan_file", &self.plan_file)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[allow(dead_code)]
pub(crate) enum LaunchIdentityError {
    #[error("invalid launch intention at {line}:{column}")]
    InvalidIntent { line: usize, column: usize },
    #[error("unsafe orchestrated launch control")]
    UnsafeControl,
    #[error("existing launch plan requires recovery")]
    ExistingPlanRecoveryRequired,
    #[error("container name collision")]
    NameCollision,
    #[error("provider inspection unavailable")]
    ProviderInspectionUnavailable,
    #[error("provider inspection unsupported")]
    UnsupportedProviderInspection,
    #[error("immutable image identity mismatch")]
    ImageIdentityMismatch,
    #[error("launch plan publication failed")]
    LaunchPlanPublicationFailed,
    #[error("launch receipt failed")]
    LaunchReceiptFailed,
}

#[derive(Serialize)]
struct LaunchPlanRecord<'a> {
    version: u32,
    provider: &'a str,
    name: &'a str,
    #[serde(rename = "tokenDigest")]
    token_digest: String,
    #[serde(rename = "imageId")]
    image_id: &'a str,
    #[serde(rename = "createdNotBefore")]
    created_not_before: String,
    #[serde(rename = "requestDigest")]
    request_digest: String,
}

#[allow(dead_code)]
pub(crate) fn prepare_orchestrated_launch(
    adapter: &dyn GatedProviderAdapter,
    image: &ImageRef,
    identity: &GatedLaunchIdentity,
    controls: Arc<OrchestratedControlAuthority>,
    deadline: Instant,
) -> Result<DurableLaunchPlan, LaunchIdentityError> {
    if controls.request.request_digest != identity.request_digest {
        return Err(LaunchIdentityError::UnsafeControl);
    }
    revalidate_mount_source(&controls).map_err(|_| LaunchIdentityError::UnsafeControl)?;
    match controls.host_parent.existing_launch_plan() {
        Ok(None) => {}
        Ok(Some(_)) | Err(PlanFileError::ExistingOrUnsafe) => {
            return Err(LaunchIdentityError::ExistingPlanRecoveryRequired);
        }
        Err(PlanFileError::PublicationFailed) => {
            return Err(LaunchIdentityError::ExistingPlanRecoveryRequired);
        }
    }

    let provider = adapter.provider();
    let immutable_image_id =
        adapter.resolve_image_identity(image, unexpired_provider_call_deadline(deadline)?)?;
    let absence = match adapter.inspect_name_absence(
        &identity.container_name,
        unexpired_provider_call_deadline(deadline)?,
    ) {
        Ok(absence) => absence,
        Err(NamePresence::Present) => return Err(LaunchIdentityError::NameCollision),
        Err(NamePresence::Ambiguous) => {
            return Err(LaunchIdentityError::UnsupportedProviderInspection);
        }
        Err(NamePresence::Unavailable) => {
            return Err(LaunchIdentityError::ProviderInspectionUnavailable);
        }
    };
    if absence.provider != provider || absence.exact_name != identity.container_name {
        return Err(LaunchIdentityError::UnsupportedProviderInspection);
    }

    revalidate_mount_source(&controls).map_err(|_| LaunchIdentityError::UnsafeControl)?;

    let created_not_before = Utc::now()
        .with_nanosecond(0)
        .ok_or(LaunchIdentityError::LaunchPlanPublicationFailed)?;
    let key = ProviderLaunchKey {
        provider,
        container_name: identity.container_name.clone(),
        token_digest: identity.token_digest,
        immutable_image_id,
        created_not_before,
    };
    let record = LaunchPlanRecord {
        version: 1,
        provider: key.provider.plan_name(),
        name: key.container_name.as_str(),
        token_digest: lower_hex(&key.token_digest),
        image_id: key.immutable_image_id.as_str(),
        created_not_before: key
            .created_not_before
            .to_rfc3339_opts(SecondsFormat::Secs, true),
        request_digest: lower_hex(&identity.request_digest),
    };
    let mut bytes = serde_json::to_vec(&record)
        .map_err(|_| LaunchIdentityError::LaunchPlanPublicationFailed)?;
    bytes.push(b'\n');
    let plan_file =
        controls
            .host_parent
            .publish_launch_plan(&bytes)
            .map_err(|error| match error {
                PlanFileError::ExistingOrUnsafe => {
                    LaunchIdentityError::ExistingPlanRecoveryRequired
                }
                PlanFileError::PublicationFailed => {
                    LaunchIdentityError::LaunchPlanPublicationFailed
                }
            })?;
    if revalidate_mount_source(&controls).is_err()
        || !controls.host_parent.verify_launch_plan(&plan_file)
    {
        return Err(LaunchIdentityError::LaunchPlanPublicationFailed);
    }
    Ok(DurableLaunchPlan {
        key,
        request_digest: identity.request_digest,
        controls,
        plan_file: Arc::new(plan_file),
    })
}

fn unexpired_provider_call_deadline(
    enclosing: Instant,
) -> Result<ProviderCallDeadline, LaunchIdentityError> {
    if Instant::now() >= enclosing {
        Err(LaunchIdentityError::ProviderInspectionUnavailable)
    } else {
        Ok(provider_call_deadline(enclosing))
    }
}

pub(crate) fn canonical_inspection_revision(
    input: CanonicalInspectionRevisionInput<'_>,
) -> InspectionRevision {
    #[derive(Serialize)]
    struct RevisionRecord<'a> {
        version: u32,
        provider: &'a str,
        #[serde(rename = "exactName")]
        exact_name: &'a str,
        #[serde(rename = "runtimeId")]
        runtime_id: Option<&'a str>,
        #[serde(rename = "tokenDigest")]
        token_digest: Option<String>,
        #[serde(rename = "imageId")]
        image_id: Option<&'a str>,
        #[serde(rename = "createdAt")]
        created_at: Option<String>,
        state: RevisionState,
        #[serde(rename = "observationKind")]
        observation_kind: &'a str,
    }

    #[derive(Serialize)]
    #[serde(tag = "kind")]
    enum RevisionState {
        #[serde(rename = "absent")]
        Absent,
        #[serde(rename = "known")]
        Known { value: &'static str },
        #[serde(rename = "unrecognized-digest")]
        UnrecognizedDigest { sha256: String },
    }

    let state = match input.state {
        SanitizedProviderStateObservation::Absent => RevisionState::Absent,
        SanitizedProviderStateObservation::Known(state) => RevisionState::Known {
            value: provider_state_name(&state),
        },
        SanitizedProviderStateObservation::UnrecognizedDigest(digest) => {
            RevisionState::UnrecognizedDigest {
                sha256: lower_hex(&digest),
            }
        }
    };
    let record = RevisionRecord {
        version: 1,
        provider: input.provider.plan_name(),
        exact_name: input.exact_name.as_str(),
        runtime_id: input.runtime_id,
        token_digest: input.token_digest.map(|digest| lower_hex(&digest)),
        image_id: input.immutable_image_id.map(ImmutableImageId::as_str),
        created_at: input
            .created_at
            .map(|created| created.to_rfc3339_opts(SecondsFormat::AutoSi, true)),
        state,
        observation_kind: match input.kind {
            InspectionObservationKind::Absent => "absent",
            InspectionObservationKind::Matching => "matching",
            InspectionObservationKind::PresentUnusable => "present-unusable",
        },
    };
    let bytes = match serde_json::to_vec(&record) {
        Ok(bytes) => bytes,
        Err(_) => unreachable!("canonical revision record contains only serializable values"),
    };
    InspectionRevision(Sha256::digest(bytes).into())
}

fn provider_state_name(state: &ProviderState) -> &'static str {
    match state {
        ProviderState::Apple(AppleProviderState::Running) => "running",
        ProviderState::Apple(AppleProviderState::Stopped) => "stopped",
        ProviderState::Docker(DockerProviderState::Created) => "created",
        ProviderState::Docker(DockerProviderState::Running) => "running",
        ProviderState::Docker(DockerProviderState::Paused) => "paused",
        ProviderState::Docker(DockerProviderState::Restarting) => "restarting",
        ProviderState::Docker(DockerProviderState::Removing) => "removing",
        ProviderState::Docker(DockerProviderState::Exited) => "exited",
        ProviderState::Docker(DockerProviderState::Dead) => "dead",
    }
}

pub(crate) fn lower_hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn parse_lower_hex_32(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut decoded = [0u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        decoded[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
    }
    Some(decoded)
}

fn valid_sha256_id(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .and_then(parse_lower_hex_32)
        .is_some()
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => 0,
    }
}

#[cfg(test)]
#[path = "gated_launch_p2_test.rs"]
mod p2;
