use serde_json::{Value, json};

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

    let is_bool = !arg.get_action().takes_values();
    if is_bool {
        obj.insert("type".into(), json!("boolean"));
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

    if arg.is_positional() {
        obj.insert("required".into(), json!(arg.is_required_set()));
    }

    if let Some(default) = arg.get_default_values().first() {
        obj.insert("default".into(), json!(default.to_string_lossy()));
    }

    Value::Object(obj)
}

fn make_extra_arg(name: &str, typ: &str, description: &str) -> Value {
    json!({
        "name": name,
        "type": typ,
        "description": description,
    })
}

fn is_mutating(path: &str) -> bool {
    let mutating_verbs = [
        "create", "update", "delete", "move", "add", "upload", "set", "apply", "pull",
    ];
    let last_word = path.rsplit_once(' ').map(|(_, w)| w).unwrap_or(path);
    mutating_verbs.contains(&last_word)
}

fn output_fields_for(path: &str) -> Vec<Value> {
    let content_fields = || {
        vec![
            json!({"name": "id", "type": "string"}),
            json!({"name": "kind", "type": "string"}),
            json!({"name": "space_key", "type": "string"}),
            json!({"name": "title", "type": "string"}),
            json!({"name": "status", "type": "string"}),
            json!({"name": "version", "type": "integer"}),
            json!({"name": "parent_id", "type": "string"}),
        ]
    };

    match path {
        "page get" | "page list" | "page tree" | "page move" | "page create" | "page update"
        | "blog get" | "blog list" | "blog create" | "blog update" => content_fields(),

        "page delete" | "blog delete" | "attachment delete" | "comment delete"
        | "property delete" | "label remove" => {
            vec![
                json!({"name": "id", "type": "string"}),
                json!({"name": "deleted", "type": "boolean"}),
            ]
        }

        "space list" | "space get" => vec![
            json!({"name": "id", "type": "string"}),
            json!({"name": "key", "type": "string"}),
            json!({"name": "name", "type": "string"}),
            json!({"name": "type", "type": "string"}),
            json!({"name": "homepage_id", "type": "string"}),
        ],

        "search" => vec![
            json!({"name": "id", "type": "string"}),
            json!({"name": "kind", "type": "string"}),
            json!({"name": "space_key", "type": "string"}),
            json!({"name": "title", "type": "string"}),
            json!({"name": "web_url", "type": "string"}),
        ],

        "attachment list" | "attachment upload" => vec![
            json!({"name": "id", "type": "string"}),
            json!({"name": "title", "type": "string"}),
            json!({"name": "media_type", "type": "string"}),
            json!({"name": "file_size", "type": "integer"}),
            json!({"name": "download_url", "type": "string"}),
        ],

        "comment list" | "comment add" | "comment update" => vec![
            json!({"name": "id", "type": "string"}),
            json!({"name": "author", "type": "string"}),
            json!({"name": "created_at", "type": "string"}),
            json!({"name": "body_storage", "type": "string"}),
        ],

        "property list" | "property get" | "property set" => vec![
            json!({"name": "id", "type": "string"}),
            json!({"name": "key", "type": "string"}),
            json!({"name": "version", "type": "integer"}),
            json!({"name": "value", "type": "string"}),
        ],

        "auth status" | "auth login" | "profile add" => vec![
            json!({"name": "name", "type": "string"}),
            json!({"name": "provider", "type": "string"}),
            json!({"name": "base_url", "type": "string"}),
            json!({"name": "api_path", "type": "string"}),
            json!({"name": "read_only", "type": "boolean"}),
        ],

        "plan" | "apply" => vec![
            json!({"name": "action", "type": "string"}),
            json!({"name": "title", "type": "string"}),
            json!({"name": "content_id", "type": "string"}),
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
    let global_ids = ["help", "version", "output", "profile", "yes"];

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
                if global_ids.contains(&arg.get_id().as_str()) {
                    continue;
                }
                args.push(arg_to_json(arg));
            }

            // Inject list-command pagination flags when not already present
            if LIST_COMMANDS.contains(&path.as_str()) {
                let has_limit = args.iter().any(|a| a["name"] == "--limit");
                let has_offset = args.iter().any(|a| a["name"] == "--offset");
                let has_fields = args.iter().any(|a| a["name"] == "--fields");
                if !has_limit {
                    args.push(make_extra_arg(
                        "--limit",
                        "integer",
                        "Maximum number of items to return",
                    ));
                }
                if !has_offset {
                    args.push(make_extra_arg(
                        "--offset",
                        "integer",
                        "Number of items to skip",
                    ));
                }
                if !has_fields {
                    args.push(make_extra_arg(
                        "--fields",
                        "string",
                        "Comma-separated list of fields to include in JSON output",
                    ));
                }
            }

            let mut entry = serde_json::Map::new();
            entry.insert("name".into(), json!(path));

            if let Some(about) = sub.get_about().map(|a| a.to_string()) {
                entry.insert("description".into(), json!(about));
            }

            entry.insert("mutating".into(), json!(is_mutating(&path)));

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

    json!({
        "clispec": "0.2",
        "name": "confluence",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Markdown-sync-first Confluence CLI in Rust",
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
                "name": "--yes",
                "type": "boolean",
                "description": "Skip confirmation prompts for destructive operations"
            }
        ],
        "errors": [
            {
                "kind": "invalid_input",
                "exit_code": 1,
                "retryable": false,
                "description": "The command arguments or input data were invalid."
            },
            {
                "kind": "confirmation_required",
                "exit_code": 2,
                "retryable": false,
                "description": "A destructive operation requires confirmation. Re-run with --yes to confirm."
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
                "kind": "network",
                "exit_code": 5,
                "retryable": true,
                "description": "A network or connectivity error occurred."
            },
            {
                "kind": "conflict",
                "exit_code": 6,
                "retryable": false,
                "description": "A conflict occurred, such as a version mismatch or duplicate resource."
            }
        ],
        "commands": commands
    })
}

pub fn print_schema() {
    use clap::CommandFactory;
    let cmd = super::cli::Cli::command();
    let schema = generate(&cmd);
    println!(
        "{}",
        serde_json::to_string_pretty(&schema).expect("serialize schema")
    );
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
        assert_eq!(schema["clispec"], "0.2");
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
}
