use std::collections::VecDeque;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use super::gated_launch::{
    canonical_inspection_revision, parse_gated_docker_image_inspection,
    parse_gated_docker_launch_inspection, provider_call_deadline, run_bounded_provider_cli,
    validate_immediate_pre_spawn, AbsenceObservation, CanonicalInspectionRevisionInput,
    CreateCliExitClass, CreateCliExitEvidence, DockerProviderState, DurableLaunchPlan,
    ExactInspection, GatedProviderAdapter, ImmutableImageId, InspectionObservationKind,
    InspectionRevision, LaunchIdentityError, NamePresence, ProviderCallDeadline,
    ProviderCliCustodyRegistry, ProviderCliReapedFailureKind, ProviderCliRunOutcome,
    ProviderCliStartFailure, ProviderKind, ProviderLaunchInspection, ProviderLaunchKey,
    ProviderState, SanitizedProviderStateObservation, MAX_PROVIDER_STDERR, MAX_PROVIDER_STDOUT,
};
use super::options::{ContainerName, ImageRef};
use crate::data::startup_gate::load_startup_gate;
use crate::engine::error::EngineError;

const IMAGE_HEX: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_IMAGE_HEX: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const CONTAINER_HEX: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const NAME: &str = "awman-p2b-exact";
const CONTROL_NAME: &str = "awman-altana-p2b-exact";

fn token_hex(token: [u8; 32]) -> String {
    token.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn digest32(value: &str) -> [u8; 32] {
    assert_eq!(value.len(), 64);
    let mut output = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair).expect("digest fixture is ASCII");
        output[index] = u8::from_str_radix(text, 16).expect("digest fixture is hexadecimal");
    }
    output
}

