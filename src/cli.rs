use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};

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

#[derive(Args, Debug)]
struct SearchArgs {
    query: String,
    #[arg(long)]
    cql: bool,
    #[arg(long)]
    space: Option<String>,
    #[arg(long, default_value_t = 20)]
    limit: usize,
}

#[derive(Subcommand, Debug)]
enum PageCommand {
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
    Create(WriteContentArgs),
    Update(UpdateContentArgs),
    Delete {
        reference: String,
    },
}

#[derive(Subcommand, Debug)]
enum BlogCommand {
    Get {
        id: String,
        #[arg(long)]
        show_body: bool,
    },
    Create(WriteContentArgs),
    Update(UpdateContentArgs),
    Delete {
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum PullCommand {
    Page { reference: String, output: PathBuf },
    Tree { reference: String, output: PathBuf },
    Space { space: String, output: PathBuf },
}

#[derive(Args, Debug)]
struct PlanArgs {
    path: PathBuf,
    #[arg(long)]
    allow_lossy: bool,
    #[arg(long)]
    delete_remote: bool,
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
    Delete {
        comment_id: String,
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
        Commands::Plan(args) => {
            let plan = sync::plan_path(&args.path, args.allow_lossy, args.delete_remote)?;
            render_plan(&plan, output)
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
            render_plan(&plan, output)
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
    let query = if let Some(space) = args.space {
        if args.cql {
            format!("space = \"{space}\" and ({})", args.query)
        } else {
            format!(
                "space = \"{space}\" and text ~ \"{}\"",
                args.query.replace('"', "\\\"")
            )
        }
    } else {
        args.query
    };
    let results = provider.search(&query, args.cql, args.limit).await?;
    render_search_results(&results, output)?;
    Ok(())
}

async fn handle_page(
    provider: &dyn ConfluenceProvider,
    command: PageCommand,
    output: OutputFormat,
) -> Result<()> {
    match command {
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
        PageCommand::Delete { reference } => {
            let id = provider.resolve_page_ref(&reference).await?;
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
        BlogCommand::Delete { id } => {
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
        PullCommand::Space { space, output } => sync::pull_space(provider, &space, &output).await?,
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
        } => {
            let id = provider.resolve_page_ref(&reference).await?;
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
        CommentCommand::Delete { comment_id } => {
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

fn render_plan(plan: &SyncPlan, output: OutputFormat) -> Result<()> {
    if matches!(output, OutputFormat::Json) {
        print_json(plan)
    } else {
        let rows = plan
            .items
            .iter()
            .map(|item| {
                vec![
                    match item.action {
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
                    .to_string(),
                    item.title.clone(),
                    item.content_id.clone().unwrap_or_default(),
                    item.path.display().to_string(),
                    item.details.clone(),
                ]
            })
            .collect::<Vec<_>>();
        print_table(&["action", "title", "id", "path", "details"], &rows);
        Ok(())
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
