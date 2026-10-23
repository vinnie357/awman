#[test]
fn isolated_settings_catalogue_has_scoped_boolean_false_default_and_supported_help() {
    use crate::command::dispatch::catalogue::{CommandCatalogue, FlagDefault, FlagKind};
    let cat = CommandCatalogue::get();
    let command = cat.lookup(&["exec", "prompt"]).unwrap();
    let flag = command
        .find_flag("isolated-settings")
        .expect("isolated settings flag");
    assert!(matches!(flag.kind, FlagKind::Bool));
    assert!(matches!(flag.default, FlagDefault::Bool(false)));
    assert!(flag.optional);
    assert!(flag.short.is_none());
    let help = flag.help.to_ascii_lowercase();
    for word in ["codex", "agy", "claude", "owned", "writable"] {
        assert!(
            help.contains(word),
            "help must describe supported owned settings copies"
        );
    }
    assert!(cat
        .lookup(&["chat"])
        .unwrap()
        .find_flag("isolated-settings")
        .is_none());
    assert!(cat
        .lookup(&["exec", "workflow"])
        .unwrap()
        .find_flag("isolated-settings")
        .is_none());
}

#[test]
fn isolated_settings_cli_and_raw_flag_parsing_agree_for_absent_and_present() {
    let cat = CommandCatalogue::get();
    for isolated in [false, true] {
        let mut args = vec!["awman", "exec", "prompt"];
        if isolated {
            args.push("--isolated-settings");
        }
        args.push("fixture prompt");
        let matches = cat.build_clap_command().try_get_matches_from(args).unwrap();
        let prompt = matches
            .subcommand_matches("exec")
            .unwrap()
            .subcommand_matches("prompt")
            .unwrap();
        assert_eq!(prompt.get_flag("isolated-settings"), isolated);
        let raw_args = if isolated {
            vec![
                "--isolated-settings".to_string(),
                "fixture prompt".to_string(),
            ]
        } else {
            vec!["fixture prompt".to_string()]
        };
        let raw = cat.parse_raw_args(&["exec", "prompt"], &raw_args).unwrap();
        assert_eq!(raw.flag_bool("isolated-settings"), isolated.then_some(true));
    }
}

#[test]
fn isolated_settings_actual_dispatch_captures_true_and_absent_false() {
    for isolated in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let mut frontend = FakeCommandFrontend::new();
        if isolated {
            frontend.bools.insert("isolated-settings".into(), true);
        }
        frontend
            .args
            .insert("prompt".into(), "fixture prompt".into());
        let dispatch = Dispatch::new(frontend, make_session(), Engines::for_tests(home.path()));
        match dispatch.build_command(&["exec", "prompt"]).unwrap() {
            BuiltCommand::ExecPrompt(command) => {
                assert_eq!(command.flags().isolated_settings, isolated)
            }
            _ => panic!("expected exec prompt"),
        }
    }
}
