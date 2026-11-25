use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tempfile::TempDir;
use url::Url;
use walkdir::WalkDir;

#[path = "support/confluence_simulator.rs"]
mod confluence_simulator;

use confluence_simulator::{ConfluenceSimulator, FAILING_USER_ID, KNOWN_USER_ID, KNOWN_USER_NAME};

#[derive(Clone, Debug)]
struct E2eConfig {
    bin: PathBuf,
    profile: Option<String>,
    envs: Vec<(String, String)>,
    space: String,
}

#[derive(Debug)]
struct EnvironmentE2eSettings {
    base_url: String,
    token: String,
    auth_type: String,
    provider: String,
    api_path: String,
    token_kind: Option<String>,
    cloud_id: Option<String>,
    username: Option<String>,
}

impl E2eConfig {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.bin);
        if let Some(profile) = &self.profile {
            command.arg("--profile").arg(profile);
        }
        for (key, value) in &self.envs {
            command.env(key, value);
        }
        command
    }

    fn run(&self, args: &[&str]) -> String {
        let output = self.command().args(args).output().expect("command to run");
        if !output.status.success() {
            panic!(
                "command failed: {:?}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
                args,
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        String::from_utf8(output.stdout).expect("utf8 stdout")
    }

    fn run_json(&self, args: &[&str]) -> Value {
        let mut all_args = Vec::with_capacity(args.len() + 1);
        all_args.push("--json");
        all_args.extend_from_slice(args);
        let stdout = self.run(&all_args);
        serde_json::from_str(&stdout).unwrap_or_else(|err| {
            panic!(
                "failed to parse JSON output for {:?}: {err}\n{stdout}",
                args
            )
        })
    }

    fn best_effort(&self, args: &[&str]) {
        let _ = self.command().args(args).output();
    }
}

#[derive(Debug)]
struct Cleanup {
    cfg: E2eConfig,
    page_id: Option<String>,
    extra_page_ids: Vec<String>,
    blog_id: Option<String>,
    comment_id: Option<String>,
    attachment_id: Option<String>,
}

impl Cleanup {
    fn new(cfg: E2eConfig) -> Self {
        Self {
            cfg,
            page_id: None,
            extra_page_ids: Vec::new(),
            blog_id: None,
            comment_id: None,
            attachment_id: None,
        }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let (Some(page_id), Some(attachment_id)) =
            (self.page_id.as_deref(), self.attachment_id.as_deref())
        {
            self.cfg
                .best_effort(&["--yes", "attachment", "delete", page_id, attachment_id]);
        }
        if let Some(comment_id) = self.comment_id.as_deref() {
            self.cfg
                .best_effort(&["--yes", "comment", "delete", comment_id]);
        }
        if let Some(blog_id) = self.blog_id.as_deref() {
            self.cfg.best_effort(&["--yes", "blog", "delete", blog_id]);
        }
        if let Some(page_id) = self.page_id.as_deref() {
            self.cfg.best_effort(&["--yes", "page", "delete", page_id]);
        }
        for page_id in self.extra_page_ids.iter().rev() {
            self.cfg.best_effort(&["--yes", "page", "delete", page_id]);
        }
    }
}

fn e2e_config() -> Option<E2eConfig> {
    let bin = find_binary_path();
    let space = env::var("CONFLUENCE_E2E_SPACE").unwrap_or_else(|_| "TEST".to_string());

    if let Some(profile) = env::var("CONFLUENCE_E2E_PROFILE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Some(E2eConfig {
            bin,
            profile: Some(profile),
            envs: Vec::new(),
            space,
        });
    }

    let base_url = env::var("CONFLUENCE_E2E_BASE_URL").ok();
    let token = env::var("CONFLUENCE_E2E_TOKEN")
        .ok()
        .or_else(|| env::var("CONFLUENCE_E2E_BEARER_TOKEN").ok());
    if base_url.is_none() || token.is_none() {
        return None;
    }

    let auth_type = env::var("CONFLUENCE_E2E_AUTH_TYPE").unwrap_or_else(|_| "bearer".to_string());
    let provider =
        env::var("CONFLUENCE_E2E_PROVIDER").unwrap_or_else(|_| "data-center".to_string());
    let api_path = env::var("CONFLUENCE_E2E_API_PATH").unwrap_or_else(|_| "/rest/api".to_string());
    let optional_e2e_env = |key| {
        env::var(key)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let token_kind = optional_e2e_env("CONFLUENCE_E2E_TOKEN_KIND");
    let cloud_id = optional_e2e_env("CONFLUENCE_E2E_CLOUD_ID");

    let username = match auth_type.as_str() {
        "basic" => Some(
            env::var("CONFLUENCE_E2E_USERNAME")
                .or_else(|_| env::var("CONFLUENCE_E2E_EMAIL"))
                .expect(
                    "basic auth e2e mode requires CONFLUENCE_E2E_USERNAME or CONFLUENCE_E2E_EMAIL",
                ),
        ),
        _ => None,
    };

    Some(environment_e2e_config(
        bin,
        space,
        EnvironmentE2eSettings {
            base_url: base_url.expect("base url present"),
            token: token.expect("token present"),
            auth_type,
            provider,
            api_path,
            token_kind,
            cloud_id,
            username,
        },
    ))
}

fn environment_e2e_config(
    bin: PathBuf,
    space: String,
    settings: EnvironmentE2eSettings,
) -> E2eConfig {
    let mut envs = vec![
        ("CONFLUENCE_DOMAIN".to_string(), settings.base_url),
        ("CONFLUENCE_PROVIDER".to_string(), settings.provider),
        (
            "CONFLUENCE_AUTH_TYPE".to_string(),
            settings.auth_type.clone(),
        ),
        ("CONFLUENCE_API_PATH".to_string(), settings.api_path),
    ];
    if let Some(token_kind) = settings.token_kind {
        envs.push(("CONFLUENCE_TOKEN_KIND".to_string(), token_kind));
    }
    if let Some(cloud_id) = settings.cloud_id {
        envs.push(("CONFLUENCE_CLOUD_ID".to_string(), cloud_id));
    }
    if settings.auth_type == "basic" {
        envs.push((
            "CONFLUENCE_USERNAME".to_string(),
            settings
                .username
                .expect("basic auth e2e settings require a username"),
        ));
        envs.push(("CONFLUENCE_API_TOKEN".to_string(), settings.token));
    } else {
        envs.push(("CONFLUENCE_BEARER_TOKEN".to_string(), settings.token));
    }

    E2eConfig {
        bin,
        profile: None,
        envs,
        space,
    }
}

fn current_user_placeholder(cfg: &E2eConfig) -> String {
    let status = cfg.run_json(&["auth", "status"]);
    let current_user = status
        .get("current_user")
        .filter(|value| !value.is_null())
        .unwrap_or_else(|| panic!("auth status did not return the current user: {status}"));

    let mut placeholder = Url::parse("confluence-user://user").expect("valid user placeholder");
    {
        let mut pairs = placeholder.query_pairs_mut();
        if let Some(account_id) = current_user.get("account_id").and_then(Value::as_str) {
            pairs.append_pair("account-id", account_id);
        }
        if let Some(user_key) = current_user.get("user_key").and_then(Value::as_str) {
            pairs.append_pair("userkey", user_key);
        }
        if let Some(username) = current_user.get("username").and_then(Value::as_str) {
            pairs.append_pair("username", username);
        }
    }
    placeholder.to_string()
}

