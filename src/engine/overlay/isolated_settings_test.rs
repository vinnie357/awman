fn isolated_settings_request(agent: &str, isolated: bool) -> OverlayRequest {
    OverlayRequest {
        agent: Some(AgentName::new(agent).unwrap()),
        container_home: Some("/isolated-agent".into()),
        isolated_settings: isolated,
        ..Default::default()
    }
}

fn isolated_settings_engine(home: &Path) -> OverlayEngine {
    OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(home))
        .with_secret_files_provider(std::sync::Arc::new(|_| Vec::new()))
        .with_credential_provider(std::sync::Arc::new(|_| None))
}

fn isolated_settings_mutate_owned_copy(source: &Path, copy: &Path) {
    assert_ne!(source.canonicalize().unwrap(), copy.canonicalize().unwrap());
    assert_eq!(std::fs::read(copy.join("marker")).unwrap(), b"host marker");
    std::fs::write(copy.join("marker"), b"container mutation").unwrap();
    std::fs::remove_file(copy.join("remove-me")).unwrap();
    std::fs::write(copy.join("new-file"), b"container only").unwrap();
    assert_eq!(
        std::fs::read(source.join("marker")).unwrap(),
        b"host marker"
    );
    assert_eq!(
        std::fs::read(source.join("remove-me")).unwrap(),
        b"host retained"
    );
    assert!(!source.join("new-file").exists());
}

#[test]
fn isolated_settings_codex_writable_copy_never_writes_back_and_is_engine_owned() {
    let home = tempfile::tempdir().unwrap();
    let source = home.path().join(".codex");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("marker"), b"host marker").unwrap();
    std::fs::write(source.join("remove-me"), b"host retained").unwrap();
    let engine = isolated_settings_engine(home.path());
    let session = make_session(home.path());
    let overlays = engine
        .build_overlays(&session, &isolated_settings_request("codex", true))
        .unwrap();
    assert_eq!(overlays.len(), 1);
    let settings = &overlays[0];
    assert_eq!(settings.container_path, Path::new("/isolated-agent/.codex"));
    assert_eq!(settings.permission, OverlayPermission::ReadWrite);
    let copy = settings.host_path.clone();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&copy).unwrap().permissions().mode() & 0o7777,
            0o700
        );
    }
    isolated_settings_mutate_owned_copy(&source, &copy);
    drop(overlays);
    assert!(
        copy.exists(),
        "engine retains owned settings after returned overlay metadata drops"
    );
    drop(engine);
    assert!(!copy.exists(), "engine owns temporary settings cleanup");
    assert_eq!(
        std::fs::read(source.join("marker")).unwrap(),
        b"host marker"
    );
}

#[test]
fn isolated_settings_agy_writable_copy_never_writes_back() {
    let home = tempfile::tempdir().unwrap();
    let source = home.path().join(".gemini");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("marker"), b"host marker").unwrap();
    std::fs::write(source.join("remove-me"), b"host retained").unwrap();
    let engine = isolated_settings_engine(home.path());
    let session = make_session(home.path());
    let overlays = engine
        .build_overlays(&session, &isolated_settings_request("agy", true))
        .unwrap();
    assert_eq!(overlays.len(), 1);
    assert_eq!(
        overlays[0].container_path,
        Path::new("/isolated-agent/.gemini")
    );
    assert_eq!(overlays[0].permission, OverlayPermission::ReadWrite);
    isolated_settings_mutate_owned_copy(&source, &overlays[0].host_path);
}

#[test]
fn isolated_settings_agy_injected_staging_failure_is_typed_and_never_host_fallback() {
    let home = tempfile::tempdir().unwrap();
    let source = home.path().join(".gemini");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("settings.json"), b"host marker").unwrap();
    let seed = crate::engine::auth::keychain::AgentSecretFile {
        relative_path: PathBuf::from("settings.json/fixture-seed"),
        contents: b"nonsecret fake seed".to_vec(),
        mode: 0o600,
    };
    let expected = stage_settings_dir_with_secrets(
        &source,
        std::slice::from_ref(&seed),
        "isolated-settings-error-fixture-",
    )
    .unwrap_err();
    let engine = isolated_settings_engine(home.path())
        .with_secret_files_provider(std::sync::Arc::new(move |_| vec![seed.clone()]));
    let session = make_session(home.path());
    match engine.build_overlays(&session, &isolated_settings_request("agy", true)) {
        Err(EngineError::Io {
            path,
            source: error,
        }) => {
            assert_eq!(path, source);
            assert_eq!(error.kind(), expected.kind());
            assert_eq!(error.raw_os_error(), expected.raw_os_error());
        }
        Err(_) => panic!("expected original typed staging I/O error"),
        Ok(_) => panic!("isolated policy must never return a host fallback after staging failure"),
    }
    assert_eq!(
        std::fs::read(source.join("settings.json")).unwrap(),
        b"host marker"
    );
    assert!(!source.join("settings.json/fixture-seed").exists());
}

