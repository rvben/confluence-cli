use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use serde::Serialize;
use serde_json::{Value, json};
use url::Url;

use crate::config::{AppConfig, LoginInput, ResolvedProfile, logout, run_login};
use crate::markdown::markdown_to_storage;
use crate::model::{
    AttachmentInfo, CommentInfo, ContentItem, ContentKind, ContentProperty, CreateContentRequest,
    PlanActionKind, ProviderKind, SearchResult, SpaceSummary, SyncPlan, UpdateContentRequest,
};
use crate::output::{OutputFormat, print_json, print_list, print_table};
use crate::provider::{ConfluenceProvider, build_provider};
use crate::sync;

#[derive(Parser, Debug)]
#[command(
    name = "confluence-cli",
    version,
    about = "Markdown-sync-first Confluence CLI in Rust"
)]
pub struct Cli {
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    profile: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    Space {
        #[command(subcommand)]
        command: SpaceCommand,
    },
    Search(SearchArgs),
    Page {
        #[command(subcommand)]
        command: PageCommand,
    },
    Blog {
        #[command(subcommand)]
        command: BlogCommand,
    },
    Pull {
        #[command(subcommand)]
        command: PullCommand,
    },
    Doctor(DoctorArgs),
    Plan(PlanArgs),
    Apply(ApplyArgs),
    Attachment {
        #[command(subcommand)]
        command: AttachmentCommand,
    },
    Label {
        #[command(subcommand)]
        command: LabelCommand,
    },
    Comment {
        #[command(subcommand)]
        command: CommentCommand,
    },
    Property {
        #[command(subcommand)]
        command: PropertyCommand,
    },
    Completions {
        shell: Shell,
    },
}

#[derive(Subcommand, Debug)]
enum AuthCommand {
    Login(ProfileInputArgs),
    Status,
    Logout,
}

#[derive(Subcommand, Debug)]
enum ProfileCommand {
    Add(ProfileInputArgs),
    List,
    Use { name: String },
    Remove { name: String },
}

