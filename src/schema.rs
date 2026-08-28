use serde_json::{Value, json};
use std::any::TypeId;

fn arg_to_json(arg: &clap::Arg) -> Value {
    let mut obj = serde_json::Map::new();

    let id = arg.get_id().as_str();
    let name = if arg.is_positional() {
        id.to_string()
    } else {
        arg.get_long()
            .map(|l| format!("--{l}"))
            .unwrap_or_else(|| id.to_string())
    };
    obj.insert("name".into(), json!(name));

    let value_type = arg.get_value_parser().type_id();
    let is_bool = !arg.get_action().takes_values() || value_type == TypeId::of::<bool>();
    if is_bool {
        obj.insert("type".into(), json!("boolean"));
    } else if value_type == TypeId::of::<usize>()
        || value_type == TypeId::of::<u64>()
        || value_type == TypeId::of::<u32>()
        || value_type == TypeId::of::<i64>()
    {
        obj.insert("type".into(), json!("integer"));
    } else {
        let possible: Vec<String> = arg
            .get_possible_values()
            .iter()
            .map(|v| v.get_name().to_string())
            .collect();
        if !possible.is_empty() {
            obj.insert("type".into(), json!("string"));
            obj.insert("enum".into(), json!(possible));
        } else {
            obj.insert("type".into(), json!("string"));
        }
    }

    if let Some(help) = arg.get_help().map(|h| h.to_string()) {
        obj.insert("description".into(), json!(help));
    }

    if arg.is_required_set() {
        obj.insert("required".into(), json!(true));
    } else if arg.is_positional() {
        obj.insert("required".into(), json!(arg.is_required_set()));
    }

    if let Some(default) = arg.get_default_values().first() {
        obj.insert("default".into(), json!(default.to_string_lossy()));
    }

    Value::Object(obj)
}

fn is_mutating(path: &str) -> bool {
    let mutating_verbs = [
        "create", "update", "delete", "move", "add", "upload", "set", "apply", "pull", "remove",
        "login", "logout", "migrate", "use", "init",
    ];
    let last_word = path.rsplit_once(' ').map(|(_, w)| w).unwrap_or(path);
    mutating_verbs.contains(&last_word)
}

fn effect_kind(path: &str) -> &'static str {
    if matches!(
        path,
        "pull page" | "pull tree" | "pull space" | "attachment download"
    ) {
        "local_write"
    } else if is_mutating(path) {
        "remote_or_config_write"
    } else {
        "read_only"
    }
}

fn is_destructive(path: &str) -> bool {
    matches!(
        path,
        "auth logout"
            | "profile remove"
            | "page delete"
            | "blog delete"
            | "attachment delete"
            | "label remove"
            | "comment delete"
            | "property delete"
    )
}

fn is_idempotent(path: &str) -> bool {
    !is_mutating(path)
        || matches!(
            path,
            "profile use" | "label add" | "label remove" | "property set" | "property delete"
        )
}

