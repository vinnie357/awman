//! Exact provider identity and durable pre-spawn launch planning.

mod child_lifecycle;
mod provider_cli;
mod retention;

#[allow(unused_imports)]
pub(crate) use child_lifecycle::{
    BindStartedChildError, ChildLifecycleAuthority, ChildLifecycleSlot, ChildLifecycleState,
    PreparedChildLifecycle, RetainedExecution, SpawnStageError, SpawnedCreateCli,
    UnboundStartedCli,
};
#[allow(unused_imports)]
pub(crate) use retention::{
    LaunchRetentionInitError, LaunchRetentionReason, LaunchRetentionRegistry,
    LaunchRetentionTicket, NotCreatedProof, RetainedAgentLaunch, REGISTRY_SHUTDOWN_GRACE,
};

#[cfg(test)]
pub(crate) use child_lifecycle::test_support as child_lifecycle_test_support;
#[cfg(test)]
pub(crate) use retention::test_support as retention_test_support;

#[allow(unused_imports)]
// Consumed when the later native-provider packet enables this private seam.
pub(crate) use provider_cli::{
    run_bounded_provider_cli, BoundedProviderOutput, ProviderCliCustodyRegistry,
    ProviderCliCustodyTicket, ProviderCliReapedFailure, ProviderCliReapedFailureKind,
    ProviderCliRunOutcome, ProviderCliStartFailure, RetainedProviderCli,
    RetainedProviderCliTermination, MAX_PROVIDER_CUSTODY_SHUTDOWN, MAX_PROVIDER_STDERR,
    MAX_PROVIDER_STDOUT,
};

use crate::data::container::ContainerName;
use crate::data::startup_gate::{
    revalidate_mount_source, GatedLaunchIdentity, OrchestratedControlAuthority, PinnedRegularFile,
    PlanFileError,
};
use crate::engine::container::options::ImageRef;
use crate::engine::error::EngineError;
use chrono::{DateTime, SecondsFormat, Timelike, Utc};
use serde::{Deserialize, Serialize};
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

#[must_use = "the validated barrier must be consumed by the matching spawn"]
pub(crate) struct ValidatedSpawnBarrier {
    _plan_file: Arc<PinnedRegularFile>,
    _controls: Arc<OrchestratedControlAuthority>,
    _key: ProviderLaunchKey,
    _absence: AbsenceObservation,
}