#[test]
fn isolated_settings_agy_false_retains_legacy_staging_failure_fallback() {
    let home = tempfile::tempdir().unwrap();
    let source = home.path().join(".gemini");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("settings.json"), b"host marker").unwrap();
    let seed = crate::engine::auth::keychain::AgentSecretFile {
        relative_path: PathBuf::from("settings.json/fixture-seed"),
        contents: b"nonsecret fake seed".to_vec(),
        mode: 0o600,
    };
    let engine = isolated_settings_engine(home.path())
        .with_secret_files_provider(std::sync::Arc::new(move |_| vec![seed.clone()]));
    let session = make_session(home.path());
    let overlays = engine
        .build_overlays(&session, &isolated_settings_request("agy", false))
        .unwrap();
    assert_eq!(overlays.len(), 1);
    assert_eq!(overlays[0].host_path, source);
    assert_eq!(overlays[0].permission, OverlayPermission::ReadWrite);
    assert_eq!(
        std::fs::read(source.join("settings.json")).unwrap(),
        b"host marker"
    );
}

#[test]
fn isolated_settings_claude_config_failure_preserves_original_io_error_and_host_bytes() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".claude.json");
    std::fs::write(&config, [0xff, 0xfe]).unwrap();
    let expected = sanitize_claude_config(&config).unwrap_err();
    let engine = isolated_settings_engine(home.path());
    let session = make_session(home.path());
    match engine.build_overlays(&session, &isolated_settings_request("claude", true)) {
        Err(EngineError::Io { path, source }) => {
            assert_eq!(path, config);
            assert_eq!(source.kind(), expected.kind());
            assert_eq!(source.raw_os_error(), expected.raw_os_error());
        }
        Err(_) => panic!("expected original typed Claude config I/O error"),
        Ok(_) => panic!("isolated policy must not mount original Claude config after failure"),
    }
    assert_eq!(std::fs::read(&config).unwrap(), [0xff, 0xfe]);
    let legacy = engine
        .build_overlays(&session, &isolated_settings_request("claude", false))
        .unwrap();
    assert!(legacy
        .iter()
        .any(|overlay| overlay.host_path == config
            && overlay.permission == OverlayPermission::ReadWrite));
    assert_eq!(std::fs::read(&config).unwrap(), [0xff, 0xfe]);
}

