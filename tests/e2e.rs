use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tempfile::TempDir;
use walkdir::WalkDir;

#[derive(Clone, Debug)]
struct E2eConfig {
    bin: PathBuf,
    profile: Option<String>,
    envs: Vec<(String, String)>,
    space: String,
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
                .best_effort(&["attachment", "delete", page_id, attachment_id]);
        }
        if let Some(comment_id) = self.comment_id.as_deref() {
            self.cfg.best_effort(&["comment", "delete", comment_id]);
        }
        if let Some(blog_id) = self.blog_id.as_deref() {
            self.cfg.best_effort(&["blog", "delete", blog_id]);
        }
        if let Some(page_id) = self.page_id.as_deref() {
            self.cfg.best_effort(&["page", "delete", page_id]);
        }
        for page_id in self.extra_page_ids.iter().rev() {
            self.cfg.best_effort(&["page", "delete", page_id]);
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

    let mut envs = vec![
        ("CONFLUENCE_PROFILE".to_string(), "__e2e_env__".to_string()),
        (
            "CONFLUENCE_DOMAIN".to_string(),
            base_url.expect("base url present"),
        ),
        ("CONFLUENCE_PROVIDER".to_string(), provider),
        ("CONFLUENCE_AUTH_TYPE".to_string(), auth_type.clone()),
        ("CONFLUENCE_API_PATH".to_string(), api_path),
    ];

    match auth_type.as_str() {
        "basic" => {
            let username = env::var("CONFLUENCE_E2E_USERNAME")
                .or_else(|_| env::var("CONFLUENCE_E2E_EMAIL"))
                .expect(
                    "basic auth e2e mode requires CONFLUENCE_E2E_USERNAME or CONFLUENCE_E2E_EMAIL",
                );
            envs.push(("CONFLUENCE_USERNAME".to_string(), username));
            envs.push((
                "CONFLUENCE_API_TOKEN".to_string(),
                token.expect("token present"),
            ));
        }
        _ => {
            envs.push((
                "CONFLUENCE_BEARER_TOKEN".to_string(),
                token.expect("token present"),
            ));
        }
    }

    Some(E2eConfig {
        bin,
        profile: None,
        envs,
        space,
    })
}

fn find_binary_path() -> PathBuf {
    if let Some(path) = env::var_os("CARGO_BIN_EXE_confluence-cli") {
        return PathBuf::from(path);
    }

    let current_exe = env::current_exe().expect("current exe path");
    for ancestor in current_exe.ancestors() {
        let candidate = ancestor.join("confluence-cli");
        if candidate.is_file() {
            return candidate;
        }
        let exe_candidate = ancestor.join("confluence-cli.exe");
        if exe_candidate.is_file() {
            return exe_candidate;
        }
    }

    panic!(
        "failed to locate confluence-cli binary from {}",
        current_exe.display()
    );
}

fn first_item<'a>(value: &'a Value, context: &str) -> &'a Value {
    value
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or_else(|| panic!("expected non-empty array for {context}: {value}"))
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

fn wait_until<F>(timeout: Duration, interval: Duration, mut check: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = std::time::Instant::now();
    loop {
        if check() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        thread::sleep(interval);
    }
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
        space_list.as_array().is_some_and(|items| items
            .iter()
            .any(|item| string_field(item, "key") == cfg.space)),
        "expected space list to include {}: {space_list}",
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

    let found_search_hit = wait_until(Duration::from_secs(30), Duration::from_secs(2), || {
        let search = cfg.run_json(&["search", &page_title, "--limit", "10"]);
        search.as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("id").and_then(Value::as_str) == Some(page_id.as_str()))
        })
    });
    assert!(found_search_hit, "expected search to find page {page_id}");

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
        labels
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item.as_str() == Some("e2e-auto"))),
        "expected label list to contain e2e-auto: {labels}"
    );
    cfg.run(&["label", "remove", &page_id, "e2e-auto"]);

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
    cfg.run(&["property", "delete", &page_id, "e2e_verify"]);

    let comment_body_arg = comment_body.to_string_lossy().into_owned();
    let comment_add = cfg.run_json(&["comment", "add", &page_id, "--body-file", &comment_body_arg]);
    let comment_id = string_field(first_item(&comment_add, "comment add"), "id").to_string();
    cleanup.comment_id = Some(comment_id.clone());
    let comments = cfg.run_json(&["comment", "list", &page_id]);
    assert!(
        comments.as_array().is_some_and(|items| items
            .iter()
            .any(|item| item.get("id").and_then(Value::as_str) == Some(comment_id.as_str()))),
        "expected comment list to contain {comment_id}: {comments}"
    );
    cfg.run(&["comment", "delete", &comment_id]);
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
        attachments.as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("id").and_then(Value::as_str) == Some(attachment_id.as_str()))
        }),
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
    cfg.run(&["attachment", "delete", &page_id, &attachment_id]);
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
    cfg.run(&["pull", "page", &page_id, &pull_arg]);
    let index_md = find_index_md(&pull_dir);
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
    fs::create_dir_all(&macro_root_dir).expect("create macro root dir");
    fs::create_dir_all(&macro_source_dir).expect("create macro source dir");
    fs::create_dir_all(&macro_target_dir).expect("create macro target dir");
    fs::create_dir_all(&macro_child_dir).expect("create macro child dir");
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
            "---\ntitle: {macro_source_title}\ntype: page\nlabels: []\nstatus: current\nparent: null\nproperties: {{}}\n---\n\n# Macro Source\n\n:::confluence-excerpt-include\nnopanel: true\npage: ../target/index.md\n:::\n\n:::confluence-include-page\npage: ../target/index.md\n:::\n\n:::confluence-page-tree\nroot: index.md\nsearchBox: true\n:::\n\n:::confluence-page-tree-search\nroot: ../target/index.md\nspaceKey: {space}\n:::\n\n:::confluence-children\nall: true\nsort: creation\n:::\n",
            space = cfg.space
        ),
    )
    .expect("write macro source markdown");
    fs::write(
        macro_target_dir.join("index.md"),
        format!(
            "---\ntitle: {macro_target_title}\ntype: page\nlabels: []\nstatus: current\nparent: null\nproperties: {{}}\n---\n\n# Shared Excerpt\n\nPulled and reapplied through the e2e macro test.\n"
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
    cfg.run(&["blog", "delete", &blog_id]);
    cleanup.blog_id = None;

    cfg.run(&["page", "delete", &page_id]);
    cleanup.page_id = None;
}