fn expected_user_resource_fragment(user_placeholder: &str) -> String {
    let placeholder = Url::parse(user_placeholder).expect("valid user placeholder url");
    for (key, value) in placeholder.query_pairs() {
        match key.as_ref() {
            "account-id" => {
                return format!(r#"ri:account-id="{}""#, value);
            }
            "userkey" => {
                return format!(r#"ri:userkey="{}""#, value);
            }
            "username" => {
                return format!(r#"ri:username="{}""#, value);
            }
            _ => {}
        }
    }
    panic!("expected at least one user identifier in {user_placeholder}");
}

fn expected_user_placeholder_fragment(user_placeholder: &str) -> String {
    let placeholder = Url::parse(user_placeholder).expect("valid user placeholder url");
    for pair in placeholder.query().unwrap_or_default().split('&') {
        if pair.starts_with("account-id=")
            || pair.starts_with("userkey=")
            || pair.starts_with("username=")
        {
            return pair.to_string();
        }
    }
    panic!("expected at least one user identifier in {user_placeholder}");
}

fn find_binary_path() -> PathBuf {
    if let Some(path) = env::var_os("CONFLUENCE_CLI_BIN") {
        return PathBuf::from(path);
    }

    if let Some(path) = env::var_os("CARGO_BIN_EXE_confluence") {
        return PathBuf::from(path);
    }

    let current_exe = env::current_exe().expect("current exe path");
    for ancestor in current_exe.ancestors() {
        let candidate = ancestor.join("confluence");
        if candidate.is_file() {
            return candidate;
        }
        let exe_candidate = ancestor.join("confluence.exe");
        if exe_candidate.is_file() {
            return exe_candidate;
        }
    }

    panic!(
        "failed to locate confluence binary from {}",
        current_exe.display()
    );
}

#[test]
fn binary_discovery_finds_the_declared_confluence_executable() {
    let path = find_binary_path();
    assert!(path.is_file(), "binary does not exist: {}", path.display());
    assert!(
        matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some("confluence" | "confluence.exe")
        ),
        "unexpected binary name: {}",
        path.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_validation_makes_no_requests_and_valid_stdin_reaches_the_server_unchanged() {
    use std::io::Write;
    use std::process::Stdio;

    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().unwrap();
    let cfg = simulated_cloud_config(&simulator, &config_home);
    let invalid_cfg = cfg.clone();
    tokio::task::spawn_blocking(move || {
        for command in [
            vec!["page", "create", "Title", "TEST"],
            vec!["page", "update", "100"],
            vec!["blog", "create", "Title", "TEST"],
            vec!["blog", "update", "100"],
            vec!["comment", "add", "100"],
            vec!["comment", "update", "100"],
        ] {
            for (format, body) in [
                ("storage", "<p></wrong>"),
                ("markdown", "```confluence-storage\n<p></wrong>\n```"),
            ] {
                let mut args = command.clone();
                args.extend(["--format", format, "--body", body, "-o", "json"]);
                let output = invalid_cfg.command().args(&args).output().unwrap();
                assert_eq!(output.status.code(), Some(2), "{args:?}");
                let error: Value = serde_json::from_slice(&output.stderr).unwrap();
                assert_eq!(error["error"]["kind"], "invalid_input");
                assert!(output.stdout.is_empty());
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        simulator.request_count().await,
        0,
        "invalid bodies must not even fetch metadata"
    );

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let storage = "<p>é &nbsp; &amp;</p>\n<ac:structured-macro ac:name='code'><ac:plain-text-body><![CDATA[<literal>&unknown;]]]]><![CDATA[>]]></ac:plain-text-body></ac:structured-macro>";
        let mut child = cfg.command()
            .args(["page", "update", "100", "--format", "storage", "--body-file", "-", "-o", "json"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().unwrap();
        child.stdin.take().unwrap().write_all(storage.as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let fetched = cfg.run_json(&["page", "get", "100", "--show-body"]);
        assert_eq!(string_field(first_item(&fetched, "page get"), "body_storage"), storage);
        assert_eq!(first_item(&fetched, "page get")["version"], 2);
    }).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simulated_cloud_versioned_body_update_preserves_intervening_edits() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().unwrap();
    let cfg = simulated_cloud_config(&simulator, &config_home);
    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let snapshot = cfg.run_json(&["page", "get", "100", "--show-body"]);
        let base_version = first_item(&snapshot, "snapshot")["version"].to_string();
        cfg.run_json(&[
            "page",
            "update",
            "100",
            "--version",
            &base_version,
            "--format",
            "storage",
            "--body",
            "<p>Intervening edit</p>",
        ]);
        let stale = cfg
            .command()
            .args([
                "page",
                "update",
                "100",
                "--version",
                &base_version,
                "--format",
                "storage",
                "--body",
                "<p>Stale candidate</p>",
                "-o",
                "json",
            ])
            .output()
            .unwrap();
        assert_eq!(stale.status.code(), Some(7));
        let error: Value = serde_json::from_slice(&stale.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "conflict");
        let fetched = cfg.run_json(&["page", "get", "100", "--show-body"]);
        assert_eq!(
            string_field(first_item(&fetched, "page get"), "body_storage"),
            "<p>Intervening edit</p>"
        );
        assert_eq!(first_item(&fetched, "page get")["version"], 2);
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simulated_cloud_user_can_manage_a_page() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().expect("simulator config home");
    let cfg = simulated_cloud_config(&simulator, &config_home);

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let created = cfg.run_json(&[
            "page",
            "create",
            "Simulator page",
            "TEST",
            "--body",
            "Initial body",
        ]);
        let page_id = string_field(first_item(&created, "page create"), "id").to_string();

        let fetched = cfg.run_json(&["page", "get", &page_id, "--show-body"]);
        assert_eq!(
            string_field(first_item(&fetched, "page get"), "body_storage"),
            "<p>Initial body</p>"
        );

        let updated = cfg.run_json(&[
            "page",
            "update",
            &page_id,
            "--title",
            "Updated simulator page",
            "--body",
            "Updated body",
        ]);
        assert_eq!(
            string_field(first_item(&updated, "page update"), "title"),
            "Updated simulator page"
        );

        cfg.run(&["--yes", "page", "delete", &page_id]);
    })
    .await
    .expect("simulated lifecycle task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simulated_cloud_supports_the_live_canary_contract() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().expect("simulator config home");
    let cfg = simulated_cloud_config(&simulator, &config_home);

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        run_cli_canary(cfg);
    })
    .await
    .expect("simulated canary task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_shows_the_name_of_a_mentioned_user_and_plans_no_change() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().expect("simulator config home");
    let cfg = simulated_cloud_config(&simulator, &config_home);
    let storage = format!(
        r#"<p>Ask <ac:link><ri:user ri:account-id="{KNOWN_USER_ID}" /></ac:link> or <ac:link><ri:user ri:account-id="gone-user" /></ac:link>.</p>"#
    );
    let page_id = simulator.add_page("Mentions", Some("100"), &storage);

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let temp = TempDir::new().expect("pull destination");
        let pull_dir = temp.path().join("pull");
        let pull_arg = pull_dir.to_string_lossy().into_owned();

        let pulled = cfg.run_json(&["pull", "page", &page_id, &pull_arg]);
        assert_eq!(
            list_items(&pulled, "pull")[0]["storage_differs_from_remote"],
            false
        );
        let markdown = fs::read_to_string(find_index_md(&pull_dir)).expect("pulled Markdown");
        assert!(
            markdown.contains(&format!("Ask [@{KNOWN_USER_NAME}](confluence-user://user?")),
            "{markdown}"
        );
        assert!(
            markdown.contains("or [gone-user](confluence-user://user?"),
            "{markdown}"
        );

        let plan = cfg.run_json(&["plan", &pull_arg]);
        let items = list_items(&plan, "plan");
        assert!(items.iter().all(|item| item["action"] == "noop"), "{plan}");
    })
    .await
    .expect("mention pull task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_goes_on_when_the_name_of_a_mentioned_user_cannot_be_looked_up() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().expect("simulator config home");
    let cfg = simulated_cloud_config(&simulator, &config_home);
    let storage =
        format!(r#"<p>Ask <ac:link><ri:user ri:account-id="{FAILING_USER_ID}" /></ac:link>.</p>"#);
    let page_id = simulator.add_page("Mentions", Some("100"), &storage);

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let temp = TempDir::new().expect("pull destination");
        let pull_dir = temp.path().join("pull");
        let pull_arg = pull_dir.to_string_lossy().into_owned();

        let pulled = cfg
            .command()
            .args(["pull", "page", &page_id, &pull_arg, "-o", "json"])
            .output()
            .expect("pull");
        assert_eq!(pulled.status.code(), Some(0), "{pulled:?}");
        let warning = String::from_utf8_lossy(&pulled.stderr);
        assert!(
            warning.contains("names of mentioned users could not be looked up"),
            "{warning}"
        );
        let markdown = fs::read_to_string(find_index_md(&pull_dir)).expect("pulled Markdown");
        assert!(
            markdown.contains(&format!("Ask [{FAILING_USER_ID}](confluence-user://user?")),
            "{markdown}"
        );

        let plan = cfg.run_json(&["plan", &pull_arg]);
        let items = list_items(&plan, "plan");
        assert!(items.iter().all(|item| item["action"] == "noop"), "{plan}");
    })
    .await
    .expect("mention pull task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_marks_a_page_that_markdown_does_not_carry_until_an_update_rewrites_it() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().expect("simulator config home");
    let cfg = simulated_cloud_config(&simulator, &config_home);
    let wrapped_storage = "<p><span>Wrapped text</span></p>";
    let page_id = simulator.add_page("Wrapped", Some("100"), wrapped_storage);

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let temp = TempDir::new().expect("pull destination");
        let pull_dir = temp.path().join("pull");
        let pull_arg = pull_dir.to_string_lossy().into_owned();
        let sidecar = |index: &PathBuf| -> Value {
            serde_json::from_str(
                &fs::read_to_string(index.with_file_name(".confluence.json")).expect("sidecar"),
            )
            .expect("sidecar JSON")
        };
        let remote_body = || {
            let fetched = cfg.run_json(&["page", "get", &page_id, "--show-body"]);
            string_field(first_item(&fetched, "page get"), "body_storage").to_string()
        };

        let pulled = cfg
            .command()
            .args(["pull", "page", &page_id, &pull_arg, "-o", "json"])
            .output()
            .expect("pull");
        assert!(pulled.status.success(), "{pulled:?}");
        let listing: Value = serde_json::from_slice(&pulled.stdout).expect("pull JSON");
        assert_eq!(
            list_items(&listing, "pull")[0]["storage_differs_from_remote"],
            true
        );
        assert!(
            String::from_utf8_lossy(&pulled.stderr).contains("1 of 1 pulled pages"),
            "{pulled:?}"
        );
        let index = find_index_md(&pull_dir);
        assert_eq!(sidecar(&index)["storage_differs_from_remote"], true);
        let pulled_storage_hash = sidecar(&index)["remote_storage_hash"].clone();
        assert!(pulled_storage_hash.is_string(), "{pulled_storage_hash}");

        let plan = cfg.run_json(&["plan", &pull_arg]);
        let item = &list_items(&plan, "plan")[0];
        assert_eq!(item["action"], "noop");
        assert_eq!(item["content_id"], page_id.as_str());
        assert!(
            string_field(item, "details").contains("the first update also rewrites markup"),
            "{item}"
        );

        // An apply with nothing to publish leaves the page, and its mark, alone.
        cfg.run(&["--yes", "apply", &pull_arg]);
        assert_eq!(remote_body(), wrapped_storage);
        assert_eq!(sidecar(&index)["storage_differs_from_remote"], true);
        assert_eq!(sidecar(&index)["remote_storage_hash"], pulled_storage_hash);

        let edited = fs::read_to_string(&index)
            .expect("pulled markdown")
            .replace("Wrapped text", "Edited text");
        fs::write(&index, edited).expect("edit markdown");
        let plan = cfg.run_json(&["plan", &pull_arg]);
        let item = &list_items(&plan, "plan")[0];
        assert_eq!(item["action"], "update_content");
        assert!(
            string_field(item, "details").contains("also rewrites markup"),
            "{item}"
        );

        cfg.run(&["--yes", "apply", &pull_arg]);
        assert_eq!(remote_body(), "<p>Edited text</p>");
        assert!(sidecar(&index).get("storage_differs_from_remote").is_none());
        assert_ne!(sidecar(&index)["remote_storage_hash"], pulled_storage_hash);
        let plan = cfg.run_json(&["plan", &pull_arg]);
        assert_eq!(
            string_field(&list_items(&plan, "plan")[0], "details"),
            "no changes detected"
        );
    })
    .await
    .expect("simulated pull task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_checkout_with_old_sidecar_cannot_publish_twice() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().expect("simulator config home");
    let cfg = simulated_data_center_config(&simulator, &config_home);
    let page_id = simulator.add_page("CI guide", None, "<p>Initial body</p>");

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let temp = TempDir::new().expect("clean checkouts");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        cfg.run_json(&["pull", "page", &page_id, first.to_str().unwrap()]);
        let first_index = find_index_md(&first);
        let original = fs::read_to_string(&first_index).unwrap();
        let second_page = second.join(first_index.parent().unwrap().file_name().unwrap());
        fs::create_dir_all(&second_page).unwrap();
        fs::copy(
            first_index.with_file_name(".confluence.json"),
            second_page.join(".confluence.json"),
        )
        .unwrap();
        fs::write(
            &first_index,
            original.replace("Initial body", "First publish"),
        )
        .unwrap();
        fs::write(
            second_page.join("index.md"),
            original.replace("Initial body", "Second publish"),
        )
        .unwrap();

        cfg.run(&["label", "add", &page_id, "changed-in-confluence"]);
        let preview = cfg
            .command()
            .args([
                "plan",
                first.to_str().unwrap(),
                "--check-remote",
                "-o",
                "json",
            ])
            .output()
            .unwrap();
        assert_eq!(preview.status.code(), Some(7), "{preview:?}");
        assert!(
            String::from_utf8_lossy(&preview.stderr).contains("remote metadata drift"),
            "{preview:?}"
        );
        cfg.run(&[
            "--yes",
            "label",
            "remove",
            &page_id,
            "changed-in-confluence",
        ]);
        cfg.run_json(&["plan", first.to_str().unwrap(), "--check-remote"]);

        cfg.run_json(&["apply", first.to_str().unwrap()]);
        let output = cfg
            .command()
            .args(["apply", second.to_str().unwrap(), "-o", "json"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(7), "{output:?}");
        let remote = cfg.run_json(&["page", "get", &page_id, "--show-body"]);
        assert_eq!(
            first_item(&remote, "page get")["body_storage"],
            "<p>First publish</p>"
        );
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_and_apply_resolve_pulled_links_for_equivalent_paths() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().expect("simulator config home");
    let cfg = simulated_cloud_config(&simulator, &config_home);
    let root_id = simulator.add_page(
        "Linked tree",
        Some("100"),
        r#"<p><ac:link><ri:page ri:content-title="Child" /><ac:plain-text-link-body><![CDATA[Read the child]]></ac:plain-text-link-body></ac:link></p>"#,
    );
    let child_id = simulator.add_page("Child", Some(&root_id), "<p>Child body</p>");

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let temp = TempDir::new().expect("pull destination");
        let tree = temp.path().join("tree");
        let work = temp.path().join("work");
        fs::create_dir(&work).expect("working directory");
        let absolute = tree.to_str().expect("UTF-8 path");
        cfg.run_json(&["pull", "tree", &root_id, absolute]);
        let index = find_index_md_by_title(&tree, "Linked tree");
        let markdown = fs::read_to_string(&index).expect("pulled Markdown");
        assert!(
            markdown.contains(&format!("[Read the child](child--{child_id}/index.md)")),
            "{markdown}"
        );

        let run_here = |cwd: &Path, args: &[&str]| {
            let output = cfg
                .command()
                .current_dir(cwd)
                .arg("--json")
                .args(args)
                .output()
                .expect("CLI command");
            assert!(output.status.success(), "{args:?}: {output:?}");
            serde_json::from_slice::<Value>(&output.stdout).expect("CLI JSON")
        };
        let plan_paths = [
            (temp.path(), "tree"),
            (temp.path(), "./tree"),
            (temp.path(), absolute),
            (work.as_path(), "../tree"),
        ];
        for (cwd, path) in plan_paths {
            let plan = run_here(cwd, &["plan", path, "--diff"]);
            let items = list_items(&plan, "plan");
            assert_eq!(items.len(), 1, "{path}: {plan}");
            assert_eq!(items[0]["action"], "noop", "{path}: {plan}");
        }

        let remote_page = || cfg.run_json(&["page", "get", &root_id, "--show-body"]);
        let before = remote_page();
        assert_eq!(first_item(&before, "page get")["version"], 1);
        run_here(temp.path(), &["--yes", "apply", "./tree"]);
        assert_eq!(remote_page(), before);

        fs::write(&index, markdown.replace("Read the child", "Open the child")).expect("edit");
        for (cwd, path) in plan_paths {
            let plan = run_here(cwd, &["plan", path, "--diff"]);
            let items = list_items(&plan, "plan");
            assert_eq!(items.len(), 1, "{path}: {plan}");
            let item = &items[0];
            assert_eq!(item["content_id"], root_id.as_str(), "{path}: {plan}");
            assert_eq!(item["action"], "update_content", "{path}: {plan}");
        }
        run_here(temp.path(), &["--yes", "apply", "./tree"]);
        let after = remote_page();
        let page = first_item(&after, "page get");
        assert_eq!(page["version"], 2);
        assert_eq!(
            page["body_storage"],
            format!(r#"<p><a href="/wiki/pages/viewpage.action?pageId={child_id}">Open the child</a></p>"#)
        );
    })
    .await
    .expect("linked tree task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tree_refresh_keeps_folder_names_and_links_and_refuses_remote_moves() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().unwrap();
    let cfg = simulated_cloud_config(&simulator, &config_home);
    let root_id = simulator.add_page("✨ Handbook", Some("100"), "<p>Intro</p>");
    let outside_id = simulator.add_page("Outside", Some("100"), "<p>Outside tree</p>");
    let chapter_id = simulator.add_page("First Chapter", Some(&root_id), "<p>Chapter text</p>");
    let appendix_id = simulator.add_page("Appendix", Some(&root_id), "<p>Appendix text</p>");
    let reserved_id = simulator.add_page("Attachments", Some(&root_id), "<p>Page text</p>");
    let collision_id =
        simulator.add_page("👩‍💻 First Chapter", Some(&root_id), "<p>Another chapter</p>");

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let temp = TempDir::new().unwrap();
        let tree = temp.path().join("tree");
        let path = tree.to_str().unwrap();
        let refresh = ["pull", "tree", &root_id, path, "--layout", "slug"];
        cfg.run_json(&refresh);
        let generated_root = tree.join("handbook");
        assert!(generated_root.join("first-chapter/index.md").is_file());
        assert!(
            generated_root
                .join(format!("first-chapter--{collision_id}/index.md"))
                .is_file()
        );
        assert!(generated_root.join("appendix/index.md").is_file());
        assert!(
            generated_root
                .join(format!("attachments--{reserved_id}/index.md"))
                .is_file()
        );

        let handbook = tree.join("Handbook Manual");
        fs::rename(&generated_root, &handbook).unwrap();
        let chapter = handbook.join("Chapter Notes (#1)");
        fs::rename(handbook.join("first-chapter"), &chapter).unwrap();
        fs::rename(handbook.join("appendix"), handbook.join("Chapter")).unwrap();
        let newcomer_id = simulator.add_page("Chapter", Some(&root_id), "<p>New page</p>");
        let body = format!(
            r#"<p><a href="/wiki/pages/viewpage.action?pageId={chapter_id}">Read chapter</a></p><p><ac:link><ri:page ri:content-title="First Chapter" /><ac:plain-text-link-body><![CDATA[Read again]]></ac:plain-text-link-body></ac:link></p>"#
        );
        cfg.run_json(&[
            "page",
            "update",
            &root_id,
            "--title",
            "Handbook Revised",
            "--format",
            "storage",
            "--body",
            &body,
        ]);
        cfg.run_json(&refresh);
        let index = handbook.join("index.md");
        let markdown = fs::read_to_string(&index).unwrap();
        assert!(markdown.contains("title: Handbook Revised"), "{markdown}");
        for text in ["Read chapter", "Read again"] {
            assert!(markdown.contains(&format!("[{text}](Chapter%20Notes%20%28%231%29/index.md)")), "{markdown}");
        }
        assert!(chapter.join("index.md").is_file());
        assert!(
            handbook
                .join(format!("chapter--{newcomer_id}/index.md"))
                .is_file()
        );
        assert!(!generated_root.exists());
        cfg.run_json(&["pull", "tree", &root_id, path]);
        assert_eq!(fs::read_to_string(&index).unwrap(), markdown);
        assert!(!tree.join("handbook-revised").exists());
        assert!(
            list_items(&cfg.run_json(&["plan", path]), "plan")
                .iter()
                .all(|item| item["action"] == "noop")
        );

        let chapter_index = chapter.join("index.md");
        let contents = fs::read_to_string(&chapter_index)
            .unwrap()
            .replace("Chapter text", "Published text");
        fs::write(&chapter_index, &contents).unwrap();
        let rejected = cfg.command().args(refresh).args(["-o", "json"]).output().unwrap();
        assert!(!rejected.status.success(), "{rejected:?}");
        let error: Value = serde_json::from_slice(&rejected.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "conflict");
        assert_eq!(
            error["error"]["details"]["blockers"][0]["path"],
            "Handbook Manual/Chapter Notes (#1)/index.md"
        );
        assert_eq!(fs::read_to_string(&chapter_index).unwrap(), contents);
        let applied = cfg.run_json(&["--yes", "apply", path]);
        assert_eq!(list_items(&applied, "apply")[0]["content_id"], chapter_id);

        cfg.run_json(&["page", "update", &root_id, "--parent", &outside_id]);
        cfg.run_json(&refresh);
        assert!(chapter_index.is_file());
        let sidecar: Value = serde_json::from_slice(&fs::read(handbook.join(".confluence.json")).unwrap()).unwrap();
        assert_eq!(sidecar["remote_parent_id"], outside_id);

        let subset = temp.path().join("subset");
        let subset_arg = subset.to_str().unwrap();
        cfg.run_json(&["pull", "tree", &chapter_id, subset_arg, "--layout", "slug"]);
        let expanded = cfg.command().args(["pull", "tree", &root_id, subset_arg]).output().unwrap();
        assert_eq!(expanded.status.code(), Some(7), "{expanded:?}");
        assert!(subset.join("first-chapter/index.md").is_file());
        assert!(!subset.join("index.md").exists());

        cfg.run_json(&["page", "update", &chapter_id, "--parent", &appendix_id]);
        let rejected = cfg.command().args(refresh).output().unwrap();
        assert_eq!(rejected.status.code(), Some(7), "{rejected:?}");
        let error: Value = serde_json::from_slice(&rejected.stderr).unwrap();
        assert!(
            string_field(&error["error"], "message").contains("changed parent"),
            "{error}"
        );
        assert!(chapter_index.is_file());
        assert!(!handbook.join("Chapter/first-chapter").exists());

        cfg.run_json(&[
            "pull", "tree", &root_id, path, "--layout", "slug", "--force",
        ]);
        assert!(
            tree.join("handbook-revised/appendix/first-chapter/index.md")
                .is_file()
        );
        assert!(!handbook.exists());
        cfg.run_json(&["page", "update", &chapter_id, "--parent", &outside_id]);
        let missing = cfg.command().args(refresh).output().unwrap();
        assert_eq!(missing.status.code(), Some(7), "{missing:?}");
        let error: Value = serde_json::from_slice(&missing.stderr).unwrap();
        assert!(string_field(&error["error"], "message").contains("no longer in the remote tree"), "{error}");
        let preview = cfg.run_json(&["pull", "preview-tree", &root_id, path]);
        let missing = list_items(&preview, "pull preview-tree")
            .iter()
            .find(|row| row["content_id"] == chapter_id)
            .unwrap();
        assert_eq!(missing["change"], "missing");
        assert_eq!(missing["previous_path"], "handbook-revised/appendix/first-chapter");
        assert_eq!(missing["path"], Value::Null);
        assert!(tree.join("handbook-revised/appendix/first-chapter/index.md").is_file());
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tree_pull_previews_and_can_accept_specific_remote_moves() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().unwrap();
    let cfg = simulated_cloud_config(&simulator, &config_home);
    let root = simulator.add_page("Guide", None, "<p>Intro</p>");
    let source = simulator.add_page("Source", Some(&root), "<p>Source</p>");
    let target = simulator.add_page("Target", Some(&root), "<p>Target</p>");
    let moved = simulator.add_page("First page", Some(&source), "<p>First</p>");
    let second = simulator.add_page("Second page", Some(&source), "<p>Second</p>");
    let child = simulator.add_page("Child", Some(&moved), "<p>Child</p>");
    simulator.add_page("Existing page", Some(&target), "<p>Existing</p>");

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let destination = TempDir::new().unwrap();
        let tree = destination.path().join("tree");
        let path = tree.to_str().unwrap();
        let pull = ["pull", "tree", &root, path, "--layout", "slug"];
        let fresh = destination.path().join("uncreated/tree");
        let preview = cfg.run_json(&[
            "pull",
            "preview-tree",
            &root,
            fresh.to_str().unwrap(),
            "--layout",
            "slug",
        ]);
        assert!(
            list_items(&preview, "pull preview-tree")
                .iter()
                .all(|row| row["change"] == "new")
        );
        assert!(!fresh.parent().unwrap().exists());
        cfg.run_json(&pull);
        let before = tree.join("guide/source/Custom Page");
        fs::rename(tree.join("guide/source/first-page"), &before).unwrap();
        fs::rename(before.join("child"), before.join("My Child")).unwrap();
        let occupied = tree.join("guide/target/Custom Page");
        fs::rename(tree.join("guide/target/existing-page"), &occupied).unwrap();
        let old_markdown = fs::read(tree.join("guide/index.md")).unwrap();
        for id in [&moved, &second] {
            cfg.run_json(&["page", "update", id, "--parent", &target]);
        }
        let newcomer = simulator.add_page("Second-Page", Some(&source), "<p>New page</p>");
        let body = format!(
            r#"<p><a href="/wiki/pages/viewpage.action?pageId={child}">Read child</a></p>"#
        );
        cfg.run_json(&[
            "page", "update", &root, "--format", "storage", "--body", &body,
        ]);

        let read_only = cfg
            .command()
            .env("CONFLUENCE_READ_ONLY", "true")
            .args([
                "pull",
                "preview-tree",
                &root,
                path,
                "--layout",
                "slug",
                "-o",
                "json",
            ])
            .output()
            .unwrap();
        assert!(read_only.status.success(), "{read_only:?}");
        let preview: Value = serde_json::from_slice(&read_only.stdout).unwrap();
        let rows = list_items(&preview, "pull preview-tree");
        let row = |id: &str| rows.iter().find(|item| item["content_id"] == id).unwrap();
        let proposed = format!("guide/target/Custom Page--{moved}");
        assert_eq!(row(&moved)["change"], "move");
        assert_eq!(row(&moved)["previous_path"], "guide/source/Custom Page");
        assert_eq!(row(&moved)["path"], proposed);
        assert_eq!(row(&moved)["parent_id"], target);
        assert_eq!(row(&second)["change"], "move");
        assert_eq!(
            row(&newcomer)["path"],
            format!("guide/source/second-page--{newcomer}")
        );
        assert_eq!(row(&child)["change"], "relocated");
        assert_eq!(row(&child)["path"], format!("{proposed}/My Child"));
        assert_eq!(fs::read(tree.join("guide/index.md")).unwrap(), old_markdown);
        assert!(before.is_dir());
        assert!(!tree.join(&proposed).exists());

        let moved_arg = format!("{moved}={target}");
        let second_arg = format!("{second}={target}");
        let mut partial = pull.to_vec();
        partial.extend(["--accept-move", &moved_arg]);
        let rejected = cfg.command().args(partial).arg("--json").output().unwrap();
        assert_eq!(rejected.status.code(), Some(7), "{rejected:?}");
        assert!(before.is_dir());
        let error: Value = serde_json::from_slice(&rejected.stderr).unwrap();
        assert_eq!(error["error"]["details"]["moves"][0]["content_id"], second);

        let mut accepted = pull.to_vec();
        accepted.extend(["--accept-move", &moved_arg, "--accept-move", &second_arg]);
        let changed_destination =
            simulator.add_page("Different parent", Some(&root), "<p>Other</p>");
        cfg.run_json(&["page", "update", &moved, "--parent", &changed_destination]);
        let rejection = cfg
            .command()
            .args(&accepted)
            .arg("--json")
            .output()
            .unwrap();
        assert_eq!(rejection.status.code(), Some(7), "{rejection:?}");
        assert!(String::from_utf8_lossy(&rejection.stderr).contains("current parent"));
        assert!(before.is_dir());
        cfg.run_json(&["page", "update", &moved, "--parent", &target]);
        cfg.run_json(&accepted);
        assert!(tree.join(&proposed).join("My Child/index.md").is_file());
        assert!(tree.join("guide/target/second-page/index.md").is_file());
        assert!(
            tree.join(format!("guide/source/second-page--{newcomer}/index.md"))
                .is_file()
        );
        assert!(occupied.join("index.md").is_file());
        assert!(!before.exists());
        let markdown = fs::read_to_string(tree.join("guide/index.md")).unwrap();
        assert!(
            markdown.contains(&format!(
                "[Read child](target/Custom%20Page--{moved}/My%20Child/index.md)"
            )),
            "{markdown}"
        );
        assert!(
            list_items(&cfg.run_json(&["plan", path]), "plan")
                .iter()
                .all(|item| item["action"] == "noop")
        );
        let stale = cfg.command().args(accepted).output().unwrap();
        assert!(!stale.status.success(), "{stale:?}");
        assert!(
            String::from_utf8_lossy(&stale.stderr).contains("does not have a remote parent move")
        );
        cfg.run_json(&pull);
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tree_pull_requires_acceptance_for_moves_of_previously_unconvertible_ancestors() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().unwrap();
    let cfg = simulated_cloud_config(&simulator, &config_home);
    let broken = r#"<ac:structured-macro ac:name="code"><ac:rich-text-body><p>x</p></ac:rich-text-body></ac:structured-macro>"#;
    let root = simulator.add_page("Root", None, "<p>Root text</p>");
    let parent = simulator.add_page("Old parent", Some(&root), broken);
    let moved = simulator.add_page("Movable", Some(&parent), broken);
    let leaf = simulator.add_page("Leaf", Some(&moved), "<p>Leaf text</p>");

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let temp = TempDir::new().unwrap();
        let tree = temp.path().join("tree");
        let path = tree.to_str().unwrap();
        let pull = ["pull", "tree", &root, path, "--layout", "slug"];
        let run = |args: &[&str]| cfg.command().args(args).arg("--json").output().unwrap();
        let partial = run(&pull);
        assert_eq!(partial.status.code(), Some(2), "{partial:?}");
        let old_leaf = tree.join("root/old-parent/movable/leaf/index.md");
        assert!(old_leaf.is_file());
        assert!(!tree.join("root/old-parent/movable/index.md").exists());
        let new_parent = simulator.add_page("New parent", Some(&root), "<p>New parent text</p>");
        cfg.run_json(&[
            "page",
            "update",
            &moved,
            "--parent",
            &new_parent,
            "--format",
            "storage",
            "--body",
            "<p>Recovered</p>",
        ]);
        let preview = cfg.run_json(&["pull", "preview-tree", &root, path, "--layout", "slug"]);
        let rows = list_items(&preview, "pull preview-tree");
        let row = |id: &str| rows.iter().find(|row| row["content_id"] == id).unwrap();
        assert_eq!(row(&moved)["change"], "move");
        assert_eq!(row(&moved)["previous_parent_id"], parent);
        assert_eq!(row(&moved)["path"], "root/new-parent/movable");
        assert_eq!(row(&leaf)["change"], "relocated");

        let failures = tree.join(".confluence-pull-failures");
        let manifest_path = failures.join("manifest.json");
        let original_manifest = fs::read(&manifest_path).unwrap();
        let mut legacy_manifest: serde_json::Value =
            serde_json::from_slice(&original_manifest).unwrap();
        let mut placements = Vec::new();
        for artifact in [
            format!("old-parent--{parent}/placement.json"),
            format!("movable--{moved}/placement.json"),
        ] {
            let artifact_path = failures.join(&artifact);
            placements.push((artifact_path.clone(), fs::read(&artifact_path).unwrap()));
            fs::remove_file(&artifact_path).unwrap();
            assert!(
                legacy_manifest
                    .as_object_mut()
                    .unwrap()
                    .remove(&artifact)
                    .is_some()
            );
            fs::write(
                &manifest_path,
                serde_json::to_vec(&legacy_manifest).unwrap(),
            )
            .unwrap();
            let legacy_preview = run(&["pull", "preview-tree", &root, path, "--layout", "slug"]);
            if placements.len() == 1 {
                assert!(legacy_preview.status.success(), "{legacy_preview:?}");
                let legacy_rows: serde_json::Value =
                    serde_json::from_slice(&legacy_preview.stdout).unwrap();
                assert_eq!(legacy_rows, preview);
            } else {
                assert_eq!(legacy_preview.status.code(), Some(7), "{legacy_preview:?}");
                assert!(old_leaf.is_file());
            }
        }
        for (artifact_path, original) in placements {
            fs::write(artifact_path, original).unwrap();
        }
        fs::write(manifest_path, original_manifest).unwrap();

        let refused = run(&pull);
        assert_eq!(refused.status.code(), Some(7), "{refused:?}");
        assert!(old_leaf.is_file());
        let acceptance = format!("{moved}={new_parent}");
        let mut accepted = pull.to_vec();
        accepted.extend(["--accept-move", &acceptance]);
        let partial = run(&accepted);
        assert_eq!(partial.status.code(), Some(2), "{partial:?}");
        assert!(tree.join("root/new-parent/movable/leaf/index.md").is_file());
        assert!(tree.join("root/new-parent/movable/index.md").is_file());
        assert!(!old_leaf.exists());
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_page_resolves_links_to_a_sibling_without_applying_it() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().expect("simulator config home");
    let cfg = simulated_cloud_config(&simulator, &config_home);
    let root_id = simulator.add_page("Linked tree", Some("100"), "<p>Root</p>");
    let sibling_id = simulator.add_page("Sibling", Some(&root_id), "<p>Sibling body</p>");
    let source_id = simulator.add_page(
        "Source",
        Some(&root_id),
        concat!(
            r#"<p><ac:link><ri:page ri:content-title="Sibling" /><ac:plain-text-link-body><![CDATA[Read the sibling]]></ac:plain-text-link-body></ac:link></p>"#,
            r#"<ac:structured-macro ac:name="include"><ac:parameter ac:name=""><ac:link><ri:page ri:space-key="TEST" ri:content-title="Sibling" /></ac:link></ac:parameter></ac:structured-macro>"#,
        ),
    );

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let temp = TempDir::new().expect("pull destination");
        let attachment = temp.path().join("report.pdf");
        fs::write(&attachment, "attachment fixture").expect("write attachment");
        cfg.run_json(&[
            "attachment",
            "upload",
            &sibling_id,
            "--file",
            attachment.to_str().unwrap(),
        ]);
        let tree = temp.path().join("tree");
        let tree_arg = tree.to_str().unwrap();
        cfg.run_json(&["pull", "tree", &root_id, tree_arg]);
        let source = find_index_md_by_title(&tree, "Source");
        let sibling = find_index_md_by_title(&tree, "Sibling");
        let selected = source.parent().unwrap().to_str().unwrap();
        let sibling_link = format!("../sibling--{sibling_id}");
        let markdown = fs::read_to_string(&source).expect("source Markdown");
        assert!(
            markdown.contains(&format!("[Read the sibling]({sibling_link}/index.md)")),
            "{markdown}"
        );
        assert!(
            markdown.contains(&format!("page: {sibling_link}/index.md")),
            "{markdown}"
        );

        for path in [tree_arg, selected] {
            let plan = cfg.run_json(&["plan", path, "--diff"]);
            let items = list_items(&plan, "plan");
            assert!(
                !items.is_empty() && items.iter().all(|item| item["action"] == "noop"),
                "{path}: {plan}"
            );
        }
        let remote_page = |id| cfg.run_json(&["page", "get", id, "--show-body"]);
        let source_before = remote_page(&source_id);
        let sibling_before = remote_page(&sibling_id);
        cfg.run_json(&["--yes", "apply", selected]);
        assert_eq!(remote_page(&source_id), source_before);

        let sibling_markdown = fs::read_to_string(&sibling).expect("sibling Markdown");
        let sibling_edit = sibling_markdown.replace("title: Sibling", "title: Pending rename");
        assert_ne!(sibling_edit, sibling_markdown);
        fs::write(&sibling, &sibling_edit).expect("edit sibling outside selection");
        let sibling_sidecar_path = sibling.with_file_name(".confluence.json");
        let sibling_sidecar = fs::read(&sibling_sidecar_path).expect("sibling sidecar");

        let edited = format!("{markdown}\n\n[Download]({sibling_link}/attachments/report.pdf)\n");
        fs::write(
            &source,
            edited.replace("Read the sibling", "Open the sibling"),
        )
        .expect("edit source");
        let plan = cfg.run_json(&["plan", selected, "--diff"]);
        let items = list_items(&plan, "plan");
        assert_eq!(items.len(), 1, "{plan}");
        assert_eq!(items[0]["content_id"], source_id.as_str());
        assert_eq!(items[0]["action"], "update_content");

        cfg.run_json(&["--yes", "apply", selected]);
        let after = remote_page(&source_id);
        let page = first_item(&after, "page get");
        assert_eq!(page["version"], 2);
        let storage = string_field(page, "body_storage");
        assert!(
            storage.contains(&format!(
                r#"href="/wiki/pages/viewpage.action?pageId={sibling_id}">Open the sibling"#
            )),
            "{storage}"
        );
        assert!(
            storage.contains(r#"ri:content-title="Sibling""#),
            "{storage}"
        );
        assert!(
            storage.contains(&format!(
                r#"href="/wiki/download/attachments/{sibling_id}/report.pdf""#
            )),
            "{storage}"
        );
        assert_eq!(remote_page(&sibling_id), sibling_before);
        assert_eq!(fs::read_to_string(&sibling).unwrap(), sibling_edit);
        assert_eq!(fs::read(&sibling_sidecar_path).unwrap(), sibling_sidecar);
    })
    .await
    .expect("selected page task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_sync_rejects_unpublished_attachments_and_foreign_sites() {
    let local = ConfluenceSimulator::start().await;
    let foreign = ConfluenceSimulator::start().await;
    let config = TempDir::new().expect("simulator config home");
    let local_cfg = simulated_cloud_config(&local, &config);
    let foreign_cfg = simulated_cloud_config(&foreign, &config);
    let root_id = local.add_page("Root", Some("100"), "<p>Root</p>");
    let source_id = local.add_page("Source", Some(&root_id), "<p>Original</p>");
    let sibling_id = local.add_page("Sibling", Some(&root_id), "<p>Sibling</p>");
    let foreign_id = foreign.add_page("Foreign", Some("100"), "<p>Other site</p>");

    tokio::task::spawn_blocking(move || {
        let _config = config;
        let temp = TempDir::new().expect("pull destination");
        let tree = temp.path().join("tree");
        local_cfg.run_json(&["pull", "tree", &root_id, tree.to_str().unwrap()]);
        let elsewhere = temp.path().join("foreign");
        foreign_cfg.run_json(&["pull", "page", &foreign_id, elsewhere.to_str().unwrap()]);
        let source = find_index_md_by_title(&tree, "Source");
        let selected = source.parent().unwrap().to_str().unwrap();
        let sibling = find_index_md_by_title(&tree, "Sibling");
        let local_file = sibling.parent().unwrap().join("attachments/local-only.pdf");
        fs::create_dir_all(local_file.parent().unwrap()).expect("attachment directory");
        fs::write(local_file, "unpublished file").expect("local attachment");
        let original = fs::read_to_string(&source).expect("pulled source Markdown");
        let page = || local_cfg.run_json(&["page", "get", &source_id, "--show-body"]);
        let before = page();
        let foreign_markdown = find_index_md_by_title(&elsewhere, "Foreign");
        let foreign_target = pathdiff::diff_paths(foreign_markdown, source.parent().unwrap())
            .expect("relative foreign target");
        let cases = [
            (format!("[Download](../sibling--{sibling_id}/attachments/local-only.pdf)"), "not recorded as published"),
            (format!(":::confluence-view-file\npage: ../sibling--{sibling_id}/index.md\nattachment: local-only.pdf\n:::"), "not recorded as published"),
            (format!("[Foreign]({})", foreign_target.display()), "different Confluence site"),
        ];
        for (link, expected_error) in cases {
            fs::write(&source, format!("{original}\n\n{link}\n")).expect("edit source");
            for args in [&["plan", selected][..], &["--yes", "apply", selected]] {
                let result = local_cfg.command().arg("--json").args(args).output().expect("sync command");
                assert_eq!(result.status.code(), Some(2), "{args:?}: {result:?}");
                let error = String::from_utf8_lossy(&result.stderr);
                assert!(error.contains(expected_error), "{args:?}: {error}");
            }
            assert_eq!(page(), before);
        }

        let same_site = temp.path().join("same-site");
        local_cfg.run_json(&["pull", "page", &sibling_id, same_site.to_str().unwrap()]);
        let same_site_page = find_index_md_by_title(&same_site, "Sibling");
        let relative = pathdiff::diff_paths(&same_site_page, source.parent().unwrap()).unwrap();
        fs::write(&source, format!("{original}\n\n[Read]({})\n", relative.display()))
            .expect("link to an independently pulled page on the same site");
        let plan = local_cfg.run_json(&["plan", selected]);
        assert_eq!(list_items(&plan, "plan")[0]["action"], "update_content");

        for index in [&source, &sibling, &same_site_page, &find_index_md_by_title(&tree, "Root")] {
            let sidecar = index.with_file_name(".confluence.json");
            let mut metadata: Value = serde_json::from_slice(&fs::read(&sidecar).unwrap()).unwrap();
            assert!(metadata.as_object_mut().unwrap().remove("site_fingerprint").is_some());
            fs::write(sidecar, serde_json::to_vec(&metadata).unwrap()).expect("legacy sidecar");
        }
        let result = local_cfg.command().arg("--json").args(["plan", selected]).output().unwrap();
        assert_eq!(result.status.code(), Some(2), "{result:?}");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("cannot verify the Confluence site"),
            "{result:?}"
        );
        fs::write(&source, format!("{original}\n\n[Read](../sibling--{sibling_id}/index.md)\n"))
            .expect("link within a legacy tree");
        let plan = local_cfg.run_json(&["plan", selected]);
        assert_eq!(list_items(&plan, "plan")[0]["action"], "update_content");

        fs::write(&source, format!("{original}\n\n[Parent](../index.md)\n"))
            .expect("link to the legacy parent");
        let selected_dir = source.parent().unwrap();
        let result = local_cfg
            .command()
            .current_dir(selected_dir.parent().unwrap())
            .arg("--json")
            .args(["plan".as_ref(), selected_dir.file_name().unwrap()])
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
        let plan: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(list_items(&plan, "plan")[0]["action"], "update_content");
    })
    .await
    .expect("scoped link validation task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_writes_the_rest_of_a_tree_when_a_page_does_not_convert() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().expect("simulator config home");
    let cfg = simulated_cloud_config(&simulator, &config_home);
    // A code macro holds plain text, so this body is one the converter
    // refuses to write back.
    let broken_storage = r#"<ac:structured-macro ac:name="code"><ac:rich-text-body><p>x</p></ac:rich-text-body></ac:structured-macro>"#;
    let root_id = simulator.add_page(
        "Tree root",
        Some("100"),
        r#"<p>Root text <ac:link><ri:page ri:content-title="Broken" /><ac:plain-text-link-body><![CDATA[the broken page]]></ac:plain-text-link-body></ac:link> and <ac:link><ri:page ri:content-title="Leaf" /><ac:plain-text-link-body><![CDATA[the leaf]]></ac:plain-text-link-body></ac:link></p>"#,
    );
    let broken_id = simulator.add_page("Broken", Some(&root_id), broken_storage);
    let leaf_id = simulator.add_page("Leaf", Some(&broken_id), "<p>Leaf text</p>");

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let temp = TempDir::new().expect("pull destination");
        let pull_dir = temp.path().join("pull");
        let pull_arg = pull_dir.to_string_lossy().into_owned();

        let pulled = cfg
            .command()
            .args(["pull", "tree", &root_id, &pull_arg, "-o", "json"])
            .output()
            .expect("pull");
        assert_eq!(pulled.status.code(), Some(2), "{pulled:?}");
        assert!(pulled.stdout.is_empty(), "{pulled:?}");
        let error: Value = serde_json::from_slice(&pulled.stderr).expect("error JSON");
        let error = &error["error"];
        assert_eq!(error["kind"], "invalid_input");
        assert!(string_field(error, "message").contains("1 of 3"), "{error}");
        let details = &error["details"];
        assert_eq!(details["written"].as_array().map(Vec::len), Some(2));
        let page = &details["pages"][0];
        assert_eq!(page["content_id"], broken_id.as_str());
        assert_eq!(page["title"], "Broken");
        assert!(
            string_field(page, "error").contains("requires ac:plain-text-body"),
            "{page}"
        );
        let artifacts = PathBuf::from(string_field(page, "artifacts"));
        assert!(artifacts.starts_with(&pull_dir), "{artifacts:?}");
        assert_eq!(
            fs::read_to_string(artifacts.join("remote.xml")).expect("remote storage"),
            broken_storage
        );
        assert_eq!(
            fs::read_to_string(artifacts.join("generated.xml")).expect("generated storage"),
            broken_storage
        );
        assert!(artifacts.join("generated.md").is_file());

        let root_index = find_index_md_by_title(&pull_dir, "Tree root");
        let leaf_index = find_index_md_by_title(&pull_dir, "Leaf");
        assert!(
            fs::read_to_string(&leaf_index)
                .expect("leaf markdown")
                .contains("Leaf text")
        );
        let broken_dir = leaf_index
            .parent()
            .and_then(Path::parent)
            .expect("directory of the page left out");
        assert!(
            broken_dir.starts_with(root_index.parent().expect("root directory")),
            "{broken_dir:?}"
        );
        assert!(!broken_dir.join("index.md").exists());

        // The pages that were written sync as usual: the leaf keeps the parent
        // it has remotely, and a second pull may replace the snapshot.
        // A link to the page left out has no local file to point at, so it
        // stays the remote reference; a link to a page that is written is local.
        let root_markdown = fs::read_to_string(&root_index).expect("root markdown");
        assert!(
            root_markdown.contains("[the broken page](confluence-page://"),
            "{root_markdown}"
        );
        assert!(root_markdown.contains("/index.md)"), "{root_markdown}");
        let plan = cfg.run_json(&["plan", &pull_arg]);
        for item in list_items(&plan, "plan") {
            assert_eq!(item["action"], "noop", "{plan}");
        }
        let edited = fs::read_to_string(&leaf_index)
            .expect("leaf markdown")
            .replace("Leaf text", "Edited leaf");
        fs::write(&leaf_index, edited).expect("edit leaf");
        cfg.run(&["--yes", "apply", &pull_arg]);
        let leaf = cfg.run_json(&["page", "get", &leaf_id, "--show-body"]);
        let leaf = first_item(&leaf, "page get");
        assert_eq!(string_field(leaf, "body_storage"), "<p>Edited leaf</p>");
        assert_eq!(string_field(leaf, "parent_id"), broken_id);
        let broken = cfg.run_json(&["page", "get", &broken_id, "--show-body"]);
        assert_eq!(
            string_field(first_item(&broken, "page get"), "body_storage"),
            broken_storage
        );

        let again = cfg
            .command()
            .args(["pull", "tree", &root_id, &pull_arg, "-o", "json"])
            .output()
            .expect("second pull");
        assert_eq!(again.status.code(), Some(2), "{again:?}");
        assert!(
            fs::read_to_string(&leaf_index)
                .expect("leaf markdown")
                .contains("Edited leaf")
        );

        cfg.run_json(&[
            "page",
            "update",
            &broken_id,
            "--title",
            "Recovered parent",
            "--format",
            "storage",
            "--body",
            "<p>Recovered text</p>",
        ]);
        cfg.run_json(&["pull", "tree", &root_id, &pull_arg, "--layout", "slug"]);
        assert!(broken_dir.join("index.md").is_file());
        assert!(leaf_index.is_file());
        assert!(!root_index.with_file_name("recovered-parent").exists());
    })
    .await
    .expect("simulated pull task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cloud_ci_state_publishes_from_clean_checkouts_and_preserves_remote_edits() {
    test_ci_state(simulated_cloud_config).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_center_ci_state_publishes_from_clean_checkouts_and_preserves_remote_edits() {
    test_ci_state(simulated_data_center_config).await;
}

async fn test_ci_state(config: fn(&ConfluenceSimulator, &TempDir) -> E2eConfig) {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().expect("simulator config home");
    let cfg = config(&simulator, &config_home);
    let page_id = simulator.add_page("CI guide", None, "<p>Initial body</p>");

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let temp = TempDir::new().unwrap();
        let seed = temp.path().join("seed");
        let state = temp.path().join("production.json");
        let state_arg = state.to_str().unwrap();
        cfg.run_json(&["pull", "page", &page_id, seed.to_str().unwrap()]);
        let seed_index = find_index_md(&seed);
        let page_dir = seed_index.parent().unwrap().file_name().unwrap();
        let original = fs::read_to_string(&seed_index).unwrap();
        let original_sidecar = fs::read(seed_index.with_file_name(".confluence.json")).unwrap();
        let checkout = |name: &str, body: &str| {
            let root = temp.path().join(name);
            let doc = root.join(page_dir);
            fs::create_dir_all(&doc).unwrap();
            fs::write(doc.join(".confluence.json"), &original_sidecar).unwrap();
            fs::write(doc.join("index.md"), original.replace("Initial body", body)).unwrap();
            let new_page = doc.join("faq");
            fs::create_dir(&new_page).unwrap();
            fs::write(
                new_page.join("index.md"),
                "---\ntitle: FAQ\ntype: page\nproperties:\n  space_key: TEST\n---\n\nFAQ body\n",
            )
            .unwrap();
            root
        };
        let first = checkout("first", "First publish");
        let first_arg = first.to_str().unwrap();
        let second = checkout("second", "Second publish");
        let second_arg = second.to_str().unwrap();
        let renamed = second.join("Publishing Guide");
        fs::rename(second.join(page_dir), &renamed).unwrap();
        let third = checkout("third", "Third publish");
        let third_arg = third.to_str().unwrap();
        let third_renamed = third.join("Publishing Guide v2");
        fs::rename(third.join(page_dir), &third_renamed).unwrap();

        let run = |args: &[&str]| cfg.command().args(args).output().unwrap();
        let remote = || cfg.run_json(&["page", "get", &page_id, "--show-body"]);
        let missing = run(&["apply", first_arg, "--state-file", state_arg]);
        assert!(!missing.status.success(), "{missing:?}");
        assert_eq!(first_item(&remote(), "page")["version"], 1);
        let first_apply = cfg.run_json(&[
            "apply",
            first_arg,
            "--state-file",
            state_arg,
            "--init-state",
        ]);
        let created = list_items(&first_apply, "apply")
            .iter()
            .find(|item| item["action"] == "create_content")
            .expect("new CI page");
        let faq_id = string_field(created, "content_id");
        let before_preview = fs::read(&state).unwrap();
        let ambiguous = checkout("ambiguous", "First publish");
        let ambiguous_path = ambiguous.join(page_dir);
        fs::rename(
            ambiguous_path.join("faq"),
            ambiguous_path.join("renamed-faq"),
        )
        .unwrap();
        let missing_identity = run(&[
            "plan",
            ambiguous.to_str().unwrap(),
            "--state-file",
            state_arg,
        ]);
        assert!(!missing_identity.status.success(), "{missing_identity:?}");
        assert!(
            String::from_utf8_lossy(&missing_identity.stderr)
                .contains("refers to missing document")
        );

        let reused_path = checkout("reused-path", "First publish");
        fs::rename(reused_path.join(page_dir), reused_path.join("Moved Guide")).unwrap();
        let newcomer_seed = temp.path().join("newcomer-seed");
        let newcomer = simulator.add_page("Newcomer", None, "<p>Newcomer body</p>");
        cfg.run_json(&["pull", "page", &newcomer, newcomer_seed.to_str().unwrap()]);
        fs::rename(
            find_index_md(&newcomer_seed).parent().unwrap(),
            reused_path.join(page_dir),
        )
        .unwrap();
        let reused_plan = [
            "plan",
            reused_path.to_str().unwrap(),
            "--state-file",
            state_arg,
        ];
        let overlap = run(&reused_plan);
        assert!(!overlap.status.success(), "{overlap:?}");
        assert!(String::from_utf8_lossy(&overlap.stderr).contains("both paths are in use"));
        fs::rename(reused_path.join(page_dir), reused_path.join("Newcomer")).unwrap();
        let resolved = cfg.run_json(&reused_plan);
        assert!(
            list_items(&resolved, "plan")
                .iter()
                .all(|item| item["action"] == "noop")
        );
        assert_eq!(fs::read(&state).unwrap(), before_preview);

        let lock_path = temp.path().join("production.json.lock");
        let reserved = run(&[
            "apply",
            first_arg,
            "--state-file",
            lock_path.to_str().unwrap(),
            "--init-state",
        ]);
        assert!(!reserved.status.success(), "{reserved:?}");
        assert!(
            String::from_utf8_lossy(&reserved.stderr).contains("reserved for publish state locks")
        );

        let preview = cfg
            .command()
            .current_dir(temp.path())
            .env("CONFLUENCE_READ_ONLY", "true")
            .args([
                "plan",
                "./second",
                "--check-remote",
                "--diff",
                "--state-file",
                state_arg,
                "-o",
                "json",
            ])
            .output()
            .unwrap();
        assert!(preview.status.success(), "{preview:?}");
        let plan: Value = serde_json::from_slice(&preview.stdout).unwrap();
        let items = list_items(&plan, "plan");
        assert_eq!(items.len(), 1, "{plan}");
        assert_eq!(items[0]["action"], "update_content");
        let diff = string_field(&items[0], "diff");
        assert!(
            diff.contains("-First publish") && diff.contains("+Second publish"),
            "{diff}"
        );
        assert_eq!(fs::read(&state).unwrap(), before_preview);
        assert_eq!(
            fs::read(renamed.join(".confluence.json")).unwrap(),
            original_sidecar
        );
        assert!(!renamed.join("faq/.confluence.json").exists());
        assert_eq!(first_item(&remote(), "page")["version"], 2);

        let lock = fs::OpenOptions::new().write(true).open(lock_path).unwrap();
        lock.try_lock().unwrap();
        let concurrent = run(&["apply", second_arg, "--state-file", state_arg, "-o", "json"]);
        assert!(!concurrent.status.success(), "{concurrent:?}");
        assert!(String::from_utf8_lossy(&concurrent.stderr).contains("cannot lock publish state"));
        assert_eq!(first_item(&remote(), "page")["version"], 2);
        drop(lock);

        let second_apply = cfg.run_json(&["apply", second_arg, "--state-file", state_arg]);
        let items = list_items(&second_apply, "apply");
        assert_eq!(items.len(), 1, "{second_apply}");
        assert_eq!(items[0]["content_id"], page_id);
        assert_eq!(
            first_item(&remote(), "page")["body_storage"],
            "<p>Second publish</p>"
        );
        assert_eq!(first_item(&remote(), "page")["version"], 3);
        let faq = cfg.run_json(&["page", "get", faq_id, "--show-body"]);
        assert_eq!(first_item(&faq, "page")["parent_id"], page_id);
        assert_eq!(first_item(&faq, "page")["version"], 1);
        let deployed_state = fs::read(&state).unwrap();
        let stored: Value = serde_json::from_slice(&deployed_state).unwrap();
        assert_eq!(
            stored["pages"]["Publishing Guide/index.md"]["sidecar"]["content_id"],
            page_id
        );
        assert_eq!(
            stored["pages"]["Publishing Guide/faq/index.md"]["sidecar"]["content_id"],
            faq_id
        );
        assert_eq!(stored["pages"].as_object().unwrap().len(), 2);

        let sidecar = third_renamed.join(".confluence.json");
        let mut foreign: Value = serde_json::from_slice(&original_sidecar).unwrap();
        foreign["site_fingerprint"] = serde_json::json!("different-site");
        fs::write(&sidecar, serde_json::to_vec(&foreign).unwrap()).unwrap();
        let wrong_site = run(&["plan", third_arg, "--state-file", state_arg, "-o", "json"]);
        assert!(!wrong_site.status.success(), "{wrong_site:?}");
        assert!(String::from_utf8_lossy(&wrong_site.stderr).contains("different Confluence site"));
        foreign.as_object_mut().unwrap().remove("site_fingerprint");
        fs::write(&sidecar, serde_json::to_vec(&foreign).unwrap()).unwrap();
        let unknown_site = run(&["plan", third_arg, "--state-file", state_arg]);
        assert!(!unknown_site.status.success(), "{unknown_site:?}");
        assert!(
            String::from_utf8_lossy(&unknown_site.stderr)
                .contains("cannot verify the Confluence site")
        );
        fs::write(&sidecar, &original_sidecar).unwrap();

        cfg.run_json(&[
            "page",
            "update",
            &page_id,
            "--format",
            "storage",
            "--body",
            "<p>Independent edit</p>",
        ]);
        let offline = cfg.run_json(&["plan", third_arg, "--state-file", state_arg]);
        assert_eq!(list_items(&offline, "plan")[0]["action"], "update_content");
        for args in [
            vec![
                "plan",
                third_arg,
                "--check-remote",
                "--state-file",
                state_arg,
                "-o",
                "json",
            ],
            vec!["apply", third_arg, "--state-file", state_arg, "-o", "json"],
        ] {
            let rejected = run(&args);
            assert_eq!(rejected.status.code(), Some(7), "{args:?}: {rejected:?}");
            let error: Value = serde_json::from_slice(&rejected.stderr).unwrap();
            assert_eq!(error["error"]["kind"], "conflict");
            assert!(string_field(&error["error"], "message").contains("remote version drift"));
        }
        assert_eq!(fs::read(&state).unwrap(), deployed_state);
        assert_eq!(
            first_item(&remote(), "page")["body_storage"],
            "<p>Independent edit</p>"
        );
        assert_eq!(first_item(&remote(), "page")["version"], 4);

        let reconciled = temp.path().join("reconciled");
        let reconciled_arg = reconciled.to_str().unwrap();
        let replacement = temp.path().join("replacement.json");
        let replacement_arg = replacement.to_str().unwrap();
        cfg.run_json(&["pull", "tree", &page_id, reconciled_arg]);
        let recovered_faq = find_index_md_by_title(&reconciled, "FAQ");
        let root_index = find_index_md_by_title(&reconciled, "CI guide");
        fs::rename(
            recovered_faq.parent().unwrap(),
            root_index.with_file_name("faq"),
        )
        .unwrap();
        let markdown = fs::read_to_string(&root_index).unwrap();
        assert!(markdown.contains("Independent edit"), "{markdown}");
        fs::write(
            &root_index,
            markdown.replace(
                "Independent edit",
                "Independent edit and reconciled changes",
            ),
        )
        .unwrap();
        cfg.run_json(&["plan", reconciled_arg, "--check-remote"]);
        let applied = cfg.run_json(&[
            "apply",
            reconciled_arg,
            "--state-file",
            replacement_arg,
            "--init-state",
        ]);
        let items = list_items(&applied, "apply");
        assert_eq!(items.len(), 1, "{applied}");
        assert_eq!(items[0]["content_id"], page_id);
        assert_eq!(
            first_item(&remote(), "page")["body_storage"],
            "<p>Independent edit and reconciled changes</p>"
        );
        assert_eq!(fs::read(&state).unwrap(), deployed_state);
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_state_keeps_unrelated_paths_when_a_page_moves_to_the_checkout_root() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().unwrap();
    let cfg = simulated_cloud_config(&simulator, &config_home);
    let one = simulator.add_page("One", None, "<p>One body</p>");
    let two = simulator.add_page("Two", None, "<p>Two body</p>");

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let temp = TempDir::new().unwrap();
        let tree = temp.path().join("tree");
        let state = temp.path().join("state.json");
        let path = tree.to_str().unwrap();
        let state_arg = state.to_str().unwrap();
        let other = temp.path().join("other");
        cfg.run_json(&["pull", "tree", &one, path, "--layout", "slug"]);
        cfg.run_json(&[
            "pull",
            "tree",
            &two,
            other.to_str().unwrap(),
            "--layout",
            "slug",
        ]);
        fs::create_dir(tree.join("group")).unwrap();
        fs::rename(other.join("two"), tree.join("group/two")).unwrap();
        cfg.run_json(&["apply", path, "--state-file", state_arg, "--init-state"]);
        let before = fs::read(&state).unwrap();
        for name in ["index.md", ".confluence.json"] {
            fs::rename(tree.join("one").join(name), tree.join(name)).unwrap();
        }
        let preview = cfg.run_json(&["plan", path, "--state-file", state_arg, "--check-remote"]);
        assert!(
            list_items(&preview, "plan")
                .iter()
                .all(|item| item["action"] == "noop")
        );
        assert_eq!(fs::read(&state).unwrap(), before);
        cfg.run_json(&["apply", path, "--state-file", state_arg]);
        let after: Value = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
        assert_eq!(after["pages"]["index.md"]["sidecar"]["content_id"], one);
        assert_eq!(
            after["pages"]["group/two/index.md"]["sidecar"]["content_id"],
            two
        );
        assert_eq!(after["pages"].as_object().unwrap().len(), 2);
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unchanged_pulls_produce_stable_sidecars_and_read_legacy_timestamps() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().unwrap();
    let cfg = simulated_cloud_config(&simulator, &config_home);
    let page_id = simulator.add_page("Stable snapshot", None, "<p>Original body</p>");
    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let temp = TempDir::new().unwrap();
        let tree = temp.path().join("tree");
        let path = tree.to_str().unwrap();
        cfg.run_json(&["pull", "page", &page_id, path]);
        let sidecar = find_index_md(&tree).with_file_name(".confluence.json");
        let first = fs::read(&sidecar).unwrap();
        let mut legacy: Value = serde_json::from_slice(&first).unwrap();
        assert!(legacy.get("last_sync_at").is_none());
        legacy["last_sync_at"] = serde_json::json!("2025-01-01T00:00:00Z");
        fs::write(&sidecar, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let preview = cfg.run_json(&["plan", path]);
        assert_eq!(list_items(&preview, "plan")[0]["action"], "noop");
        cfg.run_json(&["pull", "page", &page_id, path, "--force"]);
        assert_eq!(fs::read(sidecar).unwrap(), first);
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_state_blocks_retry_when_create_succeeds_but_response_fails() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().unwrap();
    let cfg = simulated_cloud_config(&simulator, &config_home);
    simulator.fail_cloud_create_response_after(1);
    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let temp = TempDir::new().unwrap();
        let tree = temp.path().join("tree");
        let state = temp.path().join("deploy.json");
        let path = tree.to_str().unwrap();
        let state_arg = state.to_str().unwrap();
        let child = tree.join("child");
        fs::create_dir_all(&child).unwrap();
        fs::write(
            tree.join("index.md"),
            "---\ntitle: Confirmed create\ntype: page\nproperties:\n  space_key: TEST\n---\n\nBody\n",
        ).unwrap();
        fs::write(
            child.join("index.md"),
            "---\ntitle: Uncertain create\ntype: page\nproperties:\n  space_key: TEST\n---\n\nBody\n",
        ).unwrap();
        let publish = cfg.command()
            .args(["apply", path, "--state-file", state_arg, "--init-state", "-o", "json"])
            .output().unwrap();
        assert!(!publish.status.success(), "{publish:?}");
        assert!(!child.join(".confluence.json").exists());
        let baseline = fs::read(&state).unwrap();
        let pending: Value = serde_json::from_slice(&baseline).unwrap();
        let confirmed = cfg.run_json(&["search", "Confirmed create"]);
        let confirmed = list_items(&confirmed, "search").iter()
            .find(|item| item["title"] == "Confirmed create").unwrap();
        assert_eq!(pending["pages"]["index.md"]["sidecar"]["content_id"], confirmed["id"]);
        let child_key = Path::new("child").join("index.md");
        let uncertain = pending["pages"].get(child_key.to_str().unwrap()).expect("pending child");
        assert_eq!(uncertain["sidecar"].get("content_id"), Some(&Value::Null));
        let remote = cfg.run_json(&["search", "Uncertain create"]);
        let count = list_items(&remote, "search").iter()
            .filter(|page| page["title"] == "Uncertain create").count();
        assert_eq!(count, 1, "{remote}");
        let preview = || cfg.command().args(["plan", path, "--state-file", state_arg, "-o", "json"])
            .output().unwrap();
        let lock = fs::OpenOptions::new().write(true).open(temp.path().join("deploy.json.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        let active = preview();
        assert_eq!(active.status.code(), Some(7), "{active:?}");
        assert!(String::from_utf8_lossy(&active.stderr).contains("publish is in progress"));
        drop(lock);
        let unfinished = preview();
        assert_eq!(unfinished.status.code(), Some(7), "{unfinished:?}");
        assert!(String::from_utf8_lossy(&unfinished.stderr).contains("unfinished apply"));
        let retry = cfg.command().args(["apply", path, "--state-file", state_arg, "-o", "json"])
            .output().unwrap();
        assert_eq!(retry.status.code(), Some(7), "{retry:?}");
        assert!(String::from_utf8_lossy(&retry.stderr).contains("unfinished apply"), "{retry:?}");
        assert_eq!(fs::read(&state).unwrap(), baseline);
        assert_eq!(cfg.run_json(&["search", "Uncertain create"]), remote);
    }).await.unwrap();
}

fn simulated_cloud_config(simulator: &ConfluenceSimulator, config_home: &TempDir) -> E2eConfig {
    let config_home_path = config_home.path().to_string_lossy().into_owned();
    E2eConfig {
        bin: find_binary_path(),
        profile: None,
        envs: vec![
            ("CONFLUENCE_DOMAIN".to_string(), simulator.base_url()),
            ("CONFLUENCE_PROVIDER".to_string(), "cloud".to_string()),
            ("CONFLUENCE_AUTH_TYPE".to_string(), "basic".to_string()),
            (
                "CONFLUENCE_API_PATH".to_string(),
                "/wiki/rest/api".to_string(),
            ),
            ("CONFLUENCE_TOKEN_KIND".to_string(), "classic".to_string()),
            (
                "CONFLUENCE_USERNAME".to_string(),
                "simulator@example.test".to_string(),
            ),
            (
                "CONFLUENCE_API_TOKEN".to_string(),
                "simulated-token".to_string(),
            ),
            ("HOME".to_string(), config_home_path.clone()),
            ("XDG_CONFIG_HOME".to_string(), config_home_path),
        ],
        space: "TEST".to_string(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn environment_driven_e2e_does_not_require_a_saved_profile() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().expect("empty e2e config home");
    let config_home_path = config_home.path().to_string_lossy().into_owned();
    let mut cfg = environment_e2e_config(
        find_binary_path(),
        "TEST".to_string(),
        EnvironmentE2eSettings {
            base_url: simulator.base_url(),
            token: "simulated-token".to_string(),
            auth_type: "basic".to_string(),
            provider: "cloud".to_string(),
            api_path: "/wiki/rest/api".to_string(),
            token_kind: Some("classic".to_string()),
            cloud_id: None,
            username: Some("simulator@example.test".to_string()),
        },
    );
    cfg.envs.extend([
        ("HOME".to_string(), config_home_path.clone()),
        ("XDG_CONFIG_HOME".to_string(), config_home_path),
    ]);

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        assert!(
            cfg.envs.iter().all(|(key, _)| key != "CONFLUENCE_PROFILE"),
            "environment-driven E2E must not select a saved profile"
        );
        let status = cfg.run_json(&["auth", "status"]);
        assert_eq!(
            status.pointer("/current_user/account_id"),
            Some(&Value::from("simulator-account-id"))
        );
    })
    .await
    .expect("environment-driven auth task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simulated_cloud_supports_the_complete_cli_lifecycle() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().expect("simulator config home");
    let cfg = simulated_cloud_config(&simulator, &config_home);

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        run_cli_lifecycle(cfg);
    })
    .await
    .expect("complete simulated lifecycle task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simulated_cloud_rejects_invalid_credentials_through_the_cli() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().expect("simulator config home");
    let mut cfg = simulated_cloud_config(&simulator, &config_home);
    let token = cfg
        .envs
        .iter_mut()
        .find(|(key, _)| key == "CONFLUENCE_API_TOKEN")
        .expect("simulated API token");
    token.1 = "invalid-token".to_string();

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        let output = cfg
            .command()
            .args(["--json", "auth", "status"])
            .output()
            .expect("auth status command to run");
        assert!(
            !output.status.success(),
            "invalid credentials were accepted"
        );
        let error: Value = serde_json::from_slice(&output.stderr)
            .unwrap_or_else(|err| panic!("invalid JSON error: {err}"));
        assert_eq!(error.pointer("/error/kind"), Some(&Value::from("auth")));
    })
    .await
    .expect("invalid-credentials task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simulated_data_center_supports_the_complete_cli_lifecycle() {
    let simulator = ConfluenceSimulator::start().await;
    let config_home = TempDir::new().expect("simulator config home");
    let cfg = simulated_data_center_config(&simulator, &config_home);

    tokio::task::spawn_blocking(move || {
        let _config_home = config_home;
        run_cli_lifecycle(cfg);
    })
    .await
    .expect("complete simulated lifecycle task");
}

fn simulated_data_center_config(
    simulator: &ConfluenceSimulator,
    config_home: &TempDir,
) -> E2eConfig {
    let config_home_path = config_home.path().to_string_lossy().into_owned();
    E2eConfig {
        bin: find_binary_path(),
        profile: None,
        envs: vec![
            ("CONFLUENCE_DOMAIN".to_string(), simulator.base_url()),
            ("CONFLUENCE_PROVIDER".to_string(), "data-center".to_string()),
            ("CONFLUENCE_AUTH_TYPE".to_string(), "bearer".to_string()),
            ("CONFLUENCE_API_PATH".to_string(), "/rest/api".to_string()),
            (
                "CONFLUENCE_BEARER_TOKEN".to_string(),
                "simulated-token".to_string(),
            ),
            ("HOME".to_string(), config_home_path.clone()),
            ("XDG_CONFIG_HOME".to_string(), config_home_path),
        ],
        space: "TEST".to_string(),
    }
}

fn first_item<'a>(value: &'a Value, context: &str) -> &'a Value {
    value
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or_else(|| panic!("expected non-empty array for {context}: {value}"))
}

fn list_items<'a>(value: &'a Value, context: &str) -> &'a [Value] {
    value
        .get("items")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_else(|| panic!("expected list envelope for {context}: {value}"))
}

fn string_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("expected string field `{key}` in {value}"))
}