fn output_fields_for(path: &str) -> Vec<Value> {
    let content_fields = || {
        vec![
            json!({"name": "id", "type": "string"}),
            json!({"name": "kind", "type": "string"}),
            json!({"name": "space_key", "type": "string|null"}),
            json!({"name": "title", "type": "string"}),
            json!({"name": "status", "type": "string"}),
            json!({"name": "version", "type": "integer|null"}),
            json!({"name": "parent_id", "type": "string|null"}),
        ]
    };

    match path {
        "page get" | "page list" | "page tree" | "page move" | "page create" | "page update"
        | "blog get" | "blog list" | "blog create" | "blog update" => content_fields(),

        "page delete" | "blog delete" => {
            vec![
                json!({"name": "id", "type": "string"}),
                json!({"name": "deleted", "type": "boolean"}),
            ]
        }

        "attachment delete" => vec![
            json!({"name": "attachment_id", "type": "string"}),
            json!({"name": "deleted", "type": "boolean"}),
        ],

        "comment delete" => vec![
            json!({"name": "comment_id", "type": "string"}),
            json!({"name": "deleted", "type": "boolean"}),
        ],

        "property delete" => vec![
            json!({"name": "key", "type": "string"}),
            json!({"name": "deleted", "type": "boolean"}),
        ],

        "label remove" => vec![
            json!({"name": "label", "type": "string"}),
            json!({"name": "removed", "type": "boolean"}),
        ],

        "space list" | "space get" => vec![
            json!({"name": "id", "type": "string"}),
            json!({"name": "key", "type": "string"}),
            json!({"name": "name", "type": "string"}),
            json!({"name": "space_type", "type": "string|null"}),
            json!({"name": "homepage_id", "type": "string|null"}),
        ],

        "search" => vec![
            json!({"name": "id", "type": "string"}),
            json!({"name": "kind", "type": "string"}),
            json!({"name": "space_key", "type": "string|null"}),
            json!({"name": "title", "type": "string"}),
            json!({"name": "web_url", "type": "string|null"}),
        ],

        "attachment list" | "attachment upload" => vec![
            json!({"name": "id", "type": "string"}),
            json!({"name": "title", "type": "string"}),
            json!({"name": "media_type", "type": "string|null"}),
            json!({"name": "file_size", "type": "integer|null"}),
            json!({"name": "download_url", "type": "string|null"}),
        ],

        "comment list" | "comment add" | "comment update" => vec![
            json!({"name": "id", "type": "string"}),
            json!({"name": "author", "type": "string|null"}),
            json!({"name": "created_at", "type": "string|null"}),
            json!({"name": "body_storage", "type": "string"}),
        ],

        "property list" | "property get" | "property set" => vec![
            json!({"name": "id", "type": "string|null"}),
            json!({"name": "key", "type": "string"}),
            json!({"name": "version", "type": "integer|null"}),
            json!({"name": "value", "type": "json"}),
        ],

        "auth login" | "profile add" => vec![
            json!({"name": "name", "type": "string"}),
            json!({"name": "provider", "type": "string"}),
            json!({"name": "base_url", "type": "string"}),
            json!({"name": "api_path", "type": "string"}),
            json!({"name": "credential_store", "type": "string"}),
            json!({"name": "cloud_id", "type": "string|null"}),
            json!({"name": "token_kind", "type": "string"}),
            json!({"name": "expires_at", "type": "string|null"}),
            json!({"name": "read_only", "type": "boolean"}),
        ],

        "auth status" => vec![
            json!({"name": "name", "type": "string"}),
            json!({"name": "provider", "type": "string"}),
            json!({"name": "base_url", "type": "string"}),
            json!({"name": "api_path", "type": "string"}),
            json!({"name": "credential_store", "type": "string"}),
            json!({"name": "cloud_id", "type": "string|null"}),
            json!({"name": "token_kind", "type": "string"}),
            json!({"name": "expires_at", "type": "string|null"}),
            json!({"name": "expiration_status", "type": "string"}),
            json!({"name": "read_only", "type": "boolean"}),
        ],

        "auth logout" => vec![
            json!({"name": "profile", "type": "string"}),
            json!({"name": "status", "type": "string"}),
        ],

        "auth migrate" => vec![
            json!({"name": "profile", "type": "string"}),
            json!({"name": "migrated", "type": "boolean"}),
            json!({"name": "credential_store", "type": "string"}),
        ],

        "init" => vec![
            json!({"name": "configPath", "type": "string"}),
            json!({"name": "configExists", "type": "boolean"}),
            json!({"name": "cloudTokenUrl", "type": "string"}),
            json!({"name": "dcPatDocs", "type": "string"}),
            json!({"name": "envVars", "type": "object"}),
            json!({"name": "example", "type": "object"}),
        ],

        "profile list" => vec![
            json!({"name": "name", "type": "string"}),
            json!({"name": "provider", "type": "string"}),
            json!({"name": "base_url", "type": "string"}),
            json!({"name": "api_path", "type": "string"}),
            json!({"name": "credential_store", "type": "string"}),
            json!({"name": "token_kind", "type": "string"}),
            json!({"name": "expires_at", "type": "string|null"}),
            json!({"name": "read_only", "type": "boolean"}),
            json!({"name": "active", "type": "boolean"}),
        ],

        "profile use" => vec![
            json!({"name": "profile", "type": "string"}),
            json!({"name": "active", "type": "boolean"}),
        ],

        "profile remove" => vec![
            json!({"name": "profile", "type": "string"}),
            json!({"name": "removed", "type": "boolean"}),
        ],

        "pull page" | "pull tree" | "pull space" => vec![json!({"name": "path", "type": "string"})],

        "doctor" => vec![
            json!({"name": "config_path", "type": "string"}),
            json!({"name": "config_exists", "type": "boolean"}),
            json!({"name": "active_profile", "type": "string|null"}),
            json!({"name": "stored_profiles", "type": "integer"}),
            json!({"name": "resolved_profile", "type": "object|null"}),
            json!({"name": "checks", "type": "array"}),
            json!({"name": "summary", "type": "object"}),
        ],

        "attachment download" => vec![
            json!({"name": "path", "type": "string"}),
            json!({"name": "downloaded", "type": "boolean"}),
        ],

        "label list" => vec![json!({"name": "label", "type": "string"})],

        "label add" => vec![
            json!({"name": "label", "type": "string"}),
            json!({"name": "added", "type": "boolean"}),
        ],

        // completions: outputs shell completion scripts (unstructured text, not records)
        // schema: outputs the schema itself (not a structured record)
        "plan" | "apply" => vec![
            json!({"name": "action", "type": "string"}),
            json!({"name": "title", "type": "string"}),
            json!({"name": "content_id", "type": "string|null"}),
            json!({"name": "path", "type": "string"}),
            json!({"name": "details", "type": "string"}),
        ],

        _ => vec![],
    }
}

