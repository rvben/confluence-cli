use std::process::{Command, Output};

fn confluence(args: &[&str]) -> Output {
    let temp = tempfile::tempdir().expect("temporary config directory");
    confluence_with_home(temp.path(), args)
}

fn confluence_with_home(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_confluence"))
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env_remove("CONFLUENCE_PROFILE")
        .env_remove("CONFLUENCE_DOMAIN")
        .env_remove("CONFLUENCE_API_TOKEN")
        .env_remove("CONFLUENCE_TOKEN")
        .output()
        .expect("confluence command to run")
}

#[test]
fn bare_invocation_prints_full_help_and_succeeds() {
    let output = confluence(&[]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Usage: confluence"));
    assert!(stdout.contains("Configure an account with the guided login wizard"));
    assert!(output.stderr.is_empty());
}

#[test]
fn tui_help_describes_the_read_only_workspace() {
    let output = confluence(&["tui", "--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Browse Confluence and review a local Markdown sync plan"));
    assert!(stdout.contains("The TUI is read-only"));
    assert!(stdout.contains("--space"));
    assert!(stdout.contains("--path"));
    assert!(stdout.contains("--delete-remote"));
    assert!(stdout.contains("--page-size"));
}

#[test]
fn piped_tui_refuses_before_resolving_a_profile() {
    let output = confluence(&["tui"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("structured tty error");
    assert_eq!(error["error"]["kind"], "tty_required");
    assert!(
        error["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("launch `confluence tui` directly")
    );
}

#[test]
fn tui_rejects_explicit_json_with_a_scriptable_recovery() {
    let output = confluence(&["--output", "json", "tui"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("structured invalid-input error");
    assert_eq!(error["error"]["kind"], "invalid_input");
    assert!(
        error["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("confluence plan")
    );
}

#[test]
fn explicit_text_output_overrides_the_legacy_json_flag_for_tui() {
    let output = confluence(&["--json", "--output", "text", "tui"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("interactive terminal"));
    assert!(!stderr.contains("does not support JSON output"));
}

#[test]
fn machine_parse_errors_preserve_missing_argument_and_usage() {
    let output = confluence(&["page", "get"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    let error: serde_json::Value = serde_json::from_str(&stderr).expect("JSON error");
    assert_eq!(error["error"]["kind"], "invalid_input");
    let message = error["error"]["message"].as_str().unwrap();
    assert!(message.contains("<REFERENCE>"));
    assert!(message.contains("Usage: confluence page get"));
}

#[test]
fn missing_plan_path_is_not_a_successful_noop() {
    let output = confluence(&[
        "--output",
        "json",
        "plan",
        "/tmp/confluence-cli-contract-path-that-does-not-exist",
    ]);
    assert_eq!(output.status.code(), Some(4));
    assert!(output.stdout.is_empty());
    let error: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("structured not-found error");
    assert_eq!(error["error"]["kind"], "not_found");
}

#[test]
fn empty_plan_directory_is_not_reported_as_no_changes() {
    let directory = tempfile::tempdir().expect("empty sync directory");
    let output = confluence(&[
        "--output",
        "json",
        "plan",
        directory.path().to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("structured invalid-input error");
    assert_eq!(error["error"]["kind"], "invalid_input");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("contains no index.md documents")
    );
}

#[test]
fn unknown_schema_command_emits_no_plausible_stdout_document() {
    let output = confluence(&["schema", "--command", "not-a-command"]);
    assert_eq!(output.status.code(), Some(4));
    assert!(output.stdout.is_empty());
    let error: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("structured not-found error");
    assert_eq!(error["error"]["kind"], "not_found");
}

#[test]
fn every_body_command_reaches_normal_error_handling_instead_of_panicking() {
    let cases = [
        ["page", "create", "Title", "SPACE", "--body", "text"].as_slice(),
        ["page", "update", "123", "--body", "text"].as_slice(),
        ["blog", "create", "Title", "SPACE", "--body", "text"].as_slice(),
        ["blog", "update", "123", "--body", "text"].as_slice(),
        ["comment", "add", "123", "--body", "text"].as_slice(),
        ["comment", "update", "456", "--body", "text"].as_slice(),
    ];
    for args in cases {
        let output = confluence(args);
        assert_ne!(output.status.code(), Some(101), "panicked for {args:?}");
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("panicked"),
            "panicked for {args:?}"
        );
    }
}

#[test]
fn short_output_flag_controls_parse_error_rendering() {
    let text = confluence(&["-o", "text", "not-a-command"]);
    assert_eq!(text.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&text.stderr).starts_with("error:"));
    assert!(serde_json::from_slice::<serde_json::Value>(&text.stderr).is_err());

    let json = confluence(&["-o", "json", "not-a-command"]);
    assert_eq!(json.status.code(), Some(2));
    let error: serde_json::Value = serde_json::from_slice(&json.stderr).expect("JSON error");
    assert_eq!(error["error"]["kind"], "invalid_input");
}

#[test]
fn destructive_commands_require_explicit_noninteractive_confirmation() {
    let home = tempfile::tempdir().expect("temporary config directory");
    let init = confluence_with_home(home.path(), &["--output", "json", "init"]);
    let instructions: serde_json::Value =
        serde_json::from_slice(&init.stdout).expect("init instructions");
    let config_path = std::path::PathBuf::from(
        instructions["configPath"]
            .as_str()
            .expect("configuration path"),
    );
    std::fs::create_dir_all(config_path.parent().unwrap()).expect("config directory");
    std::fs::write(
        &config_path,
        r#"{
          "active_profile": "work",
          "profiles": {
            "work": {
              "provider": "cloud",
              "base_url": "https://example.atlassian.net",
              "api_path": "/wiki/rest/api",
              "auth": {"type": "basic", "username": "user@example.test", "token": "test"},
              "credential_store": "config",
              "token_kind": "classic",
              "read_only": false
            }
          }
        }"#,
    )
    .expect("write config");

    let sync_root = home.path().join("docs");
    let page = sync_root.join("page--123");
    std::fs::create_dir_all(&page).expect("sync page directory");
    std::fs::write(
        page.join("index.md"),
        "---\ntitle: Test\ntype: page\nlabels: []\nstatus: current\nproperties: {}\n---\n\nBody\n",
    )
    .expect("write markdown");
    std::fs::write(page.join(".confluence.json"), "{}").expect("write sidecar");

    let sync_path = sync_root.to_string_lossy().into_owned();
    for args in [
        vec!["--output", "json", "profile", "remove", "work"],
        vec![
            "--output",
            "json",
            "apply",
            sync_path.as_str(),
            "--delete-remote",
        ],
    ] {
        let output = confluence_with_home(home.path(), &args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "unexpected status for {args:?}"
        );
        let error: serde_json::Value =
            serde_json::from_slice(&output.stderr).expect("confirmation JSON error");
        assert_eq!(error["error"]["kind"], "confirmation_required");
    }
}

#[test]
fn destructive_commands_validate_targets_before_prompting() {
    let missing_profile = confluence(&["--output", "json", "profile", "remove", "missing"]);
    assert_eq!(missing_profile.status.code(), Some(4));
    let error: serde_json::Value =
        serde_json::from_slice(&missing_profile.stderr).expect("not-found JSON error");
    assert_eq!(error["error"]["kind"], "not_found");

    let missing_path = confluence(&[
        "--output",
        "json",
        "apply",
        "/tmp/confluence-cli-contract-missing-apply-path",
        "--delete-remote",
    ]);
    assert_eq!(missing_path.status.code(), Some(4));
    let error: serde_json::Value =
        serde_json::from_slice(&missing_path.stderr).expect("not-found JSON error");
    assert_eq!(error["error"]["kind"], "not_found");
}

#[test]
fn piped_auto_init_emits_machine_readable_setup_instructions() {
    let output = confluence(&["init"]);
    assert!(output.status.success());
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("setup JSON document");
    assert!(document["configPath"].is_string());
    assert!(document["envVars"].is_object());
    assert!(output.stderr.is_empty());
}

#[test]
fn bare_noninteractive_auth_login_requires_a_tty() {
    for args in [
        vec!["auth", "login"],
        vec!["--output", "json", "auth", "login"],
    ] {
        let output = confluence(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "unexpected status for {args:?}"
        );
        assert!(output.stdout.is_empty());
        let error: serde_json::Value =
            serde_json::from_slice(&output.stderr).expect("structured tty error");
        assert_eq!(error["error"]["kind"], "tty_required");
        assert!(
            error["error"]["hint"]
                .as_str()
                .unwrap()
                .contains("--non-interactive")
        );
    }
}

#[test]
fn no_op_updates_fail_before_profile_resolution() {
    for args in [
        ["--output", "json", "page", "update", "123"],
        ["--output", "json", "blog", "update", "456"],
    ] {
        let output = confluence(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "unexpected status for {args:?}"
        );
        assert!(output.stdout.is_empty());
        let error: serde_json::Value =
            serde_json::from_slice(&output.stderr).expect("structured invalid-input error");
        assert_eq!(error["error"]["kind"], "invalid_input");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("at least one requested change")
        );
    }
}

#[test]
fn doctor_text_output_is_readable_in_a_standard_terminal() {
    let output = confluence(&[
        "--output",
        "text",
        "doctor",
        "--skip-network",
        "--profile",
        "missing",
    ]);
    assert_eq!(output.status.code(), Some(4));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Configuration:"));
    assert!(stdout.contains("Checks:"));
    assert!(
        stdout.lines().all(|line| line.chars().count() <= 100),
        "doctor output exceeded 100 columns:\n{stdout}"
    );
}