fn u64_field(value: &Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("expected u64 field `{key}` in {value}"))
}

fn unique_name(prefix: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis();
    format!("{prefix} {millis}")
}

fn find_index_md(root: &PathBuf) -> PathBuf {
    WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .find(|entry| entry.file_type().is_file() && entry.file_name() == "index.md")
        .map(|entry| entry.into_path())
        .unwrap_or_else(|| panic!("failed to find index.md under {}", root.display()))
}

fn find_index_md_by_title(root: &PathBuf, title: &str) -> PathBuf {
    WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .find_map(|entry| {
            if !entry.file_type().is_file() || entry.file_name() != "index.md" {
                return None;
            }
            let contents = fs::read_to_string(entry.path()).ok()?;
            contents
                .contains(&format!("title: {title}\n"))
                .then(|| entry.into_path())
        })
        .unwrap_or_else(|| {
            panic!(
                "failed to find index.md for title `{title}` under {}",
                root.display()
            )
        })
}

#[test]
#[ignore = "requires a real Confluence instance"]
fn e2e_cli_canary() {
    let Some(cfg) = e2e_config() else {
        eprintln!(
            "Skipping e2e_cli_canary: set CONFLUENCE_E2E_PROFILE or CONFLUENCE_E2E_BASE_URL / CONFLUENCE_E2E_TOKEN"
        );
        return;
    };

    run_cli_canary(cfg);
}