impl ValidatedSpawnBarrier {
    pub(crate) fn consume_for_spawn(
        self,
        plan: DurableLaunchPlan,
    ) -> Result<DurableLaunchPlan, LaunchIdentityError> {
        if !Arc::ptr_eq(&self._plan_file, &plan.plan_file)
            || !Arc::ptr_eq(&self._controls, &plan.controls)
            || self._key != plan.key
        {
            return Err(LaunchIdentityError::UnsafeControl);
        }
        Ok(plan)
    }
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

#[allow(dead_code)]
pub(crate) fn validate_immediate_pre_spawn(
    plan: &DurableLaunchPlan,
    adapter: &dyn GatedProviderAdapter,
    enclosing: Instant,
) -> Result<ValidatedSpawnBarrier, LaunchIdentityError> {
    if plan.controls.request.request_digest != plan.request_digest {
        return Err(LaunchIdentityError::UnsafeControl);
    }
    if adapter.provider() != plan.key.provider {
        return Err(LaunchIdentityError::ProviderInspectionUnavailable);
    }
    revalidate_mount_source(&plan.controls).map_err(|_| LaunchIdentityError::UnsafeControl)?;
    if !plan
        .controls
        .host_parent
        .verify_launch_plan(&plan.plan_file)
    {
        return Err(LaunchIdentityError::UnsafeControl);
    }
    let absence = match adapter.inspect_name_absence(
        &plan.key.container_name,
        unexpired_provider_call_deadline(enclosing)?,
    ) {
        Ok(absence) => absence,
        Err(NamePresence::Present) => return Err(LaunchIdentityError::NameCollision),
        Err(NamePresence::Ambiguous) => {
            return Err(LaunchIdentityError::ProviderInspectionUnavailable);
        }
        Err(NamePresence::Unavailable) => {
            return Err(LaunchIdentityError::ProviderInspectionUnavailable);
        }
    };
    if absence.provider != plan.key.provider || absence.exact_name != plan.key.container_name {
        return Err(LaunchIdentityError::ProviderInspectionUnavailable);
    }
    revalidate_mount_source(&plan.controls).map_err(|_| LaunchIdentityError::UnsafeControl)?;
    if !plan
        .controls
        .host_parent
        .verify_launch_plan(&plan.plan_file)
    {
        return Err(LaunchIdentityError::UnsafeControl);
    }
    Ok(ValidatedSpawnBarrier {
        _plan_file: Arc::clone(&plan.plan_file),
        _controls: Arc::clone(&plan.controls),
        _key: plan.key.clone(),
        _absence: absence,
    })
}

pub(crate) fn revalidate_post_spawn(plan: &DurableLaunchPlan) -> Result<(), LaunchIdentityError> {
    revalidate_mount_source(&plan.controls).map_err(|_| LaunchIdentityError::UnsafeControl)?;
    if plan.controls.request.request_digest != plan.request_digest
        || !plan
            .controls
            .host_parent
            .verify_launch_plan(&plan.plan_file)
    {
        return Err(LaunchIdentityError::UnsafeControl);
    }
    Ok(())
}

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum StartedCreateObservation {
    Matching(ProviderLaunchInspection),
    Retained(LaunchRetentionReason),
}

#[allow(dead_code)]
pub(crate) fn observe_started_create(
    plan: &DurableLaunchPlan,
    adapter: &dyn GatedProviderAdapter,
    lifecycle: &ChildLifecycleAuthority,
    enclosing: Instant,
) -> StartedCreateObservation {
    if Instant::now() >= enclosing {
        return StartedCreateObservation::Retained(LaunchRetentionReason::SpawnResultUnknown);
    }
    match lifecycle.state() {
        Ok(ChildLifecycleState::Running) => {}
        Ok(ChildLifecycleState::Exited(info)) if info.exit_code != 0 => {
            return StartedCreateObservation::Retained(LaunchRetentionReason::SpawnResultUnknown);
        }
        Ok(ChildLifecycleState::Exited(_)) => {}
        _ => {
            return StartedCreateObservation::Retained(LaunchRetentionReason::ChildStateUnknown);
        }
    }
    let deadline = provider_call_deadline(enclosing);
    match adapter.inspect_launch(&plan.key, deadline) {
        ExactInspection::Matching(inspection) => {
            if matches!(lifecycle.state(), Ok(ChildLifecycleState::Running)) {
                StartedCreateObservation::Matching(inspection)
            } else {
                StartedCreateObservation::Retained(LaunchRetentionReason::SpawnResultUnknown)
            }
        }
        ExactInspection::Unavailable => {
            StartedCreateObservation::Retained(LaunchRetentionReason::InspectionUnavailable)
        }
        ExactInspection::Absent(_) | ExactInspection::ForeignOrAmbiguous => {
            StartedCreateObservation::Retained(LaunchRetentionReason::SpawnResultUnknown)
        }
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

fn valid_docker_runtime_id(value: &str) -> bool {
    parse_lower_hex_32(value).is_some()
        || value
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

#[derive(Deserialize)]
struct DockerImageInspection {
    #[serde(rename = "Id")]
    id: String,
}

fn normalize_docker_image_id(value: &str) -> Result<ImmutableImageId, LaunchIdentityError> {
    if !value
        .as_bytes()
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"sha256:"))
    {
        return Err(LaunchIdentityError::ImageIdentityMismatch);
    }
    let digest = &value[7..];
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(LaunchIdentityError::ImageIdentityMismatch);
    }
    ImmutableImageId::new(format!("sha256:{}", digest.to_ascii_lowercase()))
}

#[allow(dead_code)]
pub(crate) fn parse_gated_docker_image_inspection(
    bytes: &[u8],
) -> Result<ImmutableImageId, LaunchIdentityError> {
    let mut records: Vec<DockerImageInspection> =
        serde_json::from_slice(bytes).map_err(|error| {
            use serde_json::error::Category;
            match error.classify() {
                Category::Syntax | Category::Eof | Category::Io => {
                    LaunchIdentityError::ProviderInspectionUnavailable
                }
                Category::Data => LaunchIdentityError::ImageIdentityMismatch,
            }
        })?;
    if records.len() != 1 {
        return Err(LaunchIdentityError::ImageIdentityMismatch);
    }
    normalize_docker_image_id(&records.remove(0).id)
}

#[derive(Deserialize)]
struct DockerLaunchInspection {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Image")]
    image: String,
    #[serde(rename = "Created")]
    created: String,
    #[serde(rename = "State")]
    state: DockerInspectionState,
    #[serde(rename = "Config")]
    config: DockerInspectionConfig,
}

#[derive(Deserialize)]
struct DockerInspectionState {
    #[serde(rename = "Status")]
    status: String,
}

#[derive(Deserialize)]
struct DockerInspectionConfig {
    #[serde(rename = "Labels")]
    labels: DockerInspectionLabels,
}

struct DockerInspectionLabels(std::collections::BTreeMap<String, String>);

impl<'de> Deserialize<'de> for DockerInspectionLabels {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct LabelsVisitor;