#[derive(Subcommand, Debug)]
enum SpaceCommand {
    List {
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    Get {
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
    query: String,
    #[arg(long)]
    cql: bool,
    #[arg(long)]
    space: Option<String>,
    #[arg(long, value_enum)]
    r#type: Option<ContentTypeFilter>,
    #[arg(long, default_value_t = 20)]
    limit: usize,
    #[arg(long, default_value_t = 0)]
    offset: usize,
}

#[derive(Subcommand, Debug)]
enum PageCommand {
    List {
        space: String,
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    Get {
        reference: String,
        #[arg(long)]
        show_body: bool,
    },
    Tree {
        reference: String,
        #[arg(long, default_value_t = true)]
        recursive: bool,
    },
    Move {
        reference: String,
        parent: String,
    },
    Create(WriteContentArgs),
    Update(UpdateContentArgs),
    Delete {
        reference: String,
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
enum BlogCommand {
    List {
        space: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    Get {
        id: String,
        #[arg(long)]
        show_body: bool,
    },
    Create(WriteContentArgs),
    Update(UpdateContentArgs),
    Delete {
        id: String,
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
enum PullCommand {
    Page { reference: String, output: PathBuf },
    Tree { reference: String, output: PathBuf },
    Space {
        space: String,
        output: PathBuf,
        #[arg(long)]
        since: Option<String>,
    },
}

#[derive(Args, Debug)]
struct PlanArgs {
    path: PathBuf,
    #[arg(long)]
    allow_lossy: bool,
    #[arg(long)]
    delete_remote: bool,
    #[arg(long)]
    diff: bool,
}

#[derive(Args, Debug)]
struct ApplyArgs {
    path: PathBuf,
    #[arg(long)]
    allow_lossy: bool,
    #[arg(long)]
    delete_remote: bool,
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct DoctorArgs {
    #[arg(long)]
    space: Option<String>,
    #[arg(long)]
    path: Option<PathBuf>,
    #[arg(long)]
    skip_network: bool,
}

#[derive(Subcommand, Debug)]
enum AttachmentCommand {
    List {
        reference: String,
    },
    Download {
        reference: String,
        attachment_id: String,
        output: PathBuf,
    },
    Upload {
        reference: String,
        #[arg(long = "file", required = true)]
        files: Vec<PathBuf>,
        #[arg(long)]
        comment: Option<String>,
        #[arg(long)]
        replace: bool,
        #[arg(long)]
        minor_edit: bool,
    },
    Delete {
        reference: String,
        attachment_id: String,
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
enum LabelCommand {
    List { reference: String },
    Add { reference: String, label: String },
    Remove { reference: String, label: String },
}

#[derive(Subcommand, Debug)]
enum CommentCommand {
    List {
        reference: String,
    },
    Add {
        reference: String,
        #[command(flatten)]
        body: BodyInput,
    },
    Update {
        comment_id: String,
        #[command(flatten)]
        body: BodyInput,
    },
    Delete {
        comment_id: String,
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
enum PropertyCommand {
    List {
        reference: String,
    },
    Get {
        reference: String,
        key: String,
    },
    Set {
        reference: String,
        key: String,
        value: String,
        #[arg(long)]
        raw: bool,
    },
    Delete {
        reference: String,
        key: String,
    },
}

#[derive(Args, Debug, Clone)]
struct BodyInput {
    #[arg(long)]
    body: Option<String>,
    #[arg(long)]
    body_file: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = BodyFormat::Markdown)]
    format: BodyFormat,
    #[arg(long)]
    allow_lossy: bool,
}

#[derive(Args, Debug)]
struct WriteContentArgs {
    title: String,
    space: String,
    #[arg(long)]
    parent: Option<String>,
    #[command(flatten)]
    body: BodyInput,
    #[arg(long = "label")]
    labels: Vec<String>,
    #[arg(long = "property")]
    properties: Vec<String>,
    #[arg(long, default_value = "current")]
    status: String,
}

#[derive(Args, Debug)]
struct UpdateContentArgs {
    reference: String,
    #[arg(long)]
    title: Option<String>,
    #[arg(long)]
    parent: Option<String>,
    #[command(flatten)]
    body: BodyInput,
    #[arg(long = "label")]
    labels: Vec<String>,
    #[arg(long = "property")]
    properties: Vec<String>,
    #[arg(long)]
    replace_labels: bool,
    #[arg(long)]
    replace_properties: bool,
    #[arg(long)]
    version: Option<u64>,
    #[arg(long, default_value = "current")]
    status: String,
}

#[derive(Args, Debug, Clone)]
struct ProfileInputArgs {
    #[arg(long)]
    name: Option<String>,
    #[arg(long, value_enum)]
    provider: Option<ProviderArg>,
    #[arg(long)]
    domain: Option<String>,
    #[arg(long)]
    api_path: Option<String>,
    #[arg(long)]
    auth_type: Option<String>,
    #[arg(long)]
    username: Option<String>,
    #[arg(long)]
    token: Option<String>,
    #[arg(long)]
    read_only: bool,
    #[arg(long)]
    non_interactive: bool,
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
    let cli = Cli::parse();
    let output = OutputFormat::from_json_flag(cli.json);

    match cli.command {
        Commands::Auth { command } => handle_auth(command, cli.profile.as_deref(), output).await,
        Commands::Profile { command } => handle_profile(command, output),
        Commands::Space { command } => {
            let provider = provider_from_profile(cli.profile.as_deref())?;
            handle_space(&*provider, command, output).await
        }
        Commands::Search(args) => {
            let provider = provider_from_profile(cli.profile.as_deref())?;
            handle_search(&*provider, args, output).await
        }
        Commands::Page { command } => {
            let provider = provider_from_profile(cli.profile.as_deref())?;
            handle_page(&*provider, command, output).await
        }
        Commands::Blog { command } => {
            let provider = provider_from_profile(cli.profile.as_deref())?;
            handle_blog(&*provider, command, output).await
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
            handle_attachment(&*provider, command, output).await
        }
        Commands::Label { command } => {
            let provider = provider_from_profile(cli.profile.as_deref())?;
            handle_label(&*provider, command, output).await
        }
        Commands::Comment { command } => {
            let provider = provider_from_profile(cli.profile.as_deref())?;
            handle_comment(&*provider, command, output).await
        }
        Commands::Property { command } => {
            let provider = provider_from_profile(cli.profile.as_deref())?;
            handle_property(&*provider, command, output).await
        }
        Commands::Completions { shell } => {
            let mut command = Cli::command();
            generate(shell, &mut command, "confluence-cli", &mut io::stdout());
            Ok(())
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

async fn handle_auth(
    command: AuthCommand,
    profile_override: Option<&str>,
    output: OutputFormat,
) -> Result<()> {
    match command {
        AuthCommand::Login(args) => {
            let resolved = run_login(LoginInput {
                profile: args.name,
                provider: args.provider.map(ProviderArg::into_model),
                domain: args.domain,
                api_path: args.api_path,
                auth_type: args.auth_type,
                username: args.username,
                token: args.token,
                read_only: Some(args.read_only),
                non_interactive: args.non_interactive,
            })?;
            if matches!(output, OutputFormat::Json) {
                print_json(&resolved.redact())?;
            } else {
                print_table(
                    &["profile", "provider", "base_url", "api_path", "read_only"],
                    &[vec![
                        resolved.name,
                        resolved.provider.to_string(),
                        resolved.base_url,
                        resolved.api_path,
                        resolved.read_only.to_string(),
                    ]],
                );
            }
        }
        AuthCommand::Status => {
            let profile = resolved_profile(profile_override)?;
            let provider = build_provider(profile.clone());
            provider.ping().await?;
            if matches!(output, OutputFormat::Json) {
                print_json(&profile.redact())?;
            } else {
                print_table(
                    &[
                        "profile",
                        "provider",
                        "base_url",
                        "api_path",
                        "read_only",
                        "status",
                    ],
                    &[vec![
                        profile.name,
                        profile.provider.to_string(),
                        profile.base_url,
                        profile.api_path,
                        profile.read_only.to_string(),
                        "ok".to_string(),
                    ]],
                );
            }
        }
        AuthCommand::Logout => {
            let name = logout(profile_override)?;
            if matches!(output, OutputFormat::Json) {
                print_json(&json!({ "profile": name, "status": "logged_out" }))?;
            } else {
                println!("Logged out profile `{name}`");
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
            push_doctor_check(
                &mut report,
                "config_load",
                DoctorCheckStatus::Fail,
                err.to_string(),
            );
            finalize_doctor_summary(&mut report);
            render_doctor(&report, output)?;
            bail!("doctor found {} failing check(s)", report.summary.failed);
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
            push_doctor_check(
                &mut report,
                "profile_resolution",
                DoctorCheckStatus::Fail,
                err.to_string(),
            );
            finalize_doctor_summary(&mut report);
            render_doctor(&report, output)?;
            bail!("doctor found {} failing check(s)", report.summary.failed);
        }
    };

    match Url::parse(&profile.base_url) {
        Ok(url) => push_doctor_check(
            &mut report,
            "base_url",
            DoctorCheckStatus::Pass,
            format!("using host {}", url.host_str().unwrap_or_default()),
        ),
        Err(err) => push_doctor_check(
            &mut report,
            "base_url",
            DoctorCheckStatus::Fail,
            format!("invalid base URL `{}`: {err}", profile.base_url),
        ),
    }

    let (auth_status, auth_details) = doctor_auth_check(&profile);
    push_doctor_check(&mut report, "auth", auth_status, auth_details);

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
            Err(err) => push_doctor_check(
                &mut report,
                "connectivity",
                DoctorCheckStatus::Fail,
                err.to_string(),
            ),
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
                Err(err) => push_doctor_check(
                    &mut report,
                    "space_access",
                    DoctorCheckStatus::Fail,
                    format!("failed to access space `{space}`: {err}"),
                ),
            }
        }
    }

    if let Some(path) = args.path.as_deref() {
        if !path.exists() {
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
                Err(err) => push_doctor_check(
                    &mut report,
                    "sync_path",
                    DoctorCheckStatus::Fail,
                    format!("failed to inspect `{}`: {err}", path.display()),
                ),
            }
        }
    }

    finalize_doctor_summary(&mut report);
    render_doctor(&report, output)?;
    if report.summary.failed > 0 {
        bail!("doctor found {} failing check(s)", report.summary.failed);
    }
    Ok(())
}

fn handle_profile(command: ProfileCommand, output: OutputFormat) -> Result<()> {
    match command {
        ProfileCommand::Add(args) => {
            let resolved = run_login(LoginInput {
                profile: args.name,
                provider: args.provider.map(ProviderArg::into_model),
                domain: args.domain,
                api_path: args.api_path,
                auth_type: args.auth_type,
                username: args.username,
                token: args.token,
                read_only: Some(args.read_only),
                non_interactive: args.non_interactive,
            })?;
            if matches!(output, OutputFormat::Json) {
                print_json(&resolved.redact())?;
            } else {
                println!("Stored profile `{}`", resolved.name);
            }
        }
        ProfileCommand::List => {
            let config = AppConfig::load()?;
            if matches!(output, OutputFormat::Json) {
                print_json(&config)?;
            } else {
                let rows = config
                    .profiles
                    .iter()
                    .map(|(name, profile)| {
                        vec![
                            name.clone(),
                            profile.provider.to_string(),
                            profile.base_url.clone(),
                            profile.api_path.clone(),
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
            println!("Active profile set to `{name}`");
        }
        ProfileCommand::Remove { name } => {
            let mut config = AppConfig::load()?;
            config.remove_profile(&name)?;
            config.save()?;
            println!("Removed profile `{name}`");
        }
    }
    Ok(())
}

async fn handle_space(
    provider: &dyn ConfluenceProvider,
    command: SpaceCommand,
    output: OutputFormat,
) -> Result<()> {
    match command {
        SpaceCommand::List { limit } => {
            let spaces = provider.list_spaces(limit).await?;
            render_spaces(&spaces, output)?;
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
            format!("text ~ \"{}\"", args.query.replace('"', "\\\""))
        };
        let mut clauses = vec![text_clause];
        if let Some(space) = &args.space {
            clauses.push(format!("space = \"{space}\""));
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
    let results = provider.search(&query, is_cql, args.limit, args.offset).await?;
    render_search_results(&results, output)?;
    Ok(())
}

async fn handle_page(
    provider: &dyn ConfluenceProvider,
    command: PageCommand,
    output: OutputFormat,
) -> Result<()> {
    match command {
        PageCommand::List { space, limit } => {
            let mut pages = provider
                .list_space_content(ContentKind::Page, &space, false)
                .await?;
            pages.truncate(limit);
            render_content_items(&pages, output, false)?;
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
                    version: current
                        .version
                        .ok_or_else(|| anyhow!("page version unavailable"))?,
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
            let id = provider.resolve_page_ref(&args.reference).await?;
            let current = provider.get_content(ContentKind::Page, &id, true).await?;
            let body_storage = if args.body.body.is_some() || args.body.body_file.is_some() {
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
                    version: args
                        .version
                        .or(current.version)
                        .ok_or_else(|| anyhow!("page version unavailable"))?,
                    message: Some("Updated via confluence-cli".to_string()),
                    status: args.status,
                    labels,
                    properties,
                })
                .await?;
            render_content_items(&[updated], output, true)?;
        }
        PageCommand::Delete { reference, yes } => {
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
) -> Result<()> {
    match command {
        BlogCommand::List { space, limit } => {
            let mut posts = provider
                .list_space_content(ContentKind::BlogPost, &space, false)
                .await?;
            posts.truncate(limit);
            render_content_items(&posts, output, false)?;
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
            let current = provider
                .get_content(ContentKind::BlogPost, &args.reference, true)
                .await?;
            let body_storage = if args.body.body.is_some() || args.body.body_file.is_some() {
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
                    version: args
                        .version
                        .or(current.version)
                        .ok_or_else(|| anyhow!("blog version unavailable"))?,
                    message: Some("Updated via confluence-cli".to_string()),
                    status: args.status,
                    labels,
                    properties,
                })
                .await?;
            render_content_items(&[updated], output, true)?;
        }
        BlogCommand::Delete { id, yes } => {
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
        PullCommand::Page { reference, output } => {
            sync::pull_page(provider, &reference, &output, false).await?
        }
        PullCommand::Tree { reference, output } => {
            sync::pull_page(provider, &reference, &output, true).await?
        }
        PullCommand::Space {
            space,
            output,
            since,
        } => {
            if let Some(since) = since {
                sync::pull_space_since(provider, &space, &output, &since).await?
            } else {
                sync::pull_space(provider, &space, &output).await?
            }
        }
    };
    if matches!(output, OutputFormat::Json) {
        print_json(&written)?;
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
) -> Result<()> {
    match command {
        AttachmentCommand::List { reference } => {
            let id = provider.resolve_page_ref(&reference).await?;
            let attachments = provider.list_attachments(&id).await?;
            render_attachments(&attachments, output)?;
        }
        AttachmentCommand::Download {
            reference,
            attachment_id,
            output: path,
        } => {
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
            let id = provider.resolve_page_ref(&reference).await?;
            let mut uploaded = Vec::new();
            for file in files {
                uploaded.push(
                    provider
                        .upload_attachment(&id, &file, comment.as_deref(), replace, minor_edit)
                        .await?,
                );
            }
            render_attachments(&uploaded, output)?;
        }
        AttachmentCommand::Delete {
            reference,
            attachment_id,
            yes,
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
) -> Result<()> {
    match command {
        LabelCommand::List { reference } => {
            let id = provider.resolve_page_ref(&reference).await?;
            let labels = provider.list_labels(&id).await?;
            if matches!(output, OutputFormat::Json) {
                print_json(&labels)?;
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
) -> Result<()> {
    match command {
        CommentCommand::List { reference } => {
            let id = provider.resolve_page_ref(&reference).await?;
            let comments = provider.list_comments(&id).await?;
            render_comments(&comments, output)?;
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
        CommentCommand::Delete { comment_id, yes } => {
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
) -> Result<()> {
    match command {
        PropertyCommand::List { reference } => {
            let id = provider.resolve_page_ref(&reference).await?;
            let properties = provider.list_properties(&id).await?;
            render_properties(&properties, output)?;
        }
        PropertyCommand::Get { reference, key } => {
            let id = provider.resolve_page_ref(&reference).await?;
            let property = provider.get_property(&id, &key).await?;
            match property {
                Some(property) => render_properties(&[property], output)?,
                None => bail!("property `{key}` not found"),
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
    bail!("missing body content; pass --body or --body-file")
}

fn parse_properties(values: &[String]) -> Result<BTreeMap<String, Value>> {
    let mut properties = BTreeMap::new();
    for item in values {
        let (key, raw_value) = item
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid property `{item}`; expected key=value"))?;
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
    if matches!(output, OutputFormat::Json) {
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

fn render_search_results(results: &[SearchResult], output: OutputFormat) -> Result<()> {
    if matches!(output, OutputFormat::Json) {
        print_json(&results)
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

fn render_content_items(
    items: &[ContentItem],
    output: OutputFormat,
    show_body: bool,
) -> Result<()> {
    if matches!(output, OutputFormat::Json) {
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
    if matches!(output, OutputFormat::Json) {
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
    if matches!(output, OutputFormat::Json) {
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
    if matches!(output, OutputFormat::Json) {
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

fn render_plan(plan: &SyncPlan, output: OutputFormat, show_diff: bool) -> Result<()> {
    if matches!(output, OutputFormat::Json) {
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
    if matches!(output, OutputFormat::Json) {
        return print_json(report);
    }

    print_table(
        &[
            "config_path",
            "config_exists",
            "active_profile",
            "stored_profiles",
        ],
        &[vec![
            report.config_path.clone(),
            report.config_exists.to_string(),
            report
                .active_profile
                .clone()
                .unwrap_or_else(|| "-".to_string()),
            report.stored_profiles.to_string(),
        ]],
    );

    if let Some(profile) = &report.resolved_profile {
        print_table(
            &[
                "profile",
                "provider",
                "base_url",
                "api_path",
                "auth",
                "read_only",
            ],
            &[vec![
                profile.name.clone(),
                profile.provider.to_string(),
                profile.base_url.clone(),
                profile.api_path.clone(),
                doctor_auth_kind(profile).to_string(),
                profile.read_only.to_string(),
            ]],
        );
    }

    let rows = report
        .checks
        .iter()
        .map(|check| {
            vec![
                check.name.clone(),
                doctor_status_label(check.status).to_string(),
                check.details.clone(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["check", "status", "details"], &rows);
    println!(
        "Doctor summary: {} passed, {} warned, {} failed",
        report.summary.passed, report.summary.warned, report.summary.failed
    );
    Ok(())
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
    eprint!("{prompt} [y/N] ");
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    if input.trim().eq_ignore_ascii_case("y") {
        Ok(())
    } else {
        bail!("aborted")
    }
}

fn print_status(output: OutputFormat, value: Value, text: &str) -> Result<()> {
    if matches!(output, OutputFormat::Json) {
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
}