fn run_cli_canary(cfg: E2eConfig) {
    let temp = TempDir::new().expect("canary tempdir");
    let pull_dir = temp.path().join("pull");
    let title = unique_name("Confluence CLI Canary");
    let mut cleanup = Cleanup::new(cfg.clone());

    let auth = cfg.run_json(&["auth", "status"]);
    assert!(
        auth.get("current_user").is_some_and(|user| !user.is_null()),
        "canary authentication did not resolve a current user: {auth}"
    );

    let spaces = cfg.run_json(&["space", "list"]);
    assert!(
        list_items(&spaces, "canary space list")
            .iter()
            .any(|space| space.get("key").and_then(Value::as_str) == Some(cfg.space.as_str())),
        "canary space is unavailable: {spaces}"
    );

    let created = cfg.run_json(&[
        "page",
        "create",
        &title,
        cfg.space.as_str(),
        "--body",
        "Confluence CLI automated canary.",
    ]);
    let page_id = string_field(first_item(&created, "canary page create"), "id").to_string();
    cleanup.page_id = Some(page_id.clone());

    let fetched = cfg.run_json(&["page", "get", &page_id, "--show-body"]);
    assert_eq!(
        string_field(first_item(&fetched, "canary page get"), "id"),
        page_id
    );

    let pull_arg = pull_dir.to_string_lossy().into_owned();
    cfg.run(&["pull", "page", &page_id, &pull_arg]);
    let index_md = find_index_md(&pull_dir);
    let markdown = fs::read_to_string(&index_md).expect("read canary markdown");
    fs::write(
        &index_md,
        format!("{markdown}\nUpdated by the automated canary.\n"),
    )
    .expect("update canary markdown");

    let plan = cfg.run_json(&["plan", &pull_arg]);
    assert!(
        list_items(&plan, "canary plan")
            .iter()
            .any(|item| { item.get("action").and_then(Value::as_str) == Some("update_content") }),
        "canary plan did not detect the local update: {plan}"
    );
    cfg.run_json(&["apply", &pull_arg]);

    let clean_plan = cfg.run_json(&["plan", &pull_arg]);
    assert!(
        list_items(&clean_plan, "clean canary plan")
            .iter()
            .all(|item| item.get("action").and_then(Value::as_str) == Some("noop")),
        "canary did not converge after apply: {clean_plan}"
    );

    cfg.run(&["--yes", "page", "delete", &page_id]);
    cleanup.page_id = None;
}