        impl<'de> serde::de::Visitor<'de> for LabelsVisitor {
            type Value = DockerInspectionLabels;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a Docker label object without duplicate keys")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut labels = std::collections::BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, String>()? {
                    if labels.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate Docker label"));
                    }
                }
                Ok(DockerInspectionLabels(labels))
            }
        }

        deserializer.deserialize_map(LabelsVisitor)
    }
}

fn docker_state(value: &str) -> Option<DockerProviderState> {
    match value {
        "created" => Some(DockerProviderState::Created),
        "running" => Some(DockerProviderState::Running),
        "paused" => Some(DockerProviderState::Paused),
        "restarting" => Some(DockerProviderState::Restarting),
        "removing" => Some(DockerProviderState::Removing),
        "exited" => Some(DockerProviderState::Exited),
        "dead" => Some(DockerProviderState::Dead),
        _ => None,
    }
}

#[allow(dead_code)]
pub(crate) fn parse_gated_docker_launch_inspection(
    bytes: &[u8],
    key: &ProviderLaunchKey,
) -> ExactInspection {
    let records: Vec<DockerLaunchInspection> = match serde_json::from_slice(bytes) {
        Ok(records) => records,
        Err(error) => {
            return match error.classify() {
                serde_json::error::Category::Syntax
                | serde_json::error::Category::Eof
                | serde_json::error::Category::Io => ExactInspection::Unavailable,
                serde_json::error::Category::Data => ExactInspection::ForeignOrAmbiguous,
            };
        }
    };
    if key.provider != ProviderKind::Docker {
        return ExactInspection::ForeignOrAmbiguous;
    }
    if records.is_empty() {
        let checked_at = Utc::now();
        let revision = canonical_inspection_revision(CanonicalInspectionRevisionInput {
            kind: InspectionObservationKind::Absent,
            provider: ProviderKind::Docker,
            exact_name: &key.container_name,
            runtime_id: None,
            token_digest: None,
            immutable_image_id: None,
            created_at: None,
            state: SanitizedProviderStateObservation::Absent,
        });
        return ExactInspection::Absent(AbsenceObservation {
            provider: ProviderKind::Docker,
            exact_name: key.container_name.clone(),
            checked_at,
            revision,
        });
    }
    if records.len() != 1 {
        return ExactInspection::ForeignOrAmbiguous;
    }
    let record = &records[0];
    let expected_name = format!("/{}", key.container_name.as_str());
    let image = match normalize_docker_image_id(&record.image) {
        Ok(image) => image,
        Err(_) => return ExactInspection::ForeignOrAmbiguous,
    };
    let created_at = match DateTime::parse_from_rfc3339(&record.created) {
        Ok(created) => created.with_timezone(&Utc),
        Err(_) => return ExactInspection::ForeignOrAmbiguous,
    };
    let token_digest = match record
        .config
        .labels
        .0
        .get("dev.awman.orchestrator-launch")
        .and_then(|value| parse_lower_hex_32(value))
    {
        Some(digest) => digest,
        None => return ExactInspection::ForeignOrAmbiguous,
    };
    if !valid_docker_runtime_id(&record.id)
        || record.name != expected_name
        || image != key.immutable_image_id
        || token_digest != key.token_digest
        || created_at < key.created_not_before
    {
        return ExactInspection::ForeignOrAmbiguous;
    }
    let state = match docker_state(&record.state.status) {
        Some(state) => state,
        None => {
            let digest = Sha256::digest(record.state.status.as_bytes()).into();
            let _ = canonical_inspection_revision(CanonicalInspectionRevisionInput {
                kind: InspectionObservationKind::PresentUnusable,
                provider: ProviderKind::Docker,
                exact_name: &key.container_name,
                runtime_id: Some(&record.id),
                token_digest: Some(token_digest),
                immutable_image_id: Some(&image),
                created_at: Some(created_at),
                state: SanitizedProviderStateObservation::UnrecognizedDigest(digest),
            });
            return ExactInspection::ForeignOrAmbiguous;
        }
    };
    let provider_state = ProviderState::Docker(state);
    let revision = canonical_inspection_revision(CanonicalInspectionRevisionInput {
        kind: InspectionObservationKind::Matching,
        provider: ProviderKind::Docker,
        exact_name: &key.container_name,
        runtime_id: Some(&record.id),
        token_digest: Some(token_digest),
        immutable_image_id: Some(&image),
        created_at: Some(created_at),
        state: SanitizedProviderStateObservation::Known(provider_state.clone()),
    });
    ExactInspection::Matching(ProviderLaunchInspection {
        provider: ProviderKind::Docker,
        runtime_id: record.id.clone(),
        exact_name: key.container_name.clone(),
        token_digest,
        immutable_image_id: image,
        created_at,
        state: provider_state,
        revision,
    })
}

#[cfg(test)]
#[path = "gated_launch_p2_test.rs"]
mod p2;