/// Commands that get --limit/--offset/--fields injected in the schema
/// because they are list commands (even if the clap definition lacks some).
const LIST_COMMANDS: &[&str] = &[
    "space list",
    "page list",
    "blog list",
    "search",
    "attachment list",
    "label list",
    "comment list",
    "property list",
];

fn walk_commands(cmd: &clap::Command, prefix: &str, out: &mut Vec<Value>) {
    let global_ids = [
        "help",
        "version",
        "output_format",
        "json",
        "profile",
        "quiet",
        "no_color",
        "yes",
    ];

    for sub in cmd.get_subcommands() {
        let name = sub.get_name();
        if name == "help" {
            continue;
        }

        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix} {name}")
        };

        let has_subcommands = sub.get_subcommands().any(|s| s.get_name() != "help");
        if has_subcommands {
            walk_commands(sub, &path, out);
        } else {
            let mut args: Vec<Value> = Vec::new();

            for arg in sub.get_arguments() {
                if global_ids.contains(&arg.get_id().as_str()) || arg.is_hide_set() {
                    continue;
                }
                args.push(arg_to_json(arg));
            }

            let mut entry = serde_json::Map::new();
            entry.insert("name".into(), json!(path));

            if let Some(about) = sub.get_about().map(|a| a.to_string()) {
                entry.insert("description".into(), json!(about));
            } else {
                entry.insert(
                    "description".into(),
                    json!(format!("Run the {path} command")),
                );
            }

            entry.insert("mutating".into(), json!(is_mutating(&path)));
            entry.insert("effects".into(), json!(effect_kind(&path)));
            entry.insert("idempotent".into(), json!(is_idempotent(&path)));
            entry.insert("destructive".into(), json!(is_destructive(&path)));
            entry.insert("requires_confirmation".into(), json!(is_destructive(&path)));

            if !args.is_empty() {
                entry.insert("args".into(), json!(args));
            }

            let fields = output_fields_for(&path);
            if !fields.is_empty() {
                entry.insert("output_fields".into(), json!(fields));
            }

            out.push(Value::Object(entry));
        }
    }
}

pub fn generate(cmd: &clap::Command) -> Value {
    let mut commands: Vec<Value> = Vec::new();
    walk_commands(cmd, "", &mut commands);

    let mut document = json!({
        "clispec": "0.3",
        "response_contract": "1",
        "name": "confluence",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Markdown-sync-first Confluence CLI in Rust",
        "output": {"tty": "text", "piped": "json"},
        "global_args": [
            {
                "name": "--output",
                "type": "string",
                "description": "Output format: auto (default), text, or json. Auto uses JSON when stdout is not a terminal.",
                "enum": ["auto", "text", "json"],
                "default": "auto"
            },
            {
                "name": "-o",
                "type": "string",
                "description": "Short form of --output.",
                "enum": ["auto", "text", "json"],
                "default": "auto"
            },
            {
                "name": "--profile",
                "type": "string",
                "description": "Configuration profile name"
            },
            {
                "name": "--quiet",
                "type": "boolean",
                "description": "Suppress progress and informational messages"
            },
            {
                "name": "--no-color",
                "type": "boolean",
                "description": "Disable ANSI color"
            },
            {
                "name": "--yes",
                "type": "boolean",
                "description": "Skip confirmation prompts for destructive operations"
            }
        ],
        "errors": [
            {
                "kind": "invalid_input",
                "exit_code": 2,
                "retryable": false,
                "description": "The command arguments or input data were invalid."
            },
            {
                "kind": "tty_required",
                "exit_code": 2,
                "retryable": false,
                "description": "Interactive setup was requested without a terminal."
            },
            {
                "kind": "confirmation_required",
                "exit_code": 2,
                "retryable": false,
                "description": "A destructive operation requires confirmation. Re-run with --yes to confirm."
            },
            {
                "kind": "read_only",
                "exit_code": 2,
                "retryable": false,
                "description": "The selected profile forbids write operations."
            },
            {
                "kind": "auth",
                "exit_code": 3,
                "retryable": false,
                "description": "Authentication failed or credentials are missing/invalid."
            },
            {
                "kind": "not_found",
                "exit_code": 4,
                "retryable": false,
                "description": "The requested resource was not found."
            },
            {
                "kind": "api_error",
                "exit_code": 5,
                "retryable": false,
                "description": "Confluence returned an API error."
            },
            {
                "kind": "network",
                "exit_code": 5,
                "retryable": true,
                "description": "A connection or timeout failure prevented the request."
            },
            {
                "kind": "rate_limit",
                "exit_code": 6,
                "retryable": true,
                "description": "Confluence rate-limited the request."
            },
            {
                "kind": "conflict",
                "exit_code": 7,
                "retryable": false,
                "description": "A conflict occurred, such as a version mismatch or duplicate resource."
            },
            {
                "kind": "unexpected_error",
                "exit_code": 1,
                "retryable": false,
                "description": "An unexpected local or transport error occurred."
            }
        ],
        "commands": commands
    });
    enrich_v03(&mut document);
    document
}