#[test]
fn isolated_settings_claude_owned_sanitization_and_authorized_fake_seed_are_preserved() {
    let home = tempfile::tempdir().unwrap();
    let settings = home.path().join(".claude");
    std::fs::create_dir_all(&settings).unwrap();
    let config = home.path().join(".claude.json");
    std::fs::write(
        &config,
        br#"{"oauthAccount":{"fixture":true},"model":"fixture-model"}"#,
    )
    .unwrap();
    std::fs::write(
        settings.join(".credentials.json"),
        b"host fake credentials must be excluded",
    )
    .unwrap();
    std::fs::write(settings.join("allowed.json"), b"host marker").unwrap();
    let fake = CredentialFile {
        relative_path: PathBuf::from(".credentials.json"),
        contents: br#"{"claudeAiOauth":{"accessToken":"fixture-access-only"}}"#.to_vec(),
        mode: 0o600,
    };
    let engine = make_engine_with_credential(home.path(), Some(fake.clone()));
    let session = make_session(home.path());
    let mut request = isolated_settings_request("claude", true);
    request.materialize_credentials = true;
    let (overlays, staged) = engine
        .build_overlays_with_credentials(&session, &request)
        .unwrap();
    let config_overlay = overlays
        .iter()
        .find(|overlay| overlay.container_path == Path::new("/isolated-agent/.claude.json"))
        .unwrap();
    assert_ne!(config_overlay.host_path, config);
    let sanitized: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_overlay.host_path).unwrap()).unwrap();
    assert!(sanitized.get("oauthAccount").is_none());
    assert_eq!(sanitized["model"], "fixture-model");
    let settings_overlay = overlays
        .iter()
        .find(|overlay| overlay.container_path == Path::new("/isolated-agent/.claude"))
        .unwrap();
    assert_ne!(settings_overlay.host_path, settings);
    assert_eq!(settings_overlay.permission, OverlayPermission::ReadWrite);
    assert!(
        std::fs::read(settings_overlay.host_path.join(".credentials.json")).unwrap()
            == fake.contents
    );
    assert_eq!(staged.len(), 1);
    assert_eq!(
        std::fs::read(settings.join(".credentials.json")).unwrap(),
        b"host fake credentials must be excluded"
    );
    assert_eq!(
        std::fs::read(settings.join("allowed.json")).unwrap(),
        b"host marker"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(settings_overlay.host_path.join(".credentials.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
    }
}

#[test]
fn isolated_settings_unsupported_true_fails_before_provider_and_false_remains_legacy() {
    let home = tempfile::tempdir().unwrap();
    let source = home.path().join(".gemini");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("marker"), b"host marker").unwrap();
    let engine = isolated_settings_engine(home.path())
        .with_secret_files_provider(std::sync::Arc::new(|_| {
            panic!("unsupported isolation must not read provider credentials")
        }))
        .with_credential_provider(std::sync::Arc::new(|_| {
            panic!("unsupported isolation must not read provider credentials")
        }));
    let session = make_session(home.path());
    assert!(matches!(
        engine.build_overlays(&session, &isolated_settings_request("gemini", true)),
        Err(EngineError::NotImplemented(_))
    ));
    let legacy = engine
        .build_overlays(&session, &isolated_settings_request("gemini", false))
        .unwrap();
    assert_eq!(legacy.len(), 1);
    assert_eq!(legacy[0].host_path, source);
    assert_eq!(legacy[0].permission, OverlayPermission::ReadWrite);
    assert_eq!(
        std::fs::read(source.join("marker")).unwrap(),
        b"host marker"
    );
}

#[test]
fn isolated_settings_codex_absent_flag_retains_direct_legacy_host_mount() {
    let home = tempfile::tempdir().unwrap();
    let source = home.path().join(".codex");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("marker"), b"host marker").unwrap();
    let engine = isolated_settings_engine(home.path());
    let session = make_session(home.path());
    let request = OverlayRequest {
        agent: Some(AgentName::new("codex").unwrap()),
        container_home: Some("/isolated-agent".into()),
        ..Default::default()
    };
    assert!(!request.isolated_settings);
    let overlays = engine.build_overlays(&session, &request).unwrap();
    assert_eq!(overlays.len(), 1);
    assert_eq!(overlays[0].host_path, source);
    assert_eq!(overlays[0].permission, OverlayPermission::ReadWrite);
    assert_eq!(
        std::fs::read(source.join("marker")).unwrap(),
        b"host marker"
    );
}

#[test]
fn isolated_settings_regular_file_source_root_fails_closed_for_codex_and_agy() {
    for (agent, basename) in [("codex", ".codex"), ("agy", ".gemini")] {
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join(basename);
        std::fs::write(&source, b"host regular-file marker").unwrap();
        let engine = isolated_settings_engine(home.path());
        let session = make_session(home.path());
        match engine.build_overlays(&session, &isolated_settings_request(agent, true)) {
            Err(EngineError::Io { path, .. }) => assert_eq!(path, source),
            Err(_) => panic!("isolated source directory I/O failure must remain typed"),
            Ok(_) => panic!("isolated settings must not return an empty or original overlay after source directory I/O failure"),
        }
        assert!(std::fs::metadata(&source).unwrap().is_file());
        assert_eq!(std::fs::read(&source).unwrap(), b"host regular-file marker");
        let legacy = engine
            .build_overlays(&session, &isolated_settings_request(agent, false))
            .unwrap();
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].permission, OverlayPermission::ReadWrite);
        if agent == "codex" {
            assert_eq!(legacy[0].host_path, source);
        } else {
            assert_ne!(legacy[0].host_path, source);
            assert!(legacy[0].host_path.is_dir());
            assert!(std::fs::read_dir(&legacy[0].host_path)
                .unwrap()
                .next()
                .is_none());
        }
        assert!(std::fs::metadata(&source).unwrap().is_file());
        assert_eq!(std::fs::read(&source).unwrap(), b"host regular-file marker");
    }
}
