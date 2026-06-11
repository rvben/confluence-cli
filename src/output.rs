use std::fmt::Display;
use std::io::IsTerminal;

use anyhow::Result;
use comfy_table::{Cell, Table, presets::UTF8_FULL};
use serde::Serialize;

pub fn use_color() -> bool {
    std::io::stderr().is_terminal() && std::env::var("NO_COLOR").is_err()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Auto,
    Text,
    Json,
}

impl OutputFormat {
    pub fn from_arg(arg: OutputArg) -> Self {
        match arg {
            OutputArg::Auto => Self::Auto,
            OutputArg::Text => Self::Text,
            OutputArg::Json => Self::Json,
        }
    }

    /// Returns true when JSON output should be used.
    /// Auto resolves to JSON when stdout is not a terminal.
    pub fn is_json(self) -> bool {
        match self {
            Self::Json => true,
            Self::Text => false,
            Self::Auto => !std::io::stdout().is_terminal(),
        }
    }
}

/// Clap-compatible enum for the --output/-o argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputArg {
    Auto,
    Text,
    Json,
}

pub fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub fn print_items_json<T: Serialize>(
    items: &[T],
    total: usize,
    limit: usize,
    offset: usize,
) -> Result<()> {
    let envelope = serde_json::json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
    });
    println!("{}", serde_json::to_string_pretty(&envelope)?);
    Ok(())
}

pub fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_header(headers.iter().map(|h| Cell::new(*h)).collect::<Vec<_>>());
    for row in rows {
        table.add_row(row.iter().map(Cell::new).collect::<Vec<_>>());
    }
    println!("{table}");
}

pub fn print_list<T: Display>(items: &[T]) {
    for item in items {
        println!("{item}");
    }
}

/// Print a structured error to stderr and exit with the given code.
pub fn print_error(kind: &str, message: &str, hint: Option<&str>) -> ! {
    let mut obj = serde_json::json!({
        "error": {
            "kind": kind,
            "message": message,
        }
    });
    if let Some(hint) = hint {
        obj["error"]["hint"] = serde_json::Value::String(hint.to_string());
    }
    eprintln!("{}", serde_json::to_string(&obj).unwrap_or_default());
    let exit_code = match kind {
        "invalid_input" => 1,
        "confirmation_required" => 2,
        "auth" => 3,
        "not_found" => 4,
        "network" => 5,
        "conflict" => 6,
        _ => 1,
    };
    std::process::exit(exit_code)
}
