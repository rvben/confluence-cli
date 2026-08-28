use std::process::{Command, Output};

fn confluence(args: &[&str]) -> Output {
    let temp = tempfile::tempdir().expect("temporary config directory");
    Command::new(env!("CARGO_BIN_EXE_confluence"))
        .args(args)
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path())
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
    for args in [
        ["--output", "json", "profile", "remove", "work"].as_slice(),
        ["--output", "json", "apply", "/tmp/docs", "--delete-remote"].as_slice(),
    ] {
        let output = confluence(args);
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
fn piped_auto_init_emits_machine_readable_setup_instructions() {
    let output = confluence(&["init"]);
    assert!(output.status.success());
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("setup JSON document");
    assert!(document["configPath"].is_string());
    assert!(document["envVars"].is_object());
    assert!(output.stderr.is_empty());
}