#[test]
#[ignore = "requires a real Confluence instance"]
fn e2e_cli_lifecycle() {
    let Some(cfg) = e2e_config() else {
        eprintln!(
            "Skipping e2e_cli_lifecycle: set CONFLUENCE_E2E_PROFILE or CONFLUENCE_E2E_BASE_URL / CONFLUENCE_E2E_TOKEN"
        );
        return;
    };

    run_cli_lifecycle(cfg);
}

fn run_cli_lifecycle(cfg: E2eConfig) {
    let temp = TempDir::new().expect("tempdir");
    let page_body_1 = temp.path().join("page-body-1.md");
    let page_body_2 = temp.path().join("page-body-2.md");
    let blog_body_1 = temp.path().join("blog-body-1.md");
    let blog_body_2 = temp.path().join("blog-body-2.md");
    let comment_body = temp.path().join("comment.md");
    let attachment_file = temp.path().join("attachment.txt");
    let download_path = temp.path().join("downloaded.txt");
    let pull_dir = temp.path().join("pull");
    let untouched_pull_dir = temp.path().join("untouched-pull");
    let fresh_links_dir = temp.path().join("fresh-links");
    let macro_tree_dir = temp.path().join("macro-tree");
    let macro_pull_dir = temp.path().join("macro-pull");

    fs::write(
        &page_body_1,
        "# E2E Scratch\n\nInitial page body from the e2e lifecycle.\n",
    )
    .expect("write page body 1");
    fs::write(
        &page_body_2,
        "# E2E Scratch\n\nUpdated page body from the e2e lifecycle.\n",
    )
    .expect("write page body 2");
    fs::write(
        &blog_body_1,
        "# E2E Blog\n\nInitial blog body from the e2e lifecycle.\n",
    )
    .expect("write blog body 1");
    fs::write(
        &blog_body_2,
        "# E2E Blog\n\nUpdated blog body from the e2e lifecycle.\n",
    )
    .expect("write blog body 2");
    fs::write(&comment_body, "CLI verification comment\n").expect("write comment body");
    fs::write(&attachment_file, "attachment payload from rust e2e\n")
        .expect("write attachment file");

    let page_title = unique_name("E2E Page");
    let updated_page_title = format!("{page_title} Updated");
    let blog_title = unique_name("E2E Blog");
    let updated_blog_title = format!("{blog_title} Updated");

    let mut cleanup = Cleanup::new(cfg.clone());

    let space_list = cfg.run_json(&["space", "list"]);
    assert!(
        list_items(&space_list, "space list")
            .iter()
            .any(|item| string_field(item, "key") == cfg.space),
        "expected space list to include {}: {space_list}",
        cfg.space
    );

    // Search indexes are eventually consistent for freshly created Cloud
    // content. Exercise search against the space's established homepage so the
    // lifecycle remains deterministic on both Cloud and Data Center.
    let space_cql = format!("space = \"{}\"", cfg.space.replace('"', "\\\""));
    let search = cfg.run_json(&["search", &space_cql, "--cql", "--limit", "20"]);
    assert!(
        list_items(&search, "search")
            .iter()
            .any(|item| item.get("space_key").and_then(Value::as_str) == Some(cfg.space.as_str())),
        "expected search to find established content in {}: {search}",
        cfg.space
    );

    let page_body_1_arg = page_body_1.to_string_lossy().into_owned();
    let page_create = cfg.run_json(&[
        "page",
        "create",
        &page_title,
        cfg.space.as_str(),
        "--body-file",
        &page_body_1_arg,
    ]);
    let page_id = string_field(first_item(&page_create, "page create"), "id").to_string();
    cleanup.page_id = Some(page_id.clone());

    let page_get = cfg.run_json(&["page", "get", &page_id, "--show-body"]);
    assert_eq!(
        string_field(first_item(&page_get, "page get"), "id"),
        page_id
    );

    let page_body_2_arg = page_body_2.to_string_lossy().into_owned();
    let page_update = cfg.run_json(&[
        "page",
        "update",
        &page_id,
        "--title",
        &updated_page_title,
        "--body-file",
        &page_body_2_arg,
    ]);
    assert_eq!(
        string_field(first_item(&page_update, "page update"), "title"),
        updated_page_title
    );

    cfg.run(&["label", "add", &page_id, "e2e-auto"]);
    let labels = cfg.run_json(&["label", "list", &page_id]);
    assert!(
        list_items(&labels, "label list")
            .iter()
            .any(|item| item.get("label").and_then(Value::as_str) == Some("e2e-auto")),
        "expected label list to contain e2e-auto: {labels}"
    );
    cfg.run(&["--yes", "label", "remove", &page_id, "e2e-auto"]);

    let property_set = cfg.run_json(&[
        "property",
        "set",
        &page_id,
        "e2e_verify",
        r#"{"ok":true,"n":1}"#,
    ]);
    assert_eq!(
        string_field(first_item(&property_set, "property set"), "key"),
        "e2e_verify"
    );
    let property_get = cfg.run_json(&["property", "get", &page_id, "e2e_verify"]);
    assert_eq!(
        first_item(&property_get, "property get")
            .get("value")
            .and_then(|value| value.get("ok"))
            .and_then(Value::as_bool),
        Some(true)
    );
    cfg.run(&["--yes", "property", "delete", &page_id, "e2e_verify"]);

    let comment_body_arg = comment_body.to_string_lossy().into_owned();
    let comment_add = cfg.run_json(&["comment", "add", &page_id, "--body-file", &comment_body_arg]);
    let comment_id = string_field(first_item(&comment_add, "comment add"), "id").to_string();
    cleanup.comment_id = Some(comment_id.clone());
    let comments = cfg.run_json(&["comment", "list", &page_id]);
    assert!(
        list_items(&comments, "comment list")
            .iter()
            .any(|item| item.get("id").and_then(Value::as_str) == Some(comment_id.as_str())),
        "expected comment list to contain {comment_id}: {comments}"
    );
    cfg.run(&["--yes", "comment", "delete", &comment_id]);
    cleanup.comment_id = None;

    let attachment_arg = attachment_file.to_string_lossy().into_owned();
    let attachment_upload = cfg.run_json(&[
        "attachment",
        "upload",
        &page_id,
        "--file",
        &attachment_arg,
        "--comment",
        "e2e upload",
    ]);
    let attachment_id =
        string_field(first_item(&attachment_upload, "attachment upload"), "id").to_string();
    cleanup.attachment_id = Some(attachment_id.clone());
    let attachments = cfg.run_json(&["attachment", "list", &page_id]);
    assert!(
        list_items(&attachments, "attachment list")
            .iter()
            .any(|item| item.get("id").and_then(Value::as_str) == Some(attachment_id.as_str())),
        "expected attachment list to contain {attachment_id}: {attachments}"
    );
    let download_arg = download_path.to_string_lossy().into_owned();
    cfg.run(&[
        "attachment",
        "download",
        &page_id,
        &attachment_id,
        &download_arg,
    ]);
    assert_eq!(
        fs::read_to_string(&attachment_file).expect("read uploaded attachment"),
        fs::read_to_string(&download_path).expect("read downloaded attachment")
    );
    cfg.run(&["--yes", "attachment", "delete", &page_id, &attachment_id]);
    cleanup.attachment_id = None;

    let page_before_untouched_pull = cfg.run_json(&["page", "get", &page_id, "--show-body"]);
    let untouched_version = u64_field(
        first_item(&page_before_untouched_pull, "page get"),
        "version",
    );

    let untouched_pull_arg = untouched_pull_dir.to_string_lossy().into_owned();
    cfg.run(&["pull", "page", &page_id, &untouched_pull_arg]);
    let untouched_plan_before = cfg.run_json(&["plan", &untouched_pull_arg]);
    assert!(
        untouched_plan_before
            .get("items")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                !items.is_empty()
                    && items
                        .iter()
                        .all(|item| item.get("action").and_then(Value::as_str) == Some("noop"))
            }),
        "expected untouched pull plan to be noop: {untouched_plan_before}"
    );
    let untouched_apply = cfg.run_json(&["apply", &untouched_pull_arg]);
    assert!(
        untouched_apply
            .get("items")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                !items.is_empty()
                    && items
                        .iter()
                        .all(|item| item.get("action").and_then(Value::as_str) == Some("noop"))
            }),
        "expected untouched apply to be noop: {untouched_apply}"
    );
    let page_after_untouched_apply = cfg.run_json(&["page", "get", &page_id, "--show-body"]);
    let untouched_version_after = u64_field(
        first_item(&page_after_untouched_apply, "page get"),
        "version",
    );
    assert_eq!(
        untouched_version_after, untouched_version,
        "expected untouched apply to preserve remote version"
    );

    let pull_arg = pull_dir.to_string_lossy().into_owned();
    cfg.run(&["pull", "tree", &page_id, &pull_arg, "--layout", "slug"]);
    let original_index = find_index_md(&pull_dir);
    let renamed_page = pull_dir.join("manually-renamed-page");
    fs::rename(original_index.parent().unwrap(), &renamed_page).expect("rename pulled page");
    let index_md = renamed_page.join("index.md");
    let original_markdown = fs::read_to_string(&index_md).expect("read pulled markdown");
    let updated_markdown = format!("{original_markdown}\nApplied from Rust e2e.\n");
    fs::write(&index_md, updated_markdown).expect("update pulled markdown");

    let plan_before = cfg.run_json(&["plan", &pull_arg]);
    assert!(
        plan_before
            .get("items")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("action").and_then(Value::as_str) == Some("update_content")
                })
            }),
        "expected plan to include update_content: {plan_before}"
    );
    let _apply = cfg.run_json(&["apply", &pull_arg]);
    let remote = cfg.run_json(&["page", "get", &page_id, "--show-body"]);
    let remote = first_item(&remote, "page get");
    assert_eq!(remote["id"], page_id);
    assert!(
        remote["body_storage"]
            .as_str()
            .is_some_and(|body| body.contains("Applied from Rust e2e.")),
        "expected apply on renamed folder to update original page: {remote}"
    );
    let plan_after = cfg.run_json(&["plan", &pull_arg]);
    assert!(
        plan_after
            .get("items")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                !items.is_empty()
                    && items
                        .iter()
                        .all(|item| item.get("action").and_then(Value::as_str) == Some("noop"))
            }),
        "expected post-apply plan to be noop: {plan_after}"
    );

    let fresh_source_title = unique_name("E2E Fresh Source");
    let fresh_target_title = unique_name("E2E Fresh Target");
    let macro_root_title = unique_name("E2E Macro Root");
    let macro_source_title = unique_name("E2E Macro Source");
    let macro_target_title = unique_name("E2E Macro Target");
    let macro_child_title = unique_name("E2E Macro Child");
    let fresh_source_dir = fresh_links_dir.join("source");
    let fresh_target_dir = fresh_links_dir.join("target");
    fs::create_dir_all(&fresh_source_dir).expect("create fresh source dir");
    fs::create_dir_all(&fresh_target_dir).expect("create fresh target dir");
    fs::write(
        fresh_source_dir.join("index.md"),
        format!(
            "---\ntitle: {fresh_source_title}\ntype: page\nlabels: []\nstatus: current\nparent: null\nproperties: {{}}\n---\n\n[Go to target](../target/index.md#intro)\n"
        ),
    )
    .expect("write fresh source markdown");
    fs::write(
        fresh_target_dir.join("index.md"),
        format!(
            "---\ntitle: {fresh_target_title}\ntype: page\nlabels: []\nstatus: current\nparent: null\nproperties: {{}}\n---\n\n# Intro\n\nFresh target page.\n"
        ),
    )
    .expect("write fresh target markdown");
    fs::write(
        fresh_source_dir.join(".confluence.json"),
        format!("{{\n  \"space_key\": \"{}\"\n}}\n", cfg.space),
    )
    .expect("write fresh source sidecar");
    fs::write(
        fresh_target_dir.join(".confluence.json"),
        format!("{{\n  \"space_key\": \"{}\"\n}}\n", cfg.space),
    )
    .expect("write fresh target sidecar");

    let fresh_links_arg = fresh_links_dir.to_string_lossy().into_owned();
    let fresh_apply = cfg.run_json(&["apply", &fresh_links_arg]);
    let fresh_items = fresh_apply
        .get("items")
        .and_then(Value::as_array)
        .expect("fresh apply items");
    let fresh_source_id = fresh_items
        .iter()
        .find(|item| {
            item.get("action").and_then(Value::as_str) == Some("create_content")
                && item.get("title").and_then(Value::as_str) == Some(fresh_source_title.as_str())
        })
        .and_then(|item| item.get("content_id"))
        .and_then(Value::as_str)
        .expect("fresh source content id")
        .to_string();
    let fresh_target_id = fresh_items
        .iter()
        .find(|item| {
            item.get("action").and_then(Value::as_str) == Some("create_content")
                && item.get("title").and_then(Value::as_str) == Some(fresh_target_title.as_str())
        })
        .and_then(|item| item.get("content_id"))
        .and_then(Value::as_str)
        .expect("fresh target content id")
        .to_string();
    cleanup.extra_page_ids.push(fresh_source_id.clone());
    cleanup.extra_page_ids.push(fresh_target_id.clone());

    let fresh_source_get = cfg.run_json(&["page", "get", &fresh_source_id, "--show-body"]);
    let fresh_source_body = string_field(
        first_item(&fresh_source_get, "fresh source get"),
        "body_storage",
    );
    assert!(
        fresh_source_body.contains(&format!("pageId={fresh_target_id}#intro")),
        "expected fresh source body to link to target id {fresh_target_id}: {fresh_source_body}"
    );

    let fresh_plan = cfg.run_json(&["plan", &fresh_links_arg]);
    assert!(
        fresh_plan
            .get("items")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                !items.is_empty()
                    && items
                        .iter()
                        .all(|item| item.get("action").and_then(Value::as_str) == Some("noop"))
            }),
        "expected fresh-links plan to be noop: {fresh_plan}"
    );

    let macro_root_dir = macro_tree_dir.join("macro-root");
    let macro_source_dir = macro_root_dir.join("source");
    let macro_target_dir = macro_root_dir.join("target");
    let macro_child_dir = macro_source_dir.join("child");
    let macro_source_attachments_dir = macro_source_dir.join("attachments");
    let macro_target_attachments_dir = macro_target_dir.join("attachments");
    let macro_user = current_user_placeholder(&cfg);
    let macro_user_fragment = expected_user_resource_fragment(&macro_user);
    let macro_user_placeholder_fragment = expected_user_placeholder_fragment(&macro_user);
    fs::create_dir_all(&macro_root_dir).expect("create macro root dir");
    fs::create_dir_all(&macro_source_dir).expect("create macro source dir");
    fs::create_dir_all(&macro_target_dir).expect("create macro target dir");
    fs::create_dir_all(&macro_child_dir).expect("create macro child dir");
    fs::create_dir_all(&macro_source_attachments_dir).expect("create macro source attachments dir");
    fs::create_dir_all(&macro_target_attachments_dir).expect("create macro target attachments dir");
    fs::write(
        macro_source_attachments_dir.join("preview.pdf"),
        "preview pdf payload\n",
    )
    .expect("write macro source preview attachment");
    fs::write(
        macro_source_attachments_dir.join("sheet.xlsx"),
        "spreadsheet payload\n",
    )
    .expect("write macro source xls attachment");
    fs::write(
        macro_source_attachments_dir.join("slides.pptx"),
        "slides payload\n",
    )
    .expect("write macro source ppt attachment");
    fs::write(
        macro_target_attachments_dir.join("manual.docx"),
        "manual payload\n",
    )
    .expect("write macro target doc attachment");
    fs::write(
        macro_root_dir.join("index.md"),
        format!(
            "---\ntitle: {macro_root_title}\ntype: page\nlabels: []\nstatus: current\nparent: null\nproperties: {{}}\n---\n\n# Macro Root\n"
        ),
    )
    .expect("write macro root markdown");
    fs::write(
        macro_source_dir.join("index.md"),
        format!(
            "---\ntitle: {macro_source_title}\ntype: page\nlabels: []\nstatus: current\nparent: null\nproperties: {{}}\n---\n\n# Macro Source\n\n:::confluence-anchor\nname: macro-source-anchor\n:::\n\n:::confluence-navmap\n$default: Macro Root,Macro Source,Shared Excerpt\ntitle: Macro navigation\nwrapAfter: 4\n:::\n\n:::confluence-excerpt-include\nnopanel: true\npage: ../target/index.md\n:::\n\n:::confluence-include-page\npage: ../target/index.md\n:::\n\n:::confluence-page-tree\nroot: index.md\nsearchBox: true\n:::\n\n:::confluence-page-tree-search\nroot: ../target/index.md\nspaceKey: {space}\n:::\n\n:::confluence-content-by-label\ncql: label = \"e2e-macro-target\"\nmaxResults: 5\n:::\n\n:::confluence-content-by-user\nuser: {macro_user}\n:::\n\n:::confluence-content-report-table\nlabels: e2e-macro-target\nspaces: {space}\nmaxResults: 5\n:::\n\n:::confluence-search\nspacekey: !space {space}\ncontributor: !user {macro_user}\nquery: Macro Source\n:::\n\n:::confluence-task-report\nspaceAndPage: {space}\nlabels: e2e-macro-target\nstatus: incomplete\npageSize: 20\ncolumns: description,assignee,location\nsortBy: page title\nreverseSort: false\n:::\n\n:::confluence-macro userlister\ncontributor: !user {macro_user}\nrelated: !page-link ../target/index.md\n:::\n\n:::confluence-content-properties-report\nlabel: e2e-content-properties\nid: decision\n:::\n\n:::confluence-attachments\npatterns: *.pdf\nsortBy: name\n:::\n\n:::confluence-view-file\nattachment: preview.pdf\n:::\n\n:::confluence-view-doc\npage: ../target/index.md\nattachment: manual.docx\n:::\n\n:::confluence-view-xls\nattachment: sheet.xlsx\n:::\n\n:::confluence-view-ppt\nattachment: slides.pptx\n:::\n\n:::confluence-blog-posts\nauthor: {macro_user}\nspaces: {space}\nmax: 5\ntime: 7\n:::\n\n:::confluence-contributors\nspaces: {space},@personal\nlabels: e2e-macro-target\nmode: list\n:::\n\n:::confluence-contributors-summary\nspaces: {space}\ncolumns: edits,comments,labels\nlimit: 10\n:::\n\n:::confluence-recently-updated\nauthor: {macro_user}\nspaces: {space}\nmax: 10\n:::\n\n:::confluence-recently-updated-dashboard\nspaces: {space}\nlimit: 10\ntheme: concise\n:::\n\n:::confluence-livesearch\nspaceKey: {space}\nlabels: e2e-macro-target\nsize: large\n:::\n\n:::confluence-page-index\n:::\n\n:::confluence-toc-zone\nlocation: top\nmaxLevel: 3\n---\n## Zoned Heading\n\nOnly this section counts.\n:::\n\n:::confluence-labels-list\nspaceKey: {space}\nexcludedLabels: drafts,test\n:::\n\n:::confluence-popular-labels\nspaceKey: {space}\ncount: 25\nstyle: heatmap\n:::\n\n:::confluence-related-labels\nlabels: e2e-macro-target\n:::\n\n:::confluence-recently-used-labels\nscope: space\nstyle: cloud\n:::\n\n:::confluence-gallery\nsortBy: name\ncolumns: 2\n:::\n\n:::confluence-favorite-pages\n:::\n\n:::confluence-change-history\n:::\n\n:::confluence-spaces-list\nscope: all\nwidth: 80%\n:::\n\n:::confluence-space-details\nwidth: 50%\n:::\n\n:::confluence-space-attachments\nspace: {space}\nshowFilter: false\n:::\n\n~~~confluence-noformat\nnopanel: true\n---\n<xml>literal</xml>\nline 2\n~~~\n\n:::confluence-profile\nuser: {macro_user}\n:::\n\n:::confluence-status-list\nusername: {macro_user}\n:::\n\n:::confluence-network\nmode: followers\nusername: {macro_user}\nmax: 10\ntheme: full\n:::\n\n:::confluence-children\npage: ../index.md\nall: true\nsort: creation\n:::\n",
            space = cfg.space,
            macro_user = macro_user
        ),
    )
    .expect("write macro source markdown");
    fs::write(
        macro_target_dir.join("index.md"),
        format!(
            "---\ntitle: {macro_target_title}\ntype: page\nlabels:\n  - e2e-macro-target\n  - e2e-content-properties\nstatus: current\nparent: null\nproperties: {{}}\n---\n\n# Shared Excerpt\n\nPulled and reapplied through the e2e macro test.\n\n:::confluence-content-properties\nid: decision\n---\n| Field | Value |\n| --- | --- |\n| Owner | Ada |\n| Status | Approved |\n:::\n"
        ),
    )
    .expect("write macro target markdown");
    fs::write(
        macro_child_dir.join("index.md"),
        format!(
            "---\ntitle: {macro_child_title}\ntype: page\nlabels: []\nstatus: current\nparent: null\nproperties: {{}}\n---\n\n# Nested Child\n\nUsed to back the children macro.\n"
        ),
    )
    .expect("write macro child markdown");
    fs::write(
        macro_root_dir.join(".confluence.json"),
        format!("{{\n  \"space_key\": \"{}\"\n}}\n", cfg.space),
    )
    .expect("write macro root sidecar");
    fs::write(
        macro_source_dir.join(".confluence.json"),
        format!("{{\n  \"space_key\": \"{}\"\n}}\n", cfg.space),
    )
    .expect("write macro source sidecar");
    fs::write(
        macro_target_dir.join(".confluence.json"),
        format!("{{\n  \"space_key\": \"{}\"\n}}\n", cfg.space),
    )
    .expect("write macro target sidecar");
    fs::write(
        macro_child_dir.join(".confluence.json"),
        format!("{{\n  \"space_key\": \"{}\"\n}}\n", cfg.space),
    )
    .expect("write macro child sidecar");

    let macro_tree_arg = macro_tree_dir.to_string_lossy().into_owned();
    let macro_apply = cfg.run_json(&["apply", &macro_tree_arg]);
    let macro_items = macro_apply
        .get("items")
        .and_then(Value::as_array)
        .expect("macro apply items");
    let macro_root_id = macro_items
        .iter()
        .find(|item| {
            item.get("action").and_then(Value::as_str) == Some("create_content")
                && item.get("title").and_then(Value::as_str) == Some(macro_root_title.as_str())
        })
        .and_then(|item| item.get("content_id"))
        .and_then(Value::as_str)
        .expect("macro root content id")
        .to_string();
    let macro_source_id = macro_items
        .iter()
        .find(|item| {
            item.get("action").and_then(Value::as_str) == Some("create_content")
                && item.get("title").and_then(Value::as_str) == Some(macro_source_title.as_str())
        })
        .and_then(|item| item.get("content_id"))
        .and_then(Value::as_str)
        .expect("macro source content id")
        .to_string();
    let macro_target_id = macro_items
        .iter()
        .find(|item| {
            item.get("action").and_then(Value::as_str) == Some("create_content")
                && item.get("title").and_then(Value::as_str) == Some(macro_target_title.as_str())
        })
        .and_then(|item| item.get("content_id"))
        .and_then(Value::as_str)
        .expect("macro target content id")
        .to_string();
    let macro_child_id = macro_items
        .iter()
        .find(|item| {
            item.get("action").and_then(Value::as_str) == Some("create_content")
                && item.get("title").and_then(Value::as_str) == Some(macro_child_title.as_str())
        })
        .and_then(|item| item.get("content_id"))
        .and_then(Value::as_str)
        .expect("macro child content id")
        .to_string();
    cleanup.extra_page_ids.push(macro_root_id.clone());
    cleanup.extra_page_ids.push(macro_target_id.clone());
    cleanup.extra_page_ids.push(macro_source_id.clone());
    cleanup.extra_page_ids.push(macro_child_id.clone());

    let macro_source_get = cfg.run_json(&["page", "get", &macro_source_id, "--show-body"]);
    let macro_source_body = string_field(
        first_item(&macro_source_get, "macro source get"),
        "body_storage",
    );
    assert!(
        macro_source_body.contains(r#"ac:name="anchor""#),
        "expected anchor macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body
            .contains(r#"<ac:parameter ac:name="">macro-source-anchor</ac:parameter>"#),
        "expected anchor name parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="navmap""#),
        "expected navmap macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(
            r#"<ac:parameter ac:name="">Macro Root,Macro Source,Shared Excerpt</ac:parameter>"#
        ),
        "expected navmap default parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body
            .contains(r#"<ac:parameter ac:name="title">Macro navigation</ac:parameter>"#)
            && macro_source_body.contains(r#"<ac:parameter ac:name="wrapAfter">4</ac:parameter>"#),
        "expected navmap parameters to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="excerpt-include""#),
        "expected excerpt-include macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(&format!(
            r#"<ac:parameter ac:name="default-parameter">{}:{}</ac:parameter>"#,
            cfg.space, macro_target_title
        )),
        "expected excerpt-include page reference to target title {macro_target_title}: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="children""#),
        "expected children macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"<ac:parameter ac:name="page"><ac:link><ri:page "#)
            && macro_source_body.contains(&format!(r#"ri:content-title="{}""#, macro_root_title))
            && macro_source_body.contains(&format!(r#"ri:space-key="{}""#, cfg.space)),
        "expected children page parameter to reference root title {macro_root_title}: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="include""#),
        "expected include-page macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(&format!(r#"ri:content-title="{macro_target_title}""#)),
        "expected include-page macro to reference target title {macro_target_title}: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(&format!(r#"ri:space-key="{}""#, cfg.space)),
        "expected include-page macro to reference space {}: {macro_source_body}",
        cfg.space
    );
    assert!(
        macro_source_body.contains(r#"ac:name="pagetree""#),
        "expected page-tree macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"<ac:parameter ac:name="root"><ac:link><ri:page "#)
            && macro_source_body.contains(&format!(r#"ri:content-title="{}""#, macro_source_title))
            && macro_source_body.contains(&format!(r#"ri:space-key="{}""#, cfg.space)),
        "expected page-tree root to reference source title {macro_source_title}: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="pagetreesearch""#),
        "expected page-tree-search macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(&format!(
            r#"<ac:parameter ac:name="root">{}:{}</ac:parameter>"#,
            cfg.space, macro_target_title
        )),
        "expected page-tree-search root to reference target title {macro_target_title}: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(&format!(
            r#"<ac:parameter ac:name="spaceKey">{}</ac:parameter>"#,
            cfg.space
        )) || macro_source_body.contains(&format!(
            r#"<ac:parameter ac:name="spaceKey"><ri:space ri:space-key="{}" /></ac:parameter>"#,
            cfg.space
        )),
        "expected page-tree-search spaceKey parameter to survive storage rendering as either a plain string or a space resource: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="contentbylabel""#),
        "expected content-by-label macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(
            r#"<ac:parameter ac:name="cql">label = &quot;e2e-macro-target&quot;</ac:parameter>"#
        ),
        "expected content-by-label cql to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="content-by-user""#),
        "expected content-by-user macro in source body: {macro_source_body}"
    );
    assert!(
        (macro_source_body.contains(r#"<ac:parameter ac:name=""><ri:user "#)
            && macro_source_body.contains(&macro_user_fragment))
            || (macro_source_body.contains(r#"<ac:parameter ac:name="">"#)
                && macro_source_body.contains("UserResourceIdentifier@")),
        "expected content-by-user user parameter to survive storage rendering as either a user resource or a provider-specific user identifier string: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="content-report-table""#),
        "expected content-report-table macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body
            .contains(r#"<ac:parameter ac:name="labels">e2e-macro-target</ac:parameter>"#)
            && macro_source_body.contains(&format!(
                r#"<ac:parameter ac:name="spaces"><ri:space ri:space-key="{}" /></ac:parameter>"#,
                cfg.space
            ))
            && macro_source_body.contains(r#"<ac:parameter ac:name="maxResults">5</ac:parameter>"#),
        "expected content-report-table parameters to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="search""#),
        "expected search macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(&format!(
            r#"<ac:parameter ac:name="spacekey"><ri:space ri:space-key="{}" /></ac:parameter>"#,
            cfg.space
        )),
        "expected search spacekey resource to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"<ac:parameter ac:name="contributor"><ri:user "#)
            && macro_source_body.contains(&macro_user_fragment),
        "expected search contributor resource to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"<ac:parameter ac:name="query">Macro Source</ac:parameter>"#),
        "expected search query parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="tasks-report-macro""#),
        "expected task-report macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(&format!(
            r#"<ac:parameter ac:name="spaceAndPage">{}</ac:parameter>"#,
            cfg.space
        )) && macro_source_body
            .contains(r#"<ac:parameter ac:name="labels">e2e-macro-target</ac:parameter>"#)
            && macro_source_body
                .contains(r#"<ac:parameter ac:name="status">incomplete</ac:parameter>"#)
            && macro_source_body.contains(r#"<ac:parameter ac:name="pageSize">20</ac:parameter>"#)
            && macro_source_body.contains(
                r#"<ac:parameter ac:name="columns">description,assignee,location</ac:parameter>"#
            )
            && macro_source_body
                .contains(r#"<ac:parameter ac:name="sortBy">page title</ac:parameter>"#)
            && macro_source_body
                .contains(r#"<ac:parameter ac:name="reverseSort">false</ac:parameter>"#),
        "expected task-report parameters to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="userlister""#),
        "expected generic userlister macro in source body: {macro_source_body}"
    );
    assert!(
        ((macro_source_body.contains(r#"<ac:parameter ac:name="related"><ac:link><ri:page "#)
            && macro_source_body.contains(&format!(r#"ri:space-key="{}""#, cfg.space))
            && macro_source_body.contains(&format!(r#"ri:content-title="{macro_target_title}""#)))
            || (macro_source_body.contains(r#"<ac:parameter ac:name="related">DefaultLink["#)
                && macro_source_body.contains(&format!("spaceKey={}", cfg.space))
                && macro_source_body.contains(&format!("title={macro_target_title}")))),
        "expected generic userlister page-link parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        ((macro_source_body.contains(r#"<ac:parameter ac:name="contributor"><ri:user "#)
            && macro_source_body.contains(&macro_user_fragment))
            || (macro_source_body.contains(r#"<ac:parameter ac:name="contributor">"#)
                && macro_source_body.contains("UserResourceIdentifier@"))),
        "expected generic userlister contributor parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="detailssummary""#),
        "expected content-properties-report macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body
            .contains(r#"<ac:parameter ac:name="label">e2e-content-properties</ac:parameter>"#)
            && macro_source_body.contains(r#"<ac:parameter ac:name="id">decision</ac:parameter>"#),
        "expected content-properties-report parameters to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="attachments""#),
        "expected attachments macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"<ac:parameter ac:name="patterns">*.pdf</ac:parameter>"#)
            && macro_source_body.contains(r#"<ac:parameter ac:name="sortBy">name</ac:parameter>"#),
        "expected attachments parameters to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="view-file""#),
        "expected view-file macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(
            r#"<ac:parameter ac:name="name"><ri:attachment ri:filename="preview.pdf" /></ac:parameter>"#
        ),
        "expected view-file attachment parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="viewdoc""#),
        "expected view-doc macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"<ac:parameter ac:name="page"><ac:link><ri:page "#)
            && macro_source_body.contains(&format!(r#"ri:content-title="{}""#, macro_target_title))
            && macro_source_body.contains(&format!(r#"ri:space-key="{}""#, cfg.space))
            && macro_source_body.contains(
                r#"<ac:parameter ac:name="name"><ri:attachment ri:filename="manual.docx" /></ac:parameter>"#
            ),
        "expected view-doc parameters to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="viewxls""#),
        "expected view-xls macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(
            r#"<ac:parameter ac:name="name"><ri:attachment ri:filename="sheet.xlsx" /></ac:parameter>"#
        ),
        "expected view-xls attachment parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="viewppt""#),
        "expected view-ppt macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(
            r#"<ac:parameter ac:name="name"><ri:attachment ri:filename="slides.pptx" /></ac:parameter>"#
        ),
        "expected view-ppt attachment parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="blog-posts""#),
        "expected blog-posts macro in source body: {macro_source_body}"
    );
    assert!(
        ((macro_source_body.contains(r#"<ac:parameter ac:name="author"><ri:user "#)
            && macro_source_body.contains(&macro_user_fragment))
            || (macro_source_body.contains(r#"<ac:parameter ac:name="author">"#)
                && macro_source_body.contains("UserResourceIdentifier@")))
            && macro_source_body.contains(&format!(
                r#"<ac:parameter ac:name="spaces"><ri:space ri:space-key="{}" /></ac:parameter>"#,
                cfg.space
            ))
            && macro_source_body.contains(r#"<ac:parameter ac:name="max">5</ac:parameter>"#)
            && macro_source_body.contains(r#"<ac:parameter ac:name="time">7</ac:parameter>"#),
        "expected blog-posts parameters to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="contributors""#),
        "expected contributors macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(&format!(
            r#"<ac:parameter ac:name="spaces"><ri:space ri:space-key="{}" /><ri:space ri:space-key="@personal" /></ac:parameter>"#,
            cfg.space
        )),
        "expected contributors spaces parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body
            .contains(r#"<ac:parameter ac:name="labels">e2e-macro-target</ac:parameter>"#)
            && macro_source_body.contains(r#"<ac:parameter ac:name="mode">list</ac:parameter>"#),
        "expected contributors parameters to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="contributors-summary""#),
        "expected contributors-summary macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(&format!(
            r#"<ac:parameter ac:name="spaces"><ri:space ri:space-key="{}" /></ac:parameter>"#,
            cfg.space
        )),
        "expected contributors-summary spaces parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body
            .contains(r#"<ac:parameter ac:name="columns">edits,comments,labels</ac:parameter>"#)
            && macro_source_body.contains(r#"<ac:parameter ac:name="limit">10</ac:parameter>"#),
        "expected contributors-summary parameters to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="recently-updated""#),
        "expected recently-updated macro in source body: {macro_source_body}"
    );
    assert!(
        ((macro_source_body.contains(r#"<ac:parameter ac:name="author"><ri:user "#)
            && macro_source_body.contains(&macro_user_fragment))
            || (macro_source_body.contains(r#"<ac:parameter ac:name="author">"#)
                && macro_source_body.contains("UserResourceIdentifier@")))
            && macro_source_body.contains(&format!(
                r#"<ac:parameter ac:name="spaces"><ri:space ri:space-key="{}" /></ac:parameter>"#,
                cfg.space
            )),
        "expected recently-updated author and spaces parameters to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="recently-updated-dashboard""#),
        "expected recently-updated-dashboard macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(&format!(
            r#"<ac:parameter ac:name="spaces"><ri:space ri:space-key="{}" /></ac:parameter>"#,
            cfg.space
        )),
        "expected recently-updated-dashboard spaces parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"<ac:parameter ac:name="limit">10</ac:parameter>"#)
            && macro_source_body
                .contains(r#"<ac:parameter ac:name="theme">concise</ac:parameter>"#),
        "expected recently-updated-dashboard parameters to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="livesearch""#),
        "expected livesearch macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body
            .contains(r#"<ac:parameter ac:name="labels">e2e-macro-target</ac:parameter>"#),
        "expected livesearch labels parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"<ac:parameter ac:name="size">large</ac:parameter>"#),
        "expected livesearch size parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(&format!(
            r#"<ac:parameter ac:name="spaceKey">{}</ac:parameter>"#,
            cfg.space
        )) || macro_source_body.contains(&format!(
            r#"<ac:parameter ac:name="spaceKey"><ri:space ri:space-key="{}" /></ac:parameter>"#,
            cfg.space
        )),
        "expected livesearch spaceKey parameter to survive storage rendering as either a plain string or a space resource: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="page-index""#)
            || macro_source_body.contains(r#"ac:name="pageindex""#),
        "expected page-index macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="toc-zone""#),
        "expected toc-zone macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"<ac:parameter ac:name="location">top</ac:parameter>"#)
            && macro_source_body.contains(r#"<ac:parameter ac:name="maxLevel">3</ac:parameter>"#)
            && macro_source_body.contains("<ac:rich-text-body>")
            && macro_source_body.contains("<h2>Zoned Heading</h2>"),
        "expected toc-zone parameters and body to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="listlabels""#),
        "expected labels-list macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(&format!(
            r#"<ac:parameter ac:name="spaceKey"><ri:space ri:space-key="{}" /></ac:parameter>"#,
            cfg.space
        )),
        "expected labels-list spaceKey parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body
            .contains(r#"<ac:parameter ac:name="excludedLabels">drafts,test</ac:parameter>"#),
        "expected labels-list excludedLabels parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="popular-labels""#),
        "expected popular-labels macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(&format!(
            r#"<ac:parameter ac:name="spaceKey"><ri:space ri:space-key="{}" /></ac:parameter>"#,
            cfg.space
        )),
        "expected popular-labels spaceKey parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"<ac:parameter ac:name="count">25</ac:parameter>"#),
        "expected popular-labels count parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"<ac:parameter ac:name="style">heatmap</ac:parameter>"#),
        "expected popular-labels style parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="related-labels""#),
        "expected related-labels macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body
            .contains(r#"<ac:parameter ac:name="labels">e2e-macro-target</ac:parameter>"#),
        "expected related-labels labels parameter to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="recently-used-labels""#),
        "expected recently-used-labels macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"<ac:parameter ac:name="scope">space</ac:parameter>"#)
            && macro_source_body.contains(r#"<ac:parameter ac:name="style">cloud</ac:parameter>"#),
        "expected recently-used-labels parameters to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="gallery""#),
        "expected gallery macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"<ac:parameter ac:name="sortBy">name</ac:parameter>"#)
            && macro_source_body.contains(r#"<ac:parameter ac:name="columns">2</ac:parameter>"#),
        "expected gallery parameters to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="favpages""#),
        "expected favorite-pages macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="change-history""#),
        "expected change-history macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="spaces""#),
        "expected spaces-list macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"<ac:parameter ac:name="">all</ac:parameter>"#)
            && macro_source_body.contains(r#"<ac:parameter ac:name="width">80%</ac:parameter>"#),
        "expected spaces-list parameters to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="space-details""#),
        "expected space-details macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"<ac:parameter ac:name="width">50%</ac:parameter>"#),
        "expected space-details parameters to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="space-attachments""#),
        "expected space-attachments macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(&format!(
            r#"<ac:parameter ac:name="space"><ri:space ri:space-key="{}" /></ac:parameter>"#,
            cfg.space
        )) && macro_source_body
            .contains(r#"<ac:parameter ac:name="showFilter">false</ac:parameter>"#),
        "expected space-attachments parameters to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="noformat""#),
        "expected noformat macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"<ac:parameter ac:name="nopanel">true</ac:parameter>"#)
            && macro_source_body.contains("<ac:plain-text-body><![CDATA[<xml>literal</xml>")
            && macro_source_body.contains("line 2"),
        "expected noformat parameters and plain-text body to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="profile""#),
        "expected profile macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(&macro_user_fragment),
        "expected profile user resource to survive storage rendering: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="status-list""#),
        "expected status-list macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(&macro_user_fragment)
            || macro_source_body.contains(r#"<ac:parameter ac:name="username">"#),
        "expected status-list user parameter to survive storage rendering as either a user resource or a plain username: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"ac:name="network""#),
        "expected network macro in source body: {macro_source_body}"
    );
    assert!(
        macro_source_body.contains(r#"<ac:parameter ac:name="">followers</ac:parameter>"#)
            && macro_source_body.contains(&macro_user_fragment)
            && macro_source_body.contains(r#"<ac:parameter ac:name="max">10</ac:parameter>"#)
            && macro_source_body.contains(r#"<ac:parameter ac:name="theme">full</ac:parameter>"#),
        "expected network parameters to survive storage rendering: {macro_source_body}"
    );

    let macro_target_get = cfg.run_json(&["page", "get", &macro_target_id, "--show-body"]);
    let macro_target_body = string_field(
        first_item(&macro_target_get, "macro target get"),
        "body_storage",
    );
    assert!(
        macro_target_body.contains(r#"ac:name="details""#),
        "expected content-properties macro in target body: {macro_target_body}"
    );
    assert!(
        macro_target_body.contains(r#"<ac:parameter ac:name="id">decision</ac:parameter>"#)
            && macro_target_body.contains("<ac:rich-text-body>")
            && macro_target_body.contains("<table>")
            && macro_target_body.contains("<th>Field</th>")
            && macro_target_body.contains("<td>Ada</td>"),
        "expected content-properties parameters and table body to survive storage rendering: {macro_target_body}"
    );

    let macro_pull_arg = macro_pull_dir.to_string_lossy().into_owned();
    cfg.run(&["pull", "tree", &macro_root_id, &macro_pull_arg]);
    let pulled_macro_source = find_index_md_by_title(&macro_pull_dir, &macro_source_title);
    let pulled_macro_source_markdown =
        fs::read_to_string(&pulled_macro_source).expect("read pulled macro source markdown");
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-excerpt-include"),
        "expected pulled macro source to preserve excerpt-include block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("page: ../")
            && pulled_macro_source_markdown.contains("/index.md")
            && !pulled_macro_source_markdown.contains("confluence-page://page?"),
        "expected pulled macro source to rewrite excerpt target to a local path: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-children"),
        "expected pulled macro source to preserve children block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("page: ../index.md")
            && !pulled_macro_source_markdown.contains("page: confluence-page://page?"),
        "expected pulled children page parameter to rewrite to a local path: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-include-page"),
        "expected pulled macro source to preserve include-page block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("page: ../")
            && pulled_macro_source_markdown.contains("/index.md")
            && !pulled_macro_source_markdown.contains("confluence-page://page?"),
        "expected pulled include-page target to rewrite to a local path: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-page-tree"),
        "expected pulled macro source to preserve page-tree block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("root: index.md")
            && !pulled_macro_source_markdown.contains("root: confluence-page://page?"),
        "expected pulled page-tree root to rewrite to a local path: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-page-tree-search"),
        "expected pulled macro source to preserve page-tree-search block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("root: ../")
            && pulled_macro_source_markdown.contains("/index.md")
            && !pulled_macro_source_markdown.contains("root: confluence-page://page?"),
        "expected pulled page-tree-search root to rewrite to a local path: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(&format!("spaceKey: {}", cfg.space)),
        "expected pulled page-tree-search spaceKey to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-anchor"),
        "expected pulled macro source to preserve anchor block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("name: macro-source-anchor"),
        "expected pulled anchor name to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-navmap"),
        "expected pulled macro source to preserve navmap block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("$default: Macro Root,Macro Source,Shared Excerpt")
            && pulled_macro_source_markdown.contains("title: Macro navigation")
            && pulled_macro_source_markdown.contains("wrapAfter: 4"),
        "expected pulled navmap parameters to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-content-by-label"),
        "expected pulled macro source to preserve content-by-label block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(r#"cql: label = "e2e-macro-target""#),
        "expected pulled content-by-label cql to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-content-by-user"),
        "expected pulled macro source to preserve content-by-user block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("user: confluence-user://user?")
            && pulled_macro_source_markdown.contains(&macro_user_placeholder_fragment),
        "expected pulled content-by-user user parameter to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-content-report-table"),
        "expected pulled macro source to preserve content-report-table block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("labels: e2e-macro-target")
            && pulled_macro_source_markdown.contains(&format!("spaces: {}", cfg.space))
            && pulled_macro_source_markdown.contains("maxResults: 5"),
        "expected pulled content-report-table parameters to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-task-report"),
        "expected pulled macro source to preserve task-report block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(&format!("spaceAndPage: {}", cfg.space))
            && pulled_macro_source_markdown.contains("labels: e2e-macro-target")
            && pulled_macro_source_markdown.contains("status: incomplete")
            && pulled_macro_source_markdown.contains("pageSize: 20")
            && pulled_macro_source_markdown.contains("columns: description,assignee,location")
            && pulled_macro_source_markdown.contains("sortBy: page title")
            && pulled_macro_source_markdown.contains("reverseSort: false"),
        "expected pulled task-report parameters to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-macro userlister"),
        "expected pulled macro source to preserve generic userlister block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("contributor: !user confluence-user://user?")
            && pulled_macro_source_markdown.contains(&macro_user_placeholder_fragment),
        "expected pulled generic userlister contributor parameter to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("related: !page-link ../")
            && pulled_macro_source_markdown.contains("/index.md")
            && !pulled_macro_source_markdown.contains("!page-link confluence-page://page?"),
        "expected pulled generic userlister page-link parameter to rewrite to a local path: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-search"),
        "expected pulled macro source to preserve search block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(&format!("spacekey: !space {}", cfg.space)),
        "expected pulled search spacekey resource to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("contributor: !user confluence-user://user?")
            && pulled_macro_source_markdown.contains(&macro_user_placeholder_fragment),
        "expected pulled search contributor resource to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("query: Macro Source"),
        "expected pulled search query parameter to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-content-properties-report"),
        "expected pulled macro source to preserve content-properties-report block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("label: e2e-content-properties")
            && pulled_macro_source_markdown.contains("id: decision"),
        "expected pulled content-properties-report parameters to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-attachments"),
        "expected pulled macro source to preserve attachments block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("patterns: *.pdf")
            && pulled_macro_source_markdown.contains("sortBy: name"),
        "expected pulled attachments parameters to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-view-file"),
        "expected pulled macro source to preserve view-file block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("attachment: preview.pdf"),
        "expected pulled view-file attachment parameter to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-view-doc"),
        "expected pulled macro source to preserve view-doc block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("page: ../")
            && pulled_macro_source_markdown.contains("/index.md")
            && pulled_macro_source_markdown.contains("attachment: manual.docx")
            && !pulled_macro_source_markdown.contains("page: confluence-page://page?"),
        "expected pulled view-doc parameters to survive export and rewrite the page reference locally: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-view-xls"),
        "expected pulled macro source to preserve view-xls block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("attachment: sheet.xlsx"),
        "expected pulled view-xls attachment parameter to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-view-ppt"),
        "expected pulled macro source to preserve view-ppt block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("attachment: slides.pptx"),
        "expected pulled view-ppt attachment parameter to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-blog-posts"),
        "expected pulled macro source to preserve blog-posts block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("author: confluence-user://user?")
            && pulled_macro_source_markdown.contains(&macro_user_placeholder_fragment)
            && pulled_macro_source_markdown.contains(&format!("spaces: {}", cfg.space))
            && pulled_macro_source_markdown.contains("max: 5")
            && pulled_macro_source_markdown.contains("time: 7"),
        "expected pulled blog-posts parameters to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-contributors"),
        "expected pulled macro source to preserve contributors block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(&format!("spaces: {},@personal", cfg.space))
            && pulled_macro_source_markdown.contains("labels: e2e-macro-target")
            && pulled_macro_source_markdown.contains("mode: list"),
        "expected pulled contributors parameters to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-contributors-summary"),
        "expected pulled macro source to preserve contributors-summary block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(&format!("spaces: {}", cfg.space))
            && pulled_macro_source_markdown.contains("columns: edits,comments,labels")
            && pulled_macro_source_markdown.contains("limit: 10"),
        "expected pulled contributors-summary parameters to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-recently-updated"),
        "expected pulled macro source to preserve recently-updated block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("author: confluence-user://user?")
            && pulled_macro_source_markdown.contains(&macro_user_placeholder_fragment)
            && pulled_macro_source_markdown.contains(&format!("spaces: {}", cfg.space)),
        "expected pulled recently-updated author and spaces to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-recently-updated-dashboard"),
        "expected pulled macro source to preserve recently-updated-dashboard block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("limit: 10")
            && pulled_macro_source_markdown.contains("theme: concise"),
        "expected pulled recently-updated-dashboard parameters to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(&format!("spaces: {}", cfg.space)),
        "expected pulled recently-updated-dashboard spaces to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-livesearch"),
        "expected pulled macro source to preserve livesearch block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("labels: e2e-macro-target")
            && pulled_macro_source_markdown.contains("size: large"),
        "expected pulled livesearch parameters to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-page-index"),
        "expected pulled macro source to preserve page-index block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-toc-zone"),
        "expected pulled macro source to preserve toc-zone block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("location: top")
            && pulled_macro_source_markdown.contains("maxLevel: 3")
            && pulled_macro_source_markdown.contains("## Zoned Heading"),
        "expected pulled toc-zone parameters and body to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-labels-list"),
        "expected pulled macro source to preserve labels-list block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(&format!("spaceKey: {}", cfg.space)),
        "expected pulled labels-list spaceKey to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("excludedLabels: drafts,test"),
        "expected pulled labels-list excludedLabels to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-popular-labels"),
        "expected pulled macro source to preserve popular-labels block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("count: 25")
            && pulled_macro_source_markdown.contains("style: heatmap"),
        "expected pulled popular-labels parameters to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-related-labels"),
        "expected pulled macro source to preserve related-labels block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("labels: e2e-macro-target"),
        "expected pulled related-labels labels parameter to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-recently-used-labels"),
        "expected pulled macro source to preserve recently-used-labels block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("scope: space")
            && pulled_macro_source_markdown.contains("style: cloud"),
        "expected pulled recently-used-labels parameters to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-gallery"),
        "expected pulled macro source to preserve gallery block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("sortBy: name")
            && pulled_macro_source_markdown.contains("columns: 2"),
        "expected pulled gallery parameters to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-favorite-pages"),
        "expected pulled macro source to preserve favorite-pages block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-change-history"),
        "expected pulled macro source to preserve change-history block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-spaces-list"),
        "expected pulled macro source to preserve spaces-list block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("scope: all")
            && pulled_macro_source_markdown.contains("width: 80%"),
        "expected pulled spaces-list parameters to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-space-details"),
        "expected pulled macro source to preserve space-details block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("width: 50%"),
        "expected pulled space-details parameters to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-space-attachments"),
        "expected pulled macro source to preserve space-attachments block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(&format!("space: {}", cfg.space))
            && pulled_macro_source_markdown.contains("showFilter: false"),
        "expected pulled space-attachments parameters to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("~~~confluence-noformat"),
        "expected pulled macro source to preserve noformat block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("nopanel: true")
            && pulled_macro_source_markdown.contains("<xml>literal</xml>")
            && pulled_macro_source_markdown.contains("line 2"),
        "expected pulled noformat parameters and body to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-profile"),
        "expected pulled macro source to preserve profile block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("user: confluence-user://user?"),
        "expected pulled profile user parameter to survive export as a user placeholder: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-status-list"),
        "expected pulled macro source to preserve status-list block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("username: "),
        "expected pulled status-list user parameter to survive export: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains(":::confluence-network"),
        "expected pulled macro source to preserve network block: {pulled_macro_source_markdown}"
    );
    assert!(
        pulled_macro_source_markdown.contains("mode: followers")
            && pulled_macro_source_markdown.contains("username: confluence-user://user?")
            && pulled_macro_source_markdown.contains("max: 10")
            && pulled_macro_source_markdown.contains("theme: full"),
        "expected pulled network parameters to survive export: {pulled_macro_source_markdown}"
    );

    let pulled_macro_target = find_index_md_by_title(&macro_pull_dir, &macro_target_title);
    let pulled_macro_target_markdown =
        fs::read_to_string(&pulled_macro_target).expect("read pulled macro target markdown");
    assert!(
        pulled_macro_target_markdown.contains(":::confluence-content-properties"),
        "expected pulled macro target to preserve content-properties block: {pulled_macro_target_markdown}"
    );
    assert!(
        pulled_macro_target_markdown.contains("id: decision")
            && pulled_macro_target_markdown.contains("| Field | Value |")
            && pulled_macro_target_markdown.contains("| Owner | Ada |")
            && pulled_macro_target_markdown.contains("| Status | Approved |"),
        "expected pulled content-properties parameters and table to survive export: {pulled_macro_target_markdown}"
    );

    let macro_plan = cfg.run_json(&["plan", &macro_pull_arg]);
    assert!(
        macro_plan
            .get("items")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                !items.is_empty()
                    && items
                        .iter()
                        .all(|item| item.get("action").and_then(Value::as_str) == Some("noop"))
            }),
        "expected macro pull plan to be noop: {macro_plan}"
    );

    let blog_body_1_arg = blog_body_1.to_string_lossy().into_owned();
    let blog_create = cfg.run_json(&[
        "blog",
        "create",
        &blog_title,
        cfg.space.as_str(),
        "--body-file",
        &blog_body_1_arg,
    ]);
    let blog_id = string_field(first_item(&blog_create, "blog create"), "id").to_string();
    cleanup.blog_id = Some(blog_id.clone());
    let blog_get = cfg.run_json(&["blog", "get", &blog_id, "--show-body"]);
    assert_eq!(
        string_field(first_item(&blog_get, "blog get"), "id"),
        blog_id
    );

    let blog_body_2_arg = blog_body_2.to_string_lossy().into_owned();
    let blog_update = cfg.run_json(&[
        "blog",
        "update",
        &blog_id,
        "--title",
        &updated_blog_title,
        "--body-file",
        &blog_body_2_arg,
    ]);
    assert_eq!(
        string_field(first_item(&blog_update, "blog update"), "title"),
        updated_blog_title
    );

    cfg.run(&["--yes", "blog", "delete", &blog_id]);
    cleanup.blog_id = None;

    cfg.run(&["--yes", "page", "delete", &page_id]);
    cleanup.page_id = None;
}