fn enrich_v03(document: &mut Value) {
    let Some(commands) = document.get_mut("commands").and_then(Value::as_array_mut) else {
        return;
    };
    for command in commands {
        let Some(object) = command.as_object_mut() else {
            continue;
        };
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if name == "completions" {
            object.remove("output_fields");
            object.insert("output_kind".into(), json!("opaque"));
            object.insert("media_type".into(), json!("text/plain"));
            continue;
        }
        let unbounded = LIST_COMMANDS.contains(&name.as_str());
        object.insert(
            "cardinality".into(),
            json!(if unbounded { "unbounded" } else { "bounded" }),
        );
        if unbounded {
            object.insert(
                "pagination".into(),
                json!({"style":"offset","limit_arg":"--limit","offset_arg":"--offset"}),
            );
            object.insert("fields_arg".into(), json!("--fields"));
            object.insert(
                "output_envelope".into(),
                json!({
                    "items": "array",
                    "total": if name == "search" { "integer|null" } else { "integer" },
                    "limit": "integer",
                    "offset": "integer"
                }),
            );
        }
        if name == "apply" {
            object.insert("destructive_when".into(), json!(["--delete-remote"]));
            object.insert(
                "requires_confirmation_when".into(),
                json!(["--delete-remote"]),
            );
        }
        if name == "attachment upload" {
            object.insert("destructive_when".into(), json!(["--replace"]));
            object.insert("partial_success_possible".into(), json!(true));
        }
        if matches!(
            name.as_str(),
            "page create" | "page update" | "page move" | "blog create" | "blog update"
        ) {
            object.insert("partial_success_possible".into(), json!(true));
        }
        if name == "attachment download" {
            object.insert("overwrites_when".into(), json!(["--force"]));
        }
        if name == "profile list" {
            object.insert("example".into(), json!({"args":["profile","list"]}));
        }
        if name == "schema" {
            object.insert("cardinality".into(), json!("single"));
            object.insert(
                "stdout_schema".into(),
                json!({"$ref":"https://clispec.dev/schema/v0.3.json"}),
            );
        }
        if !object.contains_key("output_fields") && !object.contains_key("stdout_schema") {
            object.insert("stdout_schema".into(), json!({}));
        }
    }
}

