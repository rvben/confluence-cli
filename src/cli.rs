use std::collections::BTreeMap;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use serde::Serialize;
use serde_json::{Value, json};
use url::Url;

use crate::config::{self, AppConfig, LoginInput, ResolvedProfile, logout, run_login};
use crate::markdown::markdown_to_storage;
use crate::model::{
    AttachmentInfo, CommentInfo, ContentItem, ContentKind, ContentProperty, CreateContentRequest,
    PlanActionKind, ProviderKind, SearchResult, SpaceSummary, SyncPlan, UpdateContentRequest,
};
use crate::output::{
    OutputArg, OutputFormat, print_items_json, print_json, print_list, print_message, print_table,
};
use crate::provider::{ConfluenceProvider, build_provider};
use crate::sync;

#[derive(Parser, Debug)]
#[command(
    name = "confluence",
    version,
    about = "Fast, safe Confluence workflows for humans and agents",
    after_help = "Get started:\n  confluence init                 Configure an account\n  confluence doctor               Check configuration and access\n  confluence search 'release'     Find content\n  confluence schema --command 'page get'\n                                  Inspect one command for automation",
    arg_required_else_help = true
)]
pub struct Cli {
    /// Output format: auto uses text on a terminal and JSON when piped
    #[arg(
        id = "output_format",
        long = "output",
        short = 'o',
        value_name = "format",
        global = true,
        default_value = "auto"
    )]
    output: OutputArg,
    /// Output JSON (hidden alias for --output json; --output takes precedence if both given)
    #[arg(long, global = true, hide = true)]
    json: bool,
    /// Configuration profile to use instead of the active profile
    #[arg(long, global = true, env = "CONFLUENCE_PROFILE")]
    profile: Option<String>,
    /// Suppress progress and informational messages
    #[arg(long, global = true)]
    quiet: bool,
    /// Disable ANSI color even on a terminal
    #[arg(long, global = true)]
    no_color: bool,
    /// Skip confirmation prompts (for destructive operations)
    #[arg(long, short = 'y', global = true)]
    yes: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Configure an account with the guided login wizard
    Init,
    /// Log in, inspect authentication, log out, or migrate credentials
    #[command(
        after_help = "Examples:\n  confluence auth login\n  confluence auth status --profile work\n  printf '%s' \"$CONFLUENCE_PAT\" | confluence auth login --provider data-center --domain https://wiki.example.com --auth-type bearer --token-stdin --non-interactive"
    )]
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Add, list, select, or remove configuration profiles
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// List spaces or inspect one space
    Space {
        #[command(subcommand)]
        command: SpaceCommand,
    },
    /// Search pages and blog posts with plain text or CQL
    #[command(
        after_help = "Examples:\n  confluence search 'release notes'\n  confluence search 'owner = currentUser()' --cql --space DOCS"
    )]
    Search(SearchArgs),
    /// List, inspect, create, update, move, or delete pages
    #[command(
        after_help = "Page references accept a numeric ID, a Confluence URL, or SPACE:Title.\n\nExamples:\n  confluence page get 'DOCS:Getting Started'\n  confluence page create 'Release Notes' DOCS --body-file release.md\n  confluence page update DOCS:Roadmap --body-file roadmap.md"
    )]
    Page {
        #[command(subcommand)]
        command: PageCommand,
    },
    /// List, inspect, create, update, or delete blog posts
    Blog {
        #[command(subcommand)]
        command: BlogCommand,
    },
    /// Export pages, trees, or spaces to the local Markdown format
    #[command(
        after_help = "Examples:\n  confluence pull page DOCS:Home ./docs/home\n  confluence pull tree DOCS:Home ./docs/site\n  confluence pull space DOCS ./docs"
    )]
    Pull {
        #[command(subcommand)]
        command: PullCommand,
    },
    /// Check configuration, credentials, connectivity, and local sync data
    Doctor(DoctorArgs),
    /// Preview local changes without contacting or modifying Confluence
    #[command(
        after_help = "`plan` reads local Markdown and sidecar state only. Remote drift is checked by `apply`.\n\nExample:\n  confluence plan ./docs --diff"
    )]
    Plan(PlanArgs),
    /// Apply local changes with remote-version drift protection
    #[command(
        after_help = "Run `confluence plan PATH` first. Apply refuses remote-version drift unless --force is passed."
    )]
    Apply(ApplyArgs),
    /// List, download, upload, or delete attachments; uploads can replace existing files
    Attachment {
        #[command(subcommand)]
        command: AttachmentCommand,
    },
    /// List, add, or remove page labels
    Label {
        #[command(subcommand)]
        command: LabelCommand,
    },
    /// List, add, update, or delete page comments
    Comment {
        #[command(subcommand)]
        command: CommentCommand,
    },
    /// List, inspect, set, or delete page content properties
    Property {
        #[command(subcommand)]
        command: PropertyCommand,
    },
    /// Generate a completion script for a supported shell
    #[command(
        after_help = "This command always writes an opaque shell script; --output does not transform it."
    )]
    Completions {
        /// Shell whose completion syntax should be generated
        shell: Shell,
    },
    /// Output JSON schema for agent integration
    #[command(
        after_help = "This command always writes JSON; --output does not transform the schema."
    )]
    Schema {
        /// Return only one complete command path
        #[arg(long)]
        command: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum AuthCommand {
    /// Configure a profile; starts a credential-verifying guided wizard without flags
    Login(Box<AuthLoginArgs>),
    /// Verify the selected profile and display its authentication status
    Status,
    /// Remove the selected profile credential
    Logout,
    /// Move an inline legacy token into the operating-system keychain
    Migrate,
}

#[derive(Subcommand, Debug)]
enum ProfileCommand {
    /// Add a profile using explicit command-line options
    Add(ProfileAddArgs),
    /// List stored profiles and identify the active one
    List {
        /// Maximum number of items to return
        #[arg(long)]
        limit: Option<usize>,
        /// Number of items to skip
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Comma-separated list of fields to include in JSON output
        #[arg(long)]
        fields: Option<String>,
    },
    /// Select the profile used when --profile is omitted
    Use {
        /// Stored profile name
        name: String,
    },
    /// Remove a stored profile and its credential
    Remove {
        /// Stored profile name
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum SpaceCommand {
    /// List spaces visible to the selected profile
    List {
        /// Maximum number of spaces to return
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Number of spaces to skip
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Fetch all spaces, ignoring --limit
        #[arg(long)]
        all: bool,
        /// Comma-separated list of fields to include in JSON output
        #[arg(long)]
        fields: Option<String>,
    },
    /// Get one space by key or ID
    Get {
        /// Space key or numeric ID
        space: String,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ContentTypeFilter {
    Page,
    Blog,
}

#[derive(Args, Debug)]
struct SearchArgs {
    /// Plain-text search phrase, or a full CQL expression with --cql
    query: String,
    /// Interpret QUERY as Confluence Query Language instead of plain text
    #[arg(long)]
    cql: bool,
    /// Restrict results to this space key
    #[arg(long)]
    space: Option<String>,
    /// Restrict results to pages or blog posts
    #[arg(long, value_enum)]
    r#type: Option<ContentTypeFilter>,
    /// Maximum number of results to return
    #[arg(long, default_value_t = 20)]
    limit: usize,
    /// Number of matching results to skip
    #[arg(long, default_value_t = 0)]
    offset: usize,
    /// Comma-separated list of fields to include in JSON output
    #[arg(long)]
    fields: Option<String>,
}

#[derive(Subcommand, Debug)]
enum PageCommand {
    /// List pages in a space
    List {
        /// Space key or numeric ID
        space: String,
        /// Maximum number of pages to return
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// Number of pages to skip
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Fetch all pages, ignoring --limit
        #[arg(long)]
        all: bool,
        /// Comma-separated list of fields to include in JSON output
        #[arg(long)]
        fields: Option<String>,
    },
    /// Get one page by ID, URL, or SPACE:Title
    Get {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
        /// Include storage-format body content in the result
        #[arg(long)]
        show_body: bool,
    },
    /// Show a page and its descendants
    Tree {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
        /// Recurse through all descendants; pass false for direct children only
        #[arg(long, action = ArgAction::Set, default_value_t = true)]
        recursive: bool,
    },
    /// Move a page beneath a new parent
    Move {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
        /// New parent page ID, URL, or SPACE:Title
        parent: String,
    },
    /// Create a page from Markdown or Confluence storage-format content
    Create(PageWriteContentArgs),
    /// Update a page while preserving unspecified content and metadata
    Update(PageUpdateContentArgs),
    /// Permanently delete a page after confirmation
    Delete {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
    },
}

#[derive(Subcommand, Debug)]
enum BlogCommand {
    /// List blog posts in a space
    List {
        /// Space key or numeric ID
        space: String,
        /// Maximum number of blog posts to return
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Number of blog posts to skip
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Fetch all blog posts, ignoring --limit
        #[arg(long)]
        all: bool,
        /// Comma-separated list of fields to include in JSON output
        #[arg(long)]
        fields: Option<String>,
    },
    /// Get one blog post by content ID
    Get {
        /// Blog post content ID
        id: String,
        /// Include storage-format body content in the result
        #[arg(long)]
        show_body: bool,
    },
    /// Create a blog post from Markdown or storage-format content
    Create(BlogWriteContentArgs),
    /// Update a blog post while preserving unspecified content and metadata
    Update(BlogUpdateContentArgs),
    /// Permanently delete a blog post after confirmation
    Delete {
        /// Blog post content ID
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum PullCommand {
    /// Export one page to a Markdown directory
    Page {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
        /// Destination directory
        output: PathBuf,
        /// Replace a dirty or unmanaged destination with the remote snapshot
        #[arg(long)]
        force: bool,
    },
    /// Export a page and all descendants to a Markdown tree
    Tree {
        /// Root page ID, Confluence URL, or SPACE:Title
        reference: String,
        /// Destination directory
        output: PathBuf,
        /// Replace a dirty or unmanaged destination with the remote snapshot
        #[arg(long)]
        force: bool,
    },
    /// Export every page in a space
    Space {
        /// Space key or numeric ID
        space: String,
        /// Destination directory
        output: PathBuf,
        /// Export only recently updated content into a new or empty destination
        #[arg(long, value_parser = validate_since)]
        since: Option<String>,
        /// Replace a dirty destination for a full-space pull; filtered pulls still require an empty destination
        #[arg(long)]
        force: bool,
    },
}

#[derive(Args, Debug)]
struct PlanArgs {
    /// Local Markdown sync directory created by pull
    path: PathBuf,
    /// Allow conversions that may escape or degrade unsupported Confluence XML
    #[arg(long)]
    allow_lossy: bool,
    /// Plan deletion of remote attachments removed from the local tree
    #[arg(long)]
    delete_remote: bool,
    /// Include unified body diffs in text output
    #[arg(long)]
    diff: bool,
}

#[derive(Args, Debug)]
struct ApplyArgs {
    /// Local Markdown sync directory created by pull
    path: PathBuf,
    /// Allow conversions that may escape or degrade unsupported Confluence XML
    #[arg(long)]
    allow_lossy: bool,
    /// Delete remote attachments removed from the local tree; requires confirmation
    #[arg(long)]
    delete_remote: bool,
    /// Overwrite content despite remote-version drift
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct DoctorArgs {
    /// Verify access to this space key or ID
    #[arg(long)]
    space: Option<String>,
    /// Validate this local Markdown sync directory
    #[arg(long)]
    path: Option<PathBuf>,
    /// Check local configuration without contacting Confluence
    #[arg(long)]
    skip_network: bool,
}

#[derive(Subcommand, Debug)]
enum AttachmentCommand {
    /// List attachments on a page
    List {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
        /// Maximum number of items to return
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Number of items to skip
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Comma-separated list of fields to include in JSON output
        #[arg(long)]
        fields: Option<String>,
    },
    /// Download one attachment without overwriting by default
    Download {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
        /// Attachment content ID
        attachment_id: String,
        /// Destination file path
        output: PathBuf,
        /// Replace an existing output file
        #[arg(long)]
        force: bool,
    },
    /// Upload one or more files to a page, optionally replacing same-named attachments
    Upload {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
        /// Local file to upload; repeat for multiple files
        #[arg(long = "file", required = true)]
        files: Vec<PathBuf>,
        /// Version comment recorded by Confluence
        #[arg(long)]
        comment: Option<String>,
        /// Replace same-named remote attachments after confirmation
        #[arg(long)]
        replace: bool,
        /// Mark uploaded attachment versions as minor edits
        #[arg(long)]
        minor_edit: bool,
    },
    /// Permanently delete an attachment after confirmation
    Delete {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
        /// Attachment content ID
        attachment_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum LabelCommand {
    /// List labels on a page
    List {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
        /// Maximum number of items to return
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Number of items to skip
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Comma-separated list of fields to include in JSON output
        #[arg(long)]
        fields: Option<String>,
    },
    /// Add a label to a page
    Add {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
        /// Label name
        label: String,
    },
    /// Remove a label from a page after confirmation
    Remove {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
        /// Label name
        label: String,
    },
}

#[derive(Subcommand, Debug)]
enum CommentCommand {
    /// List footer comments on a page
    List {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
        /// Maximum number of items to return
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Number of items to skip
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Comma-separated list of fields to include in JSON output
        #[arg(long)]
        fields: Option<String>,
    },
    /// Add a footer comment to a page
    Add {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
        #[command(flatten)]
        body: BodyInput,
    },
    /// Replace an existing comment body
    Update {
        /// Comment content ID
        comment_id: String,
        #[command(flatten)]
        body: BodyInput,
    },
    /// Permanently delete a comment after confirmation
    Delete {
        /// Comment content ID
        comment_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum PropertyCommand {
    /// List content properties on a page
    List {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
        /// Maximum number of items to return
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Number of items to skip
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Comma-separated list of fields to include in JSON output
        #[arg(long)]
        fields: Option<String>,
    },
    /// Get one content property
    Get {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
        /// Property key
        key: String,
    },
    /// Create or replace a content property
    Set {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
        /// Property key
        key: String,
        /// JSON value by default; invalid JSON is stored as a string and may be process-visible
        value: String,
        /// Always store VALUE as a string instead of parsing JSON
        #[arg(long)]
        raw: bool,
    },
    /// Permanently delete a content property after confirmation
    Delete {
        /// Page ID, Confluence URL, or SPACE:Title
        reference: String,
        /// Property key
        key: String,
    },
}

#[derive(Args, Debug, Clone)]
struct BodyInput {
    /// Body text; process-visible, so prefer --body-file for sensitive content
    #[arg(long)]
    body: Option<String>,
    /// Read body text from a file, or from stdin when PATH is -
    #[arg(long, conflicts_with = "body")]
    body_file: Option<PathBuf>,
    /// Input representation: Markdown is converted to Confluence storage format
    #[arg(long, value_enum, default_value_t = BodyFormat::Markdown)]
    format: BodyFormat,
    /// Allow raw Confluence XML to be escaped or degraded instead of refusing conversion
    #[arg(long)]
    allow_lossy: bool,
}

#[derive(Args, Debug)]
struct PageWriteContentArgs {
    /// New page title
    title: String,
    /// Destination space key
    space: String,
    /// Parent page ID, URL, or SPACE:Title
    #[arg(long)]
    parent: Option<String>,
    #[command(flatten)]
    body: BodyInput,
    /// Label to add; repeat for multiple labels
    #[arg(long = "label")]
    labels: Vec<String>,
    /// Content property as key=value; repeat for multiple properties
    #[arg(long = "property")]
    properties: Vec<String>,
    /// Confluence content status
    #[arg(long, default_value = "current", value_parser = ["current", "draft"])]
    status: String,
}

#[derive(Args, Debug)]
struct PageUpdateContentArgs {
    /// Page ID, page URL, or SPACE:Title
    reference: String,
    /// Replacement title; preserves the existing title when omitted
    #[arg(long)]
    title: Option<String>,
    /// Replacement parent page ID, URL, or SPACE:Title
    #[arg(long)]
    parent: Option<String>,
    #[command(flatten)]
    body: BodyInput,
    /// Label to merge into existing labels; repeat for multiple labels
    #[arg(long = "label")]
    labels: Vec<String>,
    /// Content property as key=value to merge; repeat for multiple properties
    #[arg(long = "property")]
    properties: Vec<String>,
    /// Replace all labels instead of merging supplied labels
    #[arg(long)]
    replace_labels: bool,
    /// Replace all properties instead of merging supplied properties
    #[arg(long)]
    replace_properties: bool,
    /// Expected current version; defaults to the version fetched before updating
    #[arg(long)]
    version: Option<u64>,
    /// Replacement content status; preserves the existing status when omitted
    #[arg(long, value_parser = ["current", "draft"])]
    status: Option<String>,
}

#[derive(Args, Debug)]
struct BlogWriteContentArgs {
    /// New blog post title
    title: String,
    /// Destination space key
    space: String,
    #[command(flatten)]
    body: BodyInput,
    /// Label to add; repeat for multiple labels
    #[arg(long = "label")]
    labels: Vec<String>,
    /// Content property as key=value; repeat for multiple properties
    #[arg(long = "property")]
    properties: Vec<String>,
    /// Confluence content status
    #[arg(long, default_value = "current", value_parser = ["current", "draft"])]
    status: String,
}

#[derive(Args, Debug)]
struct BlogUpdateContentArgs {
    /// Blog post content ID
    reference: String,
    /// Replacement title; preserves the existing title when omitted
    #[arg(long)]
    title: Option<String>,
    #[command(flatten)]
    body: BodyInput,
    /// Label to merge into existing labels; repeat for multiple labels
    #[arg(long = "label")]
    labels: Vec<String>,
    /// Content property as key=value to merge; repeat for multiple properties
    #[arg(long = "property")]
    properties: Vec<String>,
    /// Replace all labels instead of merging supplied labels
    #[arg(long)]
    replace_labels: bool,
    /// Replace all properties instead of merging supplied properties
    #[arg(long)]
    replace_properties: bool,
    /// Expected current version; defaults to the version fetched before updating
    #[arg(long)]
    version: Option<u64>,
    /// Replacement content status; preserves the existing status when omitted
    #[arg(long, value_parser = ["current", "draft"])]
    status: Option<String>,
}

impl BodyInput {
    fn is_provided(&self) -> bool {
        self.body.is_some() || self.body_file.is_some()
    }
}

impl PageUpdateContentArgs {
    fn has_changes(&self) -> bool {
        self.title.is_some()
            || self.parent.is_some()
            || self.body.is_provided()
            || !self.labels.is_empty()
            || !self.properties.is_empty()
            || self.replace_labels
            || self.replace_properties
            || self.status.is_some()
    }
}

impl BlogUpdateContentArgs {
    fn has_changes(&self) -> bool {
        self.title.is_some()
            || self.body.is_provided()
            || !self.labels.is_empty()
            || !self.properties.is_empty()
            || self.replace_labels
            || self.replace_properties
            || self.status.is_some()
    }
}

fn validate_page_update(args: &PageUpdateContentArgs) -> Result<()> {
    if args.has_changes() {
        return Ok(());
    }
    Err(crate::output::typed_error_with_hint(
        crate::output::ErrorKind::InvalidInput,
        "page update requires at least one requested change",
        "pass --title, --parent, --body, --body-file, --label, --property, --replace-labels, --replace-properties, or --status",
    ))
}

fn validate_blog_update(args: &BlogUpdateContentArgs) -> Result<()> {
    if args.has_changes() {
        return Ok(());
    }
    Err(crate::output::typed_error_with_hint(
        crate::output::ErrorKind::InvalidInput,
        "blog update requires at least one requested change",
        "pass --title, --body, --body-file, --label, --property, --replace-labels, --replace-properties, or --status",
    ))
}

#[derive(Args, Debug, Clone)]
struct AuthLoginArgs {
    /// Profile name; alternatively pass the global --profile option
    #[arg(long, conflicts_with = "profile")]
    name: Option<String>,
    #[command(flatten)]
    input: ProfileInputArgs,
}

#[derive(Args, Debug)]
struct ProfileAddArgs {
    /// New profile name
    #[arg(long, conflicts_with = "profile")]
    name: String,
    #[command(flatten)]
    input: ProfileInputArgs,
}

#[derive(Args, Debug, Clone)]
struct ProfileInputArgs {
    /// Confluence Cloud or self-hosted Data Center
    #[arg(long, value_enum)]
    provider: Option<ProviderArg>,
    /// Confluence site hostname or base URL
    #[arg(long)]
    domain: Option<String>,
    /// REST API path override; normally discovered from the provider
    #[arg(long)]
    api_path: Option<String>,
    /// Authentication scheme: basic for Cloud, bearer for most Data Center PATs
    #[arg(long, value_parser = ["basic", "bearer"])]
    auth_type: Option<String>,
    /// Username or email required by basic authentication
    #[arg(long)]
    username: Option<String>,
    /// Deprecated: use CONFLUENCE_API_TOKEN or --token-stdin to avoid process-list exposure
    #[arg(long, hide = true, conflicts_with = "token_stdin")]
    token: Option<String>,
    /// Read the API token or PAT from stdin
    #[arg(long)]
    token_stdin: bool,
    /// Refuse all write operations through this profile
    #[arg(long)]
    read_only: bool,
    /// Disable prompts and require all necessary options or environment variables
    #[arg(long)]
    non_interactive: bool,
    /// Store the credential in the protected config file instead of the OS keychain
    #[arg(long)]
    insecure_storage: bool,
    /// Atlassian Cloud ID required by scoped API tokens
    #[arg(long, env = "CONFLUENCE_CLOUD_ID")]
    cloud_id: Option<String>,
    /// Cloud token type: scoped or classic
    #[arg(long, env = "CONFLUENCE_TOKEN_KIND", value_parser = ["scoped", "classic"])]
    token_kind: Option<String>,
    /// Recorded token expiry date (YYYY-MM-DD)
    #[arg(long, value_parser = validate_date)]
    expires_at: Option<String>,
}

fn validate_date(value: &str) -> std::result::Result<String, String> {
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map(|_| value.to_string())
        .map_err(|_| "expected a calendar date in YYYY-MM-DD format".to_string())
}

fn validate_since(value: &str) -> std::result::Result<String, String> {
    if chrono::DateTime::parse_from_rfc3339(value).is_ok()
        || chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok()
    {
        Ok(value.to_string())
    } else {
        Err("expected YYYY-MM-DD or an RFC 3339 timestamp".to_string())
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ProviderArg {
    Cloud,
    DataCenter,
}

impl ProviderArg {
    fn into_model(self) -> ProviderKind {
        match self {
            Self::Cloud => ProviderKind::Cloud,
            Self::DataCenter => ProviderKind::DataCenter,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BodyFormat {
    Markdown,
    Storage,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorReport {
    config_path: String,
    config_exists: bool,
    active_profile: Option<String>,
    stored_profiles: usize,
    resolved_profile: Option<ResolvedProfile>,
    checks: Vec<DoctorCheck>,
    summary: DoctorSummary,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorCheck {
    name: String,
    status: DoctorCheckStatus,
    details: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DoctorCheckStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize, Default)]
struct DoctorSummary {
    passed: usize,
    warned: usize,
    failed: usize,
}

pub async fn run() -> Result<()> {
    let raw_args = std::env::args().collect::<Vec<_>>();
    if raw_args.len() == 1 {
        let mut command = Cli::command();
        command.print_long_help()?;
        println!();
        return Ok(());
    }
    let cli = Cli::try_parse().unwrap_or_else(|err| {
        use clap::error::ErrorKind;
        match err.kind() {
            // Let clap handle help and version display normally (exit 0)
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => err.exit(),
            // Keep Clap's full, actionable human diagnostics. Machine consumers
            // receive the same diagnostic in a stable JSON error envelope.
            _ => {
                let machine = crate::output::machine_readable_errors(
                    raw_args.iter().skip(1),
                    std::io::stdout().is_terminal(),
                );
                if machine {
                    let rendered = err.to_string();
                    let message = rendered
                        .trim()
                        .strip_prefix("error: ")
                        .unwrap_or(rendered.trim())
                        .to_string();
                    let exit = crate::output::render_error("invalid_input", &message, None, true);
                    std::process::exit(exit);
                }
                err.exit();
            }
        }
    });
    // --json is a hidden alias for --output json.
    // Explicit --output wins when both flags are given.
    let output = if cli.json && cli.output == OutputArg::Auto {
        OutputFormat::Json
    } else {
        OutputFormat::from_arg(cli.output)
    };
    crate::output::configure(cli.quiet, cli.no_color, output.is_json());
    let yes = cli.yes;

    match cli.command {
        Commands::Init => config::init(output).await,
        Commands::Auth { command } => {
            handle_auth(command, cli.profile.as_deref(), output, yes).await
        }
        Commands::Profile { command } => handle_profile(command, output, yes),
        Commands::Space { command } => {
            let provider = provider_from_profile(cli.profile.as_deref())?;
            handle_space(&*provider, command, output).await
        }
        Commands::Search(args) => {
            let provider = provider_from_profile(cli.profile.as_deref())?;
            handle_search(&*provider, args, output).await
        }
        Commands::Page { command } => {
            if let PageCommand::Update(args) = &command {
                validate_page_update(args)?;
            }
            let provider = provider_from_profile(cli.profile.as_deref())?;
            handle_page(&*provider, command, output, yes).await
        }
        Commands::Blog { command } => {
            if let BlogCommand::Update(args) = &command {
                validate_blog_update(args)?;
            }
            let provider = provider_from_profile(cli.profile.as_deref())?;
            handle_blog(&*provider, command, output, yes).await
        }
        Commands::Pull { command } => {
            let provider = provider_from_profile(cli.profile.as_deref())?;
            handle_pull(&*provider, command, output).await
        }
        Commands::Doctor(args) => handle_doctor(cli.profile.as_deref(), args, output).await,
        Commands::Plan(args) => {
            let show_diff = args.diff;
            let plan =
                sync::plan_path(&args.path, args.allow_lossy, args.delete_remote, show_diff)?;
            render_plan(&plan, output, show_diff)
        }
        Commands::Apply(args) => {
            sync::validate_sync_path(&args.path)?;
            if args.delete_remote {
                confirm_destructive(
                    yes,
                    "Apply remote attachment deletions requested by --delete-remote?",
                )?;
            }
            let provider = provider_from_profile(cli.profile.as_deref())?;
            let plan = sync::apply_path(
                &*provider,
                &args.path,
                args.allow_lossy,
                args.delete_remote,
                args.force,
            )
            .await?;
            render_plan(&plan, output, false)
        }
        Commands::Attachment { command } => {
            let provider = provider_from_profile(cli.profile.as_deref())?;
            handle_attachment(&*provider, command, output, yes).await
        }
        Commands::Label { command } => {
            let provider = provider_from_profile(cli.profile.as_deref())?;
            handle_label(&*provider, command, output, yes).await
        }
        Commands::Comment { command } => {
            let provider = provider_from_profile(cli.profile.as_deref())?;
            handle_comment(&*provider, command, output, yes).await
        }
        Commands::Property { command } => {
            let provider = provider_from_profile(cli.profile.as_deref())?;
            handle_property(&*provider, command, output, yes).await
        }
        Commands::Completions { shell } => write_completions(shell, &mut io::stdout()),
        Commands::Schema { command } => {
            if crate::schema::print_schema(command.as_deref()) {
                Ok(())
            } else {
                Err(crate::output::typed_error(
                    crate::output::ErrorKind::NotFound,
                    format!("command `{}` is not declared", command.unwrap_or_default()),
                ))
            }
        }
    }
}

fn provider_from_profile(profile_override: Option<&str>) -> Result<Box<dyn ConfluenceProvider>> {
    let config = AppConfig::load()?;
    let profile = config.resolved_profile(profile_override)?;
    Ok(build_provider(profile))
}

fn resolved_profile(profile_override: Option<&str>) -> Result<ResolvedProfile> {
    let config = AppConfig::load()?;
    config.resolved_profile(profile_override)
}

fn write_completions<W: Write>(shell: Shell, writer: &mut W) -> Result<()> {
    let mut command = Cli::command();
    let mut buffer = Vec::new();
    generate(shell, &mut command, "confluence", &mut buffer);
    if let Err(error) = writer.write_all(&buffer) {
        if error.kind() == io::ErrorKind::BrokenPipe {
            return Ok(());
        }
        return Err(error).context("failed to write shell completions");
    }
    if let Err(error) = writer.flush() {
        if error.kind() == io::ErrorKind::BrokenPipe {
            return Ok(());
        }
        return Err(error).context("failed to flush shell completions");
    }
    Ok(())
}

async fn handle_auth(
    command: AuthCommand,
    profile_override: Option<&str>,
    output: OutputFormat,
    yes: bool,
) -> Result<()> {
    match command {
        AuthCommand::Login(args) => {
            let args = *args;
            let guided = args.name.is_none()
                && profile_override.is_none()
                && args.input.provider.is_none()
                && args.input.domain.is_none()
                && args.input.api_path.is_none()
                && args.input.auth_type.is_none()
                && args.input.username.is_none()
                && args.input.token.is_none()
                && !args.input.token_stdin
                && !args.input.read_only
                && !args.input.non_interactive
                && !args.input.insecure_storage
                && args.input.cloud_id.is_none()
                && args.input.token_kind.is_none()
                && args.input.expires_at.is_none();
            if guided {
                if output.is_json() || !io::stdin().is_terminal() {
                    return Err(crate::output::typed_error_with_hint(
                        crate::output::ErrorKind::TtyRequired,
                        "guided authentication requires an interactive terminal",
                        "run `confluence init --output json` for setup instructions, or pass explicit options to `confluence auth login --non-interactive`",
                    ));
                }
                return crate::config::init(output).await;
            }
            let name = args.name.or_else(|| profile_override.map(str::to_owned));
            let resolved = run_login(login_input(args.input, name)?)?;
            if output.is_json() {
                print_json(&resolved.redact())?;
            } else {
                print_table(
                    &[
                        "profile",
                        "provider",
                        "base_url",
                        "api_path",
                        "credential_store",
                        "token_kind",
                        "expires_at",
                        "expiration_status",
                        "read_only",
                    ],
                    &[vec![
                        resolved.name,
                        resolved.provider.to_string(),
                        resolved.base_url,
                        resolved.api_path,
                        resolved.credential_store,
                        resolved.token_kind,
                        resolved
                            .expires_at
                            .clone()
                            .unwrap_or_else(|| "-".to_string()),
                        crate::config::expiration_status(resolved.expires_at.as_deref())
                            .to_string(),
                        resolved.read_only.to_string(),
                    ]],
                );
            }
        }
        AuthCommand::Status => {
            let profile = resolved_profile(profile_override)?;
            let provider = build_provider(profile.clone());
            provider.ping().await?;
            let current_user = provider.current_user().await.ok();
            if output.is_json() {
                let mut value = serde_json::to_value(profile.redact())?;
                value["expiration_status"] = json!(crate::config::expiration_status(
                    profile.expires_at.as_deref()
                ));
                value["current_user"] = serde_json::to_value(&current_user)?;
                print_json(&value)?;
            } else {
                let user = current_user
                    .as_ref()
                    .and_then(|user| {
                        user.display_name
                            .as_deref()
                            .or(user.username.as_deref())
                            .or(user.account_id.as_deref())
                    })
                    .unwrap_or("-")
                    .to_string();
                let account_id = current_user
                    .as_ref()
                    .and_then(|user| user.account_id.clone())
                    .unwrap_or_else(|| "-".to_string());
                print_table(
                    &[
                        "profile",
                        "provider",
                        "base_url",
                        "api_path",
                        "credential_store",
                        "token_kind",
                        "expires_at",
                        "expiration_status",
                        "read_only",
                        "status",
                        "user",
                        "account_id",
                    ],
                    &[vec![
                        profile.name,
                        profile.provider.to_string(),
                        profile.base_url,
                        profile.api_path,
                        profile.credential_store,
                        profile.token_kind,
                        profile
                            .expires_at
                            .clone()
                            .unwrap_or_else(|| "-".to_string()),
                        crate::config::expiration_status(profile.expires_at.as_deref()).to_string(),
                        profile.read_only.to_string(),
                        "ok".to_string(),
                        user,
                        account_id,
                    ]],
                );
            }
        }
        AuthCommand::Logout => {
            let config = AppConfig::load()?;
            let name = profile_override
                .map(ToOwned::to_owned)
                .or_else(|| config.active_profile.clone())
                .ok_or_else(|| {
                    crate::output::typed_error_with_hint(
                        crate::output::ErrorKind::Auth,
                        "no active profile configured",
                        "select a stored profile with --profile or run `confluence auth login`",
                    )
                })?;
            if !config.profiles.contains_key(&name) {
                return Err(crate::output::typed_error_with_hint(
                    crate::output::ErrorKind::NotFound,
                    format!("profile `{name}` not found"),
                    "run `confluence profile list` to see configured profiles",
                ));
            }
            confirm_destructive(yes, &format!("Remove credential for profile `{name}`?"))?;
            let name = logout(Some(&name))?;
            if output.is_json() {
                print_json(&json!({ "profile": name, "status": "logged_out" }))?;
            } else {
                print_message(&format!("Logged out profile `{name}`"));
            }
        }
        AuthCommand::Migrate => {
            let profile = resolved_profile(profile_override)?;
            build_provider(profile).ping().await?;
            let name = crate::config::migrate_credential(profile_override)?;
            if output.is_json() {
                print_json(&json!({
                    "profile": name,
                    "migrated": true,
                    "credential_store": "os-keychain"
                }))?;
            } else {
                print_message(&format!(
                    "Migrated profile `{name}` to the operating-system keychain"
                ));
            }
        }
    }
    Ok(())
}

async fn handle_doctor(
    profile_override: Option<&str>,
    args: DoctorArgs,
    output: OutputFormat,
) -> Result<()> {
    let config_path = AppConfig::config_path()?;
    let mut report = DoctorReport {
        config_path: config_path.display().to_string(),
        config_exists: config_path.exists(),
        active_profile: None,
        stored_profiles: 0,
        resolved_profile: None,
        checks: Vec::new(),
        summary: DoctorSummary::default(),
    };
    let mut failure_kind = None;

    if report.config_exists {
        push_doctor_check(
            &mut report,
            "config_file",
            DoctorCheckStatus::Pass,
            format!("config file found at {}", config_path.display()),
        );
    } else {
        push_doctor_check(
            &mut report,
            "config_file",
            DoctorCheckStatus::Warn,
            format!(
                "config file not found at {}; doctor can still use CONFLUENCE_* environment variables",
                config_path.display()
            ),
        );
    }

    let config = match AppConfig::load() {
        Ok(config) => config,
        Err(err) => {
            let (kind, _) = crate::output::classify_anyhow(&err);
            push_doctor_check(
                &mut report,
                "config_load",
                DoctorCheckStatus::Fail,
                err.to_string(),
            );
            finalize_doctor_summary(&mut report);
            render_doctor(&report, output)?;
            return Err(crate::output::typed_error(
                kind,
                format!(
                    "doctor found {} failing check(s): {err:#}",
                    report.summary.failed
                ),
            ));
        }
    };
    report.active_profile = config.active_profile.clone();
    report.stored_profiles = config.profiles.len();
    let stored_profiles = report.stored_profiles;
    push_doctor_check(
        &mut report,
        "profile_store",
        DoctorCheckStatus::Pass,
        format!("{stored_profiles} stored profile(s)"),
    );

    let profile = match config.resolved_profile(profile_override) {
        Ok(profile) => {
            report.resolved_profile = Some(profile.redact());
            push_doctor_check(
                &mut report,
                "profile_resolution",
                DoctorCheckStatus::Pass,
                format!("resolved profile `{}`", profile.name),
            );
            profile
        }
        Err(err) => {
            let (kind, _) = crate::output::classify_anyhow(&err);
            push_doctor_check(
                &mut report,
                "profile_resolution",
                DoctorCheckStatus::Fail,
                err.to_string(),
            );
            finalize_doctor_summary(&mut report);
            render_doctor(&report, output)?;
            return Err(crate::output::typed_error(
                kind,
                format!(
                    "doctor found {} failing check(s): {err:#}",
                    report.summary.failed
                ),
            ));
        }
    };

    match Url::parse(&profile.base_url) {
        Ok(url) => push_doctor_check(
            &mut report,
            "base_url",
            DoctorCheckStatus::Pass,
            format!("using host {}", url.host_str().unwrap_or_default()),
        ),
        Err(err) => {
            failure_kind.get_or_insert(crate::output::ErrorKind::InvalidInput);
            push_doctor_check(
                &mut report,
                "base_url",
                DoctorCheckStatus::Fail,
                format!("invalid base URL `{}`: {err}", profile.base_url),
            )
        }
    }

    let (auth_status, auth_details) = doctor_auth_check(&profile);
    if auth_status == DoctorCheckStatus::Fail {
        failure_kind.get_or_insert(crate::output::ErrorKind::Auth);
    }
    push_doctor_check(&mut report, "auth", auth_status, auth_details);

    let expiration = crate::config::expiration_status(profile.expires_at.as_deref());
    let expiration_check = match expiration {
        "valid" => (
            DoctorCheckStatus::Pass,
            format!(
                "token expires {}",
                profile.expires_at.as_deref().unwrap_or_default()
            ),
        ),
        "expiring-soon" => (
            DoctorCheckStatus::Warn,
            format!(
                "token expires soon ({})",
                profile.expires_at.as_deref().unwrap_or_default()
            ),
        ),
        "expired" => (
            DoctorCheckStatus::Fail,
            format!(
                "token expired {}; run `confluence auth login`",
                profile.expires_at.as_deref().unwrap_or_default()
            ),
        ),
        _ => (
            DoctorCheckStatus::Warn,
            "token expiry is not recorded; renew with `confluence auth login` to enable reminders"
                .to_string(),
        ),
    };
    if expiration_check.0 == DoctorCheckStatus::Fail {
        failure_kind.get_or_insert(crate::output::ErrorKind::Auth);
    }
    push_doctor_check(
        &mut report,
        "token_expiration",
        expiration_check.0,
        expiration_check.1,
    );

    if args.skip_network {
        push_doctor_check(
            &mut report,
            "connectivity",
            DoctorCheckStatus::Warn,
            "network checks skipped".to_string(),
        );
    } else {
        let provider = build_provider(profile.clone());
        match provider.ping().await {
            Ok(()) => push_doctor_check(
                &mut report,
                "connectivity",
                DoctorCheckStatus::Pass,
                format!("{} API reachable at {}", profile.provider, profile.base_url),
            ),
            Err(err) => {
                let (kind, _) = crate::output::classify_anyhow(&err);
                failure_kind.get_or_insert(kind);
                push_doctor_check(
                    &mut report,
                    "connectivity",
                    DoctorCheckStatus::Fail,
                    err.to_string(),
                )
            }
        }

        if let Some(space) = args.space.as_deref() {
            let provider = build_provider(profile.clone());
            match provider.get_space(space).await {
                Ok(found) => push_doctor_check(
                    &mut report,
                    "space_access",
                    DoctorCheckStatus::Pass,
                    format!("resolved space `{}` ({})", found.key, found.name),
                ),
                Err(err) => {
                    let (kind, _) = crate::output::classify_anyhow(&err);
                    failure_kind.get_or_insert(kind);
                    push_doctor_check(
                        &mut report,
                        "space_access",
                        DoctorCheckStatus::Fail,
                        format!("failed to access space `{space}`: {err}"),
                    )
                }
            }
        }
    }

    if let Some(path) = args.path.as_deref() {
        if !path.exists() {
            failure_kind.get_or_insert(crate::output::ErrorKind::NotFound);
            push_doctor_check(
                &mut report,
                "sync_path",
                DoctorCheckStatus::Fail,
                format!("path `{}` does not exist", path.display()),
            );
        } else {
            match sync::plan_path(path, false, false, false) {
                Ok(plan) => push_doctor_check(
                    &mut report,
                    "sync_path",
                    DoctorCheckStatus::Pass,
                    format!(
                        "local sync tree parsed successfully ({} planned item(s))",
                        plan.items.len()
                    ),
                ),
                Err(err) => {
                    let (kind, _) = crate::output::classify_anyhow(&err);
                    failure_kind.get_or_insert(kind);
                    push_doctor_check(
                        &mut report,
                        "sync_path",
                        DoctorCheckStatus::Fail,
                        format!("failed to inspect `{}`: {err}", path.display()),
                    )
                }
            }
        }
    }

    finalize_doctor_summary(&mut report);
    render_doctor(&report, output)?;
    if report.summary.failed > 0 {
        return Err(crate::output::typed_error(
            failure_kind.unwrap_or(crate::output::ErrorKind::Unexpected),
            format!("doctor found {} failing check(s)", report.summary.failed),
        ));
    }
    Ok(())
}

fn handle_profile(command: ProfileCommand, output: OutputFormat, yes: bool) -> Result<()> {
    match command {
        ProfileCommand::Add(args) => {
            let resolved = run_login(login_input(args.input, Some(args.name))?)?;
            if output.is_json() {
                print_json(&resolved.redact())?;
            } else {
                print_message(&format!("Stored profile `{}`", resolved.name));
            }
        }
        ProfileCommand::List {
            limit,
            offset,
            fields,
        } => {
            let config = AppConfig::load()?;
            if output.is_json() {
                let items: Vec<Value> = config
                    .profiles
                    .iter()
                    .map(|(name, profile)| {
                        json!({
                            "name": name,
                            "provider": profile.provider.to_string(),
                            "base_url": profile.base_url,
                            "api_path": profile.api_path,
                            "credential_store": profile.credential_store.as_deref().unwrap_or("legacy-config"),
                            "token_kind": profile.token_kind.as_deref().unwrap_or("classic"),
                            "expires_at": profile.expires_at,
                            "read_only": profile.read_only,
                            "active": config.active_profile.as_deref() == Some(name.as_str()),
                        })
                    })
                    .collect();
                let total = items.len();
                let start = offset.min(total);
                let mut paged: Vec<Value> = items.into_iter().skip(start).collect();
                if let Some(lim) = limit {
                    paged.truncate(lim);
                }
                let shown = paged.len();
                let filtered = filter_fields_vec(paged, fields.as_deref());
                print_items_json(&filtered, total, limit.unwrap_or(shown), offset)?;
            } else {
                let mut visible = config.profiles.iter().skip(offset).collect::<Vec<_>>();
                if let Some(limit) = limit {
                    visible.truncate(limit);
                }
                let rows = visible
                    .into_iter()
                    .map(|(name, profile)| {
                        vec![
                            name.clone(),
                            profile.provider.to_string(),
                            profile.base_url.clone(),
                            profile.api_path.clone(),
                            profile
                                .credential_store
                                .clone()
                                .unwrap_or_else(|| "legacy-config".to_string()),
                            profile
                                .token_kind
                                .clone()
                                .unwrap_or_else(|| "classic".to_string()),
                            profile
                                .expires_at
                                .clone()
                                .unwrap_or_else(|| "-".to_string()),
                            profile.read_only.to_string(),
                            (config.active_profile.as_deref() == Some(name.as_str())).to_string(),
                        ]
                    })
                    .collect::<Vec<_>>();
                print_table(
                    &[
                        "name",
                        "provider",
                        "base_url",
                        "api_path",
                        "credential_store",
                        "token_kind",
                        "expires_at",
                        "read_only",
                        "active",
                    ],
                    &rows,
                );
            }
        }
        ProfileCommand::Use { name } => {
            let mut config = AppConfig::load()?;
            config.set_active_profile(&name)?;
            config.save()?;
            print_status(
                output,
                json!({ "profile": name, "active": true }),
                &format!("Active profile set to `{name}`"),
            )?;
        }
        ProfileCommand::Remove { name } => {
            let mut config = AppConfig::load()?;
            if !config.profiles.contains_key(&name) {
                return Err(crate::output::typed_error_with_hint(
                    crate::output::ErrorKind::NotFound,
                    format!("profile `{name}` not found"),
                    "run `confluence profile list` to see configured profiles",
                ));
            }
            confirm_destructive(yes, &format!("Remove profile `{name}` and its credential?"))?;
            let original = config.clone();
            let uses_keyring = config
                .profiles
                .get(&name)
                .is_some_and(|profile| profile.credential_store.as_deref() == Some("keyring"));
            config.remove_profile(&name)?;
            config.save()?;
            if uses_keyring && let Err(error) = crate::credentials::delete(&name) {
                original.save().with_context(|| {
                    format!("credential cleanup failed and profile `{name}` could not be restored")
                })?;
                return Err(error).with_context(|| {
                    format!("credential cleanup failed; profile `{name}` was restored")
                });
            }
            print_status(
                output,
                json!({ "profile": name, "removed": true }),
                &format!("Removed profile `{name}`"),
            )?;
        }
    }
    Ok(())
}

fn login_input(args: ProfileInputArgs, name: Option<String>) -> Result<LoginInput> {
    let token = if args.token_stdin {
        let mut value = String::new();
        io::stdin()
            .read_to_string(&mut value)
            .context("failed to read token from stdin")?;
        let value = value.trim_end_matches(['\r', '\n']).to_string();
        if value.is_empty() {
            return Err(crate::output::typed_error(
                crate::output::ErrorKind::InvalidInput,
                "token read from stdin is empty",
            ));
        }
        Some(value)
    } else {
        args.token
    };
    Ok(LoginInput {
        profile: name,
        provider: args.provider.map(ProviderArg::into_model),
        domain: args.domain,
        api_path: args.api_path,
        auth_type: args.auth_type,
        username: args.username,
        token,
        read_only: Some(args.read_only),
        non_interactive: args.non_interactive,
        insecure_storage: args.insecure_storage,
        cloud_id: args.cloud_id,
        token_kind: args.token_kind,
        expires_at: args.expires_at,
    })
}

async fn handle_space(
    provider: &dyn ConfluenceProvider,
    command: SpaceCommand,
    output: OutputFormat,
) -> Result<()> {
    match command {
        SpaceCommand::List {
            limit,
            offset,
            all,
            fields,
        } => {
            // Providers currently expose an all-or-prefix API. Fetch the complete
            // set so offset pages and `total` remain truthful.
            let mut spaces = provider.list_spaces(usize::MAX).await?;
            let total = spaces.len();
            if !all {
                spaces = spaces.into_iter().skip(offset).take(limit).collect();
            }
            let reported_limit = if all { total } else { limit };
            let reported_offset = if all { 0 } else { offset };
            render_spaces_list(
                &spaces,
                output,
                total,
                reported_limit,
                reported_offset,
                fields.as_deref(),
            )?;
            if !all && !output.is_json() && total > offset + spaces.len() {
                print_message(&format!(
                    "Showing {} of {total} spaces from offset {offset} - use --all to fetch all",
                    spaces.len()
                ));
            }
        }
        SpaceCommand::Get { space } => {
            let item = provider.get_space(&space).await?;
            render_spaces(&[item], output)?;
        }
    }
    Ok(())
}

async fn handle_search(
    provider: &dyn ConfluenceProvider,
    args: SearchArgs,
    output: OutputFormat,
) -> Result<()> {
    // When --space or --type are provided alongside a plain-text query, we build
    // CQL ourselves so the filters can be combined correctly.
    let needs_cql_build = args.space.is_some() || args.r#type.is_some();
    let (query, is_cql) = if needs_cql_build {
        let text_clause = if args.cql {
            args.query.clone()
        } else {
            format!(
                "text ~ \"{}\"",
                crate::provider::escape_cql_literal(&args.query)
            )
        };
        let mut clauses = vec![text_clause];
        if let Some(space) = &args.space {
            clauses.push(format!(
                "space = \"{}\"",
                crate::provider::escape_cql_literal(space)
            ));
        }
        if let Some(content_type) = args.r#type {
            let type_str = match content_type {
                ContentTypeFilter::Page => "page",
                ContentTypeFilter::Blog => "blogpost",
            };
            clauses.push(format!("type = \"{type_str}\""));
        }
        (clauses.join(" and "), true)
    } else {
        (args.query, args.cql)
    };
    let results = provider
        .search(&query, is_cql, args.limit, args.offset)
        .await?;
    render_search_results_list(
        &results.items,
        output,
        results.total,
        args.limit,
        args.offset,
        args.fields.as_deref(),
    )?;
    Ok(())
}

async fn handle_page(
    provider: &dyn ConfluenceProvider,
    command: PageCommand,
    output: OutputFormat,
    yes: bool,
) -> Result<()> {
    match command {
        PageCommand::List {
            space,
            limit,
            offset,
            all,
            fields,
        } => {
            let mut pages = provider
                .list_space_content(ContentKind::Page, &space)
                .await?;
            let total = pages.len();
            if !all {
                let start = offset.min(pages.len());
                pages = pages.into_iter().skip(start).collect();
                pages.truncate(limit);
            }
            let shown = pages.len();
            let (reported_limit, reported_offset) = list_window_metadata(all, total, limit, offset);
            render_content_items_list(
                &pages,
                output,
                false,
                total,
                reported_limit,
                reported_offset,
                fields.as_deref(),
            )?;
            if !all && !output.is_json() && total > shown + offset {
                print_message(&format!(
                    "Showing first {limit} of {total} pages - use --all to fetch all"
                ));
            }
        }
        PageCommand::Get {
            reference,
            show_body,
        } => {
            let id = provider.resolve_page_ref(&reference).await?;
            let page = provider
                .get_content(ContentKind::Page, &id, show_body)
                .await?;
            render_content_items(&[page], output, show_body)?;
        }
        PageCommand::Tree {
            reference,
            recursive,
        } => {
            let id = provider.resolve_page_ref(&reference).await?;
            let root = provider.get_content(ContentKind::Page, &id, false).await?;
            let mut items = vec![root];
            items.extend(provider.list_children(&id, recursive).await?);
            render_content_items(&items, output, false)?;
        }
        PageCommand::Move { reference, parent } => {
            let id = provider.resolve_page_ref(&reference).await?;
            let parent_id = provider.resolve_page_ref(&parent).await?;
            let current = provider.get_content(ContentKind::Page, &id, true).await?;
            let updated = provider
                .update_content(&UpdateContentRequest {
                    id: id.clone(),
                    kind: ContentKind::Page,
                    title: current.title,
                    parent_id: Some(parent_id),
                    body_storage: current.body_storage.unwrap_or_default(),
                    version: current.version.ok_or_else(|| {
                        crate::output::typed_error(
                            crate::output::ErrorKind::Api,
                            "page version unavailable",
                        )
                    })?,
                    message: Some("Moved via confluence-cli".to_string()),
                    status: current.status,
                    labels: current.labels,
                    properties: current.properties,
                })
                .await?;
            render_content_items(&[updated], output, false)?;
        }
        PageCommand::Create(args) => {
            let parent_id = if let Some(parent) = args.parent.as_deref() {
                Some(provider.resolve_page_ref(parent).await?)
            } else {
                None
            };
            let body_storage = read_body_storage(&args.body)?;
            let created = provider
                .create_content(&CreateContentRequest {
                    kind: ContentKind::Page,
                    title: args.title,
                    space: args.space,
                    parent_id,
                    body_storage,
                    status: args.status,
                    labels: args.labels,
                    properties: parse_properties(&args.properties)?,
                })
                .await?;
            render_content_items(&[created], output, true)?;
        }
        PageCommand::Update(args) => {
            validate_page_update(&args)?;
            let id = provider.resolve_page_ref(&args.reference).await?;
            let current = provider.get_content(ContentKind::Page, &id, true).await?;
            let body_storage = if args.body.is_provided() {
                read_body_storage(&args.body)?
            } else {
                current.body_storage.clone().unwrap_or_default()
            };
            let labels = if args.replace_labels {
                args.labels
            } else if args.labels.is_empty() {
                current.labels.clone()
            } else {
                merge_strings(current.labels.clone(), args.labels)
            };
            let properties = if args.replace_properties {
                parse_properties(&args.properties)?
            } else if args.properties.is_empty() {
                current.properties.clone()
            } else {
                merge_properties(
                    current.properties.clone(),
                    parse_properties(&args.properties)?,
                )
            };
            let parent_id = if let Some(parent) = args.parent.as_deref() {
                Some(provider.resolve_page_ref(parent).await?)
            } else {
                current.parent_id.clone()
            };
            let updated = provider
                .update_content(&UpdateContentRequest {
                    id,
                    kind: ContentKind::Page,
                    title: args.title.unwrap_or(current.title),
                    parent_id,
                    body_storage,
                    version: args.version.or(current.version).ok_or_else(|| {
                        crate::output::typed_error(
                            crate::output::ErrorKind::Api,
                            "page version unavailable",
                        )
                    })?,
                    message: Some("Updated via confluence-cli".to_string()),
                    status: args.status.unwrap_or(current.status),
                    labels,
                    properties,
                })
                .await?;
            render_content_items(&[updated], output, true)?;
        }
        PageCommand::Delete { reference } => {
            let id = provider.resolve_page_ref(&reference).await?;
            confirm_destructive(yes, &format!("Delete page {id}?"))?;
            provider.delete_content(ContentKind::Page, &id).await?;
            print_status(output, json!({ "id": id, "deleted": true }), "Deleted page")?;
        }
    }
    Ok(())
}

async fn handle_blog(
    provider: &dyn ConfluenceProvider,
    command: BlogCommand,
    output: OutputFormat,
    yes: bool,
) -> Result<()> {
    match command {
        BlogCommand::List {
            space,
            limit,
            offset,
            all,
            fields,
        } => {
            let mut posts = provider
                .list_space_content(ContentKind::BlogPost, &space)
                .await?;
            let total = posts.len();
            if !all {
                let start = offset.min(posts.len());
                posts = posts.into_iter().skip(start).collect();
                posts.truncate(limit);
            }
            let shown = posts.len();
            let (reported_limit, reported_offset) = list_window_metadata(all, total, limit, offset);
            render_content_items_list(
                &posts,
                output,
                false,
                total,
                reported_limit,
                reported_offset,
                fields.as_deref(),
            )?;
            if !all && !output.is_json() && total > shown + offset {
                print_message(&format!(
                    "Showing first {limit} of {total} blog posts - use --all to fetch all"
                ));
            }
        }
        BlogCommand::Get { id, show_body } => {
            let item = provider
                .get_content(ContentKind::BlogPost, &id, show_body)
                .await?;
            render_content_items(&[item], output, show_body)?;
        }
        BlogCommand::Create(args) => {
            let body_storage = read_body_storage(&args.body)?;
            let created = provider
                .create_content(&CreateContentRequest {
                    kind: ContentKind::BlogPost,
                    title: args.title,
                    space: args.space,
                    parent_id: None,
                    body_storage,
                    status: args.status,
                    labels: args.labels,
                    properties: parse_properties(&args.properties)?,
                })
                .await?;
            render_content_items(&[created], output, true)?;
        }
        BlogCommand::Update(args) => {
            validate_blog_update(&args)?;
            let current = provider
                .get_content(ContentKind::BlogPost, &args.reference, true)
                .await?;
            let body_storage = if args.body.is_provided() {
                read_body_storage(&args.body)?
            } else {
                current.body_storage.clone().unwrap_or_default()
            };
            let labels = if args.replace_labels {
                args.labels
            } else if args.labels.is_empty() {
                current.labels.clone()
            } else {
                merge_strings(current.labels.clone(), args.labels)
            };
            let properties = if args.replace_properties {
                parse_properties(&args.properties)?
            } else if args.properties.is_empty() {
                current.properties.clone()
            } else {
                merge_properties(
                    current.properties.clone(),
                    parse_properties(&args.properties)?,
                )
            };
            let updated = provider
                .update_content(&UpdateContentRequest {
                    id: args.reference,
                    kind: ContentKind::BlogPost,
                    title: args.title.unwrap_or(current.title),
                    parent_id: None,
                    body_storage,
                    version: args.version.or(current.version).ok_or_else(|| {
                        crate::output::typed_error(
                            crate::output::ErrorKind::Api,
                            "blog version unavailable",
                        )
                    })?,
                    message: Some("Updated via confluence-cli".to_string()),
                    status: args.status.unwrap_or(current.status),
                    labels,
                    properties,
                })
                .await?;
            render_content_items(&[updated], output, true)?;
        }
        BlogCommand::Delete { id } => {
            confirm_destructive(yes, &format!("Delete blog post {id}?"))?;
            provider.delete_content(ContentKind::BlogPost, &id).await?;
            print_status(
                output,
                json!({ "id": id, "deleted": true }),
                "Deleted blog post",
            )?;
        }
    }
    Ok(())
}

async fn handle_pull(
    provider: &dyn ConfluenceProvider,
    command: PullCommand,
    output: OutputFormat,
) -> Result<()> {
    let written = match command {
        PullCommand::Page {
            reference,
            output,
            force,
        } => sync::pull_page(provider, &reference, &output, false, force).await?,
        PullCommand::Tree {
            reference,
            output,
            force,
        } => sync::pull_page(provider, &reference, &output, true, force).await?,
        PullCommand::Space {
            space,
            output,
            since,
            force,
        } => {
            if let Some(since) = since {
                sync::pull_space_since(provider, &space, &output, &since, force).await?
            } else {
                sync::pull_space(provider, &space, &output, force).await?
            }
        }
    };
    if output.is_json() {
        let items = written
            .iter()
            .map(|path| json!({ "path": path }))
            .collect::<Vec<_>>();
        print_items_json(&items, items.len(), items.len(), 0)?;
    } else {
        let items = written
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        print_list(&items);
    }
    Ok(())
}

async fn handle_attachment(
    provider: &dyn ConfluenceProvider,
    command: AttachmentCommand,
    output: OutputFormat,
    yes: bool,
) -> Result<()> {
    match command {
        AttachmentCommand::List {
            reference,
            limit,
            offset,
            fields,
        } => {
            let id = provider.resolve_page_ref(&reference).await?;
            let attachments = provider.list_attachments(&id).await?;
            let (attachments, total) = paginate(attachments, limit, offset);
            render_attachments_list(
                &attachments,
                output,
                total,
                limit,
                offset,
                fields.as_deref(),
            )?;
        }
        AttachmentCommand::Download {
            reference,
            attachment_id,
            output: path,
            force,
        } => {
            if path.exists() && !force {
                crate::output::print_error(
                    "conflict",
                    &format!("output file `{}` already exists", path.display()),
                    Some("pass --force to replace it"),
                );
            }
            let id = provider.resolve_page_ref(&reference).await?;
            let bytes = provider.download_attachment(&id, &attachment_id).await?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, bytes)?;
            print_status(
                output,
                json!({ "path": path, "downloaded": true }),
                &format!("Downloaded to {}", path.display()),
            )?;
        }
        AttachmentCommand::Upload {
            reference,
            files,
            comment,
            replace,
            minor_edit,
        } => {
            for file in &files {
                let metadata = fs::metadata(file)
                    .with_context(|| format!("cannot read upload file `{}`", file.display()))?;
                if !metadata.is_file() {
                    return Err(crate::output::typed_error(
                        crate::output::ErrorKind::InvalidInput,
                        format!("upload path `{}` is not a regular file", file.display()),
                    ));
                }
            }
            if replace {
                confirm_destructive(
                    yes,
                    "Replace same-named remote attachments requested by --replace?",
                )?;
            }
            let id = provider.resolve_page_ref(&reference).await?;
            let upload_count = files.len();
            let mut uploaded = Vec::new();
            for file in files {
                match provider
                    .upload_attachment(&id, &file, comment.as_deref(), replace, minor_edit)
                    .await
                {
                    Ok(item) => uploaded.push(item),
                    Err(error) => {
                        let completed_names = uploaded
                            .iter()
                            .map(|item: &AttachmentInfo| item.title.as_str())
                            .collect::<Vec<_>>();
                        let (kind, hint) = crate::output::classify_anyhow(&error);
                        return Err(crate::output::typed_error_with_details(
                            kind,
                            format!(
                                "attachment upload failed for `{}` after {}/{} completed: {error:#}",
                                file.display(),
                                uploaded.len(),
                                upload_count,
                            ),
                            hint,
                            json!({
                                "operation": "attachment_upload",
                                "failed_path": file,
                                "completed_count": uploaded.len(),
                                "requested_count": upload_count,
                                "completed_items": completed_names,
                            }),
                        ));
                    }
                }
            }
            render_attachments(&uploaded, output)?;
        }
        AttachmentCommand::Delete {
            reference,
            attachment_id,
        } => {
            let id = provider.resolve_page_ref(&reference).await?;
            confirm_destructive(yes, &format!("Delete attachment {attachment_id}?"))?;
            provider.delete_attachment(&id, &attachment_id).await?;
            print_status(
                output,
                json!({ "deleted": true, "attachment_id": attachment_id }),
                "Deleted attachment",
            )?;
        }
    }
    Ok(())
}

async fn handle_label(
    provider: &dyn ConfluenceProvider,
    command: LabelCommand,
    output: OutputFormat,
    yes: bool,
) -> Result<()> {
    match command {
        LabelCommand::List {
            reference,
            limit,
            offset,
            fields,
        } => {
            let id = provider.resolve_page_ref(&reference).await?;
            let labels = provider.list_labels(&id).await?;
            let (labels, total) = paginate(labels, limit, offset);
            if output.is_json() {
                let items: Vec<Value> = labels.iter().map(|l| json!({"label": l})).collect();
                let filtered = filter_fields_vec(items, fields.as_deref());
                print_items_json(&filtered, total, limit, offset)?;
            } else {
                print_list(&labels);
            }
        }
        LabelCommand::Add { reference, label } => {
            let id = provider.resolve_page_ref(&reference).await?;
            provider.add_label(&id, &label).await?;
            print_status(
                output,
                json!({ "label": label, "added": true }),
                "Added label",
            )?;
        }
        LabelCommand::Remove { reference, label } => {
            let id = provider.resolve_page_ref(&reference).await?;
            confirm_destructive(yes, &format!("Remove label `{label}` from page {id}?"))?;
            provider.remove_label(&id, &label).await?;
            print_status(
                output,
                json!({ "label": label, "removed": true }),
                "Removed label",
            )?;
        }
    }
    Ok(())
}

async fn handle_comment(
    provider: &dyn ConfluenceProvider,
    command: CommentCommand,
    output: OutputFormat,
    yes: bool,
) -> Result<()> {
    match command {
        CommentCommand::List {
            reference,
            limit,
            offset,
            fields,
        } => {
            let id = provider.resolve_page_ref(&reference).await?;
            let comments = provider.list_comments(&id).await?;
            let (comments, total) = paginate(comments, limit, offset);
            render_comments_list(&comments, output, total, limit, offset, fields.as_deref())?;
        }
        CommentCommand::Add { reference, body } => {
            let id = provider.resolve_page_ref(&reference).await?;
            let storage = read_body_storage(&body)?;
            let comment = provider.add_comment(&id, &storage).await?;
            render_comments(&[comment], output)?;
        }
        CommentCommand::Update { comment_id, body } => {
            let storage = read_body_storage(&body)?;
            let comment = provider.update_comment(&comment_id, &storage).await?;
            render_comments(&[comment], output)?;
        }
        CommentCommand::Delete { comment_id } => {
            confirm_destructive(yes, &format!("Delete comment {comment_id}?"))?;
            provider.delete_comment(&comment_id).await?;
            print_status(
                output,
                json!({ "comment_id": comment_id, "deleted": true }),
                "Deleted comment",
            )?;
        }
    }
    Ok(())
}

async fn handle_property(
    provider: &dyn ConfluenceProvider,
    command: PropertyCommand,
    output: OutputFormat,
    yes: bool,
) -> Result<()> {
    match command {
        PropertyCommand::List {
            reference,
            limit,
            offset,
            fields,
        } => {
            let id = provider.resolve_page_ref(&reference).await?;
            let properties = provider.list_properties(&id).await?;
            let (properties, total) = paginate(properties, limit, offset);
            render_properties_list(&properties, output, total, limit, offset, fields.as_deref())?;
        }
        PropertyCommand::Get { reference, key } => {
            let id = provider.resolve_page_ref(&reference).await?;
            let property = provider.get_property(&id, &key).await?;
            match property {
                Some(property) => render_properties(&[property], output)?,
                None => {
                    return Err(crate::output::typed_error(
                        crate::output::ErrorKind::NotFound,
                        format!("property `{key}` not found"),
                    ));
                }
            }
        }
        PropertyCommand::Set {
            reference,
            key,
            value,
            raw,
        } => {
            let id = provider.resolve_page_ref(&reference).await?;
            let value = if raw {
                Value::String(value)
            } else {
                serde_json::from_str(&value).unwrap_or(Value::String(value))
            };
            let property = provider.set_property(&id, &key, value).await?;
            render_properties(&[property], output)?;
        }
        PropertyCommand::Delete { reference, key } => {
            let id = provider.resolve_page_ref(&reference).await?;
            confirm_destructive(yes, &format!("Delete property `{key}` from page {id}?"))?;
            provider.delete_property(&id, &key).await?;
            print_status(
                output,
                json!({ "key": key, "deleted": true }),
                "Deleted property",
            )?;
        }
    }
    Ok(())
}

fn read_body_storage(input: &BodyInput) -> Result<String> {
    let raw = read_body_text(input)?;
    match input.format {
        BodyFormat::Markdown => Ok(markdown_to_storage(&raw, input.allow_lossy)?.storage),
        BodyFormat::Storage => Ok(raw),
    }
}

fn read_body_text(input: &BodyInput) -> Result<String> {
    if let Some(body) = &input.body {
        return Ok(body.clone());
    }
    if let Some(path) = &input.body_file {
        if path == Path::new("-") {
            let mut buffer = String::new();
            io::stdin().read_to_string(&mut buffer)?;
            return Ok(buffer);
        }
        return fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()));
    }
    Err(crate::output::typed_error_with_hint(
        crate::output::ErrorKind::InvalidInput,
        "missing body content",
        "pass --body TEXT or --body-file PATH",
    ))
}

fn parse_properties(values: &[String]) -> Result<BTreeMap<String, Value>> {
    let mut properties = BTreeMap::new();
    for item in values {
        let (key, raw_value) = item.split_once('=').ok_or_else(|| {
            crate::output::typed_error(
                crate::output::ErrorKind::InvalidInput,
                format!("invalid property `{item}`; expected key=value"),
            )
        })?;
        let value = serde_json::from_str(raw_value).unwrap_or(Value::String(raw_value.to_string()));
        properties.insert(key.to_string(), value);
    }
    Ok(properties)
}

fn merge_strings(existing: Vec<String>, extra: Vec<String>) -> Vec<String> {
    let mut merged = existing;
    for item in extra {
        if !merged.contains(&item) {
            merged.push(item);
        }
    }
    merged
}

fn merge_properties(
    mut existing: BTreeMap<String, Value>,
    extra: BTreeMap<String, Value>,
) -> BTreeMap<String, Value> {
    for (key, value) in extra {
        existing.insert(key, value);
    }
    existing
}

fn render_spaces(spaces: &[SpaceSummary], output: OutputFormat) -> Result<()> {
    if output.is_json() {
        print_json(&spaces)
    } else {
        let rows = spaces
            .iter()
            .map(|space| {
                vec![
                    space.id.clone(),
                    space.key.clone(),
                    space.name.clone(),
                    space.space_type.clone().unwrap_or_default(),
                    space.homepage_id.clone().unwrap_or_default(),
                ]
            })
            .collect::<Vec<_>>();
        print_table(&["id", "key", "name", "type", "homepage"], &rows);
        Ok(())
    }
}

fn render_content_items(
    items: &[ContentItem],
    output: OutputFormat,
    show_body: bool,
) -> Result<()> {
    if output.is_json() {
        print_json(&items)
    } else {
        let mut headers = vec![
            "id", "kind", "space", "title", "status", "version", "parent",
        ];
        if show_body {
            headers.push("body");
        }
        let rows = items
            .iter()
            .map(|item| {
                let mut row = vec![
                    item.id.clone(),
                    item.kind.to_string(),
                    item.space_key
                        .clone()
                        .or(item.space_id.clone())
                        .unwrap_or_default(),
                    item.title.clone(),
                    item.status.clone(),
                    item.version
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                    item.parent_id.clone().unwrap_or_default(),
                ];
                if show_body {
                    row.push(item.body_storage.clone().unwrap_or_default());
                }
                row
            })
            .collect::<Vec<_>>();
        print_table(&headers, &rows);
        Ok(())
    }
}

fn render_attachments(items: &[AttachmentInfo], output: OutputFormat) -> Result<()> {
    if output.is_json() {
        print_json(&items)
    } else {
        let rows = items
            .iter()
            .map(|item| {
                vec![
                    item.id.clone(),
                    item.title.clone(),
                    item.media_type.clone().unwrap_or_default(),
                    item.file_size
                        .map(|size| size.to_string())
                        .unwrap_or_default(),
                    item.download_url.clone().unwrap_or_default(),
                ]
            })
            .collect::<Vec<_>>();
        print_table(
            &["id", "title", "media_type", "size", "download_url"],
            &rows,
        );
        Ok(())
    }
}

fn render_comments(items: &[CommentInfo], output: OutputFormat) -> Result<()> {
    if output.is_json() {
        print_json(&items)
    } else {
        let rows = items
            .iter()
            .map(|item| {
                vec![
                    item.id.clone(),
                    item.author.clone().unwrap_or_default(),
                    item.created_at
                        .map(|time| time.to_rfc3339())
                        .unwrap_or_default(),
                    item.body_storage.clone(),
                ]
            })
            .collect::<Vec<_>>();
        print_table(&["id", "author", "created_at", "body"], &rows);
        Ok(())
    }
}

fn render_properties(items: &[ContentProperty], output: OutputFormat) -> Result<()> {
    if output.is_json() {
        print_json(&items)
    } else {
        let rows = items
            .iter()
            .map(|item| {
                vec![
                    item.id.clone().unwrap_or_default(),
                    item.key.clone(),
                    item.version
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                    item.value.to_string(),
                ]
            })
            .collect::<Vec<_>>();
        print_table(&["id", "key", "version", "value"], &rows);
        Ok(())
    }
}

/// Filter a list of JSON objects to only include the requested fields.
fn filter_fields_vec(items: Vec<Value>, fields: Option<&str>) -> Vec<Value> {
    let Some(fields_str) = fields else {
        return items;
    };
    let field_names: Vec<&str> = fields_str.split(',').map(str::trim).collect();
    items
        .into_iter()
        .map(|item| {
            if let Value::Object(map) = item {
                let filtered: serde_json::Map<String, Value> = map
                    .into_iter()
                    .filter(|(k, _)| field_names.contains(&k.as_str()))
                    .collect();
                Value::Object(filtered)
            } else {
                item
            }
        })
        .collect()
}

fn paginate<T>(items: Vec<T>, limit: usize, offset: usize) -> (Vec<T>, usize) {
    let total = items.len();
    let page = items.into_iter().skip(offset).take(limit).collect();
    (page, total)
}

fn list_window_metadata(all: bool, total: usize, limit: usize, offset: usize) -> (usize, usize) {
    if all { (total, 0) } else { (limit, offset) }
}

fn render_spaces_list(
    spaces: &[SpaceSummary],
    output: OutputFormat,
    total: usize,
    limit: usize,
    offset: usize,
    fields: Option<&str>,
) -> Result<()> {
    if output.is_json() {
        let items: Vec<Value> = spaces
            .iter()
            .map(|s| serde_json::to_value(s).unwrap_or(Value::Null))
            .collect();
        let filtered = filter_fields_vec(items, fields);
        print_items_json(&filtered, total, limit, offset)
    } else {
        let rows = spaces
            .iter()
            .map(|space| {
                vec![
                    space.id.clone(),
                    space.key.clone(),
                    space.name.clone(),
                    space.space_type.clone().unwrap_or_default(),
                    space.homepage_id.clone().unwrap_or_default(),
                ]
            })
            .collect::<Vec<_>>();
        print_table(&["id", "key", "name", "type", "homepage"], &rows);
        Ok(())
    }
}

fn render_search_results_list(
    results: &[SearchResult],
    output: OutputFormat,
    total: Option<usize>,
    limit: usize,
    offset: usize,
    fields: Option<&str>,
) -> Result<()> {
    if output.is_json() {
        let items: Vec<Value> = results
            .iter()
            .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
            .collect();
        let filtered = filter_fields_vec(items, fields);
        print_json(&json!({
            "items": filtered,
            "total": total,
            "limit": limit,
            "offset": offset,
        }))
    } else {
        let rows = results
            .iter()
            .map(|result| {
                vec![
                    result.id.clone(),
                    result.kind.to_string(),
                    result.space_key.clone().unwrap_or_default(),
                    result.title.clone(),
                    result.web_url.clone().unwrap_or_default(),
                ]
            })
            .collect::<Vec<_>>();
        print_table(&["id", "kind", "space", "title", "url"], &rows);
        Ok(())
    }
}

fn render_content_items_list(
    items: &[ContentItem],
    output: OutputFormat,
    show_body: bool,
    total: usize,
    limit: usize,
    offset: usize,
    fields: Option<&str>,
) -> Result<()> {
    if output.is_json() {
        let json_items: Vec<Value> = items
            .iter()
            .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
            .collect();
        let filtered = filter_fields_vec(json_items, fields);
        print_items_json(&filtered, total, limit, offset)
    } else {
        render_content_items(items, output, show_body)
    }
}

fn render_attachments_list(
    items: &[AttachmentInfo],
    output: OutputFormat,
    total: usize,
    limit: usize,
    offset: usize,
    fields: Option<&str>,
) -> Result<()> {
    if output.is_json() {
        let json_items: Vec<Value> = items
            .iter()
            .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
            .collect();
        let filtered = filter_fields_vec(json_items, fields);
        print_items_json(&filtered, total, limit, offset)
    } else {
        render_attachments(items, output)
    }
}

fn render_comments_list(
    items: &[CommentInfo],
    output: OutputFormat,
    total: usize,
    limit: usize,
    offset: usize,
    fields: Option<&str>,
) -> Result<()> {
    if output.is_json() {
        let json_items: Vec<Value> = items
            .iter()
            .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
            .collect();
        let filtered = filter_fields_vec(json_items, fields);
        print_items_json(&filtered, total, limit, offset)
    } else {
        render_comments(items, output)
    }
}

fn render_properties_list(
    items: &[ContentProperty],
    output: OutputFormat,
    total: usize,
    limit: usize,
    offset: usize,
    fields: Option<&str>,
) -> Result<()> {
    if output.is_json() {
        let json_items: Vec<Value> = items
            .iter()
            .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
            .collect();
        let filtered = filter_fields_vec(json_items, fields);
        print_items_json(&filtered, total, limit, offset)
    } else {
        render_properties(items, output)
    }
}

fn render_plan(plan: &SyncPlan, output: OutputFormat, show_diff: bool) -> Result<()> {
    if output.is_json() {
        print_json(plan)
    } else {
        let summary = summarize_plan(plan);
        println!("Plan summary: {summary}");
        let rows = plan
            .items
            .iter()
            .map(|item| {
                vec![
                    plan_action_label(item.action.clone()).to_string(),
                    item.title.clone(),
                    item.content_id.clone().unwrap_or_default(),
                    item.path.display().to_string(),
                    item.details.clone(),
                ]
            })
            .collect::<Vec<_>>();
        print_table(&["action", "title", "id", "path", "details"], &rows);
        if show_diff {
            for item in &plan.items {
                if let Some(diff) = &item.diff {
                    println!("\n--- diff: {} ---", item.title);
                    print!("{diff}");
                }
            }
        }
        Ok(())
    }
}

fn render_doctor(report: &DoctorReport, output: OutputFormat) -> Result<()> {
    if output.is_json() {
        return print_json(report);
    }
    print!("{}", doctor_text(report));
    Ok(())
}

const DOCTOR_TEXT_WIDTH: usize = 100;

fn doctor_text(report: &DoctorReport) -> String {
    let mut output = String::from("Configuration:\n");
    push_doctor_field(&mut output, "path", &report.config_path);
    push_doctor_field(
        &mut output,
        "exists",
        if report.config_exists { "yes" } else { "no" },
    );
    push_doctor_field(
        &mut output,
        "active profile",
        report.active_profile.as_deref().unwrap_or("-"),
    );
    push_doctor_field(
        &mut output,
        "stored profiles",
        &report.stored_profiles.to_string(),
    );

    if let Some(profile) = &report.resolved_profile {
        output.push_str("\nResolved profile:\n");
        push_doctor_field(&mut output, "name", &profile.name);
        push_doctor_field(&mut output, "provider", &profile.provider.to_string());
        push_doctor_field(&mut output, "base URL", &profile.base_url);
        push_doctor_field(&mut output, "API path", &profile.api_path);
        push_doctor_field(&mut output, "auth", doctor_auth_kind(profile));
        push_doctor_field(&mut output, "read only", &profile.read_only.to_string());
    }

    output.push_str("\nChecks:\n");
    for check in &report.checks {
        let label = format!("{} {}", doctor_status_label(check.status), check.name);
        push_doctor_field(&mut output, &label, &check.details);
    }
    output.push_str(&format!(
        "\nDoctor summary: {} passed, {} warned, {} failed\n",
        report.summary.passed, report.summary.warned, report.summary.failed
    ));
    output
}

fn push_doctor_field(output: &mut String, label: &str, value: &str) {
    let prefix = format!("  {label}: ");
    let continuation = " ".repeat(prefix.chars().count());
    let content_width = DOCTOR_TEXT_WIDTH
        .saturating_sub(prefix.chars().count())
        .max(20);
    for (index, line) in wrap_text(value, content_width).into_iter().enumerate() {
        output.push_str(if index == 0 { &prefix } else { &continuation });
        output.push_str(&line);
        output.push('\n');
    }
}

fn wrap_text(value: &str, width: usize) -> Vec<String> {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        while start < chars.len() && chars[start].is_whitespace() {
            start += 1;
        }
        if start == chars.len() {
            break;
        }
        let hard_end = (start + width).min(chars.len());
        let end = if hard_end == chars.len() {
            hard_end
        } else {
            (start..hard_end)
                .rev()
                .find(|&index| chars[index].is_whitespace())
                .filter(|&index| index > start)
                .unwrap_or(hard_end)
        };
        lines.push(chars[start..end].iter().collect());
        start = end;
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn push_doctor_check(
    report: &mut DoctorReport,
    name: impl Into<String>,
    status: DoctorCheckStatus,
    details: impl Into<String>,
) {
    report.checks.push(DoctorCheck {
        name: name.into(),
        status,
        details: details.into(),
    });
}

fn finalize_doctor_summary(report: &mut DoctorReport) {
    let mut summary = DoctorSummary::default();
    for check in &report.checks {
        match check.status {
            DoctorCheckStatus::Pass => summary.passed += 1,
            DoctorCheckStatus::Warn => summary.warned += 1,
            DoctorCheckStatus::Fail => summary.failed += 1,
        }
    }
    report.summary = summary;
}

fn doctor_auth_kind(profile: &ResolvedProfile) -> &'static str {
    match &profile.auth {
        crate::config::AuthConfig::Basic { .. } => "basic",
        crate::config::AuthConfig::Bearer { .. } => "bearer",
    }
}

fn doctor_auth_details(profile: &ResolvedProfile) -> String {
    match &profile.auth {
        crate::config::AuthConfig::Basic { username, token } => format!(
            "basic auth configured for `{username}` with a {} token",
            redact_token_shape(token)
        ),
        crate::config::AuthConfig::Bearer { token } => {
            format!(
                "bearer auth configured with a {}",
                redact_token_shape(token)
            )
        }
    }
}

fn doctor_auth_check(profile: &ResolvedProfile) -> (DoctorCheckStatus, String) {
    let details = doctor_auth_details(profile);
    let status = match &profile.auth {
        crate::config::AuthConfig::Basic { username, token } => {
            if username.trim().is_empty() || token.trim().is_empty() {
                DoctorCheckStatus::Fail
            } else {
                DoctorCheckStatus::Pass
            }
        }
        crate::config::AuthConfig::Bearer { token } => {
            if token.trim().is_empty() {
                DoctorCheckStatus::Fail
            } else {
                DoctorCheckStatus::Pass
            }
        }
    };
    (status, details)
}

fn redact_token_shape(token: &str) -> String {
    if token.is_empty() {
        "missing token".to_string()
    } else {
        format!("{}-character secret", token.chars().count())
    }
}

fn summarize_plan(plan: &SyncPlan) -> String {
    let mut counts = BTreeMap::new();
    for item in &plan.items {
        *counts
            .entry(plan_action_label(item.action.clone()))
            .or_insert(0usize) += 1;
    }
    counts
        .into_iter()
        .map(|(label, count)| format!("{label}={count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn plan_action_label(action: PlanActionKind) -> &'static str {
    match action {
        PlanActionKind::CreateContent => "create",
        PlanActionKind::UpdateContent => "update",
        PlanActionKind::MoveContent => "move",
        PlanActionKind::UploadAttachment => "upload_attachment",
        PlanActionKind::DeleteAttachment => "delete_attachment",
        PlanActionKind::UpdateLabels => "update_labels",
        PlanActionKind::UpdateProperties => "update_properties",
        PlanActionKind::DeleteRemote => "delete_remote",
        PlanActionKind::Noop => "noop",
    }
}

fn doctor_status_label(status: DoctorCheckStatus) -> &'static str {
    match status {
        DoctorCheckStatus::Pass => "pass",
        DoctorCheckStatus::Warn => "warn",
        DoctorCheckStatus::Fail => "fail",
    }
}

fn confirm_destructive(yes: bool, prompt: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        return Err(crate::output::typed_error_with_hint(
            crate::output::ErrorKind::ConfirmationRequired,
            prompt,
            "Re-run with --yes to confirm.",
        ));
    }
    eprint!("{prompt} [y/N] ");
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    if input.trim().eq_ignore_ascii_case("y") {
        Ok(())
    } else {
        Err(crate::output::typed_error(
            crate::output::ErrorKind::ConfirmationRequired,
            "operation cancelled; no changes were made",
        ))
    }
}

fn print_status(output: OutputFormat, value: Value, text: &str) -> Result<()> {
    if output.is_json() {
        print_json(&value)
    } else {
        println!("{text}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_includes_completions_command() {
        let command = Cli::command();
        assert!(
            command
                .get_subcommands()
                .any(|subcommand| subcommand.get_name() == "completions")
        );
    }

    #[test]
    fn cli_includes_doctor_command() {
        let command = Cli::command();
        assert!(
            command
                .get_subcommands()
                .any(|subcommand| subcommand.get_name() == "doctor")
        );
    }

    /// --json is a hidden alias for --output json and must resolve to JSON format
    /// via the same production code path used at runtime.
    #[test]
    fn json_flag_resolves_to_json_format() {
        let cli = Cli::parse_from(["confluence", "--json", "schema"]);
        let output = if cli.json && cli.output == OutputArg::Auto {
            OutputFormat::Json
        } else {
            OutputFormat::from_arg(cli.output)
        };
        assert_eq!(output, OutputFormat::Json);
    }

    /// Explicit --output text wins when both --json and --output are given.
    #[test]
    fn explicit_output_wins_over_json_flag() {
        let cli = Cli::parse_from(["confluence", "--json", "--output", "text", "schema"]);
        let output = if cli.json && cli.output == OutputArg::Auto {
            OutputFormat::Json
        } else {
            OutputFormat::from_arg(cli.output)
        };
        // --output text was explicit, so text wins even though --json was passed
        assert_eq!(output, OutputFormat::Text);
    }

    /// --json is hidden (does not appear in --help output).
    #[test]
    fn json_flag_is_hidden() {
        use clap::CommandFactory;
        let cmd = Cli::command();
        let json_arg = cmd.get_arguments().find(|a| a.get_long() == Some("json"));
        assert!(
            json_arg.map(|a| a.is_hide_set()).unwrap_or(false),
            "--json must be a hidden flag"
        );
    }

    /// Regression: the global `--output` (OutputArg) must not share a clap arg id
    /// with the positional `output: PathBuf` in subcommands. If it does, clap panics
    /// at parse time with a downcast mismatch. See `pull`/`attachment download`.
    #[test]
    fn pull_page_parses_positional_output_path() {
        let cli = Cli::parse_from(["confluence", "pull", "page", "REF", "/tmp/dest"]);
        match cli.command {
            Commands::Pull {
                command:
                    PullCommand::Page {
                        reference, output, ..
                    },
            } => {
                assert_eq!(reference, "REF");
                assert_eq!(output, std::path::PathBuf::from("/tmp/dest"));
            }
            other => panic!("expected `pull page`, got {other:?}"),
        }
    }

    /// Same collision surfaces on `attachment download` (positional `output: PathBuf`).
    #[test]
    fn attachment_download_parses_positional_output_path() {
        let cli = Cli::parse_from([
            "confluence",
            "attachment",
            "download",
            "REF",
            "ATT",
            "/tmp/dest",
        ]);
        match cli.command {
            Commands::Attachment {
                command:
                    AttachmentCommand::Download {
                        reference,
                        attachment_id,
                        output,
                        force,
                    },
            } => {
                assert_eq!(reference, "REF");
                assert_eq!(attachment_id, "ATT");
                assert_eq!(output, std::path::PathBuf::from("/tmp/dest"));
                assert!(!force);
            }
            other => panic!("expected `attachment download`, got {other:?}"),
        }
    }

    /// The global format flag keeps its `--output`/`-o` surface after the id rename.
    #[test]
    fn global_output_flag_still_parses() {
        let cli = Cli::parse_from(["confluence", "--output", "json", "schema"]);
        assert_eq!(cli.output, OutputArg::Json);
        let cli = Cli::parse_from(["confluence", "-o", "text", "schema"]);
        assert_eq!(cli.output, OutputArg::Text);
    }

    #[test]
    fn body_commands_parse_without_global_output_id_collisions() {
        for args in [
            vec![
                "confluence",
                "page",
                "create",
                "Title",
                "SPACE",
                "--body",
                "text",
            ],
            vec!["confluence", "page", "update", "123", "--body", "text"],
            vec![
                "confluence",
                "blog",
                "create",
                "Title",
                "SPACE",
                "--body",
                "text",
            ],
            vec!["confluence", "blog", "update", "123", "--body", "text"],
            vec!["confluence", "comment", "add", "123", "--body", "text"],
            vec!["confluence", "comment", "update", "456", "--body", "text"],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|error| {
                panic!("failed to parse {args:?}: {error}");
            });
        }
    }

    #[test]
    fn body_and_body_file_conflict() {
        let error = Cli::try_parse_from([
            "confluence",
            "page",
            "create",
            "Title",
            "SPACE",
            "--body",
            "text",
            "--body-file",
            "page.md",
        ])
        .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn update_status_is_optional_and_explicit_status_is_a_change() {
        let cli = Cli::parse_from(["confluence", "page", "update", "123"]);
        let Commands::Page {
            command: PageCommand::Update(page),
        } = cli.command
        else {
            panic!("expected page update");
        };
        assert_eq!(page.status, None);
        assert!(!page.has_changes());
        assert!(validate_page_update(&page).is_err());

        let cli = Cli::parse_from(["confluence", "blog", "update", "456", "--status", "draft"]);
        let Commands::Blog {
            command: BlogCommand::Update(blog),
        } = cli.command
        else {
            panic!("expected blog update");
        };
        assert_eq!(blog.status.as_deref(), Some("draft"));
        assert!(blog.has_changes());
        validate_blog_update(&blog).expect("explicit status is a requested change");
    }

    #[test]
    fn version_alone_does_not_turn_a_noop_into_an_update() {
        let cli = Cli::parse_from(["confluence", "page", "update", "123", "--version", "9"]);
        let Commands::Page {
            command: PageCommand::Update(page),
        } = cli.command
        else {
            panic!("expected page update");
        };
        assert!(!page.has_changes());
        let error = validate_page_update(&page).expect_err("version is only a precondition");
        assert!(error.to_string().contains("at least one requested change"));
    }

    #[test]
    fn all_list_metadata_describes_the_returned_window() {
        assert_eq!(list_window_metadata(true, 37, 5, 12), (37, 0));
        assert_eq!(list_window_metadata(false, 37, 5, 12), (5, 12));
    }

    #[test]
    fn completions_ignore_broken_pipe_writes() {
        struct BrokenPipeWriter;

        impl Write for BrokenPipeWriter {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader closed"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        write_completions(Shell::Zsh, &mut BrokenPipeWriter)
            .expect("a closed completion consumer is a successful exit");
    }

    #[test]
    fn doctor_text_stays_within_a_readable_width() {
        let report = DoctorReport {
            config_path: format!("/{}", "very-long-directory-name/".repeat(10)),
            config_exists: true,
            active_profile: Some("work".to_string()),
            stored_profiles: 1,
            resolved_profile: None,
            checks: vec![DoctorCheck {
                name: "connectivity".to_string(),
                status: DoctorCheckStatus::Fail,
                details: "a detailed diagnostic that should wrap cleanly ".repeat(10),
            }],
            summary: DoctorSummary {
                passed: 2,
                warned: 0,
                failed: 1,
            },
        };
        let rendered = doctor_text(&report);
        assert!(rendered.contains("Configuration:"));
        assert!(rendered.contains("fail connectivity:"));
        assert!(
            rendered
                .lines()
                .all(|line| line.chars().count() <= DOCTOR_TEXT_WIDTH),
            "doctor output exceeded {DOCTOR_TEXT_WIDTH} columns:\n{rendered}"
        );
    }

    #[test]
    fn blog_commands_do_not_advertise_page_parenting() {
        let error = Cli::try_parse_from([
            "confluence",
            "blog",
            "create",
            "Title",
            "SPACE",
            "--body",
            "text",
            "--parent",
            "123",
        ])
        .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);

        let blog = Cli::command()
            .find_subcommand("blog")
            .unwrap()
            .find_subcommand("create")
            .unwrap()
            .clone();
        assert!(
            blog.get_arguments()
                .all(|argument| argument.get_long() != Some("parent"))
        );
    }

    #[test]
    fn every_pull_variant_accepts_force() {
        for args in [
            ["confluence", "pull", "page", "123", "dest", "--force"],
            ["confluence", "pull", "tree", "123", "dest", "--force"],
            ["confluence", "pull", "space", "DOCS", "dest", "--force"],
        ] {
            Cli::try_parse_from(args).expect("pull --force should parse");
        }
    }

    #[test]
    fn page_tree_recursive_can_be_disabled() {
        let cli = Cli::parse_from(["confluence", "page", "tree", "123", "--recursive", "false"]);
        match cli.command {
            Commands::Page {
                command: PageCommand::Tree { recursive, .. },
            } => assert!(!recursive),
            other => panic!("expected page tree, got {other:?}"),
        }
    }

    #[test]
    fn profile_add_requires_an_explicit_name() {
        let error = Cli::try_parse_from(["confluence", "profile", "add"]).unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        assert!(error.to_string().contains("--name <NAME>"));
    }

    #[test]
    fn every_public_command_and_argument_has_help() {
        fn walk(command: &clap::Command) {
            for subcommand in command
                .get_subcommands()
                .filter(|sub| sub.get_name() != "help")
            {
                assert!(
                    subcommand.get_about().is_some(),
                    "command `{}` is missing an about description",
                    subcommand.get_name()
                );
                for argument in subcommand.get_arguments().filter(|arg| {
                    arg.get_id() != "help" && !arg.is_hide_set() && !arg.is_global_set()
                }) {
                    assert!(
                        argument.get_help().is_some(),
                        "argument `{}` on `{}` is missing help",
                        argument.get_id(),
                        subcommand.get_name()
                    );
                }
                walk(subcommand);
            }
        }
        walk(&Cli::command());
    }
}