fn image_document(id: &str, reference: &str, ignored: &str) -> Vec<u8> {
    format!(r#"[{{"Id":"{id}","RepoTags":["{reference}"],"ignored":{ignored}}}]"#).into_bytes()
}

fn launch_document(
    image: &str,
    reference: &str,
    runtime_id: &str,
    token: [u8; 32],
    ignored: &str,
) -> Vec<u8> {
    format!(
        r#"[{{"Id":"{runtime_id}","Name":"/{NAME}","Image":"{image}","Created":"2026-10-04T00:20:57.123456789Z","State":{{"Status":"running"}},"Config":{{"Image":"{reference}","Labels":{{"dev.awman.orchestrator-launch":"{}"}}}},"ignored":{ignored}}}]"#,
        token_hex(token),
    )
    .into_bytes()
}

fn launch_key(token: [u8; 32]) -> ProviderLaunchKey {
    let image = parse_gated_docker_image_inspection(&image_document(
        &format!("SHA256:{IMAGE_HEX}"),
        "registry.example/agent:mutable",
        "true",
    ))
    .expect("valid Docker image inspection");
    ProviderLaunchKey {
        provider: ProviderKind::Docker,
        container_name: ContainerName::new(NAME),
        token_digest: token,
        immutable_image_id: image,
        created_not_before: DateTime::parse_from_rfc3339("2026-10-04T00:20:56Z")
            .expect("fixed RFC3339 timestamp")
            .with_timezone(&Utc),
    }
}

fn matching_inspection(exact: ExactInspection) -> ProviderLaunchInspection {
    match exact {
        ExactInspection::Matching(inspection) => inspection,
        ExactInspection::Absent(_) => panic!("matching Docker fixture was classified absent"),
        ExactInspection::ForeignOrAmbiguous => {
            panic!("matching Docker fixture was classified foreign")
        }
        ExactInspection::Unavailable => panic!("matching Docker fixture was unavailable"),
    }
}

#[test]
fn docker_image_id_and_container_image_normalize_as_the_same_immutable_identity() {
    let token = [0x17; 32];
    let key = launch_key(token);
    let launch = matching_inspection(parse_gated_docker_launch_inspection(
        &launch_document(
            &format!("sha256:{IMAGE_HEX}"),
            "registry.example/agent:a-different-reference",
            &format!("sha256:{CONTAINER_HEX}"),
            token,
            r#"{"providerNoise":"not-part-of-identity"}"#,
        ),
        &key,
    ));

    assert_eq!(launch.provider, ProviderKind::Docker);
    assert!(launch.exact_name == key.container_name);
    assert_eq!(launch.token_digest, token);
    assert!(launch.immutable_image_id == key.immutable_image_id);
    assert_eq!(
        launch.state,
        ProviderState::Docker(DockerProviderState::Running)
    );
}

#[test]
fn docker_reference_match_cannot_mask_container_image_identity_mismatch() {
    let token = [0x28; 32];
    let key = launch_key(token);
    let exact = parse_gated_docker_launch_inspection(
        &launch_document(
            &format!("sha256:{OTHER_IMAGE_HEX}"),
            "registry.example/agent:mutable",
            &format!("sha256:{CONTAINER_HEX}"),
            token,
            "null",
        ),
        &key,
    );
    assert!(matches!(exact, ExactInspection::ForeignOrAmbiguous));
}

#[test]
fn canonical_revision_ignores_raw_json_but_changes_with_normalized_tuple() {
    let token = [0x39; 32];
    let key = launch_key(token);
    let first = matching_inspection(parse_gated_docker_launch_inspection(
        &launch_document(
            &format!("sha256:{IMAGE_HEX}"),
            "first.example/agent:tag",
            &format!("sha256:{CONTAINER_HEX}"),
            token,
            r#"{"large":"raw provider text one"}"#,
        ),
        &key,
    ));
    let same_tuple = matching_inspection(parse_gated_docker_launch_inspection(
        &launch_document(
            &format!("sha256:{IMAGE_HEX}"),
            "second.example/renamed:latest",
            &format!("sha256:{CONTAINER_HEX}"),
            token,
            r#"["different","raw","json"]"#,
        ),
        &key,
    ));
    let changed_runtime = matching_inspection(parse_gated_docker_launch_inspection(
        &launch_document(
            &format!("sha256:{IMAGE_HEX}"),
            "first.example/agent:tag",
            &format!(
                "sha256:{}",
                "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
            ),
            token,
            "false",
        ),
        &key,
    ));

    assert!(first.revision == same_tuple.revision);
    assert!(first.revision != changed_runtime.revision);
    assert_eq!(
        first.revision.as_bytes(),
        &digest32("fca83f6f2c2b0f0a866605cd661563b91842647f4bdce798dcfb1ca962fd2ebd")
    );
}

fn unknown_state_revision(key: &ProviderLaunchKey, state_bytes: &[u8]) -> InspectionRevision {
    let state_digest: [u8; 32] = Sha256::digest(state_bytes).into();
    canonical_inspection_revision(CanonicalInspectionRevisionInput {
        kind: InspectionObservationKind::PresentUnusable,
        provider: ProviderKind::Docker,
        exact_name: &key.container_name,
        runtime_id: Some("sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"),
        token_digest: Some(key.token_digest),
        immutable_image_id: Some(&key.immutable_image_id),
        created_at: Some(
            DateTime::parse_from_rfc3339("2026-10-04T00:20:57Z")
                .expect("fixed RFC3339 timestamp")
                .with_timezone(&Utc),
        ),
        state: SanitizedProviderStateObservation::UnrecognizedDigest(state_digest),
    })
}

#[derive(Clone, Copy)]
enum AbsenceReply {
    Exact,
    WrongName,
    WrongProvider,
    Present,
    Ambiguous,
    Unavailable,
}

struct BarrierAdapter {
    image: ImmutableImageId,
    replies: Mutex<VecDeque<AbsenceReply>>,
    absence_calls: std::sync::atomic::AtomicUsize,
}

impl BarrierAdapter {
    fn new(second: AbsenceReply) -> Self {
        let image = parse_gated_docker_image_inspection(&image_document(
            &format!("sha256:{IMAGE_HEX}"),
            "unused-by-apple-fake:latest",
            "null",
        ))
        .expect("fixed immutable image fixture");
        Self {
            image,
            replies: Mutex::new(VecDeque::from([AbsenceReply::Exact, second])),
            absence_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn absence_calls(&self) -> usize {
        self.absence_calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn absence(&self, name: &ContainerName, reply: AbsenceReply) -> AbsenceObservation {
        let exact_name = match reply {
            AbsenceReply::WrongName => ContainerName::new("awman-p2b-foreign-name"),
            _ => name.clone(),
        };
        let provider = match reply {
            AbsenceReply::WrongProvider => ProviderKind::Docker,
            _ => ProviderKind::AppleContainers,
        };
        let revision = canonical_inspection_revision(CanonicalInspectionRevisionInput {
            kind: InspectionObservationKind::Absent,
            provider,
            exact_name: &exact_name,
            runtime_id: None,
            token_digest: None,
            immutable_image_id: None,
            created_at: None,
            state: SanitizedProviderStateObservation::Absent,
        });
        AbsenceObservation {
            provider,
            exact_name,
            checked_at: Utc::now(),
            revision,
        }
    }
}

impl GatedProviderAdapter for BarrierAdapter {
    fn provider(&self) -> ProviderKind {
        ProviderKind::AppleContainers
    }

    fn resolve_image_identity(
        &self,
        _image: &ImageRef,
        _deadline: ProviderCallDeadline,
    ) -> Result<ImmutableImageId, LaunchIdentityError> {
        Ok(self.image.clone())
    }

    fn inspect_name_absence(
        &self,
        name: &ContainerName,
        _deadline: ProviderCallDeadline,
    ) -> Result<AbsenceObservation, NamePresence> {
        self.absence_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let reply = self
            .replies
            .lock()
            .expect("absence response mutex")
            .pop_front()
            .expect("one response per planned inspection");
        match reply {
            AbsenceReply::Exact | AbsenceReply::WrongName | AbsenceReply::WrongProvider => {
                Ok(self.absence(name, reply))
            }
            AbsenceReply::Present => Err(NamePresence::Present),
            AbsenceReply::Ambiguous => Err(NamePresence::Ambiguous),
            AbsenceReply::Unavailable => Err(NamePresence::Unavailable),
        }
    }

    fn inspect_launch(
        &self,
        _key: &ProviderLaunchKey,
        _deadline: ProviderCallDeadline,
    ) -> ExactInspection {
        ExactInspection::Unavailable
    }

    fn classify_create_exit(&self, _exit: &CreateCliExitEvidence) -> CreateCliExitClass {
        CreateCliExitClass::Other
    }

    fn stop_inspected(
        &self,
        _inspection: &ProviderLaunchInspection,
        _deadline: ProviderCallDeadline,
    ) -> Result<(), EngineError> {
        Err(EngineError::Container(
            "Packet 1B barrier fixture never stops a provider object".into(),
        ))
    }

    fn remove_inspected(
        &self,
        _inspection: &ProviderLaunchInspection,
        _deadline: ProviderCallDeadline,
    ) -> Result<(), EngineError> {
        Err(EngineError::Container(
            "Packet 1B barrier fixture never removes a provider object".into(),
        ))
    }
}

#[cfg(unix)]
struct ControlFixture {
    temp: tempfile::TempDir,
    parent: std::path::PathBuf,
}

#[cfg(unix)]
impl ControlFixture {
    fn new() -> Self {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

        let temp = tempfile::tempdir().expect("private control fixture root");
        let parent = temp.path().join("control");
        std::fs::create_dir(&parent).expect("create control parent");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))
            .expect("protect control parent");
        let guest = parent.join("guest-control");
        std::fs::create_dir(&guest).expect("create guest-control");
        std::fs::set_permissions(&guest, std::fs::Permissions::from_mode(0o700))
            .expect("protect guest-control");

        let manifest = b"{\"version\":1,\"entries\":[]}\n";
        let manifest_digest = token_hex(Sha256::digest(manifest).into());
        let request = format!(
            r#"{{"version":1,"bindings":[{{"id":"workspace","workspace_path":"/workspace","manifest_id":"{manifest_digest}","manifest_file":"manifest.json","access":"read-only"}}]}}
"#
        );
        let intent = format!(
            r#"{{"version":1,"container_name":"{CONTROL_NAME}","launch_token":"{}"}}
"#,
            token_hex([0x5b; 32]),
        );
        for (name, bytes) in [
            ("manifest.json", manifest.as_slice()),
            ("request.json", request.as_bytes()),
            ("launch-intent.json", intent.as_bytes()),
        ] {
            let path = parent.join(name);
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .expect("create private control file");
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .expect("protect control fixture file");
            file.write_all(bytes).expect("write control fixture");
            file.sync_all().expect("sync control fixture");
        }
        Self { temp, parent }
    }
}

#[cfg(unix)]
fn prepared_plan(second: AbsenceReply) -> (ControlFixture, BarrierAdapter, DurableLaunchPlan) {
    let fixture = ControlFixture::new();
    let spec = load_startup_gate(&fixture.parent, Duration::from_secs(30))
        .expect("load orchestrated control fixture");
    let (controls, identity) = spec
        .control
        .orchestrated_parts()
        .expect("launch intention selects orchestrated controls");
    let controls = std::sync::Arc::clone(controls);
    let identity = identity.clone();
    let adapter = BarrierAdapter::new(second);
    let plan = super::gated_launch::prepare_orchestrated_launch(
        &adapter,
        &ImageRef::new("awman-p2b-final-image:latest"),
        &identity,
        controls,
        Instant::now() + Duration::from_secs(3),
    )
    .expect("first absence and durable plan publication");
    (fixture, adapter, plan)
}

#[cfg(unix)]
#[test]
fn immediate_pre_spawn_barrier_requires_a_fresh_exact_absence() {
    let (_fixture, adapter, plan) = prepared_plan(AbsenceReply::Exact);
    let _barrier =
        validate_immediate_pre_spawn(&plan, &adapter, Instant::now() + Duration::from_secs(3))
            .expect("second exact absence returns the one-shot spawn barrier");
    assert_eq!(adapter.absence_calls(), 2);
}

#[cfg(unix)]
#[test]
fn present_ambiguous_unavailable_or_mismatched_absence_yields_no_spawn_barrier() {
    for (reply, expected) in [
        (AbsenceReply::Present, "collision"),
        (AbsenceReply::Ambiguous, "unavailable"),
        (AbsenceReply::Unavailable, "unavailable"),
        (AbsenceReply::WrongName, "unavailable"),
        (AbsenceReply::WrongProvider, "unavailable"),
    ] {
        let (_fixture, adapter, plan) = prepared_plan(reply);
        let result =
            validate_immediate_pre_spawn(&plan, &adapter, Instant::now() + Duration::from_secs(3));
        match (result, expected) {
            (Err(LaunchIdentityError::NameCollision), "collision") => {}
            (Err(LaunchIdentityError::ProviderInspectionUnavailable), "unavailable") => {}
            (Ok(_), _) => panic!("non-exact pre-spawn observation returned a barrier"),
            (Err(_), _) => panic!("non-exact pre-spawn observation used the wrong fixed error"),
        }
        assert_eq!(adapter.absence_calls(), 2);
    }
}

#[cfg(unix)]
#[test]
fn expired_pre_spawn_deadline_yields_no_barrier_or_second_inspection() {
    let (_fixture, adapter, plan) = prepared_plan(AbsenceReply::Exact);
    let result =
        validate_immediate_pre_spawn(&plan, &adapter, Instant::now() - Duration::from_millis(1));
    assert!(matches!(
        result,
        Err(LaunchIdentityError::ProviderInspectionUnavailable)
    ));
    assert_eq!(adapter.absence_calls(), 1);
}

#[cfg(unix)]
#[test]
fn substituted_parent_or_guest_control_fails_before_the_second_provider_inspection() {
    use std::os::unix::fs::PermissionsExt as _;

    {
        let (fixture, adapter, plan) = prepared_plan(AbsenceReply::Exact);
        let guest = fixture.parent.join("guest-control");
        let displaced = fixture.parent.join("guest-control-held");
        std::fs::rename(&guest, &displaced).expect("displace held guest-control name");
        std::fs::create_dir(&guest).expect("create substitute guest-control");
        std::fs::set_permissions(&guest, std::fs::Permissions::from_mode(0o700))
            .expect("protect substitute guest-control");

        let result =
            validate_immediate_pre_spawn(&plan, &adapter, Instant::now() + Duration::from_secs(3));
        assert!(matches!(result, Err(LaunchIdentityError::UnsafeControl)));
        assert_eq!(adapter.absence_calls(), 1);
        assert!(fixture.temp.path().exists());
    }
    {
        let (fixture, adapter, plan) = prepared_plan(AbsenceReply::Exact);
        let displaced = fixture.temp.path().join("control-held");
        std::fs::rename(&fixture.parent, &displaced).expect("displace held control parent");
        std::fs::create_dir(&fixture.parent).expect("create substitute control parent");
        std::fs::set_permissions(&fixture.parent, std::fs::Permissions::from_mode(0o700))
            .expect("protect substitute control parent");

        let result =
            validate_immediate_pre_spawn(&plan, &adapter, Instant::now() + Duration::from_secs(3));
        assert!(matches!(result, Err(LaunchIdentityError::UnsafeControl)));
        assert_eq!(adapter.absence_calls(), 1);
    }
}

#[test]
fn unrecognized_provider_state_enters_revision_only_as_its_digest() {
    let key = launch_key([0x4a; 32]);
    let first = unknown_state_revision(&key, b"provider-state-without-authority");
    let same = unknown_state_revision(&key, b"provider-state-without-authority");
    let changed = unknown_state_revision(&key, b"different-unrecognized-state");

    assert!(first == same);
    assert!(first != changed);
}

fn provider_fixture_command(mode: &str, marker: &std::path::Path) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    command
        .arg("--exact")
        .arg("engine::container::gated_launch_p2b_test::provider_cli_fixture_child")
        .arg("--ignored")
        .arg("--nocapture")
        .env("AWMAN_P2B_PROVIDER_FIXTURE", mode)
        .env("AWMAN_P2B_PROVIDER_MARKER", marker)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn runner_deadline(after: Duration) -> ProviderCallDeadline {
    provider_call_deadline(Instant::now() + after)
}

fn provider_custody() -> std::sync::Arc<ProviderCliCustodyRegistry> {
    ProviderCliCustodyRegistry::try_new().expect("prepare provider CLI custody")
}

fn marker_pid(marker: &std::path::Path) -> u32 {
    std::fs::read_to_string(marker)
        .expect("provider fixture wrote its PID marker")
        .trim()
        .parse()
        .expect("PID marker is decimal")
}

#[cfg(unix)]
fn assert_reaped(pid: u32) {
    let result = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None);
    assert_eq!(result, Err(nix::errno::Errno::ESRCH));
}

#[test]
fn expired_provider_deadline_starts_no_process() {
    let temp = tempfile::tempdir().expect("private temporary fixture");
    let marker = temp.path().join("started");
    let custody = provider_custody();
    let outcome = run_bounded_provider_cli(
        provider_fixture_command("small", &marker),
        provider_call_deadline(Instant::now() - Duration::from_millis(1)),
        &custody,
    );

    assert!(matches!(
        outcome,
        ProviderCliRunOutcome::NotStarted(ProviderCliStartFailure::DeadlineExpired)
    ));
    assert!(!marker.exists());
}

#[cfg(unix)]
#[test]
fn stdout_and_stderr_caps_kill_and_actually_reap_the_provider_process() {
    for (mode, expected) in [
        (
            "stdout-overflow",
            ProviderCliReapedFailureKind::StdoutLimitExceeded,
        ),
        (
            "stderr-overflow",
            ProviderCliReapedFailureKind::StderrLimitExceeded,
        ),
    ] {
        let temp = tempfile::tempdir().expect("private temporary fixture");
        let marker = temp.path().join("started");
        let custody = provider_custody();
        let outcome = run_bounded_provider_cli(
            provider_fixture_command(mode, &marker),
            runner_deadline(Duration::from_secs(2)),
            &custody,
        );
        let failure = match outcome {
            ProviderCliRunOutcome::ReapedFailure(failure) => failure,
            ProviderCliRunOutcome::Completed(_) => panic!("overflowing provider output completed"),
            ProviderCliRunOutcome::NotStarted(_) => panic!("provider fixture did not start"),
            ProviderCliRunOutcome::RetainedFailure(_) => {
                panic!("provider fixture was not reaped after output overflow")
            }
        };
        assert_eq!(failure.kind, expected);
        assert_reaped(marker_pid(&marker));
    }
}

#[cfg(unix)]
#[test]
fn completed_provider_call_has_both_bounded_drains_at_eof_and_an_actual_wait() {
    let temp = tempfile::tempdir().expect("private temporary fixture");
    let marker = temp.path().join("started");
    let custody = provider_custody();
    let outcome = run_bounded_provider_cli(
        provider_fixture_command("small", &marker),
        runner_deadline(Duration::from_secs(2)),
        &custody,
    );
    let output = match outcome {
        ProviderCliRunOutcome::Completed(output) => output,
        ProviderCliRunOutcome::NotStarted(_) => panic!("provider fixture did not start"),
        ProviderCliRunOutcome::ReapedFailure(_) => panic!("small provider fixture was killed"),
        ProviderCliRunOutcome::RetainedFailure(_) => panic!("small provider fixture was retained"),
    };
    assert!(output.status.success());
    assert!(output.stdout.len() <= MAX_PROVIDER_STDOUT);
    assert!(output.stderr.len() <= MAX_PROVIDER_STDERR);
    assert_reaped(marker_pid(&marker));
}

#[cfg(unix)]
fn assert_deadline_reaps_without_waiting_for_descendant(mode: &str) {
    let temp = tempfile::tempdir().expect("private temporary fixture");
    let marker = temp.path().join("started");
    let started = Instant::now();
    let custody = provider_custody();
    let outcome = run_bounded_provider_cli(
        provider_fixture_command(mode, &marker),
        runner_deadline(Duration::from_millis(100)),
        &custody,
    );
    let elapsed = started.elapsed();
    let failure = match outcome {
        ProviderCliRunOutcome::ReapedFailure(failure) => failure,
        ProviderCliRunOutcome::Completed(_) => panic!("blocking provider fixture completed"),
        ProviderCliRunOutcome::NotStarted(_) => panic!("provider fixture did not start"),
        ProviderCliRunOutcome::RetainedFailure(_) => {
            panic!("provider fixture was not reaped at its deadline")
        }
    };
    assert_eq!(failure.kind, ProviderCliReapedFailureKind::DeadlineExceeded);
    assert!(elapsed < Duration::from_secs(1));
    assert_reaped(marker_pid(&marker));
}

#[cfg(unix)]
#[test]
fn timeout_reaps_live_provider_without_waiting_for_descendant_pipe_holder() {
    assert_deadline_reaps_without_waiting_for_descendant("descendant-holds-pipes");
}

#[cfg(unix)]
#[test]
fn exited_provider_is_not_completed_while_descendant_holds_drain_pipes() {
    assert_deadline_reaps_without_waiting_for_descendant("exited-descendant-holds-pipes");
}

#[test]
#[ignore = "subprocess fixture invoked by bounded-provider tests"]
fn provider_cli_fixture_child() {
    let Some(mode) = std::env::var_os("AWMAN_P2B_PROVIDER_FIXTURE") else {
        return;
    };
    let marker = std::path::PathBuf::from(
        std::env::var_os("AWMAN_P2B_PROVIDER_MARKER").expect("fixture marker path"),
    );
    std::fs::write(&marker, std::process::id().to_string()).expect("write fixture PID");

    match mode.to_string_lossy().as_ref() {
        "small" => {}
        "stdout-overflow" => {
            let bytes = vec![b'o'; MAX_PROVIDER_STDOUT + 1];
            let _ = std::io::stdout().write_all(&bytes);
            let _ = std::io::stdout().flush();
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        "stderr-overflow" => {
            let bytes = vec![b'e'; MAX_PROVIDER_STDERR + 1];
            let _ = std::io::stderr().write_all(&bytes);
            let _ = std::io::stderr().flush();
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        #[cfg(unix)]
        "descendant-holds-pipes" => {
            let _descendant = Command::new("/bin/sh")
                .args(["-c", "sleep 1"])
                .stdin(Stdio::null())
                .spawn()
                .expect("spawn pipe-holding descendant");
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        #[cfg(unix)]
        "exited-descendant-holds-pipes" => {
            let _descendant = Command::new("/bin/sh")
                .args(["-c", "sleep 1"])
                .stdin(Stdio::null())
                .spawn()
                .expect("spawn pipe-holding descendant");
        }
        other => panic!("unknown provider fixture mode: {other}"),
    }
}
