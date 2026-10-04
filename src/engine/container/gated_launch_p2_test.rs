use super::*;
use crate::data::startup_gate::{load_startup_gate, StartupGateSpec};
use crate::engine::container::{ContainerName, ImageRef};
use crate::engine::error::EngineError;
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const NAME: &str = "awman-altana-plan-p2";
const TOKEN: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const IMAGE: &str = "sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";

#[derive(Clone, Debug, Eq, PartialEq)]
enum Call {
    ResolveImage,
    InspectAbsence,
    InspectLaunch,
    Stop,
    Remove,
}

struct ScriptedAdapter {
    calls: Mutex<Vec<Call>>,
    image: ImmutableImageId,
    absence: Mutex<VecDeque<Result<AbsenceObservation, NamePresence>>>,
}

impl ScriptedAdapter {
    fn absent(name: &ContainerName) -> Self {
        let observation = AbsenceObservation {
            provider: ProviderKind::AppleContainers,
            exact_name: name.clone(),
            checked_at: Utc::now(),
            revision: InspectionRevision([0x31; 32]),
        };
        Self {
            calls: Mutex::new(Vec::new()),
            image: ImmutableImageId(IMAGE.into()),
            absence: Mutex::new(VecDeque::from([Ok(observation)])),
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn record(&self, call: Call) {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(call);
    }
}

impl GatedProviderAdapter for ScriptedAdapter {
    fn provider(&self) -> ProviderKind {
        ProviderKind::AppleContainers
    }

    fn resolve_image_identity(
        &self,
        _image: &ImageRef,
        _deadline: ProviderCallDeadline,
    ) -> Result<ImmutableImageId, LaunchIdentityError> {
        self.record(Call::ResolveImage);
        Ok(self.image.clone())
    }

    fn inspect_name_absence(
        &self,
        _name: &ContainerName,
        _deadline: ProviderCallDeadline,
    ) -> Result<AbsenceObservation, NamePresence> {
        self.record(Call::InspectAbsence);
        self.absence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
            .unwrap_or(Err(NamePresence::Unavailable))
    }

    fn inspect_launch(
        &self,
        _key: &ProviderLaunchKey,
        _deadline: ProviderCallDeadline,
    ) -> ExactInspection {
        self.record(Call::InspectLaunch);
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
        self.record(Call::Stop);
        Err(EngineError::Container("unexpected stop".into()))
    }

    fn remove_inspected(
        &self,
        _inspection: &ProviderLaunchInspection,
        _deadline: ProviderCallDeadline,
    ) -> Result<(), EngineError> {
        self.record(Call::Remove);
        Err(EngineError::Container("unexpected remove".into()))
    }
}

struct PlanFixture {
    _root: tempfile::TempDir,
    parent: PathBuf,
    spec: StartupGateSpec,
}

impl PlanFixture {
    fn new() -> Result<Self, Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let parent = root.path().join("control-parent");
        let guest = parent.join("guest-control");
        std::fs::create_dir(&parent)?;
        std::fs::create_dir(&guest)?;
        set_mode(&parent, 0o700)?;
        set_mode(&guest, 0o700)?;
        let manifest = br#"{"version":1,"entries":[]}"#;
        let manifest_digest = lower_hex(&Sha256::digest(manifest));
        write_private(&parent.join("workspace.manifest.json"), manifest)?;
        let request = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "bindings": [{
                "id": "workspace",
                "workspace_path": "/workspace",
                "manifest_id": manifest_digest,
                "manifest_file": "workspace.manifest.json",
                "access": "read-write"
            }]
        }))?;
        write_private(&parent.join("request.json"), &request)?;
        write_private(
            &parent.join("launch-intent.json"),
            format!(
                "{{\"version\":1,\"container_name\":\"{NAME}\",\"launch_token\":\"{TOKEN}\"}}\n"
            )
            .as_bytes(),
        )?;
        let spec = load_startup_gate(&parent, Duration::from_secs(30))?;
        Ok(Self {
            _root: root,
            parent,
            spec,
        })
    }

    fn parts(
        &self,
    ) -> Result<(Arc<OrchestratedControlAuthority>, GatedLaunchIdentity), Box<dyn Error>> {
        self.spec
            .control
            .orchestrated_parts()
            .map(|(authority, identity)| (Arc::clone(authority), identity.clone()))
            .ok_or_else(|| "expected orchestrated startup gate".into())
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    set_mode(path, 0o600)
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn assert_existing_plan(error: LaunchIdentityError) {
    assert!(matches!(
        error,
        LaunchIdentityError::ExistingPlanRecoveryRequired
    ));
}

#[test]
#[cfg(unix)]
fn durable_plan_has_exact_private_bytes_and_precedes_any_spawn_surface(
) -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::MetadataExt;
    let fixture = PlanFixture::new()?;
    let (controls, identity) = fixture.parts()?;
    let adapter = ScriptedAdapter::absent(&identity.container_name);
    let plan = prepare_orchestrated_launch(
        &adapter,
        &ImageRef::new("example:latest"),
        &identity,
        controls,
        Instant::now() + Duration::from_secs(60),
    )?;
    assert_eq!(
        adapter.calls(),
        vec![Call::ResolveImage, Call::InspectAbsence]
    );
    assert_eq!(plan.request_digest, identity.request_digest);
    assert_eq!(plan.key.created_not_before.timestamp_subsec_nanos(), 0);

    let path = fixture.parent.join("launch-plan.json");
    let bytes = std::fs::read(&path)?;
    let expected = format!(
        "{{\"version\":1,\"provider\":\"apple-containers\",\"name\":\"{}\",\"tokenDigest\":\"{}\",\"imageId\":\"{}\",\"createdNotBefore\":\"{}\",\"requestDigest\":\"{}\"}}\n",
        plan.key.container_name.as_str(),
        lower_hex(&plan.key.token_digest),
        plan.key.immutable_image_id.0,
        plan.key.created_not_before.format("%Y-%m-%dT%H:%M:%SZ"),
        lower_hex(&plan.request_digest),
    );
    assert_eq!(bytes, expected.as_bytes());
    assert!(!expected.contains(TOKEN));
    let metadata = std::fs::symlink_metadata(&path)?;
    assert!(metadata.file_type().is_file());
    assert_eq!(metadata.mode() & 0o7777, 0o600);
    assert_eq!(metadata.nlink(), 1);
    assert_eq!(metadata.uid(), nix::unistd::Uid::current().as_raw());
    Ok(())
}

