use std::fmt::Display;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use comfy_table::{Cell, Table, presets::UTF8_FULL};
use serde::Serialize;
use serde_json::Value;

pub fn use_color() -> bool {
    !COLOR_DISABLED.load(Ordering::Relaxed)
        && std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
}

static QUIET: AtomicBool = AtomicBool::new(false);
static COLOR_DISABLED: AtomicBool = AtomicBool::new(false);
static MACHINE_ERRORS: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidInput,
    ConfirmationRequired,
    ReadOnly,
    TtyRequired,
    Auth,
    NotFound,
    Api,
    Network,
    RateLimit,
    Conflict,
    Unexpected,
}

impl ErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::ConfirmationRequired => "confirmation_required",
            Self::ReadOnly => "read_only",
            Self::TtyRequired => "tty_required",
            Self::Auth => "auth",
            Self::NotFound => "not_found",
            Self::Api => "api_error",
            Self::Network => "network",
            Self::RateLimit => "rate_limit",
            Self::Conflict => "conflict",
            Self::Unexpected => "unexpected_error",
        }
    }
}

#[derive(Debug)]
pub struct CliError {
    pub kind: ErrorKind,
    pub message: String,
    pub hint: Option<String>,
    pub details: Option<Value>,
}

impl Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

pub fn typed_error(kind: ErrorKind, message: impl Into<String>) -> anyhow::Error {
    CliError {
        kind,
        message: message.into(),
        hint: None,
        details: None,
    }
    .into()
}

pub fn typed_error_with_hint(
    kind: ErrorKind,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> anyhow::Error {
    CliError {
        kind,
        message: message.into(),
        hint: Some(hint.into()),
        details: None,
    }
    .into()
}

pub fn typed_error_with_details(
    kind: ErrorKind,
    message: impl Into<String>,
    hint: Option<String>,
    details: Value,
) -> anyhow::Error {
    CliError {
        kind,
        message: message.into(),
        hint,
        details: Some(details),
    }
    .into()
}

pub fn http_error(status: reqwest::StatusCode, message: impl Into<String>) -> anyhow::Error {
    let kind = match status {
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => ErrorKind::Auth,
        reqwest::StatusCode::NOT_FOUND => ErrorKind::NotFound,
        reqwest::StatusCode::TOO_MANY_REQUESTS => ErrorKind::RateLimit,
        reqwest::StatusCode::CONFLICT => ErrorKind::Conflict,
        _ => ErrorKind::Api,
    };
    typed_error(kind, message)
}

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
    debug_assert!(
        rows.iter().all(|row| row.len() == headers.len()),
        "table rows must have the same number of cells as headers"
    );
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
    render_error_with_details(kind, message, hint, None, machine)
}

fn render_error_with_details(
    kind: &str,
    message: &str,
    hint: Option<&str>,
    details: Option<&Value>,
    machine: bool,
) -> i32 {
    let mut obj = serde_json::json!({
        "error": {
            "kind": kind,
            "message": message,
        }
    });
    if let Some(hint) = hint {
        obj["error"]["hint"] = serde_json::Value::String(hint.to_string());
    }
    if let Some(details) = details {
        obj["error"]["details"] = details.clone();
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
    for cause in error.chain() {
        if let Some(typed) = cause.downcast_ref::<CliError>() {
            return render_error_with_details(
                typed.kind.as_str(),
                &message,
                typed.hint.as_deref(),
                typed.details.as_ref(),
                machine,
            );
        }
        if let Some(request) = cause.downcast_ref::<reqwest::Error>() {
            let kind = if request.is_connect() || request.is_timeout() {
                ErrorKind::Network.as_str()
            } else {
                ErrorKind::Api.as_str()
            };
            return render_error(
                kind,
                &message,
                Some("run `confluence doctor` to check connectivity"),
                machine,
            );
        }
        if let Some(io_error) = cause.downcast_ref::<std::io::Error>()
            && io_error.kind() == std::io::ErrorKind::NotFound
        {
            return render_error(ErrorKind::NotFound.as_str(), &message, None, machine);
        }
    }
    render_error(ErrorKind::Unexpected.as_str(), &message, None, machine)
}

pub fn classify_anyhow(error: &anyhow::Error) -> (ErrorKind, Option<String>) {
    for cause in error.chain() {
        if let Some(typed) = cause.downcast_ref::<CliError>() {
            return (typed.kind, typed.hint.clone());
        }
        if let Some(request) = cause.downcast_ref::<reqwest::Error>() {
            let kind = if request.is_connect() || request.is_timeout() {
                ErrorKind::Network
            } else {
                ErrorKind::Api
            };
            return (
                kind,
                Some("run `confluence doctor` to check connectivity".to_string()),
            );
        }
        if let Some(io_error) = cause.downcast_ref::<std::io::Error>()
            && io_error.kind() == std::io::ErrorKind::NotFound
        {
            return (ErrorKind::NotFound, None);
        }
    }
    (ErrorKind::Unexpected, None)
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
    let explicit_text = args
        .windows(2)
        .any(|pair| pair == ["--output", "text"] || pair == ["-o", "text"])
        || args
            .iter()
            .any(|arg| arg == "--output=text" || arg == "-otext");
    let explicit_json = args
        .windows(2)
        .any(|pair| pair == ["--output", "json"] || pair == ["-o", "json"])
        || args
            .iter()
            .any(|arg| arg == "--output=json" || arg == "-ojson" || arg == "--json");
    !explicit_text && (explicit_json || !stdout_is_terminal)
}
