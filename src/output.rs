use std::fmt::Display;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use comfy_table::{Cell, Table, presets::UTF8_FULL};
use serde::Serialize;

pub fn use_color() -> bool {
    !COLOR_DISABLED.load(Ordering::Relaxed)
        && std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
}

static QUIET: AtomicBool = AtomicBool::new(false);
static COLOR_DISABLED: AtomicBool = AtomicBool::new(false);
static MACHINE_ERRORS: AtomicBool = AtomicBool::new(false);

pub fn configure(quiet: bool, no_color: bool, machine_errors: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
    COLOR_DISABLED.store(no_color, Ordering::Relaxed);
    MACHINE_ERRORS.store(machine_errors, Ordering::Relaxed);
}

pub fn print_message(message: &str) {
    if !QUIET.load(Ordering::Relaxed) {
        eprintln!("{message}");
    }
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

/// Print an error using the same human/machine split as the rest of the suite.
pub fn print_error(kind: &str, message: &str, hint: Option<&str>) -> ! {
    let exit_code = render_error(kind, message, hint, MACHINE_ERRORS.load(Ordering::Relaxed));
    std::process::exit(exit_code)
}

pub fn render_error(kind: &str, message: &str, hint: Option<&str>, machine: bool) -> i32 {
    let mut obj = serde_json::json!({
        "error": {
            "kind": kind,
            "message": message,
        }
    });
    if let Some(hint) = hint {
        obj["error"]["hint"] = serde_json::Value::String(hint.to_string());
    }
    let exit_code = match kind {
        "invalid_input" | "confirmation_required" | "read_only" | "tty_required" => 2,
        "auth" => 3,
        "not_found" => 4,
        "api_error" | "network" => 5,
        "rate_limit" => 6,
        "conflict" => 7,
        _ => 1,
    };
    obj["error"]["exit_code"] = serde_json::json!(exit_code);
    if machine {
        eprintln!("{}", serde_json::to_string(&obj).unwrap_or_default());
    } else {
        eprintln!("error: {message}");
        if let Some(hint) = hint {
            eprintln!("\nTry: {hint}");
        }
    }
    exit_code
}

pub fn render_anyhow(error: &anyhow::Error, machine: bool) -> i32 {
    let message = format!("{error:#}");
    let lower = message.to_ascii_lowercase();
    let (kind, hint) = if lower.contains("requires a terminal")
        || lower.contains("interactive setup")
        || lower.contains("non-interactive")
    {
        (
            "tty_required",
            Some("run the command in a terminal or use its documented non-interactive flags"),
        )
    } else if lower.contains("read-only") || lower.contains("read only") {
        (
            "read_only",
            Some("disable read-only mode for the active profile"),
        )
    } else if lower.contains("not found") || lower.contains("not declared") || lower.contains("404")
    {
        ("not_found", None)
    } else if lower.contains("unauthorized")
        || lower.contains("authentication")
        || lower.contains("credential")
        || lower.contains("401")
        || lower.contains("403")
    {
        (
            "auth",
            Some("run `confluence doctor` to verify the active profile"),
        )
    } else if lower.contains("rate limit") || lower.contains("429") {
        ("rate_limit", Some("wait and retry the same command"))
    } else if lower.contains("conflict")
        || lower.contains("version mismatch")
        || lower.contains("409")
    {
        ("conflict", None)
    } else if lower.contains("http") || lower.contains("request") || lower.contains("network") {
        (
            "api_error",
            Some("run `confluence doctor` to check connectivity"),
        )
    } else {
        ("unexpected_error", None)
    };
    render_error(kind, &message, hint, machine)
}

pub fn machine_readable_errors<I>(args: I, stdout_is_terminal: bool) -> bool
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect::<Vec<_>>();
    let explicit_text = args.windows(2).any(|pair| pair == ["--output", "text"])
        || args
            .iter()
            .any(|arg| arg == "--output=text" || arg == "-otext");
    let explicit_json = args.windows(2).any(|pair| pair == ["--output", "json"])
        || args
            .iter()
            .any(|arg| arg == "--output=json" || arg == "-ojson" || arg == "--json");
    !explicit_text && (explicit_json || !stdout_is_terminal)
}