#[test]
#[cfg(unix)]
fn any_existing_plan_is_retained_without_image_or_provider_inspection() -> Result<(), Box<dyn Error>>
{
    for bytes in [
        b"not json\n".as_slice(),
        b"{}\n".as_slice(),
        b"{\"version\":1}\n".as_slice(),
    ] {
        let fixture = PlanFixture::new()?;
        let (controls, identity) = fixture.parts()?;
        write_private(&fixture.parent.join("launch-plan.json"), bytes)?;
        let adapter = ScriptedAdapter::absent(&identity.container_name);
        let error = match prepare_orchestrated_launch(
            &adapter,
            &ImageRef::new("example:latest"),
            &identity,
            controls,
            Instant::now() + Duration::from_secs(60),
        ) {
            Ok(_) => return Err("existing plan allowed a new plan".into()),
            Err(error) => error,
        };
        assert_existing_plan(error);
        assert!(adapter.calls().is_empty());
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn second_prepare_never_respawns_or_overwrites_the_durable_plan() -> Result<(), Box<dyn Error>> {
    let fixture = PlanFixture::new()?;
    let (controls, identity) = fixture.parts()?;
    let first_adapter = ScriptedAdapter::absent(&identity.container_name);
    let _plan = prepare_orchestrated_launch(
        &first_adapter,
        &ImageRef::new("example:latest"),
        &identity,
        controls.clone(),
        Instant::now() + Duration::from_secs(60),
    )?;
    let before = std::fs::read(fixture.parent.join("launch-plan.json"))?;
    let second_adapter = ScriptedAdapter::absent(&identity.container_name);
    let error = match prepare_orchestrated_launch(
        &second_adapter,
        &ImageRef::new("example:latest"),
        &identity,
        controls,
        Instant::now() + Duration::from_secs(60),
    ) {
        Ok(_) => return Err("durable plan was reused for an automatic respawn".into()),
        Err(error) => error,
    };
    assert_existing_plan(error);
    assert!(second_adapter.calls().is_empty());
    assert_eq!(
        std::fs::read(fixture.parent.join("launch-plan.json"))?,
        before
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn unsafe_or_nonregular_existing_plan_is_retained_without_provider_calls(
) -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;
    for case in ["directory", "symlink", "broad-file"] {
        let fixture = PlanFixture::new()?;
        let (controls, identity) = fixture.parts()?;
        let path = fixture.parent.join("launch-plan.json");
        match case {
            "directory" => {
                std::fs::create_dir(&path)?;
                set_mode(&path, 0o700)?;
            }
            "symlink" => {
                let target = fixture.parent.join("foreign-plan");
                write_private(&target, b"foreign\n")?;
                std::os::unix::fs::symlink(target, &path)?;
            }
            "broad-file" => {
                write_private(&path, b"{}\n")?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
            }
            _ => return Err("unknown test case".into()),
        }
        let adapter = ScriptedAdapter::absent(&identity.container_name);
        let error = match prepare_orchestrated_launch(
            &adapter,
            &ImageRef::new("example:latest"),
            &identity,
            controls,
            Instant::now() + Duration::from_secs(60),
        ) {
            Ok(_) => return Err(format!("unsafe {case} plan allowed a new plan").into()),
            Err(error) => error,
        };
        assert_existing_plan(error);
        assert!(adapter.calls().is_empty());
    }
    Ok(())
}

#[test]
fn every_provider_subdeadline_is_bounded_by_enclosing_and_ten_seconds() {
    let now = Instant::now();
    let far = provider_call_deadline(now + Duration::from_secs(60));
    assert!(far.0 <= now + MAX_PROVIDER_CALL + Duration::from_millis(50));
    assert!(far.0 <= now + Duration::from_secs(60));

    let near_limit = now + Duration::from_millis(25);
    let near = provider_call_deadline(near_limit);
    assert!(near.0 <= near_limit);
}
