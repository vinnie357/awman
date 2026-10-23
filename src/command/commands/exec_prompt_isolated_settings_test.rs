#[test]
fn isolated_settings_command_base_options_preserve_flag_and_existing_base_choices() {
    for isolated in [false, true] {
        let flags = ExecPromptCommandFlags {
            startup_gate_control: None,
            startup_gate_timeout: 120,
            prompt: Some("fixture prompt".into()),
            non_interactive: true,
            plan: false,
            allow_docker: false,
            yolo: true,
            auto: false,
            agent: Some("codex".into()),
            model: Some("fixture-model".into()),
            launch_mode: None,
            overlay: Vec::new(),
            issue_source: crate::engine::issue::IssueSourceFlags { issue: None },
            isolated_settings: isolated,
        };
        let run = super::exec_prompt_base_run_options(&flags);
        assert_eq!(run.isolated_settings, isolated);
        assert_eq!(run.clone().isolated_settings, isolated);
        assert!(run.non_interactive);
        assert!(!run.allow_docker);
        assert_eq!(run.model.as_deref(), Some("fixture-model"));
        assert!(matches!(
            run.yolo,
            Some(crate::engine::container::options::YoloMode::Enabled)
        ));
        assert!(run.plan.is_none());
        assert!(run.auto.is_none());
    }
    assert!(!AgentRunOptions::default().isolated_settings);
    assert!(!crate::engine::overlay::OverlayRequest::default().isolated_settings);
}