pub fn print_schema(command_filter: Option<&str>) -> bool {
    use clap::CommandFactory;
    let cmd = super::cli::Cli::command();
    let mut schema = generate(&cmd);
    if let Some(filter) = command_filter {
        schema["commands"] = Value::Array(
            schema["commands"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|command| command["name"] == filter)
                .collect(),
        );
    }
    let found = command_filter.is_none()
        || schema["commands"]
            .as_array()
            .is_some_and(|commands| !commands.is_empty());
    if found {
        println!(
            "{}",
            serde_json::to_string_pretty(&schema).expect("serialize schema")
        );
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn test_cmd() -> clap::Command {
        crate::cli::Cli::command()
    }

    #[test]
    fn schema_has_required_top_level_keys() {
        let schema = generate(&test_cmd());
        assert!(schema.get("name").is_some());
        assert!(schema.get("version").is_some());
        assert!(schema.get("global_args").is_some());
        assert!(schema.get("errors").is_some());
        assert!(schema.get("commands").is_some());
        assert_eq!(schema["clispec"], "0.3");
    }

    #[test]
    fn schema_is_valid_json() {
        let schema = generate(&test_cmd());
        let serialized = serde_json::to_string_pretty(&schema).unwrap();
        let _: Value = serde_json::from_str(&serialized).unwrap();
    }

    #[test]
    fn schema_commands_is_array() {
        let schema = generate(&test_cmd());
        assert!(schema["commands"].is_array(), "commands must be an array");
    }

    #[test]
    fn schema_includes_leaf_commands() {
        let schema = generate(&test_cmd());
        let commands = schema["commands"].as_array().unwrap();
        let names: Vec<&str> = commands.iter().filter_map(|c| c["name"].as_str()).collect();
        assert!(names.contains(&"space list"));
        assert!(names.contains(&"page get"));
        assert!(names.contains(&"auth login"));
    }

    #[test]
    fn schema_mutating_markers_present() {
        let schema = generate(&test_cmd());
        let commands = schema["commands"].as_array().unwrap();
        for cmd in commands {
            assert!(
                cmd.get("mutating").is_some(),
                "command {} missing mutating field",
                cmd["name"]
            );
        }
    }

    #[test]
    fn schema_errors_have_exit_codes() {
        let schema = generate(&test_cmd());
        let errors = schema["errors"].as_array().unwrap();
        for err in errors {
            assert!(
                err.get("exit_code").is_some(),
                "error {} missing exit_code",
                err["kind"]
            );
        }
    }

    #[test]
    fn schema_has_no_generated_placeholder_descriptions() {
        let schema = generate(&test_cmd());
        for command in schema["commands"].as_array().unwrap() {
            let description = command["description"].as_str().unwrap_or_default();
            assert!(
                !description.starts_with("Run the ") && !description.trim().is_empty(),
                "command {} has placeholder documentation",
                command["name"]
            );
            for argument in command["args"].as_array().into_iter().flatten() {
                assert!(
                    argument.get("description").is_some(),
                    "argument {} on {} lacks a description",
                    argument["name"],
                    command["name"]
                );
            }
        }
    }

    #[test]
    fn list_schema_uses_only_real_clap_arguments() {
        let schema = generate(&test_cmd());
        for command in schema["commands"].as_array().unwrap() {
            let name = command["name"].as_str().unwrap();
            if !LIST_COMMANDS.contains(&name) {
                continue;
            }
            let argument_names = command["args"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|argument| argument["name"].as_str())
                .collect::<Vec<_>>();
            for required in ["--limit", "--offset", "--fields"] {
                assert!(
                    argument_names.contains(&required),
                    "{name} lacks real {required} support"
                );
            }
        }
    }

    #[test]
    fn delete_and_remove_output_contracts_match_runtime_keys() {
        let schema = generate(&test_cmd());
        let fields = |name: &str| {
            schema["commands"]
                .as_array()
                .unwrap()
                .iter()
                .find(|command| command["name"] == name)
                .unwrap()["output_fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|field| field["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(fields("attachment delete"), ["attachment_id", "deleted"]);
        assert_eq!(fields("comment delete"), ["comment_id", "deleted"]);
        assert_eq!(fields("property delete"), ["key", "deleted"]);
        assert_eq!(fields("label remove"), ["label", "removed"]);
    }

    #[test]
    fn schema_versions_response_contract_and_declares_runtime_errors() {
        let schema = generate(&test_cmd());
        assert_eq!(schema["response_contract"], "1");
        let kinds = schema["errors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|error| error["kind"].as_str())
            .collect::<Vec<_>>();
        assert!(kinds.contains(&"read_only"));
        assert!(kinds.contains(&"network"));
    }

    #[test]
    fn schema_preserves_required_options_and_numeric_argument_types() {
        let schema = generate(&test_cmd());
        let command = |name: &str| {
            schema["commands"]
                .as_array()
                .unwrap()
                .iter()
                .find(|command| command["name"] == name)
                .unwrap()
        };
        let profile_name = command("profile add")["args"]
            .as_array()
            .unwrap()
            .iter()
            .find(|argument| argument["name"] == "--name")
            .unwrap();
        assert_eq!(profile_name["required"], true);

        for argument_name in ["--limit", "--offset"] {
            let argument = command("attachment list")["args"]
                .as_array()
                .unwrap()
                .iter()
                .find(|argument| argument["name"] == argument_name)
                .unwrap();
            assert_eq!(argument["type"], "integer");
        }
    }
}
