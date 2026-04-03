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

    if let Some(help) = arg.get_help().map(|h| h.to_string()) {
        obj.insert("description".into(), json!(help));
    }

    let is_bool = !arg.get_action().takes_values();
    if is_bool {
        obj.insert("type".into(), json!("bool"));
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

    if arg.is_positional() {
        obj.insert("required".into(), json!(arg.is_required_set()));
    }

    if let Some(default) = arg.get_default_values().first() {
        obj.insert("default".into(), json!(default.to_string_lossy()));
    }

    if let Some(short) = arg.get_short() {
        obj.insert("short".into(), json!(format!("-{short}")));
    }

    Value::Object(obj)
}

fn walk_commands(cmd: &clap::Command, prefix: &str, out: &mut serde_json::Map<String, Value>) {
    let global_ids = ["help", "version", "json", "profile"];

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
            let mut entry = serde_json::Map::new();

            if let Some(about) = sub.get_about().map(|a| a.to_string()) {
                entry.insert("description".into(), json!(about));
            }

            let mut args = Vec::new();
            let mut flags = Vec::new();
            for arg in sub.get_arguments() {
                if global_ids.contains(&arg.get_id().as_str()) {
                    continue;
                }
                if arg.is_positional() {
                    args.push(arg_to_json(arg));
                } else {
                    flags.push(arg_to_json(arg));
                }
            }

            if !args.is_empty() {
                entry.insert("args".into(), json!(args));
            }
            if !flags.is_empty() {
                entry.insert("flags".into(), json!(flags));
            }

            out.insert(path, Value::Object(entry));
        }
    }
}

pub fn generate(cmd: &clap::Command) -> Value {
    let mut commands = serde_json::Map::new();
    walk_commands(cmd, "", &mut commands);

    json!({
        "name": "confluence",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Markdown-sync-first Confluence CLI in Rust",
        "usage": "confluence [OPTIONS] <COMMAND> [SUBCOMMAND] [ARGS]",
        "global_flags": {
            "--json": {"type": "bool", "description": "Output as JSON (auto-enabled when stdout is not a terminal)"},
            "--profile": {"type": "string", "description": "Configuration profile name"}
        },
        "exit_codes": {
            "0": "success",
            "1": "general error"
        },
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
        assert!(schema.get("global_flags").is_some());
        assert!(schema.get("exit_codes").is_some());
        assert!(schema.get("commands").is_some());
    }

    #[test]
    fn schema_is_valid_json() {
        let schema = generate(&test_cmd());
        let serialized = serde_json::to_string_pretty(&schema).unwrap();
        let _: Value = serde_json::from_str(&serialized).unwrap();
    }

    #[test]
    fn schema_includes_leaf_commands() {
        let schema = generate(&test_cmd());
        let commands = schema["commands"].as_object().unwrap();
        assert!(commands.contains_key("space list"));
        assert!(commands.contains_key("page get"));
        assert!(commands.contains_key("auth login"));
    }
}
