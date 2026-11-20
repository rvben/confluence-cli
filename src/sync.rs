use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use pathdiff::diff_paths;
use regex::Regex;
use similar::{ChangeTag, TextDiff};
use tempfile::Builder;
use walkdir::WalkDir;

use crate::markdown::{
    Frontmatter, LocalDocument, PULL_FAILURES_DIR, PULL_FAILURES_MANIFEST, PageLinkPlaceholder,
    Sidecar, UserMentionPlaceholder, build_page_placeholder_url, build_storage_link_body,
    decode_character_references, document_dir_name, encode_target_segment, has_generated_label,
    is_local_target, link_body_unless_default, link_display_attributes, load_document,
    parse_page_placeholder_url, parse_status_placeholder_url, parse_user_placeholder_url,
    rewrite_outside_cdata, rewrite_outside_literal_sections, save_document, scan_local_documents,
    sha256_hex, split_target_fragment, storage_to_markdown,
};
use crate::model::{
    AttachmentState, ContentItem, ContentKind, PlanActionKind, PlanItem, SyncPlan, UserReference,
};
use crate::provider::ConfluenceProvider;

#[derive(Clone, Debug)]
struct LinkTarget {
    markdown_path: PathBuf,
    directory: PathBuf,
    content_id: Option<String>,
    title: String,
    space_key: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct LinkIndex {
    by_markdown_path: BTreeMap<PathBuf, LinkTarget>,
    by_directory: BTreeMap<PathBuf, LinkTarget>,
}

/// What `plan` says of a page whose pulled Markdown converts to storage that
/// differs from the remote storage: updating the page publishes that
/// difference along with the change.
const REWRITES_REMOTE_MARKUP: &str =
    "also rewrites markup of this page that Markdown does not carry as it is stored";

/// A page that a pull wrote as a local document.
#[derive(Debug, Clone)]
pub struct PulledPage {
    pub path: PathBuf,
    /// See [`Sidecar::storage_differs_from_remote`].
    pub storage_differs_from_remote: bool,
}

/// A page that a pull fetched and could not write as a local document.
#[derive(Debug)]
struct UnconvertiblePage {
    content_id: Option<String>,
    title: String,
    error: String,
    /// Where the generated Markdown, the storage it converts to and the
    /// remote storage are kept for inspection.
    artifacts: PathBuf,
    /// Whether the Markdown converted far enough to keep its storage.
    generated_storage: bool,
}

/// A page as fetched, on its way to becoming a local document.
#[derive(Clone)]
struct FetchedPage {
    document: LocalDocument,
    remote_storage: String,
}

pub async fn pull_page(
    provider: &dyn ConfluenceProvider,
    reference: &str,
    root: &Path,
    recursive: bool,
    force: bool,
) -> Result<Vec<PulledPage>> {
    validate_pull_destination(root, force)?;
    let root_id = provider.resolve_page_ref(reference).await?;
    let root_item = provider
        .get_content(ContentKind::Page, &root_id, true)
        .await?;
    let mut items = vec![root_item];
    if recursive {
        let children = provider.list_children(&root_id, true).await?;
        items.extend(children);
    }
    pull_items(provider, root, items).await
}

pub async fn pull_space(
    provider: &dyn ConfluenceProvider,
    space: &str,
    root: &Path,
    force: bool,
) -> Result<Vec<PulledPage>> {
    validate_pull_destination(root, force)?;
    let mut items = provider
        .list_space_content(ContentKind::Page, space)
        .await?;
    items.extend(
        provider
            .list_space_content(ContentKind::BlogPost, space)
            .await?,
    );
    pull_items(provider, root, items).await
}

pub async fn pull_space_since(
    provider: &dyn ConfluenceProvider,
    space: &str,
    root: &Path,
    since: &str,
    force: bool,
) -> Result<Vec<PulledPage>> {
    validate_pull_destination(root, force)?;
    validate_filtered_pull_destination(root)?;
    let space = crate::provider::escape_cql_literal(space);
    let since = crate::provider::escape_cql_literal(since);
    let cql = format!(r#"space = "{space}" AND lastModified > "{since}""#);
    let mut offset = 0usize;
    let mut listed = Vec::new();
    loop {
        let page = provider.search(&cql, true, 200, offset).await?;
        let count = page.items.len();
        listed.extend(page.items);
        offset += count;
        if count < 200 || page.total.is_some_and(|total| offset >= total) {
            break;
        }
    }
    let mut items = Vec::with_capacity(listed.len());
    for result in listed {
        items.push(provider.get_content(result.kind, &result.id, true).await?);
    }
    pull_items(provider, root, items).await
}

pub fn plan_path(
    root: &Path,
    allow_lossy: bool,
    delete_remote: bool,
    show_diff: bool,
) -> Result<SyncPlan> {
    let docs = load_required_local_documents(root)?;
    let indexes = scan_local_documents(root)?;
    let mut parent_ids = BTreeMap::new();
    for doc in &indexes {
        if let Some(content_id) = &doc.content_id {
            parent_ids.insert(doc.directory.clone(), content_id.clone());
        }
    }
    let link_index = build_link_index(&docs);

    let mut plan = SyncPlan::default();
    for doc in docs {
        validate_informational_parent(&doc)?;
        let web_path_prefix = effective_web_path_prefix(&doc.sidecar, None);
        let storage = render_body_storage(&doc, &link_index, allow_lossy, &web_path_prefix)?;
        let storage_hash = sha256_hex(storage.as_bytes());
        let current_hash = markdown_body_hash(&doc.body_markdown);
        let derived_parent_id =
            derive_parent_id_from_index(&doc.directory, &doc.sidecar, &parent_ids);

        if doc.sidecar.content_id.is_none() {
            plan.items.push(PlanItem {
                action: PlanActionKind::CreateContent,
                title: doc.frontmatter.title.clone(),
                content_id: None,
                path: doc.directory.clone(),
                details: "new local document".to_string(),
                diff: None,
            });
        } else {
            let mut changes = Vec::new();
            let body_is_changed = body_changed(&doc.sidecar, &current_hash, &storage_hash);
            if body_is_changed {
                changes.push("body");
            }
            if doc.sidecar.last_pulled_hash.as_deref() != Some(current_hash.as_str())
                && doc.sidecar.storage_hash.as_deref() != Some(storage_hash.as_str())
            {
                changes.push("markdown");
            }
            if doc.sidecar.remote_parent_id != derived_parent_id {
                changes.push("parent");
                plan.items.push(PlanItem {
                    action: PlanActionKind::MoveContent,
                    title: doc.frontmatter.title.clone(),
                    content_id: doc.sidecar.content_id.clone(),
                    path: doc.directory.clone(),
                    details: format!(
                        "parent changed from {:?} to {:?}",
                        doc.sidecar.remote_parent_id, derived_parent_id
                    ),
                    diff: None,
                });
            }

            // Detect metadata changes not reflected in body hashes.
            // These fields are only present in sidecars written by versions that
            // track metadata. Skip comparison when absent so old sidecars remain
            // backward-compatible and don't generate spurious plan items.
            if let Some(last_title) = &doc.sidecar.last_pulled_title
                && last_title != &doc.frontmatter.title
            {
                changes.push("title");
            }
            if doc.sidecar.last_pulled_title.is_some() {
                // Labels tracking is gated on title being present (same sidecar version).
                let mut sidecar_labels = doc.sidecar.last_pulled_labels.clone();
                sidecar_labels.sort();
                let mut local_labels = doc.frontmatter.labels.clone();
                local_labels.sort();
                if sidecar_labels != local_labels {
                    changes.push("labels");
                }
            }
            if let Some(last_status) = &doc.sidecar.last_pulled_status
                && last_status != &doc.frontmatter.status
            {
                changes.push("status");
            }
            let props_hash = properties_hash(&doc.frontmatter.properties);
            if let Some(last_props_hash) = &doc.sidecar.last_pulled_properties_hash
                && last_props_hash != &props_hash
            {
                changes.push("properties");
            }

            if !changes.is_empty() {
                let diff = if show_diff && body_is_changed {
                    Some(compute_body_diff(&doc.sidecar, &doc.body_markdown))
                } else {
                    None
                };
                let rewrite = if doc.sidecar.storage_differs_from_remote {
                    format!("; {REWRITES_REMOTE_MARKUP}")
                } else {
                    String::new()
                };
                plan.items.push(PlanItem {
                    action: PlanActionKind::UpdateContent,
                    title: doc.frontmatter.title.clone(),
                    content_id: doc.sidecar.content_id.clone(),
                    path: doc.directory.clone(),
                    details: format!("changed: {}{rewrite}", changes.join(", ")),
                    diff,
                });
            } else if doc.sidecar.storage_differs_from_remote {
                plan.items.push(PlanItem {
                    action: PlanActionKind::Noop,
                    title: doc.frontmatter.title.clone(),
                    content_id: doc.sidecar.content_id.clone(),
                    path: doc.directory.clone(),
                    details: format!(
                        "no changes detected; the first update {REWRITES_REMOTE_MARKUP}"
                    ),
                    diff: None,
                });
            }
        }

        let local_attachments = local_attachment_hashes(&doc.directory)?;
        let known = doc.sidecar.attachment_map.clone();
        for (file_name, hash) in &local_attachments {
            if known
                .get(file_name)
                .and_then(|attachment| attachment.sha256.as_deref())
                != Some(hash.as_str())
            {
                plan.items.push(PlanItem {
                    action: PlanActionKind::UploadAttachment,
                    title: file_name.clone(),
                    content_id: doc.sidecar.content_id.clone(),
                    path: doc.directory.join("attachments").join(file_name),
                    details: "new or changed local attachment".to_string(),
                    diff: None,
                });
            }
        }
        if delete_remote {
            let local_names: BTreeSet<_> = local_attachments.keys().cloned().collect();
            for (file_name, attachment) in known {
                if !local_names.contains(&file_name) {
                    plan.items.push(PlanItem {
                        action: PlanActionKind::DeleteAttachment,
                        title: file_name,
                        content_id: Some(attachment.id),
                        path: doc.directory.join("attachments"),
                        details: "attachment removed locally".to_string(),
                        diff: None,
                    });
                }
            }
        }
    }

    if plan.items.is_empty() {
        plan.items.push(PlanItem {
            action: PlanActionKind::Noop,
            title: root.display().to_string(),
            // Bug 3 fix: noop plan is only emitted when all docs are no-ops, so
            // content_id remains None here (plan is per-root, not per-page).
            content_id: None,
            path: root.to_path_buf(),
            details: "no changes detected".to_string(),
            diff: None,
        });
    }

    Ok(plan)
}

/// Hash the properties map deterministically for change detection.
fn properties_hash(properties: &BTreeMap<String, serde_json::Value>) -> String {
    let serialized = serde_json::to_string(properties).unwrap_or_default();
    sha256_hex(serialized.as_bytes())
}

fn validate_informational_parent(doc: &LocalDocument) -> Result<()> {
    if let Some(previous) = &doc.sidecar.last_pulled_parent
        && previous != &doc.frontmatter.parent
    {
        return Err(crate::output::typed_error_with_hint(
            crate::output::ErrorKind::InvalidInput,
            format!(
                "frontmatter parent is informational and was edited in {}",
                doc.markdown_path.display()
            ),
            "restore the pulled parent value and move the page directory beneath its desired local parent before applying",
        ));
    }
    Ok(())
}

fn normalized_labels(labels: &[String]) -> BTreeSet<&str> {
    labels.iter().map(String::as_str).collect()
}

fn desired_status(status: &str) -> &str {
    if status.is_empty() { "current" } else { status }
}

fn remote_metadata_drift(doc: &LocalDocument, remote: &ContentItem) -> Vec<&'static str> {
    let mut drift = Vec::new();
    // Metadata endpoints do not consistently bump the content version, so use
    // the pulled values as a three-way-merge base. A matching local/remote
    // value is safe even when both changed independently to the same value.
    if doc.sidecar.last_pulled_title.is_some()
        && normalized_labels(&remote.labels) != normalized_labels(&doc.sidecar.last_pulled_labels)
        && normalized_labels(&remote.labels) != normalized_labels(&doc.frontmatter.labels)
    {
        drift.push("labels");
    }
    if let Some(base) = doc.sidecar.last_pulled_status.as_deref()
        && remote.status != base
        && remote.status != desired_status(&doc.frontmatter.status)
    {
        drift.push("status");
    }
    if let Some(base_hash) = doc.sidecar.last_pulled_properties_hash.as_deref()
        && properties_hash(&remote.properties) != base_hash
        && remote.properties != doc.frontmatter.properties
    {
        drift.push("properties");
    }
    drift
}

fn metadata_needs_update(doc: &LocalDocument, remote: &ContentItem) -> bool {
    remote.title != doc.frontmatter.title
        || remote.status != desired_status(&doc.frontmatter.status)
        || normalized_labels(&remote.labels) != normalized_labels(&doc.frontmatter.labels)
        || remote.properties != doc.frontmatter.properties
}

fn take_create_properties(frontmatter: &mut Frontmatter) -> BTreeMap<String, serde_json::Value> {
    let mut properties = frontmatter.properties.clone();
    properties.remove("space_key");
    frontmatter.properties.remove("space_key");
    properties
}

fn attachment_bytes_conflict(
    base_hash: Option<&str>,
    local_hash: Option<&String>,
    remote_hash: &str,
) -> bool {
    base_hash.is_some_and(|base| base != remote_hash)
        && local_hash.is_none_or(|local| local != remote_hash)
}

/// Build a unified diff between the last-pulled markdown body and the current one.
fn compute_body_diff(sidecar: &Sidecar, current_body: &str) -> String {
    let old_body = sidecar
        .last_pulled_body_markdown
        .as_deref()
        .unwrap_or_default();
    let diff = TextDiff::from_lines(old_body, current_body);
    let mut output = String::new();
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        output.push_str(&format!("{sign}{change}"));
    }
    output
}

pub async fn apply_path(
    provider: &dyn ConfluenceProvider,
    root: &Path,
    allow_lossy: bool,
    delete_remote: bool,
    force: bool,
) -> Result<SyncPlan> {
    let mut docs = load_required_local_documents(root)?;
    docs.sort_by_key(|doc| doc.directory.components().count());
    let mut link_index = build_link_index(&docs);
    let web_path_prefix = provider.web_path_prefix();

    // Validate the complete collection and every expected remote version before
    // making the first remote mutation. Versioned update calls still protect
    // against drift that happens after this preflight.
    let mut preflight_remotes = BTreeMap::new();
    for doc in &docs {
        validate_informational_parent(doc)?;
        render_body_storage(doc, &link_index, allow_lossy, web_path_prefix.as_str())?;
        derive_parent_id_from_fs(&doc.directory, &doc.sidecar)?;
        local_attachment_hashes(&doc.directory)?;
        if let Some(content_id) = &doc.sidecar.content_id {
            let remote = provider
                .get_content(doc.frontmatter.content_kind(), content_id, false)
                .await?;
            if !force {
                if doc.sidecar.remote_version != remote.version {
                    return Err(remote_drift_error(doc, content_id, remote.version));
                }
                let metadata_drift = remote_metadata_drift(doc, &remote);
                if !metadata_drift.is_empty() {
                    return Err(remote_metadata_drift_error(
                        doc,
                        content_id,
                        &metadata_drift,
                    ));
                }
                validate_remote_attachment_drift(provider, doc, content_id, delete_remote).await?;
            }
            preflight_remotes.insert(content_id.clone(), remote);
        } else {
            let has_space = doc.sidecar.space_key.is_some()
                || doc
                    .frontmatter
                    .properties
                    .get("space_key")
                    .and_then(|value| value.as_str())
                    .is_some();
            if !has_space {
                return Err(crate::output::typed_error_with_hint(
                    crate::output::ErrorKind::InvalidInput,
                    format!(
                        "document {} is missing space_key metadata",
                        doc.directory.display()
                    ),
                    "set `space_key` in frontmatter properties or pull an existing page before applying",
                ));
            }
        }
    }

    let mut applied = SyncPlan::default();
    for doc in &mut docs {
        let space_key = doc.sidecar.space_key.clone().or_else(|| {
            doc.frontmatter
                .properties
                .get("space_key")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned)
        });
        let derived_parent_id = derive_parent_id_from_fs(&doc.directory, &doc.sidecar)?;

        let body_storage =
            render_body_storage(doc, &link_index, allow_lossy, web_path_prefix.as_str())?;
        let storage_hash = sha256_hex(body_storage.as_bytes());
        let markdown_hash = markdown_body_hash(&doc.body_markdown);
        let kind = doc.frontmatter.content_kind();
        let title = doc.frontmatter.title.clone();

        let mut body_published = true;
        let content = if let Some(content_id) = doc.sidecar.content_id.clone() {
            let remote = preflight_remotes.remove(&content_id).ok_or_else(|| {
                crate::output::typed_error(
                    crate::output::ErrorKind::Unexpected,
                    format!("missing preflight state for remote content {content_id}"),
                )
            })?;
            let needs_update = body_changed(&doc.sidecar, &markdown_hash, &storage_hash)
                || doc.sidecar.remote_parent_id != derived_parent_id
                || metadata_needs_update(doc, &remote);
            if needs_update {
                let version = remote.version.ok_or_else(|| {
                    crate::output::typed_error(
                        crate::output::ErrorKind::Api,
                        format!("remote content {content_id} has no version"),
                    )
                })?;
                let updated = provider
                    .update_content(&crate::model::UpdateContentRequest {
                        id: content_id.clone(),
                        kind,
                        title: title.clone(),
                        parent_id: derived_parent_id.clone(),
                        body_storage: body_storage.clone(),
                        version,
                        message: Some("Updated via confluence-cli".to_string()),
                        status: desired_status(&doc.frontmatter.status).to_string(),
                        labels: doc.frontmatter.labels.clone(),
                        properties: doc.frontmatter.properties.clone(),
                    })
                    .await
                    .map_err(|error| partial_apply_error(error, &applied))?;
                applied.items.push(PlanItem {
                    action: PlanActionKind::UpdateContent,
                    title: title.clone(),
                    content_id: Some(updated.id.clone()),
                    path: doc.directory.clone(),
                    details: "remote content updated".to_string(),
                    diff: None,
                });
                updated
            } else {
                body_published = false;
                remote
            }
        } else {
            let space_key = space_key.clone().ok_or_else(|| {
                anyhow!(
                    "document {} is missing space_key metadata. Set `space_key` in frontmatter properties or pull from an existing Confluence page/blog before applying",
                    doc.directory.display(),
                )
            })?;
            // Strip the space_key bootstrap property — it is a local-only
            // sentinel used to specify the target space for new pages and must
            // not be written as a Confluence page property.
            let create_properties = take_create_properties(&mut doc.frontmatter);
            // `space_key` is only a bootstrap hint for a new local document.
            // Once an ID exists the sidecar owns the space association, so
            // remove the hint before any later reconciliation update can leak
            // it into Confluence as a content property.
            let created = provider
                .create_content(&crate::model::CreateContentRequest {
                    kind,
                    title: title.clone(),
                    space: space_key.clone(),
                    parent_id: derived_parent_id.clone(),
                    body_storage: body_storage.clone(),
                    status: desired_status(&doc.frontmatter.status).to_string(),
                    labels: doc.frontmatter.labels.clone(),
                    properties: create_properties,
                })
                .await
                .map_err(|error| partial_apply_error(error, &applied))?;
            applied.items.push(PlanItem {
                action: PlanActionKind::CreateContent,
                title: title.clone(),
                content_id: Some(created.id.clone()),
                path: doc.directory.clone(),
                details: "remote content created".to_string(),
                diff: None,
            });
            created
        };

        doc.sidecar.content_id = Some(content.id.clone());
        link_index.set_content_id(&doc.directory, &content.id);
        sync_attachments(provider, doc, &content.id, delete_remote, &mut applied)
            .await
            .map_err(|error| partial_apply_error(error, &applied))?;

        doc.sidecar.provider = Some(provider.kind());
        doc.sidecar.web_path_prefix = Some(web_path_prefix.clone());
        doc.sidecar.space_key = content.space_key.clone().or(space_key);
        doc.sidecar.remote_version = content.version;
        doc.sidecar.remote_parent_id = derived_parent_id;
        doc.sidecar.last_pulled_hash = Some(markdown_hash);
        doc.sidecar.storage_hash = Some(storage_hash.clone());
        doc.sidecar.remote_storage_hash = Some(storage_hash);
        if body_published {
            doc.sidecar.storage_differs_from_remote = false;
        }
        doc.sidecar.last_sync_at = Some(Utc::now());
        // Track metadata so plan can detect future title/label/status/properties changes.
        doc.sidecar.last_pulled_title = Some(doc.frontmatter.title.clone());
        doc.sidecar.last_pulled_labels = doc.frontmatter.labels.clone();
        doc.sidecar.last_pulled_status = Some(desired_status(&doc.frontmatter.status).to_string());
        doc.sidecar.last_pulled_properties_hash =
            Some(properties_hash(&doc.frontmatter.properties));
        doc.sidecar.last_pulled_parent = Some(doc.frontmatter.parent.clone());
        doc.sidecar.last_pulled_body_markdown = Some(doc.body_markdown.clone());
        save_document(doc).map_err(|error| partial_apply_error(error, &applied))?;
    }

    reconcile_local_link_updates(
        provider,
        &mut docs,
        &link_index,
        allow_lossy,
        web_path_prefix.as_str(),
        force,
        &mut applied,
    )
    .await
    .map_err(|error| partial_apply_error(error, &applied))?;

    if applied.items.is_empty() {
        applied.items.push(PlanItem {
            action: PlanActionKind::Noop,
            title: root.display().to_string(),
            content_id: None,
            path: root.to_path_buf(),
            details: "no remote changes applied".to_string(),
            diff: None,
        });
    }

    Ok(applied)
}

fn partial_apply_error(error: anyhow::Error, applied: &SyncPlan) -> anyhow::Error {
    let (kind, hint) = crate::output::classify_anyhow(&error);
    let cause_details = error.chain().find_map(|cause| {
        cause
            .downcast_ref::<crate::output::CliError>()
            .and_then(|typed| typed.details.clone())
    });
    let source_partial = cause_details
        .as_ref()
        .and_then(|details| details.get("partial_success"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    crate::output::typed_error_with_details(
        kind,
        format!(
            "apply failed after {} completed remote action(s): {error:#}",
            applied.items.len()
        ),
        hint,
        serde_json::json!({
            "operation": "apply",
            "partial_success": !applied.items.is_empty() || source_partial,
            "completed_count": applied.items.len(),
            "completed_items": &applied.items,
            "cause": cause_details,
        }),
    )
}

fn remote_drift_error(
    doc: &LocalDocument,
    content_id: &str,
    remote_version: Option<u64>,
) -> anyhow::Error {
    crate::output::typed_error_with_hint(
        crate::output::ErrorKind::Conflict,
        format!(
            "remote version drift for `{}` at {} (content {}): local sidecar version {:?}, remote {:?}",
            doc.frontmatter.title,
            doc.directory.display(),
            content_id,
            doc.sidecar.remote_version,
            remote_version
        ),
        "run `confluence pull ...` to refresh local metadata, or rerun `confluence apply ... --force` if local content should win",
    )
}

fn remote_metadata_drift_error(
    doc: &LocalDocument,
    content_id: &str,
    fields: &[&str],
) -> anyhow::Error {
    crate::output::typed_error_with_hint(
        crate::output::ErrorKind::Conflict,
        format!(
            "remote metadata drift for `{}` at {} (content {}): {} changed since the last pull",
            doc.frontmatter.title,
            doc.directory.display(),
            content_id,
            fields.join(", ")
        ),
        "pull a fresh snapshot and merge the remote metadata, or rerun `confluence apply ... --force` if local metadata should win",
    )
}

async fn validate_remote_attachment_drift(
    provider: &dyn ConfluenceProvider,
    doc: &LocalDocument,
    content_id: &str,
    delete_remote: bool,
) -> Result<()> {
    let local = local_attachment_hashes(&doc.directory)?;
    let remote = provider.list_attachments(content_id).await?;
    let remote_by_title = remote
        .iter()
        .map(|attachment| (attachment.title.as_str(), attachment))
        .collect::<BTreeMap<_, _>>();

    for (title, previous) in &doc.sidecar.attachment_map {
        let local_hash = local.get(&previous.file_name);
        let Some(current) = remote_by_title.get(title.as_str()) else {
            if local_hash.is_some() {
                return Err(remote_attachment_drift_error(doc, content_id, title));
            }
            continue;
        };
        let bytes = provider
            .download_attachment(content_id, &current.id)
            .await?;
        let remote_hash = sha256_hex(&bytes);
        if attachment_bytes_conflict(previous.sha256.as_deref(), local_hash, &remote_hash) {
            return Err(remote_attachment_drift_error(doc, content_id, title));
        }
    }

    if delete_remote {
        for attachment in &remote {
            if !doc.sidecar.attachment_map.contains_key(&attachment.title)
                && !local.contains_key(&attachment.title)
            {
                return Err(remote_attachment_drift_error(
                    doc,
                    content_id,
                    &attachment.title,
                ));
            }
        }
    }
    Ok(())
}

fn remote_attachment_drift_error(
    doc: &LocalDocument,
    content_id: &str,
    title: &str,
) -> anyhow::Error {
    crate::output::typed_error_with_hint(
        crate::output::ErrorKind::Conflict,
        format!(
            "remote attachment drift for `{title}` on `{}` at {} (content {content_id})",
            doc.frontmatter.title,
            doc.directory.display(),
        ),
        "pull a fresh snapshot and merge the attachment, or rerun `confluence apply ... --force` if the local file should win",
    )
}

async fn reconcile_local_link_updates(
    provider: &dyn ConfluenceProvider,
    docs: &mut [LocalDocument],
    link_index: &LinkIndex,
    allow_lossy: bool,
    web_path_prefix: &str,
    force: bool,
    applied: &mut SyncPlan,
) -> Result<()> {
    for doc in docs {
        let Some(content_id) = doc.sidecar.content_id.clone() else {
            continue;
        };

        let final_storage = render_body_storage(doc, link_index, allow_lossy, web_path_prefix)?;
        let final_hash = sha256_hex(final_storage.as_bytes());
        if doc.sidecar.storage_hash.as_deref() == Some(final_hash.as_str()) {
            continue;
        }

        let parent_id = derive_parent_id_from_fs(&doc.directory, &doc.sidecar)?;
        let remote = provider
            .get_content(doc.frontmatter.content_kind(), &content_id, false)
            .await?;
        if !force && doc.sidecar.remote_version != remote.version {
            return Err(remote_drift_error(doc, &content_id, remote.version));
        }
        let version = remote.version.ok_or_else(|| {
            crate::output::typed_error(
                crate::output::ErrorKind::Api,
                format!("remote content {content_id} has no version"),
            )
        })?;
        let updated = provider
            .update_content(&crate::model::UpdateContentRequest {
                id: content_id.clone(),
                kind: doc.frontmatter.content_kind(),
                title: doc.frontmatter.title.clone(),
                parent_id,
                body_storage: final_storage,
                version,
                message: Some("Resolved local links via confluence-cli".to_string()),
                status: if doc.frontmatter.status.is_empty() {
                    "current".to_string()
                } else {
                    doc.frontmatter.status.clone()
                },
                labels: doc.frontmatter.labels.clone(),
                properties: doc.frontmatter.properties.clone(),
            })
            .await?;
        applied.items.push(PlanItem {
            action: PlanActionKind::UpdateContent,
            title: doc.frontmatter.title.clone(),
            content_id: Some(updated.id.clone()),
            path: doc.directory.clone(),
            details: "remote content updated after local link resolution".to_string(),
            diff: None,
        });
        doc.sidecar.remote_version = updated.version;
        doc.sidecar.storage_hash = Some(final_hash.clone());
        doc.sidecar.remote_storage_hash = Some(final_hash);
        doc.sidecar.storage_differs_from_remote = false;
        doc.sidecar.last_pulled_hash = Some(markdown_body_hash(&doc.body_markdown));
        doc.sidecar.last_pulled_body_markdown = Some(doc.body_markdown.clone());
        doc.sidecar.last_sync_at = Some(Utc::now());
        save_document(doc)?;
    }

    Ok(())
}

async fn pull_items(
    provider: &dyn ConfluenceProvider,
    root: &Path,
    items: Vec<ContentItem>,
) -> Result<Vec<PulledPage>> {
    let parent = root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let staging = Builder::new()
        .prefix(".confluence-pull-")
        .tempdir_in(parent)
        .with_context(|| format!("failed to stage pull beside {}", root.display()))?;
    let staged_root = staging.path().join("snapshot");
    let (written, unconvertible, warnings) = write_items(provider, &staged_root, items).await?;
    install_pulled_pages(&staged_root, root, written, unconvertible, warnings)
}

/// Moves a staged snapshot to its destination and reports its pages there.
/// The snapshot is installed also when some pages were left out, and the
/// result is then the error that names them. That error carries the
/// warnings of the pull, so that it is all the pull reports; a pull that
/// leaves nothing out prints them.
fn install_pulled_pages(
    staged_root: &Path,
    root: &Path,
    mut written: Vec<PulledPage>,
    mut unconvertible: Vec<UnconvertiblePage>,
    warnings: Vec<String>,
) -> Result<Vec<PulledPage>> {
    install_staged_snapshot(staged_root, root)?;

    let installed = |path: &Path| -> Result<PathBuf> {
        let relative = path.strip_prefix(staged_root).with_context(|| {
            format!(
                "staged path {} escaped snapshot {}",
                path.display(),
                staged_root.display()
            )
        })?;
        Ok(root.join(relative))
    };
    for page in &mut written {
        page.path = installed(&page.path)?;
    }
    for page in &mut unconvertible {
        page.artifacts = installed(&page.artifacts)?;
    }
    if unconvertible.is_empty() {
        for warning in &warnings {
            crate::output::print_message(&format!("warning: {warning}"));
        }
        return Ok(written);
    }
    Err(unconvertible_pages_error(
        &written,
        &unconvertible,
        &warnings,
    ))
}

/// Reports the pages a pull left out. The pages that convert are already
/// written, so the error names both.
fn unconvertible_pages_error(
    written: &[PulledPage],
    unconvertible: &[UnconvertiblePage],
    warnings: &[String],
) -> anyhow::Error {
    let summary = unconvertible
        .iter()
        .map(|page| {
            format!(
                "{} ({}): {}",
                page.content_id.as_deref().unwrap_or("unknown ID"),
                page.title,
                page.error,
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    crate::output::typed_error_with_details(
        crate::output::ErrorKind::InvalidInput,
        format!(
            "{} of {} pulled pages do not convert back to valid storage and were left out: {summary}",
            unconvertible.len(),
            written.len() + unconvertible.len(),
        ),
        Some(format!(
            "Pages written: {}. For each page left out, {} holds the remote.xml it came from, the generated.md made from it and, when that Markdown converts at all, the generated.xml it converts to; report them as a conversion bug.{}{}",
            written.len(),
            unconvertible
                .first()
                .and_then(|page| page.artifacts.parent())
                .map_or_else(
                    || PULL_FAILURES_DIR.to_string(),
                    |dir| dir.display().to_string()
                ),
            match written
                .iter()
                .filter(|page| page.storage_differs_from_remote)
                .count()
            {
                0 => String::new(),
                differing => format!(
                    " {differing} of {} pages written hold markup that Markdown does not carry as it is stored; the first update applied to such a page rewrites that markup. `confluence plan` names them.",
                    written.len()
                ),
            },
            warnings
                .iter()
                .map(|warning| format!(" Also: {warning}."))
                .collect::<String>(),
        )),
        serde_json::json!({
            "operation": "pull",
            "written": written
                .iter()
                .map(|page| serde_json::json!({
                    "path": page.path,
                    "storage_differs_from_remote": page.storage_differs_from_remote,
                }))
                .collect::<Vec<_>>(),
            "warnings": warnings,
            "pages": unconvertible
                .iter()
                .map(|page| serde_json::json!({
                    "content_id": page.content_id,
                    "title": page.title,
                    "error": page.error,
                    "artifacts": page.artifacts,
                    "files": PULL_FAILURE_ARTIFACTS
                        .iter()
                        .filter(|name| page.generated_storage || **name != "generated.xml")
                        .collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
        }),
    )
}

fn install_staged_snapshot(staged_root: &Path, root: &Path) -> Result<()> {
    let parent = root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let backup = Builder::new()
        .prefix(".confluence-backup-")
        .tempdir_in(parent)
        .with_context(|| format!("failed to create backup beside {}", root.display()))?;
    let previous = backup.path().join("previous");
    let had_previous = root.exists();
    if had_previous {
        fs::rename(root, &previous)
            .with_context(|| format!("failed to stage existing destination {}", root.display()))?;
    }
    if let Err(error) = fs::rename(staged_root, root) {
        if had_previous && let Err(restore_error) = fs::rename(&previous, root) {
            let recovery_directory = backup.keep();
            let recovery_path = recovery_directory.join("previous");
            return Err(crate::output::typed_error_with_details(
                crate::output::ErrorKind::Unexpected,
                format!(
                    "failed to install pulled snapshot at {} and failed to restore the previous destination; the previous snapshot is preserved at {}: install error: {error}; restore error: {restore_error}",
                    root.display(),
                    recovery_path.display(),
                ),
                Some(format!(
                    "move `{}` back to `{}` after resolving the filesystem error",
                    recovery_path.display(),
                    root.display()
                )),
                serde_json::json!({
                    "operation": "install_pull_snapshot",
                    "destination": root,
                    "recovery_path": recovery_path,
                    "install_error": error.to_string(),
                    "restore_error": restore_error.to_string(),
                }),
            ));
        }
        return Err(error)
            .with_context(|| format!("failed to install pulled snapshot at {}", root.display()));
    }
    Ok(())
}

fn validate_pull_destination(root: &Path, force: bool) -> Result<()> {
    let destination_metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if destination_metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(crate::output::typed_error_with_hint(
            crate::output::ErrorKind::InvalidInput,
            format!("pull destination `{}` is a symbolic link", root.display()),
            "use the link's real target path explicitly",
        ));
    }
    if destination_metadata.is_none() || force {
        return Ok(());
    }
    if !destination_metadata.is_some_and(|metadata| metadata.is_dir()) {
        return Err(crate::output::typed_error(
            crate::output::ErrorKind::InvalidInput,
            format!("pull destination `{}` is not a directory", root.display()),
        ));
    }

    let failures_dir = root.join(PULL_FAILURES_DIR);
    if !holds_only_pull_failure_artifacts(&failures_dir)? {
        return Err(dirty_pull_destination_error(root));
    }
    let docs = load_local_documents(root)?;
    if docs.is_empty() {
        for entry in fs::read_dir(root)? {
            if entry?.path() != failures_dir {
                return Err(dirty_pull_destination_error(root));
            }
        }
        return Ok(());
    }

    // Missing pulled attachments are local changes even when a normal plan would
    // not schedule remote deletion without --delete-remote.
    let plan = plan_path(root, false, true, false)?;
    if plan
        .items
        .iter()
        .any(|item| item.action != PlanActionKind::Noop)
    {
        return Err(dirty_pull_destination_error(root));
    }

    for entry in WalkDir::new(root) {
        let entry = entry?;
        if entry.path() == root || entry.path().starts_with(&failures_dir) {
            continue;
        }
        if entry.file_type().is_dir() {
            let managed_directory = docs.iter().any(|doc| {
                entry.path() == doc.directory
                    || entry.path() == doc.directory.join("attachments")
                    || doc.directory.starts_with(entry.path())
            });
            if !managed_directory {
                return Err(dirty_pull_destination_error(root));
            }
            continue;
        }
        let managed = docs.iter().any(|doc| {
            entry.file_type().is_file()
                && (entry.path() == doc.markdown_path
                    || entry.path() == doc.sidecar_path
                    || entry.path().parent() == Some(doc.directory.join("attachments").as_path()))
        });
        if !managed {
            return Err(dirty_pull_destination_error(root));
        }
    }
    Ok(())
}

/// The files a pull keeps for a page it leaves out.
const PULL_FAILURE_ARTIFACTS: [&str; 3] = ["generated.md", "generated.xml", "remote.xml"];

/// Records the files kept for the pages a pull left out, which is what lets
/// a later pull tell them from files of the user.
fn write_pull_failures_manifest(root: &Path, unconvertible: &[UnconvertiblePage]) -> Result<()> {
    let failures_dir = root.join(PULL_FAILURES_DIR);
    let mut manifest = BTreeMap::new();
    for page in unconvertible {
        let Some(directory) = page.artifacts.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        for name in PULL_FAILURE_ARTIFACTS {
            let path = page.artifacts.join(name);
            if path.is_file() {
                let contents = fs::read(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                manifest.insert(format!("{directory}/{name}"), sha256_hex(contents));
            }
        }
    }
    let path = failures_dir.join(PULL_FAILURES_MANIFEST);
    fs::write(&path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Whether a pull may replace `failures_dir`: it is absent, or it holds
/// only files its manifest records, each as the pull wrote it, and no
/// directory but the ones those files were written in. Anything else is
/// somebody's own data. A file that was removed leaves nothing to keep.
fn holds_only_pull_failure_artifacts(failures_dir: &Path) -> Result<bool> {
    let metadata = match fs::symlink_metadata(failures_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() {
        return Ok(false);
    }
    let manifest_path = failures_dir.join(PULL_FAILURES_MANIFEST);
    let is_file = |path: &Path| fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file());
    if !is_file(&manifest_path) {
        return Ok(false);
    }
    let Ok(manifest) =
        serde_json::from_slice::<BTreeMap<String, String>>(&fs::read(&manifest_path)?)
    else {
        return Ok(false);
    };
    for entry in WalkDir::new(failures_dir).min_depth(1) {
        let entry = entry?;
        if entry.path() == manifest_path {
            continue;
        }
        let Some(relative) = entry
            .path()
            .strip_prefix(failures_dir)
            .ok()
            .and_then(|relative| {
                relative
                    .iter()
                    .map(|part| part.to_str())
                    .collect::<Option<Vec<_>>>()
            })
            .map(|parts| parts.join("/"))
        else {
            return Ok(false);
        };
        let as_written = if entry.file_type().is_dir() {
            let below = format!("{relative}/");
            manifest.keys().any(|path| path.starts_with(&below))
        } else if entry.file_type().is_file() {
            manifest.get(&relative).is_some_and(|hash| {
                fs::read(entry.path()).is_ok_and(|read| sha256_hex(read) == *hash)
            })
        } else {
            false
        };
        if !as_written {
            return Ok(false);
        }
    }
    Ok(true)
}

fn validate_filtered_pull_destination(root: &Path) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let mut entries = fs::read_dir(root).with_context(|| {
        format!(
            "failed to inspect filtered pull destination {}",
            root.display()
        )
    })?;
    if entries.next().transpose()?.is_none() {
        return Ok(());
    }
    Err(crate::output::typed_error_with_hint(
        crate::output::ErrorKind::InvalidInput,
        format!(
            "filtered pull destination `{}` is not empty",
            root.display()
        ),
        "use a new empty destination for `pull space --since`; a filtered snapshot cannot safely replace a complete local export",
    ))
}

fn dirty_pull_destination_error(root: &Path) -> anyhow::Error {
    crate::output::typed_error_with_hint(
        crate::output::ErrorKind::Conflict,
        format!(
            "pull destination `{}` contains local changes or unmanaged files",
            root.display()
        ),
        "choose an empty destination, apply or preserve local changes first, or rerun pull with --force to replace the destination",
    )
}

async fn write_items(
    provider: &dyn ConfluenceProvider,
    root: &Path,
    items: Vec<ContentItem>,
) -> Result<(Vec<PulledPage>, Vec<UnconvertiblePage>, Vec<String>)> {
    let mut item_map = BTreeMap::new();
    for item in items {
        item_map.insert(item.id.clone(), item);
    }

    let paths = compute_paths(root, &item_map);
    let mut path_owners = BTreeMap::new();
    for (content_id, path) in &paths {
        if !path.starts_with(root) {
            return Err(crate::output::typed_error(
                crate::output::ErrorKind::Api,
                format!("remote content {content_id} produced a path outside the pull snapshot"),
            ));
        }
        if let Some(previous_id) = path_owners.insert(path.clone(), content_id.clone()) {
            return Err(crate::output::typed_error(
                crate::output::ErrorKind::Conflict,
                format!(
                    "remote content IDs {previous_id} and {content_id} map to the same local path {}",
                    path.display()
                ),
            ));
        }
    }
    let mut pulled = Vec::new();
    let mut user_names = BTreeMap::new();
    let mut names_available = true;
    let mut warnings = Vec::new();
    for item in item_map.values() {
        let Some(dir) = paths.get(&item.id) else {
            continue;
        };
        let attachments = provider.list_attachments(&item.id).await?;
        let attachments_dir = dir.join("attachments");
        fs::create_dir_all(&attachments_dir)
            .with_context(|| format!("failed to create {}", attachments_dir.display()))?;

        let mut attachment_map = BTreeMap::new();
        for attachment in &attachments {
            let file_name = safe_attachment_file_name(&attachment.title)?;
            let attachment_path = attachments_dir.join(file_name);
            let bytes = provider
                .download_attachment(&item.id, &attachment.id)
                .await?;
            fs::write(&attachment_path, &bytes)
                .with_context(|| format!("failed to write {}", attachment_path.display()))?;
            attachment_map.insert(
                attachment.title.clone(),
                AttachmentState {
                    id: attachment.id.clone(),
                    file_name: attachment.title.clone(),
                    remote_version: attachment.version,
                    media_type: attachment.media_type.clone(),
                    sha256: Some(sha256_hex(&bytes)),
                },
            );
        }

        let mut item = PulledItem {
            item,
            directory: dir,
            attachments,
            attachment_map,
            markdown_linking_all: String::new(),
        };
        item.markdown_linking_all = item.markdown(&paths, &item_map);
        for user in unnamed_mentions(&item.markdown_linking_all) {
            if let std::collections::btree_map::Entry::Vacant(entry) = user_names.entry(user) {
                // A name is a convenience for the reader, so a lookup that
                // fails leaves the identifier in place and ends the lookups,
                // and the pull goes on.
                let name = if names_available {
                    provider
                        .user_display_name(entry.key())
                        .await
                        .unwrap_or_else(|error| {
                            names_available = false;
                            warnings.push(format!(
                                "the names of mentioned users could not be looked up, so their mentions show identifiers: {error:#}"
                            ));
                            None
                        })
                } else {
                    None
                };
                entry.insert(name);
            }
        }
        pulled.push(item);
    }

    let provider_kind = provider.kind();
    let web_path_prefix = provider.web_path_prefix();
    let (documents, left_out) = convertible_pages(|left_out| {
        // A page that is left out has no local file, so a link to it stays
        // the remote address it was pulled as.
        let link_paths: BTreeMap<String, PathBuf> = paths
            .iter()
            .filter(|(_, directory)| !left_out.contains(*directory))
            .map(|(content_id, directory)| (content_id.clone(), directory.clone()))
            .collect();
        pulled
            .iter()
            .filter(|item| !left_out.contains(item.directory))
            .map(|item| {
                let markdown = if left_out.is_empty() {
                    std::borrow::Cow::Borrowed(item.markdown_linking_all.as_str())
                } else {
                    std::borrow::Cow::Owned(item.markdown(&link_paths, &item_map))
                };
                let body_markdown = name_user_mentions(&markdown, &user_names);
                item.fetched_page(
                    body_markdown,
                    &item_map,
                    provider_kind,
                    web_path_prefix.clone(),
                )
            })
            .collect()
    });
    let (written, unconvertible) = store_fetched_pages(root, documents, left_out)?;
    Ok((written, unconvertible, warnings))
}

/// An item of a pull with its attachments downloaded.
struct PulledItem<'a> {
    item: &'a ContentItem,
    directory: &'a Path,
    attachments: Vec<crate::model::AttachmentInfo>,
    attachment_map: BTreeMap<String, AttachmentState>,
    /// The item's Markdown with links to every page of the pull, which is
    /// what is written unless a page is left out.
    markdown_linking_all: String,
}

impl PulledItem<'_> {
    /// The item's body as Markdown, with links to the pages in `link_paths`
    /// and to its attachments pointing at their local files.
    fn markdown(
        &self,
        link_paths: &BTreeMap<String, PathBuf>,
        item_map: &BTreeMap<String, ContentItem>,
    ) -> String {
        let mut markdown =
            storage_to_markdown(self.item.body_storage.as_deref().unwrap_or_default());
        markdown = rewrite_page_links(&markdown, self.directory, link_paths, item_map);
        markdown = rewrite_generic_macro_page_placeholders(
            &markdown,
            self.directory,
            link_paths,
            item_map,
        );
        rewrite_attachment_links(&markdown, &self.attachments)
    }

    fn fetched_page(
        &self,
        body_markdown: String,
        item_map: &BTreeMap<String, ContentItem>,
        provider_kind: crate::model::ProviderKind,
        web_path_prefix: String,
    ) -> FetchedPage {
        let item = self.item;
        let frontmatter = Frontmatter {
            title: item.title.clone(),
            kind: item.kind.file_type().to_string(),
            labels: item.labels.clone(),
            status: item.status.clone(),
            parent: item
                .parent_id
                .as_ref()
                .and_then(|parent_id| item_map.get(parent_id))
                .map(|parent| parent.title.clone())
                .or_else(|| item.parent_id.clone()),
            properties: item.properties.clone(),
        };
        let props_hash = properties_hash(&item.properties);
        let sidecar = Sidecar {
            content_id: Some(item.id.clone()),
            space_key: item.space_key.clone(),
            provider: Some(provider_kind),
            web_path_prefix: Some(web_path_prefix),
            remote_version: item.version,
            remote_parent_id: item.parent_id.clone(),
            last_pulled_hash: Some(markdown_body_hash(&body_markdown)),
            storage_hash: None,
            remote_storage_hash: Some(sha256_hex(
                item.body_storage.as_deref().unwrap_or_default().as_bytes(),
            )),
            attachment_map: self.attachment_map.clone(),
            last_sync_at: Some(Utc::now()),
            last_pulled_title: Some(item.title.clone()),
            last_pulled_labels: item.labels.clone(),
            last_pulled_status: Some(item.status.clone()),
            last_pulled_properties_hash: Some(props_hash),
            last_pulled_parent: Some(frontmatter.parent.clone()),
            last_pulled_body_markdown: Some(body_markdown.clone()),
            storage_differs_from_remote: false,
        };
        FetchedPage {
            document: LocalDocument {
                directory: self.directory.to_path_buf(),
                markdown_path: self.directory.join("index.md"),
                sidecar_path: self.directory.join(".confluence.json"),
                frontmatter,
                body_markdown,
                sidecar,
            },
            remote_storage: item.body_storage.clone().unwrap_or_default(),
        }
    }
}

/// Converts the fetched pages back to storage and sorts them into the ones
/// to write and the ones to leave out. `render` gives the fetched pages
/// without those in the directories it is passed, linking to none of them.
/// It is asked again whenever more pages are left out, so that a page that is
/// written links to a local file only when that file is written too, and its
/// storage hash is the one a later plan computes.
fn convertible_pages(
    render: impl Fn(&BTreeSet<PathBuf>) -> Vec<FetchedPage>,
) -> (Vec<LocalDocument>, Vec<LeftOutPage>) {
    let mut directories = BTreeSet::new();
    let mut left_out = Vec::new();
    loop {
        let (documents, failures) = stamp_storage_hashes(render(&directories));
        if failures.is_empty() {
            return (documents, left_out);
        }
        directories.extend(
            failures
                .iter()
                .map(|failure| failure.page.document.directory.clone()),
        );
        left_out.extend(failures);
    }
}

/// Writes the pages that convert back to storage as local documents, and
/// keeps the rest for inspection outside the document tree.
fn store_fetched_pages(
    root: &Path,
    documents: Vec<LocalDocument>,
    left_out: Vec<LeftOutPage>,
) -> Result<(Vec<PulledPage>, Vec<UnconvertiblePage>)> {
    let mut written = Vec::with_capacity(documents.len());
    for doc in documents {
        save_document(&doc)?;
        written.push(PulledPage {
            path: doc.directory,
            storage_differs_from_remote: doc.sidecar.storage_differs_from_remote,
        });
    }
    let unconvertible = left_out
        .iter()
        .map(|page| keep_unconvertible_page(root, page))
        .collect::<Result<Vec<_>>>()?;
    if !unconvertible.is_empty() {
        write_pull_failures_manifest(root, &unconvertible)?;
    }
    // A page that is left out has no document, so its directory holds at
    // most the pages beneath it. The deepest goes first, so a directory whose
    // pages were all left out goes too, and so does a directory that only
    // groups pages, such as the one of the blog posts, once it is empty.
    let mut directories: Vec<&Path> = left_out
        .iter()
        .map(|left_out| left_out.page.document.directory.as_path())
        .collect();
    directories.sort_by_key(|directory| std::cmp::Reverse(directory.components().count()));
    for directory in directories {
        let attachments = directory.join("attachments");
        if attachments.exists() {
            fs::remove_dir_all(&attachments)
                .with_context(|| format!("failed to remove {}", attachments.display()))?;
        }
        let below_root = directory
            .ancestors()
            .take_while(|ancestor| *ancestor != root && ancestor.starts_with(root));
        for directory in below_root {
            if !directory.exists() || fs::read_dir(directory)?.next().is_some() {
                break;
            }
            fs::remove_dir(directory)
                .with_context(|| format!("failed to remove {}", directory.display()))?;
        }
    }

    Ok((written, unconvertible))
}

/// A fetched page whose Markdown does not convert back to valid storage.
struct LeftOutPage {
    page: FetchedPage,
    error: String,
    /// The storage the Markdown converts to, when conversion gets that far.
    generated_storage: Option<String>,
}

/// Converts the Markdown of each fetched page back to storage. A page that
/// converts gets the hash of that storage and whether it still says what the
/// remote storage says. A page that does not convert is returned with the
/// reason, so one such page does not keep the others from being written.
fn stamp_storage_hashes(fetched: Vec<FetchedPage>) -> (Vec<LocalDocument>, Vec<LeftOutPage>) {
    let link_index = build_link_index(fetched.iter().map(|page| &page.document));
    let mut stamped = Vec::with_capacity(fetched.len());
    let mut failures = Vec::new();
    for mut page in fetched {
        let generated = crate::markdown::render_document(
            &page.document.frontmatter,
            &page.document.body_markdown,
        )
        .ok();
        let lines_before = generated
            .as_deref()
            .and_then(|file| lines_before_body(file, &page.document.body_markdown));
        let web_path_prefix = effective_web_path_prefix(&page.document.sidecar, None);
        match render_storage_of_body(
            &page.document,
            &link_index,
            false,
            &web_path_prefix,
            lines_before,
        ) {
            Ok(storage) => {
                let sidecar = &mut page.document.sidecar;
                sidecar.storage_hash = Some(sha256_hex(storage.as_bytes()));
                sidecar.storage_differs_from_remote =
                    !crate::storage::same_document(&storage, &page.remote_storage);
                stamped.push(page.document);
            }
            Err(error) => {
                let generated_storage =
                    unvalidated_storage_of_body(&page.document, &link_index, &web_path_prefix).ok();
                failures.push(LeftOutPage {
                    page,
                    error: format!("{error:#}"),
                    generated_storage,
                });
            }
        }
    }
    (stamped, failures)
}

/// Keeps what a pull has of a page it leaves out: the Markdown generated for
/// it, the storage that Markdown converts to when conversion gets that far,
/// and the remote storage.
fn keep_unconvertible_page(root: &Path, left_out: &LeftOutPage) -> Result<UnconvertiblePage> {
    let document = &left_out.page.document;
    let artifacts = root.join(PULL_FAILURES_DIR).join(document_dir_name(
        &document.frontmatter.title,
        document.sidecar.content_id.as_deref(),
    ));
    fs::create_dir_all(&artifacts)
        .with_context(|| format!("failed to create {}", artifacts.display()))?;
    let write = |name: &str, contents: &str| {
        let path = artifacts.join(name);
        fs::write(&path, contents).with_context(|| format!("failed to write {}", path.display()))
    };
    write(
        "generated.md",
        &crate::markdown::render_document(&document.frontmatter, &document.body_markdown)?,
    )?;
    if let Some(storage) = &left_out.generated_storage {
        write("generated.xml", storage)?;
    }
    write("remote.xml", &left_out.page.remote_storage)?;
    Ok(UnconvertiblePage {
        content_id: document.sidecar.content_id.clone(),
        title: document.frontmatter.title.clone(),
        error: left_out.error.clone(),
        artifacts,
        generated_storage: left_out.generated_storage.is_some(),
    })
}

fn safe_attachment_file_name(title: &str) -> Result<&str> {
    if attachment_file_name_is_safe(title) {
        return Ok(title);
    }
    Err(crate::output::typed_error(
        crate::output::ErrorKind::Api,
        format!("remote attachment name `{title}` is not a safe file name"),
    ))
}

fn safe_local_attachment_file_name(title: &str) -> Result<&str> {
    if attachment_file_name_is_safe(title) {
        return Ok(title);
    }
    Err(crate::output::typed_error(
        crate::output::ErrorKind::InvalidInput,
        format!("local attachment name `{title}` is not a portable safe file name"),
    ))
}

fn attachment_file_name_is_safe(title: &str) -> bool {
    let mut components = Path::new(title).components();
    let windows_stem = title
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    let windows_reserved = matches!(windows_stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || windows_stem
            .strip_prefix("COM")
            .and_then(|suffix| suffix.parse::<u8>().ok())
            .is_some_and(|number| (1..=9).contains(&number))
        || windows_stem
            .strip_prefix("LPT")
            .and_then(|suffix| suffix.parse::<u8>().ok())
            .is_some_and(|number| (1..=9).contains(&number));
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
            if title != "."
                && title != ".."
                && !title.contains('/')
                && !title.contains('\\')
                && !title.contains('\0')
                && !title.chars().any(|character| {
                    character.is_control() || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
                })
                && !title.ends_with([' ', '.'])
                && !windows_reserved
    )
}

fn load_local_documents(root: &Path) -> Result<Vec<LocalDocument>> {
    let indexes = scan_local_documents(root)?;
    indexes
        .into_iter()
        .map(|index| load_document(&index.directory))
        .collect()
}

fn load_required_local_documents(root: &Path) -> Result<Vec<LocalDocument>> {
    let docs = load_local_documents(root)?;
    if docs.is_empty() {
        return Err(crate::output::typed_error_with_hint(
            crate::output::ErrorKind::InvalidInput,
            format!(
                "sync path `{}` contains no index.md documents",
                root.display()
            ),
            "run `confluence pull ...` first or point PATH at a pulled Markdown tree",
        ));
    }
    Ok(docs)
}

pub fn validate_sync_path(root: &Path) -> Result<()> {
    load_required_local_documents(root).map(|_| ())
}

impl LinkIndex {
    fn set_content_id(&mut self, directory: &Path, content_id: &str) {
        if let Some(target) = self.by_directory.get_mut(directory) {
            target.content_id = Some(content_id.to_string());
        }
        let markdown_path = directory.join("index.md");
        if let Some(target) = self.by_markdown_path.get_mut(&markdown_path) {
            target.content_id = Some(content_id.to_string());
        }
    }
}

fn build_link_index<'a>(docs: impl IntoIterator<Item = &'a LocalDocument>) -> LinkIndex {
    let mut index = LinkIndex::default();
    for doc in docs {
        let target = LinkTarget {
            markdown_path: doc.markdown_path.clone(),
            directory: doc.directory.clone(),
            content_id: doc.sidecar.content_id.clone(),
            title: doc.frontmatter.title.clone(),
            space_key: doc.sidecar.space_key.clone(),
        };
        index
            .by_markdown_path
            .insert(target.markdown_path.clone(), target.clone());
        index.by_directory.insert(target.directory.clone(), target);
    }
    index
}

/// The storage a document on disk converts to. A body that does not convert
/// is reported with the document's file and the lines of that file.
fn render_body_storage(
    doc: &LocalDocument,
    link_index: &LinkIndex,
    allow_lossy: bool,
    web_path_prefix: impl AsRef<str>,
) -> Result<String> {
    let file = fs::read_to_string(&doc.markdown_path).ok();
    let lines_before = file
        .as_deref()
        .and_then(|file| lines_before_body(file, &doc.body_markdown));
    render_storage_of_body(doc, link_index, allow_lossy, web_path_prefix, lines_before).map_err(
        |error| {
            let path = doc.markdown_path.display().to_string();
            let mut error = match error.downcast::<crate::output::CliError>() {
                Ok(error) => error,
                Err(error) => return error.context(path),
            };
            error.message = format!("{path}: {}", error.message);
            match &mut error.details {
                Some(details) if details.is_object() => details["path"] = serde_json::json!(path),
                Some(_) => {}
                None => error.details = Some(serde_json::json!({ "path": path })),
            }
            error.into()
        },
    )
}

/// The storage a document's body converts to, with the body's Markdown
/// following `lines_before` lines of the file an error names lines of.
fn render_storage_of_body(
    doc: &LocalDocument,
    link_index: &LinkIndex,
    allow_lossy: bool,
    web_path_prefix: impl AsRef<str>,
    lines_before: Option<usize>,
) -> Result<String> {
    let rewritten_markdown =
        rewrite_macro_page_references(&doc.body_markdown, &doc.directory, link_index);
    let converted = crate::markdown::markdown_to_storage_in_file(
        &rewritten_markdown,
        allow_lossy,
        lines_before,
    )?;
    let storage = rewrite_local_links_to_remote(
        &converted.storage,
        &doc.directory,
        link_index,
        web_path_prefix.as_ref(),
    );
    crate::storage::validate_generated_storage(&storage)?;
    Ok(storage)
}

/// The storage a document's body converts to, valid or not: what
/// `render_storage_of_body` validates.
fn unvalidated_storage_of_body(
    doc: &LocalDocument,
    link_index: &LinkIndex,
    web_path_prefix: &str,
) -> Result<String> {
    let rewritten_markdown =
        rewrite_macro_page_references(&doc.body_markdown, &doc.directory, link_index);
    let converted = crate::markdown::render_markdown_storage(&rewritten_markdown, false)?;
    Ok(rewrite_local_links_to_remote(
        &converted.storage,
        &doc.directory,
        link_index,
        web_path_prefix,
    ))
}

/// The number of lines of `file` that precede the first line of `body`, when
/// the file ends with that body.
fn lines_before_body(file: &str, body: &str) -> Option<usize> {
    let before = file.trim_end().strip_suffix(body.trim())?;
    let leading = &body[..body.len() - body.trim_start().len()];
    before
        .matches('\n')
        .count()
        .checked_sub(leading.matches('\n').count())
}

fn rewrite_macro_page_references(
    markdown: &str,
    current_dir: &Path,
    link_index: &LinkIndex,
) -> String {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut output = String::new();
    let mut index = 0;

    while index < lines.len() {
        output.push_str(lines[index]);
        output.push('\n');

        let macro_mode = match lines[index].trim() {
            line if line.starts_with(":::confluence-macro ") => {
                Some(MacroPageReferenceMode::Generic)
            }
            ":::confluence-children" => Some(MacroPageReferenceMode::Children),
            ":::confluence-excerpt-include" => Some(MacroPageReferenceMode::ExcerptInclude),
            ":::confluence-include-page" => Some(MacroPageReferenceMode::IncludePage),
            ":::confluence-page-tree" => Some(MacroPageReferenceMode::PageTree),
            ":::confluence-page-tree-search" => Some(MacroPageReferenceMode::PageTreeSearch),
            ":::confluence-view-file" => Some(MacroPageReferenceMode::AttachmentPreview),
            ":::confluence-view-doc" => Some(MacroPageReferenceMode::AttachmentPreview),
            ":::confluence-view-xls" => Some(MacroPageReferenceMode::AttachmentPreview),
            ":::confluence-view-ppt" => Some(MacroPageReferenceMode::AttachmentPreview),
            _ => None,
        };
        let Some(macro_mode) = macro_mode else {
            index += 1;
            continue;
        };

        index += 1;
        while index < lines.len() && lines[index].trim() != ":::" {
            let rewritten = rewrite_page_reference_parameter_line(
                lines[index],
                current_dir,
                link_index,
                macro_mode,
            );
            output.push_str(&rewritten);
            output.push('\n');
            index += 1;
        }

        if index < lines.len() {
            output.push_str(lines[index]);
            output.push('\n');
            index += 1;
        }
    }

    if !markdown.ends_with('\n') && output.ends_with('\n') {
        output.pop();
    }

    output
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MacroPageReferenceMode {
    Generic,
    Children,
    ExcerptInclude,
    IncludePage,
    PageTree,
    PageTreeSearch,
    AttachmentPreview,
}

fn rewrite_page_reference_parameter_line(
    line: &str,
    current_dir: &Path,
    link_index: &LinkIndex,
    mode: MacroPageReferenceMode,
) -> String {
    let Some((name, value)) = line.split_once(':') else {
        return line.to_string();
    };
    if mode == MacroPageReferenceMode::Generic {
        return rewrite_generic_macro_page_reference_parameter_line(
            name,
            value,
            current_dir,
            link_index,
        );
    }
    let expected_parameter = match mode {
        MacroPageReferenceMode::Generic => unreachable!("generic mode is handled before matching"),
        MacroPageReferenceMode::Children => "page",
        MacroPageReferenceMode::PageTree | MacroPageReferenceMode::PageTreeSearch => "root",
        MacroPageReferenceMode::AttachmentPreview => "page",
        MacroPageReferenceMode::ExcerptInclude | MacroPageReferenceMode::IncludePage => "page",
    };
    if name.trim() != expected_parameter {
        return line.to_string();
    }
    let target = value.trim();
    let rewritten = local_target_to_page_placeholder(current_dir, target, link_index, mode)
        .unwrap_or_else(|| target.to_string());
    format!("{}: {}", name.trim_end(), rewritten)
}

fn rewrite_generic_macro_page_reference_parameter_line(
    name: &str,
    value: &str,
    current_dir: &Path,
    link_index: &LinkIndex,
) -> String {
    let trimmed = value.trim();
    let (prefix, target) = if let Some(target) = trimmed.strip_prefix("!page-link ") {
        ("!page-link", target.trim())
    } else if let Some(target) = trimmed.strip_prefix("!page ") {
        ("!page", target.trim())
    } else {
        return format!("{}: {}", name.trim_end(), trimmed);
    };
    let rewritten = local_target_to_page_placeholder(
        current_dir,
        target,
        link_index,
        MacroPageReferenceMode::Generic,
    )
    .unwrap_or_else(|| target.to_string());
    format!("{}: {} {}", name.trim_end(), prefix, rewritten)
}

fn local_target_to_page_placeholder(
    current_dir: &Path,
    target: &str,
    link_index: &LinkIndex,
    mode: MacroPageReferenceMode,
) -> Option<String> {
    if parse_page_placeholder_url(target).is_some() {
        return Some(target.to_string());
    }
    if !is_local_target(target) {
        return None;
    }

    let (path_part, _) = split_target_fragment(target);
    if path_part.is_empty() {
        return None;
    }
    let resolved = normalize_path(current_dir.join(path_part));
    let markdown_target = if resolved.file_name().and_then(|name| name.to_str()) == Some("index.md")
    {
        resolved
    } else if resolved.extension().is_none() {
        resolved.join("index.md")
    } else {
        resolved
    };
    let link = link_index.by_markdown_path.get(&markdown_target)?;
    let placeholder = match mode {
        MacroPageReferenceMode::Generic => PageLinkPlaceholder {
            content_id: None,
            space_key: link.space_key.clone(),
            content_title: Some(link.title.clone()),
            anchor: None,
        },
        MacroPageReferenceMode::Children => PageLinkPlaceholder {
            content_id: None,
            space_key: link.space_key.clone(),
            content_title: Some(link.title.clone()),
            anchor: None,
        },
        MacroPageReferenceMode::ExcerptInclude => PageLinkPlaceholder {
            content_id: None,
            space_key: link.space_key.clone(),
            content_title: Some(link.title.clone()),
            anchor: None,
        },
        MacroPageReferenceMode::IncludePage => PageLinkPlaceholder {
            content_id: None,
            space_key: link.space_key.clone(),
            content_title: Some(link.title.clone()),
            anchor: None,
        },
        MacroPageReferenceMode::PageTree => PageLinkPlaceholder {
            content_id: None,
            space_key: link.space_key.clone(),
            content_title: Some(link.title.clone()),
            anchor: None,
        },
        MacroPageReferenceMode::PageTreeSearch => PageLinkPlaceholder {
            content_id: None,
            space_key: link.space_key.clone(),
            content_title: Some(link.title.clone()),
            anchor: None,
        },
        MacroPageReferenceMode::AttachmentPreview => PageLinkPlaceholder {
            content_id: None,
            space_key: link.space_key.clone(),
            content_title: Some(link.title.clone()),
            anchor: None,
        },
    };
    Some(build_page_placeholder_url(&placeholder))
}

fn body_changed(sidecar: &Sidecar, markdown_hash: &str, storage_hash: &str) -> bool {
    if sidecar.storage_hash.as_deref() == Some(storage_hash) {
        return false;
    }
    if sidecar.remote_storage_hash.is_none()
        && sidecar.content_id.is_some()
        && sidecar.last_pulled_hash.as_deref() == Some(markdown_hash)
    {
        return false;
    }
    true
}

fn effective_web_path_prefix(sidecar: &Sidecar, provider_prefix: Option<&str>) -> String {
    provider_prefix
        .map(ToOwned::to_owned)
        .or_else(|| sidecar.web_path_prefix.clone())
        .or_else(|| {
            sidecar
                .provider
                .map(default_web_path_prefix)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_default()
}

fn default_web_path_prefix(provider: crate::model::ProviderKind) -> &'static str {
    match provider {
        crate::model::ProviderKind::Cloud => "/wiki",
        crate::model::ProviderKind::DataCenter => "",
    }
}

/// Resolves the placeholder and local targets of rendered storage to their
/// Confluence form. CDATA sections hold literal text, such as the body of a
/// code macro or of a link, and pass through untouched. Each step skips them
/// on its own, so text one step wraps in CDATA is literal to the next.
fn rewrite_local_links_to_remote(
    storage: &str,
    current_dir: &Path,
    link_index: &LinkIndex,
    web_path_prefix: &str,
) -> String {
    static ELEMENT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"<(a|img)(\s[^>]*)>").expect("valid element regex"));
    static HREF: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(\s)href="([^"]+)""#).expect("valid href regex"));
    static SRC: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(\s)src="([^"]+)""#).expect("valid src regex"));

    let storage = resolve_placeholder_links(storage);
    rewrite_outside_literal_sections(&storage, |markup| {
        ELEMENT
            .replace_all(markup, |element: &regex::Captures<'_>| {
                // An anchor links through `href` and an image through `src`.
                // Other attributes are left alone, whatever their name ends in.
                let (name, target_attribute) = match &element[1] {
                    "a" => ("href", &*HREF),
                    _ => ("src", &*SRC),
                };
                let attributes =
                    target_attribute.replace(&element[2], |captures: &regex::Captures<'_>| {
                        let original = &captures[2];
                        let target = rewrite_local_target_url(
                            current_dir,
                            original,
                            link_index,
                            web_path_prefix,
                        )
                        .unwrap_or_else(|| original.to_string());
                        format!(r#"{}{name}="{target}""#, &captures[1])
                    });
                format!("<{}{attributes}>", &element[1])
            })
            .into_owned()
    })
}
fn rewrite_local_target_url(
    current_dir: &Path,
    target: &str,
    link_index: &LinkIndex,
    web_path_prefix: &str,
) -> Option<String> {
    if let Some(link) = parse_page_placeholder_url(target) {
        return placeholder_to_remote_url(&link, web_path_prefix);
    }
    if parse_user_placeholder_url(target).is_some()
        || parse_status_placeholder_url(target).is_some()
    {
        return None;
    }
    if !is_local_target(target) {
        return None;
    }

    let target = decode_character_references(target);
    let (path_part, fragment) = split_target_fragment(&target);
    if path_part.is_empty() {
        return None;
    }
    let (path_part, query) = match path_part.split_once('?') {
        Some((path, query)) => (path, Some(escape_xml_attr(query))),
        None => (path_part, None),
    };
    let path_part = urlencoding::decode(path_part).ok()?;
    let fragment = fragment.map(escape_xml_attr);

    let resolved = normalize_path(current_dir.join(path_part.as_ref()));
    let markdown_target = if resolved.file_name().and_then(|name| name.to_str()) == Some("index.md")
    {
        resolved.clone()
    } else if resolved.extension().is_none() {
        resolved.join("index.md")
    } else {
        resolved.clone()
    };

    if let Some(link) = link_index.by_markdown_path.get(&markdown_target)
        && let Some(content_id) = &link.content_id
    {
        let mut url = format!(
            "{}/pages/viewpage.action?pageId={content_id}",
            web_path_prefix.trim_end_matches('/')
        );
        if let Some(query) = &query {
            url.push_str("&amp;");
            url.push_str(query);
        }
        if let Some(fragment) = &fragment {
            url.push('#');
            url.push_str(fragment);
        }
        return Some(url);
    }

    let attachments_dir = resolved.parent()?;
    if attachments_dir.file_name().and_then(|name| name.to_str()) != Some("attachments") {
        return None;
    }
    let owner_dir = attachments_dir.parent()?;
    let file_name = resolved.file_name()?.to_str()?;
    let owner = link_index.by_directory.get(owner_dir)?;
    let content_id = owner.content_id.as_deref()?;

    let mut url = format!(
        "{}/download/attachments/{content_id}/{}",
        web_path_prefix.trim_end_matches('/'),
        urlencoding::encode(file_name)
    );
    if let Some(query) = &query {
        url.push('?');
        url.push_str(query);
    }
    if let Some(fragment) = &fragment {
        url.push('#');
        url.push_str(fragment);
    }
    Some(url)
}

fn normalize_path(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn derive_parent_id_from_index(
    directory: &Path,
    sidecar: &Sidecar,
    parent_ids: &BTreeMap<PathBuf, String>,
) -> Option<String> {
    directory
        .parent()
        .and_then(|parent| parent_ids.get(parent))
        .cloned()
        .or_else(|| sidecar.remote_parent_id.clone())
}

fn derive_parent_id_from_fs(directory: &Path, sidecar: &Sidecar) -> Result<Option<String>> {
    let local_parent_id = if let Some(parent) = directory.parent() {
        let parent_sidecar = parent.join(".confluence.json");
        if parent_sidecar.exists() {
            let parent_sidecar: Sidecar =
                serde_json::from_str(&fs::read_to_string(parent_sidecar)?)?;
            parent_sidecar.content_id
        } else {
            None
        }
    } else {
        None
    };
    Ok(local_parent_id.or_else(|| sidecar.remote_parent_id.clone()))
}

fn markdown_body_hash(markdown: &str) -> String {
    sha256_hex(markdown.trim().as_bytes())
}

fn compute_paths(root: &Path, items: &BTreeMap<String, ContentItem>) -> BTreeMap<String, PathBuf> {
    let mut paths = BTreeMap::new();
    let mut remaining: BTreeSet<_> = items.keys().cloned().collect();
    while !remaining.is_empty() {
        let mut progressed = false;
        let ids: Vec<_> = remaining.iter().cloned().collect();
        for id in ids {
            let item = &items[&id];
            let maybe_parent = item
                .parent_id
                .as_ref()
                .and_then(|parent_id| paths.get(parent_id));
            if item.kind == ContentKind::BlogPost {
                let path = root
                    .join("blog-posts")
                    .join(document_dir_name(&item.title, Some(&item.id)));
                paths.insert(id.clone(), path);
                remaining.remove(&id);
                progressed = true;
                continue;
            }
            if let Some(parent) = maybe_parent {
                let path = parent.join(document_dir_name(&item.title, Some(&item.id)));
                paths.insert(id.clone(), path);
                remaining.remove(&id);
                progressed = true;
            } else if item.parent_id.is_none()
                || !items.contains_key(item.parent_id.as_ref().unwrap())
            {
                let path = root.join(document_dir_name(&item.title, Some(&item.id)));
                paths.insert(id.clone(), path);
                remaining.remove(&id);
                progressed = true;
            }
        }
        if !progressed {
            for id in remaining.clone() {
                let item = &items[&id];
                paths.insert(
                    id.clone(),
                    root.join(document_dir_name(&item.title, Some(&item.id))),
                );
                remaining.remove(&id);
            }
        }
    }
    paths
}

fn rewrite_page_links(
    markdown: &str,
    current_dir: &Path,
    paths: &BTreeMap<String, PathBuf>,
    items: &BTreeMap<String, ContentItem>,
) -> String {
    let mut remote_links = Vec::new();
    for (content_id, target_dir) in paths {
        let target_index = target_dir.join("index.md");
        let relative = diff_paths(&target_index, current_dir).unwrap_or(target_index);
        let replacement = relative.to_string_lossy().replace('\\', "/");
        let patterns = [
            // The boundary keeps an id from matching the start of a longer one.
            format!(r#"https?://[^\s\)]+/pages/{content_id}\b(?:/[^\s\)]*)?"#),
            format!(r#"/pages/{content_id}\b(?:/[^\s\)]*)?"#),
            format!(r#"viewpage\.action\?pageId={content_id}\b"#),
        ];
        for pattern in patterns {
            let re = Regex::new(&pattern).expect("valid page link regex");
            remote_links.push((re, replacement.clone()));
        }
    }
    let placeholder_re =
        Regex::new(r#"confluence-page://page[^\s\)"]+"#).expect("valid page placeholder regex");
    rewrite_outside_raw_markup(markdown, |text| {
        let mut rewritten = text.to_string();
        for (re, replacement) in &remote_links {
            rewritten = re.replace_all(&rewritten, replacement.as_str()).to_string();
        }
        placeholder_re
            .replace_all(&rewritten, |captures: &regex::Captures<'_>| {
                let target = captures.get(0).map(|m| m.as_str()).unwrap_or_default();
                // A link pulled without a body keeps its placeholder, which
                // is what lets apply write it without a body again. So does
                // one whose placeholder says how the link is displayed.
                if has_generated_label(target) || !link_display_attributes(target).is_empty() {
                    return target.to_string();
                }
                resolve_placeholder_to_local_path(target, current_dir, paths, items)
                    .unwrap_or_else(|| target.to_string())
            })
            .to_string()
    })
}

/// A mention pulled without a link body, as Markdown writes it: the generated
/// label and the placeholder target.
static USER_MENTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\[((?:\\.|[^\]\\\n])*)\]\((confluence-user://user\?[^\s\)"]+)\)$"#)
        .expect("valid user mention regex")
});

/// The user a mention target stands for, when the mention carries a generated
/// label that does not name the user yet.
fn unnamed_mention_user(target: &str) -> Option<UserReference> {
    let user = crate::markdown::parse_user_placeholder_url(target)?;
    (has_generated_label(target)
        && crate::markdown::generated_user_label(target, &user) == user.default_label())
    .then(|| user.reference())
    .flatten()
}

/// Where pulled Markdown links to a user placeholder, with the placeholder
/// target as written, in the order of the text. Only what Markdown reads as
/// a link counts: the same characters in code or in a raw block are text of
/// the page. The cells of a layout section hold Markdown of their own.
fn user_mention_links(markdown: &str) -> Vec<(std::ops::Range<usize>, &str)> {
    let mut raw = raw_markup_ranges(markdown);
    raw.extend(macro_parameter_ranges(markdown));
    let mut links = Vec::new();
    for (event, range) in
        pulldown_cmark::Parser::new_ext(markdown, crate::markdown::markdown_options())
            .into_offset_iter()
    {
        match event {
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Link { dest_url, .. })
                if dest_url.starts_with("confluence-user://")
                    && !raw
                        .iter()
                        .any(|raw| raw.start < range.end && range.start < raw.end) =>
            {
                let Some(target) = USER_MENTION
                    .captures(&markdown[range.clone()])
                    .and_then(|captures| captures.get(2))
                else {
                    continue;
                };
                links.push((
                    range.clone(),
                    &markdown[range.start + target.start()..range.start + target.end()],
                ));
            }
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::CodeBlock(
                pulldown_cmark::CodeBlockKind::Fenced(info),
            )) if info.starts_with("confluence-layout-section") => {
                for cell in layout_cell_ranges(markdown, range) {
                    links.extend(user_mention_links(&markdown[cell.clone()]).into_iter().map(
                        |(link, target)| (cell.start + link.start..cell.start + link.end, target),
                    ));
                }
            }
            _ => {}
        }
    }
    links
}

/// The ranges of the parameter lines of the macro blocks in pulled Markdown:
/// from the line that opens a block to the line that starts its body or
/// closes it. A parameter value is text of the macro, whatever it reads as.
fn macro_parameter_ranges(markdown: &str) -> Vec<std::ops::Range<usize>> {
    const OPENER: &str = ":::confluence-macro ";
    if !markdown.contains(OPENER) {
        return Vec::new();
    }
    // A line of a code block is code, whatever it reads as.
    let code_blocks: Vec<_> =
        pulldown_cmark::Parser::new_ext(markdown, crate::markdown::markdown_options())
            .into_offset_iter()
            .filter(|(event, _)| {
                matches!(
                    event,
                    pulldown_cmark::Event::Start(pulldown_cmark::Tag::CodeBlock(_))
                )
            })
            .map(|(_, range)| range)
            .collect();
    let mut ranges = Vec::new();
    let mut parameters_start = None;
    let mut offset = 0;
    for line in markdown.split_inclusive('\n') {
        match parameters_start {
            Some(start) if matches!(line.trim(), "---" | ":::") => {
                ranges.push(start..offset);
                parameters_start = None;
            }
            None if line.trim_start().starts_with(OPENER)
                && !code_blocks.iter().any(|block| block.contains(&offset)) =>
            {
                parameters_start = Some(offset + line.len());
            }
            _ => {}
        }
        offset += line.len();
    }
    ranges.extend(parameters_start.map(|start| start..markdown.len()));
    ranges
}

/// The ranges of the cell bodies of the layout section written at `section`.
fn layout_cell_ranges(
    markdown: &str,
    section: std::ops::Range<usize>,
) -> Vec<std::ops::Range<usize>> {
    let mut cells = Vec::new();
    let mut cell_start = None;
    let mut offset = section.start;
    for line in markdown[section].split_inclusive('\n') {
        let ends_cell = matches!(line.trim_end(), "--- cell ---" | "~~~~");
        if ends_cell && let Some(start) = cell_start.take() {
            cells.push(start..offset);
        }
        offset += line.len();
        if line.trim_end() == "--- cell ---" {
            cell_start = Some(offset);
        }
    }
    cells
}

/// The users mentioned in pulled Markdown whose mention shows an identifier.
fn unnamed_mentions(markdown: &str) -> BTreeSet<UserReference> {
    user_mention_links(markdown)
        .into_iter()
        .filter_map(|(_, target)| unnamed_mention_user(target))
        .collect()
}

/// Shows the name of each mentioned user whose name is known. The target
/// records the name, which is how apply recognises the label as generated
/// and writes the mention back without a link body.
fn name_user_mentions(markdown: &str, names: &BTreeMap<UserReference, Option<String>>) -> String {
    let mut named = String::with_capacity(markdown.len());
    let mut rest = 0;
    for (link, target) in user_mention_links(markdown) {
        // A name is shown on one line with single spaces, as Markdown reads
        // the label back, so that apply still recognises it as generated.
        let Some(name) = unnamed_mention_user(target)
            .and_then(|user| names.get(&user).cloned().flatten())
            .map(|name| name.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        // A cell separator in the name would end the table cell that holds
        // the mention. Escaped, it is the same character everywhere.
        let label = crate::markdown::escape_markdown_text(&format!("@{name}")).replace('|', r"\|");
        named.push_str(&markdown[rest..link.start]);
        named.push_str(&format!(
            "[{label}]({})",
            crate::markdown::with_user_display_name(target, &name)
        ));
        rest = link.end;
    }
    named.push_str(&markdown[rest..]);
    named
}

/// Applies a rewrite to the Markdown around raw storage blocks, comments
/// and links written as inline tags. They hold markup that apply writes
/// back as it stands, so an address inside them stays as pulled.
fn rewrite_outside_raw_markup(markdown: &str, rewrite: impl Fn(&str) -> String) -> String {
    let mut rewritten = String::with_capacity(markdown.len());
    let mut rest = 0;
    for raw in raw_markup_ranges(markdown) {
        rewritten.push_str(&rewrite(&markdown[rest..raw.start]));
        rewritten.push_str(&markdown[raw.clone()]);
        rest = raw.end;
    }
    rewritten
}

/// Ranges of the raw storage blocks, the comments and the links written as
/// inline tags in pulled Markdown, in order, followed by the empty range at
/// the end of the text.
fn raw_markup_ranges(markdown: &str) -> Vec<std::ops::Range<usize>> {
    static LINK_TAG: LazyLock<Regex> = LazyLock::new(|| {
        // A blank line ends the Markdown block that holds the tag, so a
        // link does not reach past one.
        Regex::new(r"<a\s(?:[^>\n]|\n[^>\n])*\n?>(?:[^\n]|\n[^\n])*?</a>")
            .expect("valid inline link tag regex")
    });
    let mut ranges = comment_ranges(markdown);
    let mut rest = 0;
    for block in raw_storage_block_ranges(markdown) {
        ranges.extend(
            LINK_TAG
                .find_iter(&markdown[rest..block.start])
                .map(|link| rest + link.start()..rest + link.end()),
        );
        rest = block.end;
        ranges.push(block);
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<std::ops::Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start < last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

/// Ranges of the comments in pulled Markdown. A comment is one where Markdown
/// reads HTML: `<!--` in a code span or after a backslash is text.
fn comment_ranges(markdown: &str) -> Vec<std::ops::Range<usize>> {
    static COMMENT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").expect("valid comment regex"));
    if !markdown.contains("<!--") {
        return Vec::new();
    }
    pulldown_cmark::Parser::new_ext(markdown, crate::markdown::markdown_options())
        .into_offset_iter()
        .filter(|(event, _)| {
            matches!(
                event,
                pulldown_cmark::Event::Start(pulldown_cmark::Tag::HtmlBlock)
                    | pulldown_cmark::Event::InlineHtml(_)
            )
        })
        .flat_map(|(_, html)| {
            COMMENT
                .find_iter(&markdown[html.clone()])
                .map(move |comment| html.start + comment.start()..html.start + comment.end())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Ranges of the raw storage blocks in pulled Markdown, each from its opening
/// backticks through its closing ones, followed by the empty range at the end
/// of the text. A block opens anywhere on a line and closes at the next line
/// that starts with as many backticks, after the indentation and quote marks
/// of a list item or block quote that holds it.
fn raw_storage_block_ranges(markdown: &str) -> Vec<std::ops::Range<usize>> {
    static OPENING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(`{3,})confluence-storage[^\n]*\n").expect("valid storage fence regex")
    });
    let mut ranges = Vec::new();
    let mut rest = 0;
    while let Some(captures) = OPENING.captures_at(markdown, rest) {
        let opening = captures.get(0).expect("whole match");
        let fence = &captures[1];
        let mut line = opening.end();
        let end = loop {
            let text = markdown[line..].trim_start_matches([' ', '\t', '>']);
            if text.starts_with(fence) {
                break markdown.len() - text.len() + fence.len();
            }
            match markdown[line..].find('\n') {
                Some(newline) => line += newline + 1,
                None => break markdown.len(),
            }
        };
        ranges.push(opening.start()..end);
        rest = end;
    }
    ranges.push(markdown.len()..markdown.len());
    ranges
}

fn resolve_placeholder_to_local_path(
    target: &str,
    current_dir: &Path,
    paths: &BTreeMap<String, PathBuf>,
    items: &BTreeMap<String, ContentItem>,
) -> Option<String> {
    let link = parse_page_placeholder_url(target)?;
    let target_dir = if let Some(content_id) = &link.content_id {
        paths.get(content_id).cloned()
    } else {
        find_item_by_title(items, &link).and_then(|item| paths.get(&item.id).cloned())
    }?;
    let target_index = target_dir.join("index.md");
    let mut replacement = diff_paths(&target_index, current_dir)
        .unwrap_or(target_index)
        .to_string_lossy()
        .replace('\\', "/");
    if let Some(anchor) = link.anchor {
        replacement.push('#');
        replacement.push_str(&anchor);
    }
    Some(replacement)
}

fn find_item_by_title<'a>(
    items: &'a BTreeMap<String, ContentItem>,
    link: &PageLinkPlaceholder,
) -> Option<&'a ContentItem> {
    let title = link.content_title.as_deref()?;
    let space_key = link.space_key.as_deref();
    items.values().find(|item| {
        item.title == title && (space_key.is_none() || item.space_key.as_deref() == space_key)
    })
}

fn rewrite_attachment_links(
    markdown: &str,
    attachments: &[crate::model::AttachmentInfo],
) -> String {
    let mut rewritten = markdown.to_string();
    for attachment in attachments {
        // The file name is one segment of a link destination, where a
        // literal `%` or space would be read as something else.
        let replacement = format!("attachments/{}", encode_target_segment(&attachment.title));
        let escaped = regex::escape(&attachment.title);
        let re = Regex::new(&format!(r#"download/attachments/[^\s\)]+/{escaped}"#))
            .expect("valid attachment regex");
        rewritten = rewrite_outside_raw_markup(&rewritten, |text| {
            let text = match &attachment.download_url {
                Some(url) => text.replace(url, &replacement),
                None => text.to_string(),
            };
            re.replace_all(&text, replacement.as_str()).to_string()
        });
    }
    rewritten
}

fn rewrite_generic_macro_page_placeholders(
    markdown: &str,
    current_dir: &Path,
    paths: &BTreeMap<String, PathBuf>,
    items: &BTreeMap<String, ContentItem>,
) -> String {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut output = String::new();
    let mut index = 0;

    while index < lines.len() {
        output.push_str(lines[index]);
        output.push('\n');

        if !lines[index].trim().starts_with(":::confluence-macro ") {
            index += 1;
            continue;
        }

        index += 1;
        while index < lines.len() && lines[index].trim() != ":::" {
            output.push_str(&rewrite_generic_macro_page_placeholder_line(
                lines[index],
                current_dir,
                paths,
                items,
            ));
            output.push('\n');
            index += 1;
        }

        if index < lines.len() {
            output.push_str(lines[index]);
            output.push('\n');
            index += 1;
        }
    }

    if !markdown.ends_with('\n') && output.ends_with('\n') {
        output.pop();
    }

    output
}

fn rewrite_generic_macro_page_placeholder_line(
    line: &str,
    current_dir: &Path,
    paths: &BTreeMap<String, PathBuf>,
    items: &BTreeMap<String, ContentItem>,
) -> String {
    let Some((name, value)) = line.split_once(':') else {
        return line.to_string();
    };
    let trimmed = value.trim();
    let (prefix, target) = if let Some(target) = trimmed.strip_prefix("!page-link ") {
        ("!page-link", target.trim())
    } else if let Some(target) = trimmed.strip_prefix("!page ") {
        ("!page", target.trim())
    } else {
        return line.to_string();
    };
    let rewritten = resolve_placeholder_to_local_path(target, current_dir, paths, items)
        .unwrap_or_else(|| target.to_string());
    format!("{}: {} {}", name.trim_end(), prefix, rewritten)
}

/// Writes the placeholder links of rendered storage, which stand for a page,
/// a user or a status in Markdown, as the Confluence elements they stand for.
/// CDATA sections hold literal text and pass through untouched, and so does
/// a link that is written inside a comment.
pub fn resolve_placeholder_links(storage: &str) -> String {
    rewrite_outside_cdata(storage, rewrite_placeholder_links_to_storage)
}

fn rewrite_placeholder_links_to_storage(storage: &str) -> String {
    static LINK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?s)<a(\s(?:[^>]*?\s)?)href="(confluence-(?:page|user|status)://[^"]+)"([^>]*)>"#,
        )
        .expect("valid placeholder link regex")
    });
    const CLOSE: &str = "</a>";
    // A link that opens inside a comment is text of that comment. A comment
    // inside a link is part of the link's text, and so is a closing tag
    // that the comment holds.
    let mut comments: Vec<std::ops::Range<usize>> = Vec::new();
    let mut at = 0;
    while let Some(open) = storage[at..].find("<!--").map(|open| at + open) {
        let Some(close) = storage[open..].find("-->").map(|close| open + close + 3) else {
            break;
        };
        comments.push(open..close);
        at = close;
    }
    let comment_around = |at: usize| {
        let comment = comments.get(comments.partition_point(|comment| comment.end <= at))?;
        comment.contains(&at).then_some(comment.end)
    };
    let mut rewritten = String::with_capacity(storage.len());
    let mut copied = 0;
    let mut search_from = 0;
    while let Some(captures) = LINK.captures_at(storage, search_from) {
        let open = captures.get(0).expect("the whole match");
        if let Some(comment_end) = comment_around(open.start()) {
            search_from = comment_end;
            continue;
        }
        let mut close_from = open.end();
        let close = loop {
            let Some(close) = storage[close_from..].find(CLOSE).map(|at| close_from + at) else {
                break None;
            };
            match comment_around(close) {
                Some(comment_end) => close_from = comment_end,
                None => break Some(close),
            }
        };
        let Some(close) = close else {
            search_from = open.end();
            continue;
        };
        let end = close + CLOSE.len();
        let target = captures.get(2).map(|m| m.as_str()).unwrap_or_default();
        rewritten.push_str(&storage[copied..open.start()]);
        match placeholder_to_storage_macro(target, &storage[open.end()..close]) {
            Some(element) => rewritten.push_str(&element),
            None => rewritten.push_str(&storage[open.start()..end]),
        }
        copied = end;
        search_from = end;
    }
    rewritten.push_str(&storage[copied..]);
    rewritten
}

fn placeholder_to_storage_macro(target: &str, body_html: &str) -> Option<String> {
    let display_attrs: String = link_display_attributes(target)
        .iter()
        .map(|(name, value)| format!(r#" ac:{name}="{}""#, escape_xml_attr(value)))
        .collect();
    if let Some(placeholder) = parse_page_placeholder_url(target) {
        let anchor_attr = placeholder
            .anchor
            .as_deref()
            .map(|anchor| format!(r#" ac:anchor="{}""#, escape_xml_attr(anchor)))
            .unwrap_or_default();
        let resource = if let Some(title) = placeholder.content_title.as_deref() {
            let mut attrs = vec![format!(r#"ri:content-title="{}""#, escape_xml_attr(title))];
            if let Some(space_key) = placeholder.space_key.as_deref() {
                attrs.push(format!(r#"ri:space-key="{}""#, escape_xml_attr(space_key)));
            }
            format!(r#"<ri:page {} />"#, attrs.join(" "))
        } else {
            let content_id = placeholder.content_id.as_deref()?;
            format!(
                r#"<ri:page ri:content-id="{}" />"#,
                escape_xml_attr(content_id)
            )
        };
        let body = placeholder_link_body(target, body_html, &placeholder.default_label());
        return Some(format!(
            r#"<ac:link{anchor_attr}{display_attrs}>{resource}{body}</ac:link>"#
        ));
    }

    if let Some(placeholder) = parse_user_placeholder_url(target) {
        let resource = user_placeholder_to_storage_resource(&placeholder)?;
        let body = placeholder_link_body(
            target,
            body_html,
            &crate::markdown::generated_user_label(target, &placeholder),
        );
        return Some(format!(
            r#"<ac:link{display_attrs}>{resource}{body}</ac:link>"#
        ));
    }

    status_placeholder_to_storage_macro(target, body_html)
}

fn status_placeholder_to_storage_macro(target: &str, body_html: &str) -> Option<String> {
    let placeholder = parse_status_placeholder_url(target)?;
    // The link text is markup, in which a character reference stands for
    // the character the title holds.
    let title = if !body_html.trim().is_empty() && !body_html.contains('<') {
        crate::markdown::decode_character_references(body_html.trim())
    } else {
        placeholder.title
    };
    let mut parameters = vec![format!(
        r#"<ac:parameter ac:name="title">{}</ac:parameter>"#,
        escape_xml_attr(&title)
    )];
    if let Some(colour) = placeholder.colour.as_deref() {
        parameters.push(format!(
            r#"<ac:parameter ac:name="colour">{}</ac:parameter>"#,
            escape_xml_attr(colour)
        ));
    }
    Some(format!(
        r#"<ac:structured-macro ac:name="status">{}</ac:structured-macro>"#,
        parameters.join("")
    ))
}

fn user_placeholder_to_storage_resource(placeholder: &UserMentionPlaceholder) -> Option<String> {
    if let Some(account_id) = placeholder.account_id.as_deref() {
        return Some(format!(
            r#"<ri:user ri:account-id="{}" />"#,
            escape_xml_attr(account_id)
        ));
    }
    if let Some(user_key) = placeholder.user_key.as_deref() {
        return Some(format!(
            r#"<ri:user ri:userkey="{}" />"#,
            escape_xml_attr(user_key)
        ));
    }
    placeholder
        .username
        .as_deref()
        .map(|username| format!(r#"<ri:user ri:username="{}" />"#, escape_xml_attr(username)))
}

/// Builds the body of a placeholder link. A target marked as carrying a
/// generated label stands for a link without a body, and stays without one
/// for as long as its text is still that label.
fn placeholder_link_body(target: &str, body_html: &str, generated_label: &str) -> String {
    if has_generated_label(target) {
        link_body_unless_default(body_html, generated_label)
    } else {
        build_storage_link_body(body_html)
    }
}

fn placeholder_to_remote_url(link: &PageLinkPlaceholder, web_path_prefix: &str) -> Option<String> {
    let content_id = link.content_id.as_deref()?;
    let mut url = format!(
        "{}/pages/viewpage.action?pageId={content_id}",
        web_path_prefix.trim_end_matches('/')
    );
    if let Some(anchor) = link.anchor.as_deref() {
        url.push('#');
        url.push_str(anchor);
    }
    Some(url)
}

fn escape_xml_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn local_attachment_hashes(doc_dir: &Path) -> Result<BTreeMap<String, String>> {
    let attachments_dir = doc_dir.join("attachments");
    if !attachments_dir.exists() {
        return Ok(BTreeMap::new());
    }

    let mut hashes = BTreeMap::new();
    for entry in WalkDir::new(&attachments_dir).min_depth(1).max_depth(1) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            return Err(crate::output::typed_error_with_hint(
                crate::output::ErrorKind::InvalidInput,
                format!(
                    "attachment path `{}` is not a regular file",
                    entry.path().display()
                ),
                "place attachment files directly in the page's attachments directory",
            ));
        }
        let bytes = fs::read(entry.path())
            .with_context(|| format!("failed to read {}", entry.path().display()))?;
        let file_name = entry
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                anyhow!(
                    "attachment path {} has no file name",
                    entry.path().display()
                )
            })?;
        safe_local_attachment_file_name(file_name)?;
        hashes.insert(file_name.to_string(), sha256_hex(bytes));
    }
    Ok(hashes)
}

async fn sync_attachments(
    provider: &dyn ConfluenceProvider,
    doc: &mut LocalDocument,
    content_id: &str,
    delete_remote: bool,
    applied: &mut SyncPlan,
) -> Result<()> {
    let attachments_dir = doc.directory.join("attachments");
    fs::create_dir_all(&attachments_dir)
        .with_context(|| format!("failed to create {}", attachments_dir.display()))?;
    let local_hashes = local_attachment_hashes(&doc.directory)?;
    let current_remote = provider.list_attachments(content_id).await?;

    for (file_name, hash) in &local_hashes {
        let current = current_remote
            .iter()
            .find(|attachment| attachment.title == *file_name);
        let remote_hash = if let Some(attachment) = current {
            Some(sha256_hex(
                &provider
                    .download_attachment(content_id, &attachment.id)
                    .await?,
            ))
        } else {
            None
        };
        if remote_hash.as_deref() != Some(hash.as_str()) {
            let uploaded = provider
                .upload_attachment(
                    content_id,
                    &attachments_dir.join(file_name),
                    Some("Uploaded via confluence-cli"),
                    true,
                    false,
                )
                .await?;
            applied.items.push(PlanItem {
                action: PlanActionKind::UploadAttachment,
                title: file_name.clone(),
                content_id: Some(uploaded.id.clone()),
                path: attachments_dir.join(file_name),
                details: "attachment uploaded or replaced".to_string(),
                diff: None,
            });
        }
    }

    if delete_remote {
        let local_names: BTreeSet<_> = local_hashes.keys().cloned().collect();
        for attachment in &current_remote {
            if !local_names.contains(&attachment.title) {
                provider
                    .delete_attachment(content_id, &attachment.id)
                    .await?;
                applied.items.push(PlanItem {
                    action: PlanActionKind::DeleteAttachment,
                    title: attachment.title.clone(),
                    content_id: Some(attachment.id.clone()),
                    path: attachments_dir.clone(),
                    details: "attachment deleted remotely".to_string(),
                    diff: None,
                });
            }
        }
    }

    let refreshed = provider.list_attachments(content_id).await?;
    let mut attachment_map = BTreeMap::new();
    for attachment in refreshed {
        let file_name = safe_attachment_file_name(&attachment.title)?;
        let remote_bytes = provider
            .download_attachment(content_id, &attachment.id)
            .await?;
        attachment_map.insert(
            attachment.title.clone(),
            AttachmentState {
                id: attachment.id,
                file_name: file_name.to_string(),
                remote_version: attachment.version,
                media_type: attachment.media_type,
                sha256: Some(sha256_hex(&remote_bytes)),
            },
        );
    }
    doc.sidecar.attachment_map = attachment_map;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::tempdir;

    use super::*;
    use crate::markdown::markdown_to_storage;
    use crate::markdown::{
        Frontmatter, PageLinkPlaceholder, Sidecar, StatusMacroPlaceholder, UserMentionPlaceholder,
        build_page_placeholder_url, build_status_placeholder_url, build_user_placeholder_url,
        render_document,
    };
    use crate::model::ProviderKind;

    fn write_clean_managed_document(root: &Path, body: &str) -> PathBuf {
        let page_dir = root.join("root-page--123");
        fs::create_dir_all(&page_dir).expect("create page directory");
        let frontmatter = Frontmatter {
            title: "Root Page".to_string(),
            kind: "page".to_string(),
            labels: vec!["managed".to_string()],
            status: "current".to_string(),
            parent: None,
            properties: BTreeMap::new(),
        };
        let storage = markdown_to_storage(body, false).expect("storage");
        fs::write(
            page_dir.join("index.md"),
            render_document(&frontmatter, body).expect("render markdown"),
        )
        .expect("write markdown");
        fs::write(
            page_dir.join(".confluence.json"),
            serde_json::to_string_pretty(&Sidecar {
                content_id: Some("123".to_string()),
                space_key: Some("DOCS".to_string()),
                provider: Some(ProviderKind::Cloud),
                remote_version: Some(3),
                last_pulled_hash: Some(markdown_body_hash(body)),
                last_pulled_body_markdown: Some(body.to_string()),
                storage_hash: Some(sha256_hex(storage.storage.as_bytes())),
                last_pulled_title: Some("Root Page".to_string()),
                last_pulled_labels: vec!["managed".to_string()],
                last_pulled_status: Some("current".to_string()),
                last_pulled_properties_hash: Some(properties_hash(&BTreeMap::new())),
                ..Sidecar::default()
            })
            .expect("serialize sidecar"),
        )
        .expect("write sidecar");
        page_dir
    }

    /// Marks the document in `page_dir` as pulled from storage that its
    /// Markdown does not carry exactly.
    fn mark_storage_as_different(page_dir: &Path) {
        let sidecar_path = page_dir.join(".confluence.json");
        let mut sidecar: Sidecar =
            serde_json::from_str(&fs::read_to_string(&sidecar_path).expect("read sidecar"))
                .expect("parse sidecar");
        sidecar.storage_differs_from_remote = true;
        fs::write(
            sidecar_path,
            serde_json::to_string_pretty(&sidecar).expect("serialize sidecar"),
        )
        .expect("write sidecar");
    }

    #[test]
    fn plan_names_an_unchanged_page_whose_first_update_rewrites_remote_markup() {
        let dir = tempdir().expect("tempdir");
        let page_dir = write_clean_managed_document(dir.path(), "# Root");
        mark_storage_as_different(&page_dir);

        let plan = plan_path(dir.path(), false, false, false).expect("plan");

        assert_eq!(plan.items.len(), 1, "{:?}", plan.items);
        let item = &plan.items[0];
        assert_eq!(item.action, PlanActionKind::Noop);
        assert_eq!(item.content_id.as_deref(), Some("123"));
        assert_eq!(item.path, page_dir);
        assert_eq!(
            item.details,
            "no changes detected; the first update also rewrites markup of this page that Markdown does not carry as it is stored"
        );
        validate_pull_destination(dir.path(), false).expect("the page is still clean");
    }

    #[test]
    fn plan_says_when_an_update_also_rewrites_remote_markup() {
        let dir = tempdir().expect("tempdir");
        let page_dir = write_clean_managed_document(dir.path(), "# Root");
        let edited = fs::read_to_string(page_dir.join("index.md"))
            .expect("read markdown")
            .replace("# Root", "# Edited");
        fs::write(page_dir.join("index.md"), edited).expect("edit markdown");

        let plan = plan_path(dir.path(), false, false, false).expect("plan");
        assert_eq!(plan.items.len(), 1, "{:?}", plan.items);
        assert_eq!(plan.items[0].action, PlanActionKind::UpdateContent);
        assert_eq!(plan.items[0].details, "changed: body, markdown");

        mark_storage_as_different(&page_dir);
        let plan = plan_path(dir.path(), false, false, false).expect("plan");
        assert_eq!(plan.items.len(), 1, "{:?}", plan.items);
        assert_eq!(plan.items[0].action, PlanActionKind::UpdateContent);
        assert_eq!(
            plan.items[0].details,
            "changed: body, markdown; also rewrites markup of this page that Markdown does not carry as it is stored"
        );
    }

    #[test]
    fn plan_names_the_file_and_line_of_markdown_that_does_not_convert() {
        let dir = tempdir().expect("tempdir");
        let page_dir = write_clean_managed_document(dir.path(), "# Root");
        let markdown_path = page_dir.join("index.md");
        let edited = fs::read_to_string(&markdown_path)
            .expect("read markdown")
            .replace("# Root", "# Root\n\nUse the <medallion> architecture.");
        fs::write(&markdown_path, &edited).expect("edit markdown");
        let line = edited
            .lines()
            .position(|line| line.contains("<medallion>"))
            .expect("edited line")
            + 1;

        let error = plan_path(dir.path(), false, false, false)
            .expect_err("plan of Markdown that does not convert")
            .downcast::<crate::output::CliError>()
            .expect("conversion error");
        let details = error.details.as_ref().expect("details");
        assert_eq!(details["path"], markdown_path.display().to_string());
        assert_eq!(details["markdown_start_line"], line, "{details}");
        assert_eq!(details["markdown_end_line"], line, "{details}");
        assert!(
            error.message.starts_with(&format!(
                "{}: invalid storage generated from Markdown line {line}: expected 'medallion' tag, not 'p'",
                markdown_path.display()
            )),
            "{}",
            error.message
        );
    }

    #[test]
    fn plan_names_the_file_of_a_macro_block_that_does_not_convert() {
        let dir = tempdir().expect("tempdir");
        let page_dir = write_clean_managed_document(dir.path(), "# Root");
        let markdown_path = page_dir.join("index.md");
        let edited = fs::read_to_string(&markdown_path)
            .expect("read markdown")
            .replace(
                "# Root",
                "# Root\n\n:::confluence-macro name=info\nnever closed\n",
            );
        fs::write(&markdown_path, &edited).expect("edit markdown");

        let error = plan_path(dir.path(), false, false, false)
            .expect_err("plan of a macro block that is not closed");
        let message = format!("{error:#}");
        assert!(
            message.starts_with(&format!("{}: ", markdown_path.display())),
            "{message}"
        );
    }

    #[test]
    fn pull_destination_accepts_clean_managed_snapshot() {
        let dir = tempdir().expect("tempdir");
        write_clean_managed_document(dir.path(), "# Root");
        validate_pull_destination(dir.path(), false).expect("clean pull destination");
    }

    #[test]
    fn pull_destination_rejects_local_markdown_changes_without_force() {
        let dir = tempdir().expect("tempdir");
        let page_dir = write_clean_managed_document(dir.path(), "# Root");
        fs::write(
            page_dir.join("index.md"),
            render_document(
                &Frontmatter {
                    title: "Root Page".to_string(),
                    kind: "page".to_string(),
                    labels: vec!["managed".to_string()],
                    status: "current".to_string(),
                    parent: None,
                    properties: BTreeMap::new(),
                },
                "# Locally edited",
            )
            .expect("render edited markdown"),
        )
        .expect("edit markdown");

        let error = validate_pull_destination(dir.path(), false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("local changes or unmanaged files")
        );
        validate_pull_destination(dir.path(), true).expect("force permits replacement");
    }

    #[test]
    fn pull_destination_rejects_unmanaged_files_without_force() {
        let dir = tempdir().expect("tempdir");
        write_clean_managed_document(dir.path(), "# Root");
        fs::write(dir.path().join("notes.txt"), "keep me").expect("write unmanaged file");

        assert!(validate_pull_destination(dir.path(), false).is_err());
    }

    #[test]
    fn pull_destination_rejects_unmanaged_directories_without_force() {
        let dir = tempdir().expect("tempdir");
        write_clean_managed_document(dir.path(), "# Root");
        fs::create_dir(dir.path().join("private-notes")).expect("unmanaged directory");

        assert!(validate_pull_destination(dir.path(), false).is_err());
    }

    #[test]
    fn filtered_pull_requires_an_empty_destination_even_with_force() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("existing.md"), "keep").expect("existing file");

        let error = validate_filtered_pull_destination(dir.path()).unwrap_err();
        assert!(error.to_string().contains("filtered pull destination"));

        let empty = tempdir().expect("empty tempdir");
        validate_filtered_pull_destination(empty.path()).expect("empty destination");
    }

    #[cfg(unix)]
    #[test]
    fn pull_destination_rejects_symbolic_links_even_with_force() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("target");
        fs::create_dir(&target).expect("target directory");
        let link = dir.path().join("destination");
        symlink(&target, &link).expect("destination symlink");

        assert!(validate_pull_destination(&link, false).is_err());
        assert!(validate_pull_destination(&link, true).is_err());
    }

    #[test]
    fn pull_destination_treats_deleted_pulled_attachment_as_local_change() {
        let dir = tempdir().expect("tempdir");
        let page_dir = write_clean_managed_document(dir.path(), "# Root");
        let attachments = page_dir.join("attachments");
        fs::create_dir_all(&attachments).expect("attachments directory");
        let attachment_path = attachments.join("report.txt");
        fs::write(&attachment_path, "report").expect("attachment");
        let mut doc = load_document(&page_dir).expect("load document");
        doc.sidecar.attachment_map.insert(
            "report.txt".to_string(),
            AttachmentState {
                id: "attachment-1".to_string(),
                file_name: "report.txt".to_string(),
                remote_version: Some(1),
                media_type: Some("text/plain".to_string()),
                sha256: Some(sha256_hex(b"report")),
            },
        );
        save_document(&doc).expect("save attachment state");
        validate_pull_destination(dir.path(), false).expect("unchanged attachment");

        fs::remove_file(&attachment_path).expect("delete attachment locally");

        assert!(validate_pull_destination(dir.path(), false).is_err());
    }

    #[test]
    fn installing_snapshot_replaces_stale_destination_content() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("destination");
        let staged = dir.path().join("staged");
        fs::create_dir_all(&root).expect("create destination");
        fs::create_dir_all(&staged).expect("create staged snapshot");
        fs::write(root.join("stale.txt"), "stale").expect("write stale file");
        fs::write(staged.join("fresh.txt"), "fresh").expect("write fresh file");

        install_staged_snapshot(&staged, &root).expect("install snapshot");

        assert!(!root.join("stale.txt").exists());
        assert_eq!(fs::read_to_string(root.join("fresh.txt")).unwrap(), "fresh");
    }

    #[test]
    fn attachment_names_must_be_single_safe_path_components() {
        assert_eq!(
            safe_attachment_file_name("report.pdf").unwrap(),
            "report.pdf"
        );
        for unsafe_name in [
            "../secret",
            "folder/file",
            r"folder\file",
            "/absolute",
            ".",
            "..",
            "nul\0byte",
            "report:final.pdf",
            "trailing. ",
            "trailing.",
            "NUL",
            "con.txt",
            "COM1.log",
            "lpt9",
        ] {
            assert!(
                safe_attachment_file_name(unsafe_name).is_err(),
                "accepted unsafe attachment name {unsafe_name:?}"
            );
        }
    }

    #[test]
    fn local_attachment_entries_must_be_regular_files() {
        let dir = tempdir().expect("tempdir");
        let attachments = dir.path().join("attachments");
        fs::create_dir_all(attachments.join("nested")).expect("nested attachment directory");
        let error = local_attachment_hashes(dir.path()).unwrap_err();
        assert!(error.to_string().contains("is not a regular file"));
    }

    #[test]
    fn plan_diff_compares_last_pulled_body_with_local_body() {
        let dir = tempdir().expect("tempdir");
        let page_dir = write_clean_managed_document(dir.path(), "old line\n");
        let frontmatter = Frontmatter {
            title: "Root Page".to_string(),
            kind: "page".to_string(),
            labels: vec!["managed".to_string()],
            status: "current".to_string(),
            parent: None,
            properties: BTreeMap::new(),
        };
        fs::write(
            page_dir.join("index.md"),
            render_document(&frontmatter, "new line\n").expect("render changed document"),
        )
        .expect("write changed document");

        let plan = plan_path(dir.path(), false, false, true).expect("plan");
        let update = plan
            .items
            .iter()
            .find(|item| item.action == PlanActionKind::UpdateContent)
            .expect("update action");
        let diff = update.diff.as_deref().expect("body diff");
        assert!(diff.contains("-old line"));
        assert!(diff.contains("+new line"));
    }

    #[test]
    fn empty_sync_path_is_rejected() {
        let dir = tempdir().expect("tempdir");
        let error = plan_path(dir.path(), false, false, false).unwrap_err();
        assert!(error.to_string().contains("contains no index.md documents"));
    }

    #[test]
    fn plan_detects_new_document() {
        let dir = tempdir().expect("tempdir");
        let page_dir = dir.path().join("hello");
        fs::create_dir_all(&page_dir).expect("create");
        let doc = Frontmatter {
            title: "Hello".to_string(),
            kind: "page".to_string(),
            labels: vec![],
            status: "current".to_string(),
            parent: None,
            properties: BTreeMap::new(),
        };
        fs::write(
            page_dir.join("index.md"),
            render_document(&doc, "# Hello").unwrap(),
        )
        .unwrap();
        fs::write(
            page_dir.join(".confluence.json"),
            serde_json::to_string_pretty(&Sidecar {
                provider: Some(ProviderKind::Cloud),
                ..Sidecar::default()
            })
            .unwrap(),
        )
        .unwrap();

        let plan = plan_path(dir.path(), false, false, false).expect("plan");
        assert!(
            plan.items
                .iter()
                .any(|item| item.action == PlanActionKind::CreateContent)
        );
    }

    #[test]
    fn plan_preserves_remote_parent_for_pulled_root() {
        let dir = tempdir().expect("tempdir");
        let page_dir = dir.path().join("root-page");
        fs::create_dir_all(&page_dir).expect("create");

        let doc = Frontmatter {
            title: "Root Page".to_string(),
            kind: "page".to_string(),
            labels: vec![],
            status: "current".to_string(),
            parent: Some("outside-root".to_string()),
            properties: BTreeMap::new(),
        };
        let body = "# Root";
        let storage = markdown_to_storage(body, false).expect("storage");
        let markdown = render_document(&doc, body).expect("rendered");
        fs::write(page_dir.join("index.md"), markdown).expect("write markdown");
        fs::write(
            page_dir.join(".confluence.json"),
            serde_json::to_string_pretty(&Sidecar {
                content_id: Some("123".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                remote_version: Some(7),
                remote_parent_id: Some("999".to_string()),
                last_pulled_hash: Some(sha256_hex(body.as_bytes())),
                storage_hash: Some(sha256_hex(storage.storage.as_bytes())),
                ..Sidecar::default()
            })
            .expect("sidecar"),
        )
        .expect("write sidecar");

        let plan = plan_path(dir.path(), false, false, false).expect("plan");
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].action, PlanActionKind::Noop);
    }

    #[test]
    fn markdown_hash_ignores_trailing_newlines() {
        assert_eq!(markdown_body_hash("Hello"), markdown_body_hash("Hello\n"));
        assert_eq!(markdown_body_hash("Hello"), markdown_body_hash("Hello\n\n"));
    }

    #[test]
    fn plan_ignores_legacy_remote_storage_hash_when_markdown_is_unchanged() {
        let dir = tempdir().expect("tempdir");
        let page_dir = dir.path().join("root-page");
        fs::create_dir_all(&page_dir).expect("create");

        let doc = Frontmatter {
            title: "Root Page".to_string(),
            kind: "page".to_string(),
            labels: vec![],
            status: "current".to_string(),
            parent: None,
            properties: BTreeMap::new(),
        };
        let body = "# Root";
        fs::write(
            page_dir.join("index.md"),
            render_document(&doc, body).expect("rendered"),
        )
        .expect("write markdown");
        fs::write(
            page_dir.join(".confluence.json"),
            serde_json::to_string_pretty(&Sidecar {
                content_id: Some("123".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                remote_version: Some(7),
                last_pulled_hash: Some(markdown_body_hash(body)),
                storage_hash: Some(sha256_hex("legacy-remote-storage")),
                ..Sidecar::default()
            })
            .expect("sidecar"),
        )
        .expect("write sidecar");

        let plan = plan_path(dir.path(), false, false, false).expect("plan");
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].action, PlanActionKind::Noop);
    }

    /// Plans a pulled page whose sidecar records `storage_hash` for its
    /// rendered body, and returns the planned action.
    fn plan_action_with_recorded_storage_hash(body: &str, storage_hash: &str) -> PlanActionKind {
        let dir = tempdir().expect("tempdir");
        let page_dir = dir.path().join("root-page--123");
        fs::create_dir_all(&page_dir).expect("create");
        let frontmatter = Frontmatter {
            title: "Root Page".to_string(),
            kind: "page".to_string(),
            labels: vec![],
            status: "current".to_string(),
            parent: None,
            properties: BTreeMap::new(),
        };
        fs::write(
            page_dir.join("index.md"),
            render_document(&frontmatter, body).expect("rendered"),
        )
        .expect("write markdown");
        fs::write(
            page_dir.join(".confluence.json"),
            serde_json::to_string_pretty(&Sidecar {
                content_id: Some("123".to_string()),
                space_key: Some("DOCS".to_string()),
                provider: Some(ProviderKind::Cloud),
                remote_version: Some(7),
                last_pulled_hash: Some(markdown_body_hash(body)),
                storage_hash: Some(storage_hash.to_string()),
                remote_storage_hash: Some(sha256_hex("remote storage")),
                ..Sidecar::default()
            })
            .expect("sidecar"),
        )
        .expect("write sidecar");

        let plan = plan_path(dir.path(), false, false, false).expect("plan");

        assert_eq!(plan.items.len(), 1);
        plan.items[0].action.clone()
    }

    #[test]
    fn plan_keeps_a_page_whose_rendering_matches_the_recorded_hash() {
        assert_eq!(
            plan_action_with_recorded_storage_hash("# Root", &sha256_hex("<h1>Root</h1>")),
            PlanActionKind::Noop
        );
    }

    #[test]
    fn plan_updates_a_page_whose_rendering_differs_from_the_recorded_hash() {
        assert_eq!(
            plan_action_with_recorded_storage_hash(
                "# Root",
                &sha256_hex("storage an earlier release rendered"),
            ),
            PlanActionKind::UpdateContent
        );
    }

    #[test]
    fn plan_detects_title_change_when_sidecar_tracks_metadata() {
        let dir = tempdir().expect("tempdir");
        let page_dir = dir.path().join("root-page");
        fs::create_dir_all(&page_dir).expect("create");

        let doc = Frontmatter {
            title: "New Title".to_string(),
            kind: "page".to_string(),
            labels: vec![],
            status: "current".to_string(),
            parent: None,
            properties: BTreeMap::new(),
        };
        let body = "# Root";
        let storage = markdown_to_storage(body, false).expect("storage");
        fs::write(
            page_dir.join("index.md"),
            render_document(&doc, body).expect("rendered"),
        )
        .expect("write markdown");
        fs::write(
            page_dir.join(".confluence.json"),
            serde_json::to_string_pretty(&Sidecar {
                content_id: Some("123".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                remote_version: Some(7),
                last_pulled_hash: Some(markdown_body_hash(body)),
                storage_hash: Some(sha256_hex(storage.storage.as_bytes())),
                last_pulled_title: Some("Old Title".to_string()),
                last_pulled_labels: vec![],
                last_pulled_status: Some("current".to_string()),
                last_pulled_properties_hash: Some(properties_hash(&BTreeMap::new())),
                ..Sidecar::default()
            })
            .expect("sidecar"),
        )
        .expect("write sidecar");

        let plan = plan_path(dir.path(), false, false, false).expect("plan");
        assert!(
            plan.items
                .iter()
                .any(|item| item.action == PlanActionKind::UpdateContent
                    && item.details.contains("title"))
        );
    }

    #[test]
    fn status_difference_requires_a_remote_update() {
        let doc = LocalDocument {
            directory: PathBuf::from("page"),
            markdown_path: PathBuf::from("page/index.md"),
            sidecar_path: PathBuf::from("page/.confluence.json"),
            frontmatter: Frontmatter {
                title: "Page".to_string(),
                kind: "page".to_string(),
                labels: vec!["managed".to_string()],
                status: "draft".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown: String::new(),
            sidecar: Sidecar::default(),
        };
        let remote = ContentItem {
            id: "123".to_string(),
            kind: ContentKind::Page,
            title: "Page".to_string(),
            status: "current".to_string(),
            space_id: None,
            space_key: None,
            parent_id: None,
            version: Some(1),
            body_storage: None,
            labels: vec!["managed".to_string()],
            properties: BTreeMap::new(),
            web_url: None,
            created_at: None,
            updated_at: None,
        };

        assert!(metadata_needs_update(&doc, &remote));
    }

    #[test]
    fn three_way_metadata_check_rejects_remote_only_changes() {
        let mut base_properties = BTreeMap::new();
        base_properties.insert("owner".to_string(), serde_json::json!("base"));
        let mut local_properties = BTreeMap::new();
        local_properties.insert("owner".to_string(), serde_json::json!("local"));
        let mut remote_properties = BTreeMap::new();
        remote_properties.insert("owner".to_string(), serde_json::json!("remote"));
        let doc = LocalDocument {
            directory: PathBuf::from("page"),
            markdown_path: PathBuf::from("page/index.md"),
            sidecar_path: PathBuf::from("page/.confluence.json"),
            frontmatter: Frontmatter {
                title: "Page".to_string(),
                kind: "page".to_string(),
                labels: vec!["local".to_string()],
                status: "draft".to_string(),
                parent: None,
                properties: local_properties,
            },
            body_markdown: String::new(),
            sidecar: Sidecar {
                last_pulled_title: Some("Page".to_string()),
                last_pulled_labels: vec!["base".to_string()],
                last_pulled_status: Some("current".to_string()),
                last_pulled_properties_hash: Some(properties_hash(&base_properties)),
                ..Sidecar::default()
            },
        };
        let remote = ContentItem {
            id: "123".to_string(),
            kind: ContentKind::Page,
            title: "Page".to_string(),
            status: "archived".to_string(),
            space_id: None,
            space_key: None,
            parent_id: None,
            version: Some(1),
            body_storage: None,
            labels: vec!["remote".to_string()],
            properties: remote_properties,
            web_url: None,
            created_at: None,
            updated_at: None,
        };

        assert_eq!(
            remote_metadata_drift(&doc, &remote),
            vec!["labels", "status", "properties"]
        );
    }

    #[test]
    fn parent_frontmatter_edits_are_rejected_instead_of_ignored() {
        let doc = LocalDocument {
            directory: PathBuf::from("page"),
            markdown_path: PathBuf::from("page/index.md"),
            sidecar_path: PathBuf::from("page/.confluence.json"),
            frontmatter: Frontmatter {
                title: "Page".to_string(),
                kind: "page".to_string(),
                labels: Vec::new(),
                status: "current".to_string(),
                parent: Some("edited-parent".to_string()),
                properties: BTreeMap::new(),
            },
            body_markdown: String::new(),
            sidecar: Sidecar {
                last_pulled_parent: Some(Some("pulled-parent".to_string())),
                ..Sidecar::default()
            },
        };

        let error = validate_informational_parent(&doc).unwrap_err();
        assert!(error.to_string().contains("parent is informational"));
    }

    #[test]
    fn create_consumes_local_space_key_without_sending_it_as_metadata() {
        let mut frontmatter = Frontmatter {
            properties: BTreeMap::from([
                ("space_key".to_string(), serde_json::json!("DOCS")),
                ("owner".to_string(), serde_json::json!("Ada")),
            ]),
            ..Frontmatter::default()
        };

        let remote = take_create_properties(&mut frontmatter);
        assert!(!remote.contains_key("space_key"));
        assert!(!frontmatter.properties.contains_key("space_key"));
        assert_eq!(remote.get("owner"), Some(&serde_json::json!("Ada")));
    }

    #[test]
    fn attachment_three_way_check_preserves_matching_changes_and_rejects_divergence() {
        let same_as_remote = "remote".to_string();
        let local_edit = "local".to_string();

        assert!(!attachment_bytes_conflict(
            Some("base"),
            Some(&same_as_remote),
            "remote"
        ));
        assert!(attachment_bytes_conflict(
            Some("base"),
            Some(&local_edit),
            "remote"
        ));
        assert!(!attachment_bytes_conflict(
            Some("base"),
            Some(&local_edit),
            "base"
        ));
    }

    #[test]
    fn plan_detects_label_change_when_sidecar_tracks_metadata() {
        let dir = tempdir().expect("tempdir");
        let page_dir = dir.path().join("root-page");
        fs::create_dir_all(&page_dir).expect("create");

        let doc = Frontmatter {
            title: "Root Page".to_string(),
            kind: "page".to_string(),
            labels: vec!["new-label".to_string()],
            status: "current".to_string(),
            parent: None,
            properties: BTreeMap::new(),
        };
        let body = "# Root";
        let storage = markdown_to_storage(body, false).expect("storage");
        fs::write(
            page_dir.join("index.md"),
            render_document(&doc, body).expect("rendered"),
        )
        .expect("write markdown");
        fs::write(
            page_dir.join(".confluence.json"),
            serde_json::to_string_pretty(&Sidecar {
                content_id: Some("123".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                remote_version: Some(7),
                last_pulled_hash: Some(markdown_body_hash(body)),
                storage_hash: Some(sha256_hex(storage.storage.as_bytes())),
                last_pulled_title: Some("Root Page".to_string()),
                last_pulled_labels: vec![],
                last_pulled_status: Some("current".to_string()),
                last_pulled_properties_hash: Some(properties_hash(&BTreeMap::new())),
                ..Sidecar::default()
            })
            .expect("sidecar"),
        )
        .expect("write sidecar");

        let plan = plan_path(dir.path(), false, false, false).expect("plan");
        assert!(
            plan.items
                .iter()
                .any(|item| item.action == PlanActionKind::UpdateContent
                    && item.details.contains("labels"))
        );
    }

    #[test]
    fn plan_is_noop_when_metadata_unchanged_and_sidecar_tracks_metadata() {
        let dir = tempdir().expect("tempdir");
        let page_dir = dir.path().join("root-page");
        fs::create_dir_all(&page_dir).expect("create");

        let doc = Frontmatter {
            title: "Root Page".to_string(),
            kind: "page".to_string(),
            labels: vec!["my-label".to_string()],
            status: "current".to_string(),
            parent: None,
            properties: BTreeMap::new(),
        };
        let body = "# Root";
        let storage = markdown_to_storage(body, false).expect("storage");
        fs::write(
            page_dir.join("index.md"),
            render_document(&doc, body).expect("rendered"),
        )
        .expect("write markdown");
        fs::write(
            page_dir.join(".confluence.json"),
            serde_json::to_string_pretty(&Sidecar {
                content_id: Some("123".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                remote_version: Some(7),
                last_pulled_hash: Some(markdown_body_hash(body)),
                storage_hash: Some(sha256_hex(storage.storage.as_bytes())),
                last_pulled_title: Some("Root Page".to_string()),
                last_pulled_labels: vec!["my-label".to_string()],
                last_pulled_status: Some("current".to_string()),
                last_pulled_properties_hash: Some(properties_hash(&BTreeMap::new())),
                ..Sidecar::default()
            })
            .expect("sidecar"),
        )
        .expect("write sidecar");

        let plan = plan_path(dir.path(), false, false, false).expect("plan");
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].action, PlanActionKind::Noop);
    }

    #[test]
    fn render_body_storage_rewrites_local_page_and_attachment_links() {
        let current_dir = PathBuf::from("/tmp/root/current-page--123");
        let sibling_dir = PathBuf::from("/tmp/root/sibling-page--456");
        let current = LocalDocument {
            directory: current_dir.clone(),
            markdown_path: current_dir.join("index.md"),
            sidecar_path: current_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Current".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown:
                "[Sibling](../sibling-page--456/index.md#intro)\n\n![Logo](attachments/logo.png)\n\n[Comment](../sibling-page--456/index.md?focusedCommentId=42#details)\n"
                    .to_string(),
            sidecar: Sidecar {
                content_id: Some("123".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };
        let sibling = LocalDocument {
            directory: sibling_dir.clone(),
            markdown_path: sibling_dir.join("index.md"),
            sidecar_path: sibling_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Sibling".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown: "# Sibling".to_string(),
            sidecar: Sidecar {
                content_id: Some("456".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };

        let index = build_link_index(&[current.clone(), sibling]);
        let storage =
            render_body_storage(&current, &index, false, "/wiki").expect("render body storage");

        assert!(storage.contains(r#"href="/wiki/pages/viewpage.action?pageId=456#intro""#));
        assert!(storage.contains(
            r#"href="/wiki/pages/viewpage.action?pageId=456&amp;focusedCommentId=42#details""#
        ));
        assert!(storage.contains(
            r#"<ac:image ac:alt="Logo"><ri:attachment ri:filename="logo.png" /></ac:image>"#
        ));
        assert!(!storage.contains("download/attachments"));
    }

    #[test]
    fn rewrite_page_links_resolves_confluence_page_placeholders_to_local_paths() {
        let current_dir = PathBuf::from("/tmp/root/current-page--123");
        let sibling_dir = PathBuf::from("/tmp/root/sibling-page--456");
        let placeholder = build_page_placeholder_url(&PageLinkPlaceholder {
            content_title: Some("Sibling".to_string()),
            space_key: Some("MFS".to_string()),
            anchor: Some("intro".to_string()),
            ..PageLinkPlaceholder::default()
        });
        let markdown = format!("[Sibling]({placeholder})");
        let mut paths = BTreeMap::new();
        paths.insert("456".to_string(), sibling_dir.clone());
        let mut items = BTreeMap::new();
        items.insert(
            "456".to_string(),
            ContentItem {
                id: "456".to_string(),
                kind: ContentKind::Page,
                title: "Sibling".to_string(),
                status: "current".to_string(),
                space_id: None,
                space_key: Some("MFS".to_string()),
                parent_id: None,
                version: None,
                body_storage: None,
                labels: vec![],
                properties: BTreeMap::new(),
                web_url: None,
                created_at: None,
                updated_at: None,
            },
        );

        let rewritten = rewrite_page_links(&markdown, &current_dir, &paths, &items);
        assert_eq!(rewritten, "[Sibling](../sibling-page--456/index.md#intro)");
    }

    #[test]
    fn rewrite_generic_macro_page_placeholders_resolve_to_local_paths() {
        let current_dir = PathBuf::from("/tmp/root/current-page--123");
        let sibling_dir = PathBuf::from("/tmp/root/sibling-page--456");
        let source = build_page_placeholder_url(&PageLinkPlaceholder {
            content_title: Some("Sibling".to_string()),
            space_key: Some("MFS".to_string()),
            ..PageLinkPlaceholder::default()
        });
        let related = build_page_placeholder_url(&PageLinkPlaceholder {
            content_id: Some("456".to_string()),
            ..PageLinkPlaceholder::default()
        });
        let markdown = format!(
            ":::confluence-macro userlister\nsource: !page {source}\nrelated: !page-link {related}\n:::\n"
        );
        let mut paths = BTreeMap::new();
        paths.insert("456".to_string(), sibling_dir.clone());
        let mut items = BTreeMap::new();
        items.insert(
            "456".to_string(),
            ContentItem {
                id: "456".to_string(),
                kind: ContentKind::Page,
                title: "Sibling".to_string(),
                status: "current".to_string(),
                space_id: None,
                space_key: Some("MFS".to_string()),
                parent_id: None,
                version: None,
                body_storage: None,
                labels: vec![],
                properties: BTreeMap::new(),
                web_url: None,
                created_at: None,
                updated_at: None,
            },
        );

        let rewritten =
            rewrite_generic_macro_page_placeholders(&markdown, &current_dir, &paths, &items);
        assert_eq!(
            rewritten,
            ":::confluence-macro userlister\nsource: !page ../sibling-page--456/index.md\nrelated: !page-link ../sibling-page--456/index.md\n:::\n"
        );
    }

    #[test]
    fn render_body_storage_rewrites_page_placeholders_back_to_storage_links() {
        let current_dir = PathBuf::from("/tmp/root/current-page--123");
        let placeholder = build_page_placeholder_url(&PageLinkPlaceholder {
            content_title: Some("Docs Home".to_string()),
            space_key: Some("MFS".to_string()),
            anchor: Some("intro".to_string()),
            ..PageLinkPlaceholder::default()
        });
        let current = LocalDocument {
            directory: current_dir.clone(),
            markdown_path: current_dir.join("index.md"),
            sidecar_path: current_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Current".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown: format!("[Docs]({placeholder})\n"),
            sidecar: Sidecar {
                content_id: Some("123".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };

        let index = build_link_index(std::slice::from_ref(&current));
        let storage =
            render_body_storage(&current, &index, false, "/wiki").expect("render body storage");

        assert!(storage.contains(r#"<ac:link ac:anchor="intro">"#));
        assert!(storage.contains(r#"ri:content-title="Docs Home""#));
        assert!(storage.contains(r#"ri:space-key="MFS""#));
        assert!(
            storage.contains("<ac:plain-text-link-body><![CDATA[Docs]]></ac:plain-text-link-body>")
        );
    }

    #[test]
    fn render_body_storage_rewrites_generic_macro_page_parameters_to_storage_macros() {
        let current_dir = PathBuf::from("/tmp/root/current-page--123");
        let sibling_dir = PathBuf::from("/tmp/root/sibling-page--456");
        let current = LocalDocument {
            directory: current_dir.clone(),
            markdown_path: current_dir.join("index.md"),
            sidecar_path: current_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Current".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown:
                ":::confluence-macro userlister\nsource: !page ../sibling-page--456/index.md\nrelated: !page-link ../sibling-page--456/index.md\n:::\n"
                    .to_string(),
            sidecar: Sidecar {
                content_id: Some("123".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };
        let sibling = LocalDocument {
            directory: sibling_dir.clone(),
            markdown_path: sibling_dir.join("index.md"),
            sidecar_path: sibling_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Sibling".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown: "# Sibling".to_string(),
            sidecar: Sidecar {
                content_id: Some("456".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };

        let index = build_link_index(&[current.clone(), sibling]);
        let storage =
            render_body_storage(&current, &index, false, "/wiki").expect("render body storage");

        assert!(storage.contains(r#"<ac:structured-macro ac:name="userlister">"#));
        assert!(storage.contains(
            r#"<ac:parameter ac:name="source"><ri:page ri:content-title="Sibling" ri:space-key="MFS" /></ac:parameter>"#
        ));
        assert!(storage.contains(
            r#"<ac:parameter ac:name="related"><ac:link><ri:page ri:content-title="Sibling" ri:space-key="MFS" /></ac:link></ac:parameter>"#
        ));
    }

    #[test]
    fn render_body_storage_rewrites_user_placeholders_back_to_storage_links() {
        let current_dir = PathBuf::from("/tmp/root/current-page--123");
        let placeholder = build_user_placeholder_url(&UserMentionPlaceholder {
            account_id: Some("abc123".to_string()),
            ..UserMentionPlaceholder::default()
        });
        let current = LocalDocument {
            directory: current_dir.clone(),
            markdown_path: current_dir.join("index.md"),
            sidecar_path: current_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Current".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown: format!("[@Ruben]({placeholder})\n"),
            sidecar: Sidecar {
                content_id: Some("123".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };

        let index = build_link_index(std::slice::from_ref(&current));
        let storage =
            render_body_storage(&current, &index, false, "/wiki").expect("render body storage");

        assert!(storage.contains(r#"<ri:user ri:account-id="abc123" />"#));
        assert!(
            storage
                .contains("<ac:plain-text-link-body><![CDATA[@Ruben]]></ac:plain-text-link-body>")
        );
    }

    #[test]
    fn status_placeholder_keeps_a_title_that_holds_markup_characters() {
        let storage = resolve_placeholder_links(
            r#"<p><a href="confluence-status://status?title=R%26D&amp;colour=Green">R&amp;D &lt;1&gt;</a></p>"#,
        );

        assert_eq!(
            storage,
            r#"<p><ac:structured-macro ac:name="status"><ac:parameter ac:name="title">R&amp;D &lt;1&gt;</ac:parameter><ac:parameter ac:name="colour">Green</ac:parameter></ac:structured-macro></p>"#
        );
    }

    #[test]
    fn placeholder_link_written_inside_a_comment_stays_text() {
        let storage = r#"<p>a</p><!-- <a href="confluence-user://user?userkey=foo"> --><p><a href="https://example.com">site</a> end</p>"#;

        assert_eq!(resolve_placeholder_links(storage), storage);
    }

    #[test]
    fn placeholder_link_after_a_comment_and_one_holding_a_comment_are_both_written() {
        let storage = resolve_placeholder_links(
            r#"<!-- <a href="confluence-user://user?userkey=foo"> --><p><a href="confluence-user://user?userkey=bar">one</a> <a href="confluence-page://page?content-id=7">two<!-- note --></a></p>"#,
        );

        assert!(
            storage.starts_with(r#"<!-- <a href="confluence-user://user?userkey=foo"> --><p><ac:link><ri:user ri:userkey="bar" />"#),
            "{storage}"
        );
        assert!(
            storage.contains(r#"<ri:page ri:content-id="7" /><ac:link-body>two<!-- note --></ac:link-body></ac:link></p>"#),
            "{storage}"
        );
    }

    #[test]
    fn render_body_storage_rewrites_status_placeholders_back_to_storage_macros() {
        let current_dir = PathBuf::from("/tmp/root/current-page--123");
        let placeholder = build_status_placeholder_url(&StatusMacroPlaceholder {
            title: "Ready".to_string(),
            colour: Some("Green".to_string()),
        });
        let current = LocalDocument {
            directory: current_dir.clone(),
            markdown_path: current_dir.join("index.md"),
            sidecar_path: current_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Current".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown: format!("State: [Ready]({placeholder})\n"),
            sidecar: Sidecar {
                content_id: Some("123".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };

        let index = build_link_index(std::slice::from_ref(&current));
        let storage =
            render_body_storage(&current, &index, false, "/wiki").expect("render body storage");

        assert!(storage.contains(r#"<ac:structured-macro ac:name="status">"#));
        assert!(storage.contains(r#"<ac:parameter ac:name="title">Ready</ac:parameter>"#));
        assert!(storage.contains(r#"<ac:parameter ac:name="colour">Green</ac:parameter>"#));
    }

    #[test]
    fn render_body_storage_rewrites_excerpt_include_page_parameters_to_storage_macros() {
        let current_dir = PathBuf::from("/tmp/root/current-page--123");
        let sibling_dir = PathBuf::from("/tmp/root/sibling-page--456");
        let current = LocalDocument {
            directory: current_dir.clone(),
            markdown_path: current_dir.join("index.md"),
            sidecar_path: current_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Current".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown: ":::confluence-excerpt-include\nnopanel: true\npage: ../sibling-page--456/index.md\n:::\n".to_string(),
            sidecar: Sidecar {
                content_id: Some("123".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };
        let sibling = LocalDocument {
            directory: sibling_dir.clone(),
            markdown_path: sibling_dir.join("index.md"),
            sidecar_path: sibling_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Sibling".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown: "# Sibling".to_string(),
            sidecar: Sidecar {
                content_id: Some("456".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };

        let index = build_link_index(&[current.clone(), sibling]);
        let storage =
            render_body_storage(&current, &index, false, "/wiki").expect("render body storage");

        assert!(storage.contains(r#"<ac:structured-macro ac:name="excerpt-include">"#));
        assert!(storage.contains(r#"<ac:parameter ac:name="nopanel">true</ac:parameter>"#));
        assert!(
            storage.contains(
                r#"<ac:parameter ac:name="default-parameter">MFS:Sibling</ac:parameter>"#
            )
        );
    }

    #[test]
    fn render_body_storage_rewrites_children_page_parameters_to_storage_macros() {
        let current_dir = PathBuf::from("/tmp/root/current-page--123");
        let sibling_dir = PathBuf::from("/tmp/root/sibling-page--456");
        let current = LocalDocument {
            directory: current_dir.clone(),
            markdown_path: current_dir.join("index.md"),
            sidecar_path: current_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Current".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown:
                ":::confluence-children\npage: ../sibling-page--456/index.md\nall: true\nsort: creation\n:::\n"
                    .to_string(),
            sidecar: Sidecar {
                content_id: Some("123".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };
        let sibling = LocalDocument {
            directory: sibling_dir.clone(),
            markdown_path: sibling_dir.join("index.md"),
            sidecar_path: sibling_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Sibling".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown: "# Sibling".to_string(),
            sidecar: Sidecar {
                content_id: Some("456".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };

        let index = build_link_index(&[current.clone(), sibling]);
        let storage =
            render_body_storage(&current, &index, false, "/wiki").expect("render body storage");

        assert!(storage.contains(r#"<ac:structured-macro ac:name="children">"#));
        assert!(storage.contains(
            r#"<ac:parameter ac:name="page"><ac:link><ri:page ri:content-title="Sibling" ri:space-key="MFS" /></ac:link></ac:parameter>"#
        ));
        assert!(storage.contains(r#"<ac:parameter ac:name="all">true</ac:parameter>"#));
        assert!(storage.contains(r#"<ac:parameter ac:name="sort">creation</ac:parameter>"#));
    }

    #[test]
    fn render_body_storage_rewrites_include_page_parameters_to_storage_macros() {
        let current_dir = PathBuf::from("/tmp/root/current-page--123");
        let sibling_dir = PathBuf::from("/tmp/root/sibling-page--456");
        let current = LocalDocument {
            directory: current_dir.clone(),
            markdown_path: current_dir.join("index.md"),
            sidecar_path: current_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Current".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown: ":::confluence-include-page\npage: ../sibling-page--456/index.md\n:::\n"
                .to_string(),
            sidecar: Sidecar {
                content_id: Some("123".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };
        let sibling = LocalDocument {
            directory: sibling_dir.clone(),
            markdown_path: sibling_dir.join("index.md"),
            sidecar_path: sibling_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Sibling".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown: "# Sibling".to_string(),
            sidecar: Sidecar {
                content_id: Some("456".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };

        let index = build_link_index(&[current.clone(), sibling]);
        let storage =
            render_body_storage(&current, &index, false, "/wiki").expect("render body storage");

        assert!(storage.contains(r#"<ac:structured-macro ac:name="include">"#));
        assert!(storage.contains(r#"ri:content-title="Sibling""#));
        assert!(storage.contains(r#"ri:space-key="MFS""#));
    }

    #[test]
    fn render_body_storage_rewrites_page_tree_root_parameters_to_storage_macros() {
        let current_dir = PathBuf::from("/tmp/root/current-page--123");
        let sibling_dir = PathBuf::from("/tmp/root/sibling-page--456");
        let current = LocalDocument {
            directory: current_dir.clone(),
            markdown_path: current_dir.join("index.md"),
            sidecar_path: current_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Current".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown:
                ":::confluence-page-tree\nroot: ../sibling-page--456/index.md\nsearchBox: true\n:::\n"
                    .to_string(),
            sidecar: Sidecar {
                content_id: Some("123".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };
        let sibling = LocalDocument {
            directory: sibling_dir.clone(),
            markdown_path: sibling_dir.join("index.md"),
            sidecar_path: sibling_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Sibling".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown: "# Sibling".to_string(),
            sidecar: Sidecar {
                content_id: Some("456".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };

        let index = build_link_index(&[current.clone(), sibling]);
        let storage =
            render_body_storage(&current, &index, false, "/wiki").expect("render body storage");

        assert!(storage.contains(r#"<ac:structured-macro ac:name="pagetree">"#));
        assert!(storage.contains(
            r#"<ac:parameter ac:name="root"><ac:link><ri:page ri:content-title="Sibling" ri:space-key="MFS" /></ac:link></ac:parameter>"#
        ));
        assert!(storage.contains(r#"<ac:parameter ac:name="searchBox">true</ac:parameter>"#));
    }

    #[test]
    fn render_body_storage_rewrites_page_tree_search_root_parameters_to_storage_macros() {
        let current_dir = PathBuf::from("/tmp/root/current-page--123");
        let sibling_dir = PathBuf::from("/tmp/root/sibling-page--456");
        let current = LocalDocument {
            directory: current_dir.clone(),
            markdown_path: current_dir.join("index.md"),
            sidecar_path: current_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Current".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown:
                ":::confluence-page-tree-search\nroot: ../sibling-page--456/index.md\nspaceKey: MFS\n:::\n"
                    .to_string(),
            sidecar: Sidecar {
                content_id: Some("123".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };
        let sibling = LocalDocument {
            directory: sibling_dir.clone(),
            markdown_path: sibling_dir.join("index.md"),
            sidecar_path: sibling_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Sibling".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown: "# Sibling".to_string(),
            sidecar: Sidecar {
                content_id: Some("456".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };

        let index = build_link_index(&[current.clone(), sibling]);
        let storage =
            render_body_storage(&current, &index, false, "/wiki").expect("render body storage");

        assert!(storage.contains(r#"<ac:structured-macro ac:name="pagetreesearch">"#));
        assert!(storage.contains(r#"<ac:parameter ac:name="root">MFS:Sibling</ac:parameter>"#));
        assert!(storage.contains(r#"<ac:parameter ac:name="spaceKey">MFS</ac:parameter>"#));
    }

    #[test]
    fn render_body_storage_rewrites_attachment_preview_page_parameters_to_storage_macros() {
        let current_dir = PathBuf::from("/tmp/root/current-page--123");
        let sibling_dir = PathBuf::from("/tmp/root/sibling-page--456");
        let current = LocalDocument {
            directory: current_dir.clone(),
            markdown_path: current_dir.join("index.md"),
            sidecar_path: current_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Current".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown:
                ":::confluence-view-doc\npage: ../sibling-page--456/index.md\nattachment: manual.docx\n:::\n"
                    .to_string(),
            sidecar: Sidecar {
                content_id: Some("123".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };
        let sibling = LocalDocument {
            directory: sibling_dir.clone(),
            markdown_path: sibling_dir.join("index.md"),
            sidecar_path: sibling_dir.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Sibling".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown: "# Sibling".to_string(),
            sidecar: Sidecar {
                content_id: Some("456".to_string()),
                space_key: Some("MFS".to_string()),
                provider: Some(ProviderKind::Cloud),
                web_path_prefix: Some("/wiki".to_string()),
                ..Sidecar::default()
            },
        };

        let index = build_link_index(&[current.clone(), sibling]);
        let storage =
            render_body_storage(&current, &index, false, "/wiki").expect("render body storage");

        assert!(storage.contains(r#"<ac:structured-macro ac:name="viewdoc">"#));
        assert!(
            storage.contains(
                r#"<ac:parameter ac:name="page"><ac:link><ri:page ri:content-title="Sibling" ri:space-key="MFS" /></ac:link></ac:parameter>"#
            )
        );
        assert!(storage.contains(
            r#"<ac:parameter ac:name="name"><ri:attachment ri:filename="manual.docx" /></ac:parameter>"#
        ));
    }
}

/// Pulls storage to Markdown and renders it back through the apply path,
/// asserting the result is the same document as the remote original.
#[cfg(test)]
mod round_trip_tests {
    use std::collections::BTreeMap;

    use roxmltree::{Document, Node, NodeType};

    use super::*;
    use crate::markdown::{Frontmatter, Sidecar, storage_to_markdown};
    use crate::model::ProviderKind;

    const WRAPPER_OPEN: &str =
        r#"<root xmlns:ac="urn:confluence-ac" xmlns:ri="urn:confluence-ri">"#;

    fn apply_storage(markdown: &str) -> Result<String> {
        apply_storage_in("/tmp/root/current-page--123", markdown)
    }

    fn apply_storage_in(directory: &str, markdown: &str) -> Result<String> {
        let directory = PathBuf::from(directory);
        let doc = LocalDocument {
            directory: directory.clone(),
            markdown_path: directory.join("index.md"),
            sidecar_path: directory.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: "Current".to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown: markdown.to_string(),
            sidecar: Sidecar {
                content_id: Some("123".to_string()),
                provider: Some(ProviderKind::DataCenter),
                ..Sidecar::default()
            },
        };
        let index = build_link_index(std::slice::from_ref(&doc));
        render_body_storage(&doc, &index, false, "")
    }

    /// Serialises storage with attributes sorted, entities resolved and
    /// insignificant whitespace removed, so two spellings of one document
    /// compare equal.
    fn canonical(storage: &str) -> String {
        let wrapped = format!("{WRAPPER_OPEN}{storage}</root>");
        let document = Document::parse(&wrapped)
            .unwrap_or_else(|error| panic!("storage is not well-formed: {error}\n{storage}"));
        let mut out = String::new();
        for child in document.root_element().children() {
            write_canonical(child, &mut out);
        }
        out
    }

    fn write_canonical(node: Node<'_, '_>, out: &mut String) {
        match node.node_type() {
            NodeType::Text => {
                let text = node.text().unwrap_or_default();
                let literal = node.parent().is_some_and(|parent| {
                    matches!(
                        parent.tag_name().name(),
                        "plain-text-body" | "plain-text-link-body" | "pre" | "code"
                    )
                });
                if literal {
                    out.push_str(&format!("{text:?}"));
                    return;
                }
                // A line break at either end of a text node is layout. Other
                // whitespace separates words, also from a neighbouring
                // element.
                let layout = |edge: &str| edge.contains('\n');
                let leading = &text[..text.len() - text.trim_start().len()];
                let trailing = &text[text.trim_end().len()..];
                let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
                if words.is_empty() {
                    if !text.is_empty() && !layout(text) {
                        out.push_str("\" \"");
                    }
                    return;
                }
                let before = if leading.is_empty() || layout(leading) {
                    ""
                } else {
                    " "
                };
                let after = if trailing.is_empty() || layout(trailing) {
                    ""
                } else {
                    " "
                };
                out.push_str(&format!("{:?}", format!("{before}{words}{after}")));
            }
            NodeType::Element => {
                let prefix = match node.tag_name().namespace() {
                    Some("urn:confluence-ac") => "ac:",
                    Some("urn:confluence-ri") => "ri:",
                    _ => "",
                };
                out.push_str(&format!("<{prefix}{}", node.tag_name().name()));
                let mut attributes: Vec<String> = node
                    .attributes()
                    .map(|attribute| format!(" {}={:?}", attribute.name(), attribute.value()))
                    .collect();
                attributes.sort();
                out.push_str(&attributes.concat());
                out.push('>');
                for child in node.children() {
                    write_canonical(child, out);
                }
                out.push_str("</>");
            }
            _ => {}
        }
    }

    fn assert_round_trips(storage: &str) {
        let markdown = storage_to_markdown(storage);
        let applied = apply_storage(&markdown)
            .unwrap_or_else(|error| panic!("apply failed for {markdown:?}: {error:#}"));
        assert_eq!(
            canonical(&applied),
            canonical(storage),
            "pulled as {markdown:?}, applied as {applied}"
        );
    }

    fn named(user: UserReference, name: &str) -> BTreeMap<UserReference, Option<String>> {
        BTreeMap::from([(user, Some(name.to_string()))])
    }

    #[test]
    fn mention_shows_the_name_of_the_user_and_is_written_back_without_a_body() {
        for (storage, user) in [
            (
                r#"<p>ask <ac:link><ri:user ri:account-id="557058:abc-1" /></ac:link> now</p>"#,
                UserReference::AccountId("557058:abc-1".to_string()),
            ),
            (
                r#"<p>ask <ac:link><ri:user ri:userkey="8a8a8a8a" /></ac:link> now</p>"#,
                UserReference::UserKey("8a8a8a8a".to_string()),
            ),
            (
                r#"<p>ask <ac:link><ri:user ri:username="jdoe" /></ac:link> now</p>"#,
                UserReference::Username("jdoe".to_string()),
            ),
        ] {
            let pulled = storage_to_markdown(storage);
            assert_eq!(unnamed_mentions(&pulled), BTreeSet::from([user.clone()]));
            for name in ["Jane Doe", "Jane O'Neil [ops]", "A_b *c* (d)", "Zoë & Co"] {
                let markdown = name_user_mentions(&pulled, &named(user.clone(), name));
                let label = crate::markdown::escape_markdown_text(&format!("@{name}"));
                assert!(
                    markdown.contains(&format!("ask [{label}](confluence-user://user?")),
                    "{markdown}"
                );
                assert!(unnamed_mentions(&markdown).is_empty(), "{markdown}");
                let applied = apply_storage(&markdown).expect("apply");
                assert_eq!(canonical(&applied), canonical(storage), "{markdown}");
            }
        }
    }

    #[test]
    fn mention_whose_shown_name_is_edited_gets_that_text_as_its_body() {
        let storage = r#"<p>ask <ac:link><ri:user ri:account-id="abc" /></ac:link> now</p>"#;
        let user = UserReference::AccountId("abc".to_string());
        let markdown = name_user_mentions(&storage_to_markdown(storage), &named(user, "Jane Doe"));
        let edited = markdown.replace("[@Jane Doe]", "[Jane]");
        assert_ne!(edited, markdown);
        let applied = apply_storage(&edited).expect("apply");
        assert_eq!(
            canonical(&applied),
            canonical(
                r#"<p>ask <ac:link><ri:user ri:account-id="abc" /><ac:plain-text-link-body><![CDATA[Jane]]></ac:plain-text-link-body></ac:link> now</p>"#
            ),
            "{edited}"
        );
    }

    #[test]
    fn mention_of_a_user_without_a_known_name_keeps_its_identifier() {
        let storage = r#"<p>ask <ac:link><ri:user ri:account-id="abc" /></ac:link> now</p>"#;
        let pulled = storage_to_markdown(storage);
        let user = UserReference::AccountId("abc".to_string());
        assert_eq!(
            name_user_mentions(&pulled, &BTreeMap::from([(user, None)])),
            pulled
        );
        assert_eq!(name_user_mentions(&pulled, &BTreeMap::new()), pulled);
    }

    #[test]
    fn only_a_mention_without_a_body_is_given_a_name() {
        let user = UserReference::AccountId("abc".to_string());
        for storage in [
            r#"<p>ask <ac:link><ri:user ri:account-id="abc" /><ac:plain-text-link-body><![CDATA[the owner]]></ac:plain-text-link-body></ac:link> now</p>"#,
            r#"<table class="wide"><tbody><tr><td><ac:link><ri:user ri:account-id="abc" /></ac:link></td></tr></tbody></table>"#,
            r#"<ac:structured-macro ac:name="profile"><ac:parameter ac:name="user"><ri:user ri:account-id="abc" /></ac:parameter></ac:structured-macro>"#,
        ] {
            let pulled = storage_to_markdown(storage);
            assert!(unnamed_mentions(&pulled).is_empty(), "{pulled}");
            assert_eq!(
                name_user_mentions(&pulled, &named(user.clone(), "Jane Doe")),
                pulled
            );
        }
    }

    #[test]
    fn mention_in_a_table_keeps_its_cell_when_the_name_holds_a_cell_separator() {
        let storage = concat!(
            "<table><thead><tr><th>Owner</th><th>Note</th></tr></thead><tbody>",
            r#"<tr><td><ac:link><ri:user ri:account-id="abc" /></ac:link></td><td>x</td></tr>"#,
            "</tbody></table>",
        );
        let pulled = storage_to_markdown(storage);
        assert!(pulled.contains("| Owner | Note |"), "{pulled}");
        let user = UserReference::AccountId("abc".to_string());
        for name in ["Ops | Jane", "Jane|", r"Jane \| Ops"] {
            let markdown = name_user_mentions(&pulled, &named(user.clone(), name));
            assert_ne!(markdown, pulled);
            assert!(unnamed_mentions(&markdown).is_empty(), "{markdown}");
            let applied = apply_storage(&markdown).expect("apply");
            assert_eq!(canonical(&applied), canonical(storage), "{markdown}");
        }
    }

    #[test]
    fn mention_shows_a_name_on_one_line_with_single_spaces() {
        let user = UserReference::AccountId("abc".to_string());
        for storage in [
            r#"<p>ask <ac:link><ri:user ri:account-id="abc" /></ac:link> now</p>"#,
            r#"<h2>Owner <ac:link><ri:user ri:account-id="abc" /></ac:link></h2>"#,
            r#"<ul><li><ac:link><ri:user ri:account-id="abc" /></ac:link></li></ul>"#,
        ] {
            let pulled = storage_to_markdown(storage);
            for name in ["  Jane  ", "Jane\r\nDoe", "Jane\nDoe", "Jane \t Doe"] {
                let markdown = name_user_mentions(&pulled, &named(user.clone(), name));
                assert!(
                    markdown.contains("[@Jane](") || markdown.contains("[@Jane Doe]("),
                    "{markdown}"
                );
                let applied = apply_storage(&markdown).expect("apply");
                assert_eq!(canonical(&applied), canonical(storage), "{markdown}");
            }
            assert_eq!(
                name_user_mentions(&pulled, &named(user.clone(), " \n ")),
                pulled
            );
        }
    }

    #[test]
    fn mention_written_as_a_macro_parameter_is_not_given_a_name() {
        let user = UserReference::AccountId("abc".to_string());
        let mention = "[abc](confluence-user://user?account-id=abc&label=generated)";
        let markdown = format!(
            ":::confluence-macro custom\ntitle: {mention}\n---\nask {mention}\n:::\n\n:::confluence-macro other\nnote: {mention}\n:::\n\nand {mention}\n"
        );
        let named = name_user_mentions(&markdown, &named(user, "Jane Doe"));
        assert!(named.contains(&format!("title: {mention}\n")), "{named}");
        assert!(named.contains(&format!("note: {mention}\n")), "{named}");
        assert_eq!(named.matches("[@Jane Doe](").count(), 2, "{named}");
    }

    #[test]
    fn mention_after_a_code_block_that_shows_a_macro_block_is_given_a_name() {
        let user = UserReference::AccountId("abc".to_string());
        let mention = "[abc](confluence-user://user?account-id=abc&label=generated)";
        let markdown = format!("```\n:::confluence-macro custom\ntitle: x\n```\n\nask {mention}\n");
        let named = name_user_mentions(&markdown, &named(user, "Jane Doe"));
        assert_eq!(named.matches("[@Jane Doe](").count(), 1, "{named}");
    }

    #[test]
    fn address_inside_a_comment_stays_as_pulled() {
        let paths = BTreeMap::from([("12".to_string(), PathBuf::from("/tmp/root/short--12"))]);
        let current = PathBuf::from("/tmp/root/current--1");
        for markdown in [
            "<!-- see /wiki/spaces/S/pages/12/Short -->",
            "<!--\n\nsee /wiki/spaces/S/pages/12 -->\n\ntext",
        ] {
            assert_eq!(
                rewrite_page_links(markdown, &current, &paths, &BTreeMap::new()),
                markdown
            );
        }
    }

    #[test]
    fn address_after_text_that_reads_as_the_start_of_a_comment_is_rewritten() {
        let paths = BTreeMap::from([("12".to_string(), PathBuf::from("/tmp/root/short--12"))]);
        let current = PathBuf::from("/tmp/root/current--1");
        for opening in ["`<!--`", "\\<!--"] {
            let rewritten = rewrite_page_links(
                &format!(
                    "{opening} then [a](https://example.atlassian.net/wiki/spaces/S/pages/12/Short) and `-->`\n"
                ),
                &current,
                &paths,
                &BTreeMap::new(),
            );
            assert_eq!(
                rewritten,
                format!("{opening} then [a](../short--12/index.md) and `-->`\n")
            );
        }
    }

    #[test]
    fn address_of_a_page_is_rewritten_for_that_page_only() {
        let paths = BTreeMap::from([("12".to_string(), PathBuf::from("/tmp/root/short--12"))]);
        let current = PathBuf::from("/tmp/root/current--1");
        for (markdown, rewritten) in [
            (
                "[a](https://example.atlassian.net/wiki/spaces/S/pages/123/Long) [b](/wiki/spaces/S/pages/123) [c](/pages/viewpage.action?pageId=123)",
                None,
            ),
            (
                "[a](https://example.atlassian.net/wiki/spaces/S/pages/12/Short)",
                Some("[a](../short--12/index.md)"),
            ),
        ] {
            assert_eq!(
                rewrite_page_links(markdown, &current, &paths, &BTreeMap::new()),
                rewritten.unwrap_or(markdown),
            );
        }
    }

    #[test]
    fn placeholder_link_keeps_a_comment_that_holds_a_closing_tag() {
        let markdown = "[A<!-- </a> -->B](confluence-user://user?username=jdoe)";
        let applied = apply_storage(markdown).expect("applies");
        assert_eq!(applied.matches("<ac:link>").count(), 1, "{applied}");
        assert!(applied.contains("A<!-- </a> -->B"), "{applied}");
        assert!(!applied.contains("<a "), "{applied}");
        let unclosed = r#"<p><a href="confluence-user://user?username=jdoe">A<!-- </a> --></p>"#;
        assert_eq!(resolve_placeholder_links(unclosed), unclosed);
    }

    #[test]
    fn mention_in_a_layout_cell_is_found_and_given_a_name() {
        let user = UserReference::AccountId("abc".to_string());
        let storage = r#"<ac:layout><ac:layout-section ac:type="two_equal"><ac:layout-cell><p>left <ac:link><ri:user ri:account-id="abc" /></ac:link></p></ac:layout-cell><ac:layout-cell><p><code>[abc](confluence-user://user?account-id=abc&amp;label=generated)</code></p><p>right <ac:link><ri:user ri:account-id="abc" /></ac:link></p></ac:layout-cell></ac:layout-section></ac:layout>"#;
        let pulled = storage_to_markdown(storage);
        assert_eq!(unnamed_mentions(&pulled), BTreeSet::from([user.clone()]));
        let markdown = name_user_mentions(&pulled, &named(user, "Jane Doe"));
        assert_eq!(markdown.matches("[@Jane Doe](").count(), 2, "{markdown}");
        assert!(markdown.contains("`[abc](confluence-user://"), "{markdown}");
        let applied = apply_storage(&markdown).expect("apply");
        assert_eq!(canonical(&applied), canonical(storage), "{markdown}");
    }

    #[test]
    fn mention_written_as_code_or_plain_text_is_not_given_a_name() {
        let user = UserReference::AccountId("abc".to_string());
        let mention = "[abc](confluence-user://user?account-id=abc&label=generated)";
        let escaped = mention.replace('&', "&amp;");
        for storage in [
            format!("<p>write <code>{escaped}</code> for a mention</p>"),
            format!("<pre>{escaped}</pre>"),
            format!(
                r#"<ac:structured-macro ac:name="code"><ac:plain-text-body><![CDATA[{mention}]]></ac:plain-text-body></ac:structured-macro>"#
            ),
        ] {
            let pulled = storage_to_markdown(&storage);
            assert!(
                pulled.contains(mention) || pulled.contains(&escaped),
                "{pulled}"
            );
            assert!(unnamed_mentions(&pulled).is_empty(), "{pulled}");
            assert_eq!(
                name_user_mentions(&pulled, &named(user.clone(), "Jane Doe")),
                pulled
            );
        }
    }

    #[test]
    fn space_that_does_not_break_or_collapse_is_kept() {
        assert_keeps("<p>a&nbsp;&nbsp;b</p>");
        assert_keeps("<p>a&nbsp;</p>");
        assert_keeps("<p>&nbsp;b</p>");
        assert_keeps("<p>&nbsp;</p>");
        assert_keeps("<p>a</p><p>&nbsp;</p><p>b</p>");
        assert_keeps("<p>a &nbsp; b</p>");
        assert_keeps("<p><em>a</em>&nbsp;<em>b</em></p>");
        assert_keeps("<p><strong>a&nbsp;</strong>b</p>");
        assert_keeps("<h2>a&nbsp;</h2>");
        assert_keeps("<ul><li>&nbsp;</li><li>a&nbsp;&nbsp;b</li></ul>");
        assert_keeps(
            "<table><thead><tr><th>h</th></tr></thead><tbody><tr><td>&nbsp;</td></tr></tbody></table>",
        );
        assert_keeps("<p>a&#x2003;&#x2003;b&#x2009;</p>");
        assert_keeps("<p>written as text: &amp;nbsp;</p>");
    }

    #[test]
    fn space_that_does_not_break_is_kept_at_the_edge_of_emphasis() {
        for storage in [
            "<p>x<em>&nbsp;a</em>y</p>",
            "<p>x<em>a&nbsp;</em>y</p>",
            "<p>x<strong>&nbsp;</strong>y</p>",
            "<p><em>&nbsp;a</em></p>",
            "<p>x <em>a&nbsp;</em> y</p>",
            "<p>x<em>&#x2003;a&#x2003;</em>y</p>",
            "<p>x<s>&nbsp;a&nbsp;</s>y</p>",
            "<p>x<a href=\"https://example.com\">&nbsp;a&nbsp;</a>y</p>",
            "<p>x<code>&nbsp;a&nbsp;</code>y</p>",
        ] {
            assert_keeps(storage);
        }
    }

    #[test]
    fn line_break_is_kept_where_markdown_has_no_hard_break() {
        for storage in [
            "<p>x<br/><br/>y</p>",
            "<p>x<br/><br/><br/>y</p>",
            "<p>x<br/></p>",
            "<p><br/>x</p>",
            "<p>x<br/>y</p>",
            "<p>x<br/> <br/>y</p>",
            "<p><em>x<br/></em>y</p>",
            "<ul><li>x<br/><br/>y</li></ul>",
            "<ul><li>x<br/></li><li>y</li></ul>",
            "<h2>x<br/>y</h2>",
            "<table><tbody><tr><td>x<br/><br/>y</td></tr></tbody></table>",
            "<blockquote><p>x<br/><br/>y</p></blockquote>",
            "<p>a<br/><em> </em><br/>b</p>",
            "<ul><li>x<br/><ul><li>z</li></ul></li></ul>",
            "<p>x<br/><strong>y</strong></p>",
        ] {
            assert_keeps(storage);
        }
        assert_eq!(storage_to_markdown("<p>x<br/>y</p>"), "x  \ny");
    }

    #[test]
    fn code_spans_that_touch_stay_apart() {
        for storage in [
            "<p><code>a</code><code>b</code></p>",
            "<p><code>a</code><code>b</code><code>c</code></p>",
            "<p><code>a</code> <code>b</code></p>",
            "<p><code>a</code>`<code>b</code></p>",
            "<p><code>a</code><em></em><code>b</code></p>",
        ] {
            assert_keeps(storage);
        }
        assert_eq!(
            storage_to_markdown("<p><code>a</code> <code>b</code></p>"),
            "`a` `b`"
        );
        // An element that only holds a code span does not keep it apart.
        for storage in [
            "<p><code>a</code><span><code>b</code></span></p>",
            "<p><span><code>a</code></span><code>b</code></p>",
            "<p><code>a</code><span></span><code>b</code></p>",
        ] {
            let applied = apply_storage(&storage_to_markdown(storage)).expect("applies");
            assert!(
                crate::storage::same_document(&applied, "<p><code>a</code><code>b</code></p>"),
                "{storage} applied as {applied}"
            );
        }
    }

    #[test]
    fn entity_name_in_code_or_a_comment_is_text_of_the_page() {
        for storage in [
            r#"<p class="x">a&nbsp;b</p><ac:structured-macro ac:name="code"><ac:plain-text-body><![CDATA[x = &custom; + &nbsp;]]></ac:plain-text-body></ac:structured-macro>"#,
            r#"<p class="x">a</p><!-- &custom; --><p>b</p>"#,
        ] {
            let markdown = storage_to_markdown(storage);
            assert!(markdown.contains(r#"class="x""#), "{markdown}");
        }
        // A reference to an entity that has no characters is not well formed.
        assert!(storage_to_markdown("<p>a&custom;b</p>").contains("custom"));
    }

    #[test]
    fn element_that_sets_how_white_space_shows_is_kept_as_storage() {
        for storage in [
            r#"<p><span style="white-space: pre">A   B</span></p>"#,
            r#"<p>x<strong style="white-space: pre">A   B</strong></p>"#,
            r#"<p>x<em style="white-space: pre-wrap">A   B</em></p>"#,
            r#"<p>x<a href="https://example.com" style="white-space: pre">A   B</a></p>"#,
            r#"<p style="white-space: pre">A   B</p>"#,
            r#"<table><tbody><tr><td style="white-space: pre">A   B</td></tr></tbody></table>"#,
        ] {
            let markdown = storage_to_markdown(storage);
            let applied = apply_storage(&markdown).expect("applies");
            assert!(
                applied.contains("A   B"),
                "{storage}\n{markdown}\n{applied}"
            );
        }
    }

    #[test]
    fn paragraph_that_starts_with_a_line_break_stays_a_paragraph() {
        for storage in [
            "<p><br /></p>",
            "<p>a</p><p><br /></p><p>b</p>",
            "<blockquote><p><br /></p></blockquote>",
            "<p><br /> </p><p>x</p>",
            "<p><br /><br />b</p>",
            "<p><u><br />x</u></p>",
            r#"<ac:layout><ac:layout-section ac:type="single"><ac:layout-cell><p><br /></p></ac:layout-cell></ac:layout-section></ac:layout>"#,
            r#"<ac:structured-macro ac:name="info"><ac:rich-text-body><p><br /></p></ac:rich-text-body></ac:structured-macro>"#,
        ] {
            assert_keeps(storage);
            let applied = apply_storage(&storage_to_markdown(storage)).expect("applies");
            assert_eq!(
                applied.matches("<p>").count(),
                storage.matches("<p>").count(),
                "{storage}\n{applied}"
            );
        }
    }

    #[test]
    fn comment_stays_a_comment() {
        for storage in [
            "<p><code>run<!--internal note--></code></p>",
            "<p>Instructions<!--editor note--></p>",
            "<p><!--editor note-->Instructions</p>",
            "<p>a <strong>b<!--note--></strong></p>",
            "<ul><li>item<!--note--></li></ul>",
            "<table><tbody><tr><td>cell<!--note--></td></tr></tbody></table>",
            "<h2>Title<!--note--></h2>",
        ] {
            let markdown = storage_to_markdown(storage);
            let applied = apply_storage(&markdown).expect("applies");
            assert!(
                crate::storage::same_document(&applied, storage),
                "{storage}\n{markdown}\n{applied}"
            );
            assert_eq!(applied.matches("<!--").count(), 1, "{applied}");
        }
        assert!(!crate::storage::same_document(
            "<p>Instructions<!--editor note--></p>",
            "<p>Instructions</p>"
        ));
    }

    #[test]
    fn comment_between_blocks_stays_between_them() {
        for storage in [
            "<p>a</p><!--between--><p>b</p>",
            "<!--first--><p>a</p>",
            "<p>a</p><!--one\n\ntwo-->",
            "<blockquote><!--note--><p>a</p></blockquote>",
            r#"<ac:layout><ac:layout-section ac:type="single"><ac:layout-cell><!--note--><p>a</p></ac:layout-cell></ac:layout-section></ac:layout>"#,
            r#"<ac:structured-macro ac:name="info"><ac:rich-text-body><p>a<!--note--></p></ac:rich-text-body></ac:structured-macro>"#,
        ] {
            let markdown = storage_to_markdown(storage);
            let applied = apply_storage(&markdown).expect("applies");
            assert!(
                crate::storage::same_document(&applied, storage),
                "{storage}\n{markdown}\n{applied}"
            );
        }
    }

    #[test]
    fn placeholder_link_after_a_comment_that_names_a_section_is_still_a_link() {
        let storage = resolve_placeholder_links(
            r#"<!-- <![CDATA[ example --><p><a href="confluence-user://user?userkey=abc">x</a></p><ac:plain-text-body><![CDATA[<a href="confluence-user://user?userkey=abc">x</a>]]></ac:plain-text-body>"#,
        );
        assert_eq!(storage.matches("<ac:link>").count(), 1, "{storage}");
        assert_eq!(storage.matches("<a href=").count(), 1, "{storage}");
    }

    #[test]
    fn task_keeps_a_space_that_does_not_break_at_its_edges() {
        assert_keeps(
            "<ac:task-list><ac:task><ac:task-status>incomplete</ac:task-status><ac:task-body>&nbsp;a&nbsp;</ac:task-body></ac:task></ac:task-list>",
        );
    }

    #[test]
    fn page_that_names_characters_by_entity_keeps_what_markdown_cannot_carry() {
        for storage in [
            r#"<p class="x">a&nbsp;b</p>"#,
            r#"<p>it&rsquo;s <span style="color: red;">red</span> &rarr; &eacute;</p>"#,
            r#"<table><tbody><tr><td colspan="2">a&nbsp;b</td></tr></tbody></table>"#,
        ] {
            let markdown = storage_to_markdown(storage);
            assert!(
                markdown.contains("class=\"x\"")
                    || markdown.contains("color: red")
                    || markdown.contains("colspan"),
                "{markdown}"
            );
            assert_keeps(storage);
        }
    }

    /// Pull and apply give back a document that says the same as `storage`.
    fn assert_keeps(storage: &str) {
        let markdown = storage_to_markdown(storage);
        let applied = apply_storage(&markdown)
            .unwrap_or_else(|error| panic!("apply failed for {markdown:?}: {error:#}"));
        assert!(
            crate::storage::same_document(&applied, storage),
            "{storage} pulled as {markdown:?}, applied as {applied}"
        );
    }

    #[test]
    fn paragraph_after_a_macro_stays_a_paragraph_of_its_own() {
        for block in [
            r#"<ac:structured-macro ac:name="info"><ac:rich-text-body><p>b</p></ac:rich-text-body></ac:structured-macro>"#,
            r#"<ac:structured-macro ac:name="expand"><ac:parameter ac:name="title">T</ac:parameter><ac:rich-text-body><p>b</p></ac:rich-text-body></ac:structured-macro>"#,
            r#"<ac:structured-macro ac:name="code"><ac:plain-text-body><![CDATA[x]]></ac:plain-text-body></ac:structured-macro>"#,
            r#"<ac:structured-macro ac:name="noformat"><ac:plain-text-body><![CDATA[x]]></ac:plain-text-body></ac:structured-macro>"#,
        ] {
            assert_keeps(&format!("{block}<p>after</p>"));
            assert_keeps(&format!("<p>before</p>{block}<p>after</p>"));
        }
    }

    #[test]
    fn panel_keeps_its_title_and_icon_setting() {
        for parameters in [
            r#"<ac:parameter ac:name="title">Heads up</ac:parameter>"#,
            r#"<ac:parameter ac:name="icon">false</ac:parameter>"#,
            r#"<ac:parameter ac:name="icon">false</ac:parameter><ac:parameter ac:name="title">Heads up</ac:parameter>"#,
        ] {
            for name in ["info", "note", "tip", "warning"] {
                assert_keeps(&format!(
                    r#"<ac:structured-macro ac:name="{name}">{parameters}<ac:rich-text-body><p>b</p></ac:rich-text-body></ac:structured-macro><p>after</p>"#
                ));
            }
        }
        assert_eq!(
            storage_to_markdown(
                r#"<ac:structured-macro ac:name="info"><ac:parameter ac:name="title">Heads up</ac:parameter><ac:rich-text-body><p>b</p></ac:rich-text-body></ac:structured-macro>"#
            ),
            ":::confluence-macro info\ntitle: Heads up\n---\nb\n:::"
        );
    }

    #[test]
    fn macro_keeps_a_body_line_that_reads_as_the_end_of_its_parameters() {
        let plain = |name: &str, parameters: &str, text: &str| {
            format!(
                r#"<ac:structured-macro ac:name="{name}">{parameters}<ac:plain-text-body><![CDATA[{text}]]></ac:plain-text-body></ac:structured-macro>"#
            )
        };
        let title = r#"<ac:parameter ac:name="title">T</ac:parameter>"#;
        for name in ["code", "noformat"] {
            for text in ["a\n---\nb", "title: x\n---\nb", "\n---\nb", "---\n---\n---"] {
                assert_keeps(&plain(name, "", text));
                assert_keeps(&plain(name, title, text));
            }
        }
        for name in ["excerpt", "toc-zone", "details"] {
            for parameters in ["", r#"<ac:parameter ac:name="hidden">true</ac:parameter>"#] {
                assert_keeps(&format!(
                    r#"<ac:structured-macro ac:name="{name}">{parameters}<ac:rich-text-body><p>a</p><hr /><p>b</p></ac:rich-text-body></ac:structured-macro>"#
                ));
            }
        }
    }

    #[test]
    fn block_keeps_a_line_of_tildes_in_its_body() {
        let plain = |name: &str, text: &str| {
            format!(
                r#"<ac:structured-macro ac:name="{name}"><ac:plain-text-body><![CDATA[{text}]]></ac:plain-text-body></ac:structured-macro>"#
            )
        };
        let layout = |cell: &str| {
            format!(
                r#"<ac:layout><ac:layout-section ac:type="single"><ac:layout-cell>{cell}</ac:layout-cell></ac:layout-section></ac:layout>"#
            )
        };
        for name in ["code", "noformat"] {
            for text in ["a\n~~~\nb", "a\n~~~~~\nb", "~~~\n~~~~", "  ~~~  \nb"] {
                let block = plain(name, text);
                assert_keeps(&block);
                assert_keeps(&format!("{block}<p>after</p>"));
                assert_keeps(&layout(&block));
                assert_keeps(&format!(
                    r#"<ac:structured-macro ac:name="info"><ac:rich-text-body>{block}</ac:rich-text-body></ac:structured-macro>"#
                ));
            }
        }
        assert_eq!(
            storage_to_markdown(&plain("code", "a\n~~~\nb")),
            "~~~~confluence-code\na\n~~~\nb\n~~~~"
        );
    }

    #[test]
    fn layout_inside_a_layout_cell_is_kept() {
        let layout = |cell: &str| {
            format!(
                r#"<ac:layout><ac:layout-section ac:type="single"><ac:layout-cell>{cell}</ac:layout-cell></ac:layout-section></ac:layout>"#
            )
        };
        assert_keeps(&layout(&layout("<p>in</p>")));
        assert_keeps(&layout(&format!(
            "<p>a</p>{}<p>b</p>",
            layout(&layout("<p>in</p>"))
        )));
    }

    #[test]
    fn emphasis_keeps_white_space_at_its_edges() {
        assert_round_trips("<p>a <em>lead </em>b</p>");
        assert_round_trips("<p>a<strong> trail</strong> b</p>");
        assert_round_trips("<p>a<strong> </strong>b</p>");
        assert_round_trips(r#"<p><em> <span style="color: red;">b</span></em> c</p>"#);
        assert_round_trips(concat!(
            r#"<p><strong><span style="color: red;">b</span> </strong>"#,
            r#"<span style="color: red;">c</span></p>"#,
        ));
        assert_round_trips("<h2><em>a </em>b</h2>");
        assert_round_trips("<ul><li><em>a </em>b</li></ul>");
        assert_round_trips(concat!(
            "<table><thead><tr><th>H</th></tr></thead>",
            "<tbody><tr><td><em> x</em>a|b</td></tr></tbody></table>",
        ));
        assert_round_trips(r#"<p><a href="https://x.example"><em> a</em></a></p>"#);
    }

    #[test]
    fn emphasis_next_to_punctuation_stays_emphasis() {
        assert_round_trips(r#"<p>a<em>"x"</em> b</p>"#);
        assert_round_trips(r#"<p>a<em> "x"</em> b</p>"#);
        assert_round_trips(r#"<p>a <em>"x"</em>b</p>"#);
        assert_round_trips("<p><strong>Title:</strong>x</p>");
        assert_round_trips("<p>snake<em>_case_</em>x</p>");
        assert_round_trips(
            "<p>caf\u{e9}<em>\u{e9}t\u{e9}</em>s \u{2014}<em>\u{201c}q\u{201d}</em>\u{2014}</p>",
        );
        assert_round_trips(
            "<p>(<em>a</em>)<em>b</em>! 1<strong>2</strong>3 <em>*a*</em> <em>a</em>*</p>",
        );
    }

    #[test]
    fn emphasis_that_markdown_reads_is_written_with_delimiters() {
        let storage = "<p><em>a</em>, <strong>b:</strong> c (<em>d</em>) x<em>e</em>y.</p>";
        assert_eq!(storage_to_markdown(storage), "*a*, **b:** c (*d*) x*e*y.");
        assert_round_trips(storage);
    }

    #[test]
    fn emphasis_that_touches_other_emphasis_keeps_its_extent() {
        assert_round_trips("<p><em>a</em><em>b</em></p>");
        assert_round_trips("<p><em>a</em><strong>b</strong><em>c</em></p>");
        assert_eq!(
            apply_storage(&storage_to_markdown(
                "<p><em>a</em><span><em>b</em></span></p>"
            ))
            .unwrap(),
            "<p><em>a</em><em>b</em></p>"
        );
        assert_round_trips("<p><em>a<em>b</em>c</em> <strong>a<strong>b</strong></strong></p>");
        assert_round_trips(
            "<p><strong><em>c</em></strong><em>d</em> <em><strong>e</strong></em></p>",
        );
        assert_round_trips(
            "<p><em><strong>a</strong> b</em> <strong>a<em>b</em></strong> <em>a<strong>b</strong>c</em></p>",
        );
        assert_round_trips("<p>x<strong><em>a</em>b</strong></p>");
    }

    #[test]
    fn emphasis_keeps_its_attributes_and_stays_when_empty() {
        assert_round_trips(r#"<p><em class="x">a</em> <strong style="color: red;">b</strong></p>"#);
        assert_round_trips("<p>a<em></em>b</p>");
    }
    #[test]
    fn text_inside_an_inline_element_stays_literal() {
        assert_round_trips(r#"<p><span style="color: red;">a *b* c</span></p>"#);
        assert_round_trips("<p><u>a *b* c</u> <sub>x_y_z</sub> <sup>[1]</sup></p>");
        assert_round_trips(
            r#"<p><span style="color: red;"><em>a</em> [x] &amp;amp; &lt;b&gt;</span></p>"#,
        );
        assert_round_trips(concat!(
            "<table><thead><tr><th>H</th></tr></thead>",
            r#"<tbody><tr><td><span title="p|q">s|t</span></td></tr></tbody></table>"#,
        ));
    }

    #[test]
    fn inline_elements_without_markdown_syntax_survive() {
        assert_round_trips(
            "<p>a <del>x</del> <s>y</s> <kbd>k</kbd> <mark>m</mark> <ins>i</ins> <cite>c</cite></p>",
        );
        assert_round_trips("<ul><li>a<del>b</del></li></ul>");
        assert_round_trips("<p><custom>x</custom></p>");
        assert_round_trips(r#"<p><time datetime="2024-01-01" /></p>"#);
        assert_round_trips(r#"<p><span style="color: red;">a<br />b</span></p>"#);
    }

    #[test]
    fn inline_code_keeps_its_spaces_backticks_attributes_and_markup() {
        assert_round_trips("<p>a<code> b </code>c <code> </code></p>");
        assert_round_trips("<p><code>a`b</code> <code>`</code> <code>``x`</code></p>");
        assert_round_trips("<p><code>a*b_c</code> and <code>x<em>y</em></code></p>");
        assert_round_trips(r#"<p>a<code class="x">b</code></p>"#);
        assert_eq!(storage_to_markdown("<p>a <code>b</code></p>"), "a `b`");
    }
    #[test]
    fn block_with_attributes_keeps_them() {
        assert_round_trips(r#"<p style="text-align: center;">a *b*</p>"#);
        assert_round_trips(r#"<h2 style="text-align: center;">a</h2>"#);
        assert_round_trips(r#"<blockquote class="x"><p>a</p></blockquote>"#);
        assert_round_trips(r#"<div class="x"><p>a</p></div>"#);
        assert_round_trips(r#"<hr class="x" />"#);
        assert_round_trips(concat!(
            r#"<blockquote><p style="x">a</p>"#,
            r#"<ul><li data-x="1">b</li></ul></blockquote>"#,
        ));
    }

    #[test]
    fn identifier_that_confluence_assigns_does_not_keep_a_block_out_of_markdown() {
        assert_eq!(storage_to_markdown(r#"<p local-id="abc">a</p>"#), "a");
        assert_eq!(
            storage_to_markdown(r#"<ul local-id="a"><li local-id="b">one</li></ul>"#),
            "- one"
        );
    }

    #[test]
    fn table_that_is_more_than_a_markdown_table_stays_as_it_is() {
        let markdown_table = "<table><thead><tr><th>H</th><th>I</th></tr></thead><tbody><tr><td>c</td><td>d</td></tr></tbody></table>";
        assert_eq!(
            storage_to_markdown(markdown_table),
            "| H | I |\n| --- | --- |\n| c | d |"
        );
        assert_round_trips(markdown_table);
        assert_round_trips("<table><thead><tr><th>H</th></tr></thead><tbody></tbody></table>");

        for table in [
            r#"<table class="wrapped" style="width: 50%;"><tbody><tr><th>H</th></tr><tr><td>c</td></tr></tbody></table>"#,
            "<table><tbody><tr><th>H</th></tr><tr><td>c</td></tr></tbody></table>",
            r#"<table><thead><tr><th class="x">H</th></tr></thead><tbody><tr><td>c</td></tr></tbody></table>"#,
            r#"<table><thead><tr><th>H</th></tr></thead><tbody><tr><td style="x">c</td></tr></tbody></table>"#,
            "<table><thead><tr><th>H</th><th>I</th></tr></thead><tbody><tr><th>r</th><td>c</td></tr></tbody></table>",
            "<table><thead><tr><th>H</th><th>I</th></tr></thead><tbody><tr><td>c</td></tr></tbody></table>",
            "<table><thead><tr><th>H</th></tr></thead></table>",
            "<table><colgroup><col /></colgroup><thead><tr><th>H</th></tr></thead><tbody><tr><td>c</td></tr></tbody></table>",
            "<table><thead><tr><th>H</th></tr></thead><tbody><tr><td>c</td></tr></tbody><tfoot><tr><td>f</td></tr></tfoot></table>",
            "<table><thead><tr><th>H</th></tr><tr><th>H2</th></tr></thead><tbody><tr><td>c</td></tr></tbody></table>",
            "<table><thead><tr><th>H</th></tr></thead><tbody><tr><td>a<br />b</td></tr></tbody></table>",
        ] {
            assert!(
                storage_to_markdown(table).starts_with("<table"),
                "{table} pulled as {}",
                storage_to_markdown(table)
            );
            assert_round_trips(table);
        }
    }

    #[test]
    fn page_properties_keep_their_row_headers() {
        assert_round_trips(concat!(
            r#"<ac:macro ac:name="details"><ac:rich-text-body>"#,
            "<table><tbody><tr><th>Owner</th><td>Ada</td></tr>",
            "<tr><th>Status</th><td>Approved</td></tr></tbody></table>",
            "</ac:rich-text-body></ac:macro>",
        ));
    }

    #[test]
    fn list_with_attributes_or_stray_content_stays_as_it_is() {
        assert_round_trips(r#"<ul><li data-uuid="abc">one</li></ul>"#);
        assert_round_trips(r#"<ul class="x"><li>one</li></ul>"#);
        assert_round_trips(r#"<ul><li>one<ul><li data-uuid="q">two</li></ul></li></ul>"#);
        assert_round_trips(r#"<ol start="3" class="x"><li>one</li></ol>"#);
        assert_round_trips(r#"<ol start="x"><li>one</li></ol>"#);
        assert_round_trips("<ul><li>one</li>text</ul>");
        assert_round_trips(r#"<ul><li><p style="x">one</p><p>two</p></li></ul>"#);
    }

    #[test]
    fn preformatted_text_that_is_not_a_plain_code_block_stays_as_it_is() {
        assert_round_trips(r#"<pre class="x">code</pre>"#);
        assert_round_trips("<pre>code</pre>");
        assert_round_trips("<pre>a<strong>b</strong></pre>");
        assert_round_trips("<pre>a\n\nb</pre><p>after</p>");
        assert_round_trips("<pre>a\n```\nb</pre>");
        assert_round_trips("<pre><code class=\"language-rust extra\">code\n</code></pre>");
        assert_round_trips("<pre><code>a<em>b</em>\n</code></pre>");
    }

    #[test]
    fn code_block_keeps_the_blank_lines_at_its_end() {
        assert_round_trips("<pre><code>a\n\n\n</code></pre>");
        assert_round_trips("<pre><code>\n\na\n</code></pre>");
        assert_eq!(
            storage_to_markdown("<pre><code class=\"language-rust\">a\n</code></pre>"),
            "```rust\na\n```"
        );
    }

    #[test]
    fn straight_quotes_and_ascii_punctuation_survive() {
        assert_round_trips(r#"<p>NXP's "Caddy" path</p>"#);
        assert_round_trips("<p>a -- b --- c ... d</p>");
    }

    #[test]
    fn nested_numbered_list_starting_past_one_keeps_its_structure() {
        assert_round_trips(
            r#"<ul><li>Parent<ol start="3"><li>third</li><li>fourth</li></ol></li></ul>"#,
        );
        assert_round_trips(
            r#"<ul><li>Parent<ul><li>sub</li></ul><ol start="3"><li>third</li></ol></li></ul>"#,
        );
        assert_round_trips(r#"<ol><li>one<ol start="0"><li>zero</li></ol></li></ol>"#);
    }

    #[test]
    fn link_markup_quoted_in_a_comment_leaves_the_links_after_it_alone() {
        let applied = apply_storage(concat!(
            "<!-- example: <a href=\"attachments/manual.pdf\"> -->\n\n",
            "[Guide](https://example.com) and ![shot](attachments/shot.png)\n\n",
            "<!-- <img src=\"attachments/old.png\" /> -->",
        ))
        .expect("apply");

        assert!(
            applied.contains(r#"<a href="https://example.com">Guide</a>"#),
            "{applied}"
        );
        assert!(
            applied.contains(r#"<!-- example: <a href="attachments/manual.pdf"> -->"#),
            "{applied}"
        );
        assert!(
            applied.contains(r#"<!-- <img src="attachments/old.png" /> -->"#),
            "{applied}"
        );
        assert!(
            applied.contains(r#"<ri:attachment ri:filename="shot.png" />"#),
            "{applied}"
        );
    }

    #[test]
    fn link_whose_body_has_no_markdown_form_keeps_that_body() {
        for resource in [
            r#"<ri:page ri:content-title="Home" />"#,
            r#"<ri:user ri:account-id="abc" />"#,
            r#"<ri:attachment ri:filename="report.pdf" />"#,
        ] {
            assert_round_trips(&format!(
                concat!(
                    "<p><ac:link>{}<ac:link-body>",
                    r#"<ac:image ac:width="20"><ri:url ri:value="https://example.com/x.png" /></ac:image>"#,
                    "</ac:link-body></ac:link></p>"
                ),
                resource
            ));
        }
    }

    #[test]
    fn download_link_to_an_attachment_with_a_percent_in_its_name_names_that_file() {
        let url = "https://example.com/download/attachments/123/budget%2520final.pdf";
        let pulled = rewrite_attachment_links(
            &storage_to_markdown(&format!(r#"<p><a href="{url}">Budget</a></p>"#)),
            &[crate::model::AttachmentInfo {
                id: "att1".to_string(),
                title: "budget%20final.pdf".to_string(),
                version: None,
                media_type: None,
                file_size: None,
                download_url: Some(url.to_string()),
                comment: None,
            }],
        );

        let applied = apply_storage(&pulled).expect("apply");

        assert!(
            applied.contains(r#"<ri:attachment ri:filename="budget%20final.pdf" />"#),
            "pulled as {pulled:?}, applied as {applied}"
        );
    }

    #[test]
    fn mention_whose_label_holds_a_comment_is_still_a_mention() {
        let applied =
            apply_storage("[A<!--note-->B](confluence-user://user?username=jdoe)").expect("apply");

        assert!(
            applied.contains(r#"<ri:user ri:username="jdoe" />"#),
            "{applied}"
        );
        assert!(!applied.contains("confluence-user://"), "{applied}");
    }

    #[test]
    fn attachment_link_whose_text_holds_a_link_still_converts() {
        let applied = apply_storage("[<https://example.com>](attachments/manual.pdf)");

        assert!(applied.is_ok(), "{applied:?}");
    }

    #[test]
    fn confluence_markup_in_a_leading_metadata_block_is_not_refused() {
        let applied =
            apply_storage("---\nexample: <ac:link ac:key=\"A\"/>\n---\n\nBody").expect("apply");

        assert_eq!(canonical(&applied), canonical("<p>Body</p>"));
    }

    #[test]
    fn indented_or_commented_lists_that_follow_each_other_stay_separate() {
        assert_round_trips(
            "<ol>\n<li>one</li>\n<li>two</li>\n</ol>\n<ol start=\"5\">\n<li>five</li>\n</ol>",
        );
        assert_round_trips("<ul>\n<li>a</li>\n</ul>\n<ul>\n<li>b</li>\n</ul>");

        let markdown = storage_to_markdown(
            r#"<ol><li>one</li></ol><!-- note --><ol start="5"><li>five</li></ol>"#,
        );
        assert_eq!(
            canonical(&apply_storage(&markdown).unwrap()),
            canonical(r#"<ol><li>one</li></ol><ol start="5"><li>five</li></ol>"#),
            "pulled as {markdown:?}"
        );
    }

    #[test]
    fn attachment_stamped_with_its_version_at_save_reads_as_markdown() {
        let image = r#"<p><ac:image ac:alt="Shot"><ri:attachment ri:filename="shot.png" ri:version-at-save="1" /></ac:image></p>"#;
        assert_eq!(storage_to_markdown(image), "![Shot](attachments/shot.png)");

        let link = r#"<p>see <ac:link><ri:attachment ri:filename="manual.pdf" ri:version-at-save="3" /><ac:plain-text-link-body><![CDATA[the manual]]></ac:plain-text-link-body></ac:link></p>"#;
        let markdown = storage_to_markdown(link);
        assert_eq!(markdown, "see [the manual](attachments/manual.pdf)");
        assert_eq!(
            canonical(&apply_storage(&markdown).unwrap()),
            canonical(&link.replace(r#" ri:version-at-save="3""#, "")),
        );

        // Any other extra attribute still has no Markdown form.
        let other = r#"<p><ac:image><ri:attachment ri:filename="shot.png" ri:space-key="DOCS" /></ac:image></p>"#;
        assert!(storage_to_markdown(other).contains("```confluence-storage"));
    }

    #[test]
    fn raw_blocks_with_a_blank_line_keep_their_text() {
        assert_round_trips("<ul><li>a<ul><li>n</li></ul>b</li><li>\n\n*literal*\n\n</li></ul>");
        assert_round_trips("<ul><li>a<ul><li>n</li></ul>b\n\n*c*</li></ul>");
        assert_round_trips("<table><tbody><tr><td><pre>x\n\n*y*</pre></td></tr></tbody></table>");

        let markdown = storage_to_markdown("<ul><li>a<ul><li>n</li></ul>b\n \n*c*</li></ul>");
        assert!(
            markdown.starts_with("```confluence-storage\n"),
            "{markdown}"
        );
        let markdown = storage_to_markdown("<ul><li>a<ul><li>n</li></ul>b\n*c*</li></ul>");
        assert_eq!(markdown, "<ul><li>a<ul><li>n</li></ul>b\n*c*</li></ul>");
    }

    #[test]
    fn raw_blocks_holding_a_fence_line_stay_whole() {
        let code_macro = concat!(
            r#"<table><tbody><tr><td><ac:structured-macro ac:name="code">"#,
            "<ac:plain-text-body><![CDATA[a\n```\nb\n`````\nc]]></ac:plain-text-body>",
            "</ac:structured-macro><p>x</p><p>y</p></td></tr></tbody></table>"
        );
        assert_round_trips(code_macro);
        assert!(
            storage_to_markdown(code_macro).starts_with("``````confluence-storage\n"),
            "{}",
            storage_to_markdown(code_macro)
        );
        assert_round_trips(
            "<table><tbody><tr><td><pre>a\n\n```\nb</pre></td></tr></tbody></table>",
        );
        assert_round_trips(
            "<table><tbody><tr><td><pre>a\n\n```confluence-storage\nb\n```</pre></td></tr></tbody></table>",
        );
    }

    #[test]
    fn raw_blocks_inside_a_block_quote_stay_storage() {
        assert_round_trips(concat!(
            "<blockquote><ul><li>a<ul><li>n</li></ul>b",
            r#"<ac:emoticon ac:name="smile" /></li></ul></blockquote>"#
        ));
        assert_round_trips(
            "<blockquote><table><tbody><tr><td><pre>x\n\n  y\n```\nz</pre></td></tr></tbody></table></blockquote>",
        );
        assert_round_trips(concat!(
            "<blockquote><p>before</p><blockquote><ul><li>a<ul><li>n</li></ul>b",
            r#"<ac:emoticon ac:name="smile" /></li></ul></blockquote><p>after</p></blockquote>"#
        ));
    }

    #[test]
    fn storage_fences_written_by_hand_are_read() {
        let apply = |markdown: &str| canonical(&apply_storage(markdown).unwrap());
        let fragment = r#"<ac:emoticon ac:name="smile" />"#;

        assert_eq!(
            apply(&format!("```confluence-storage\n{fragment}\n```")),
            canonical(fragment)
        );
        assert_eq!(
            apply(&format!(
                "````confluence-storage\n{fragment}\n````\n\nafter"
            )),
            canonical(&format!("{fragment}<p>after</p>"))
        );
        assert_eq!(
            apply(&format!(
                "> quoted\n>\n> ```confluence-storage\n> {fragment}\n> ```"
            )),
            canonical(&format!("<blockquote><p>quoted</p>{fragment}</blockquote>"))
        );
        assert_eq!(
            apply(&format!(
                ">   ```confluence-storage\n>   {fragment}\n>   ```"
            )),
            canonical(&format!("<blockquote>{fragment}</blockquote>"))
        );
        assert_eq!(
            apply(&format!(
                "- item\n\n  ```confluence-storage\n  {fragment}\n  ```\n- next"
            )),
            canonical(&format!(
                "<ul><li><p>item</p>{fragment}</li><li><p>next</p></li></ul>"
            ))
        );
        assert_eq!(
            apply(&format!(
                "```confluence-storage\r\n{fragment}\r\n```\r\n\r\nafter\r\n"
            )),
            canonical(&format!("{fragment}<p>after</p>"))
        );
        assert_eq!(
            apply("```confluence-storage\n```\n\nafter"),
            canonical("<p>after</p>")
        );
    }

    #[test]
    fn storage_fences_sharing_a_line_with_text_are_read() {
        let apply = |markdown: &str| canonical(&apply_storage(markdown).unwrap());
        let fragment = r#"<ac:emoticon ac:name="smile" />"#;

        for (markdown, expected) in [
            (
                format!("before ```confluence-storage\n{fragment}\n``` after"),
                format!("<p>before {fragment} after</p>"),
            ),
            (
                format!("```confluence-storage\n{fragment}\n``` after"),
                format!("<p>{fragment} after</p>"),
            ),
            (
                format!("before ```confluence-storage\n{fragment}\n```"),
                format!("<p>before {fragment}</p>"),
            ),
            (
                format!(
                    "```confluence-storage\n{fragment}\n``` and ```confluence-storage\n{fragment}\n```\n\nnext"
                ),
                format!("<p>{fragment} and {fragment}</p><p>next</p>"),
            ),
            (
                format!(
                    "one ```confluence-storage\n{fragment}\n```\n\ntwo ```confluence-storage\n{fragment}\n```"
                ),
                format!("<p>one {fragment}</p><p>two {fragment}</p>"),
            ),
            (
                format!(
                    "one ```confluence-storage\n{fragment}\n```\n\n```confluence-storage\n{fragment}\n```\n\nend"
                ),
                format!("<p>one {fragment}</p>{fragment}<p>end</p>"),
            ),
            (
                format!(
                    "```confluence-storage\n{fragment}\n```123 ```confluence-storage\n{fragment}\n```0"
                ),
                format!("<p>{fragment}123 {fragment}0</p>"),
            ),
            (
                format!("```confluence-storage\n{fragment}\n```\t"),
                fragment.to_string(),
            ),
            (
                format!("x ```confluence-storage\n\n  {fragment}\n``` y"),
                format!("<p>x   {fragment} y</p>"),
            ),
            (
                "x ```confluence-storage\n\n\n``` y".to_string(),
                "<p>x  y</p>".to_string(),
            ),
        ] {
            assert_eq!(apply(&markdown), canonical(&expected), "{markdown:?}");
        }
    }

    #[test]
    fn storage_fences_without_a_closing_line_stay_code() {
        let apply = |markdown: &str| apply_storage(markdown).unwrap();

        for markdown in [
            "```confluence-storage\n<p>x</p>",
            "```confluence-storage\n<p>x</p>\n",
            "```confluence-storage\n<p>x</p>\n\n\n",
            "```confluence-storage\n<p>x</p>\n    ```",
            "```confluence-storage\n<p>x</p>\n    ```\n",
            "````confluence-storage\n<p>x</p>\n```",
            "```confluence-storage\n<p>x</p>\n~~~",
            "```confluence-storage",
            "> ```confluence-storage\n> <p>x</p>\n\nafter",
            "- item\n\n  ```confluence-storage\n  <p>x</p>\n\nafter",
        ] {
            let storage = apply(markdown);
            assert!(storage.contains("<pre>"), "{markdown:?} gave {storage}");
            assert!(!storage.contains("<p>x</p>"), "{markdown:?} gave {storage}");
        }
    }

    #[test]
    fn text_naming_a_placeholder_survives() {
        assert_round_trips(concat!(
            r#"<p><ac:emoticon ac:name="smile" /></p>"#,
            "<table><tbody><tr><td><p>CONFLUENCE_XML_PLACEHOLDER_0_END</p>",
            r#"<p><ac:emoticon ac:name="sad" /></p></td></tr></tbody></table>"#,
        ));
        assert_round_trips(concat!(
            r#"<table><tbody><tr><td><p><ac:emoticon ac:name="sad" /></p></td></tr></tbody></table>"#,
            "<p>CONFLUENCE_XML_PLACEHOLDER_0_END and CONFLUENCE_XML_PLACEHOLDERX_0_END</p>",
        ));
    }

    #[test]
    fn text_naming_a_placeholder_survives_a_fence_spelled_with_a_reference() {
        let storage = apply_storage(
            "CONFLUENCE_XML_PLACEHOLDER_0_END\n\n```confluence&#45;storage\n<p>x</p>\n```",
        )
        .unwrap();
        assert_eq!(storage, "<p>CONFLUENCE_XML_PLACEHOLDER_0_END</p>\n<p>x</p>");
    }

    #[test]
    fn text_spelling_a_placeholder_in_a_file_name_survives() {
        let markdown = "![](attachments/%43ONFLUENCE_XML_PLACEHOLDER_0_END)\n\n```confluence-storage\n<b>x</b>\n```";
        let storage = apply_storage(markdown).unwrap();
        assert_eq!(
            storage,
            concat!(
                r#"<p><ac:image><ri:attachment ri:filename="CONFLUENCE_XML_PLACEHOLDER_0_END" /></ac:image></p>"#,
                "\n<b>x</b>",
            )
        );
    }

    #[test]
    fn storage_fences_next_to_other_text_stay_in_the_paragraph() {
        for (markdown, expected) in [
            (
                "before <em>\n```confluence-storage\nend</em>\n```",
                "<p>before <em>\nend</em></p>",
            ),
            (
                "text\n```confluence-storage\n<b>x</b>\n```\nmore",
                "<p>text\n<b>x</b>\nmore</p>",
            ),
            (
                "```confluence-storage\n<em>\n```\nend</em>",
                "<p><em>\nend</em></p>",
            ),
            (
                "```confluence-storage\n<em>Title</em>\n```\n===",
                "<h1><em>Title</em></h1>",
            ),
            (
                "```confluence-storage\n<em>Title</em>\n```\n---",
                "<h2><em>Title</em></h2>",
            ),
            (
                "```confluence-storage\n<em>a</em>\n```\n```confluence-storage\n<em>b</em>\n```",
                "<p><em>a</em>\n<em>b</em></p>",
            ),
            (
                "Hi ```confluence-storage\n<b>1</b>\n``````confluence-storage\n<b>2</b>\n```!",
                "<p>Hi <b>1</b><b>2</b>!</p>",
            ),
            (
                "before ```confluence-storage\u{a0}\n<b/>\n``` after",
                "<p>before <b/> after</p>",
            ),
        ] {
            assert_eq!(apply_storage(markdown).unwrap(), expected, "{markdown:?}");
        }
    }

    #[test]
    fn preformatted_text_holding_a_fence_line_stays_whole() {
        assert_round_trips("<p>text</p><pre><code>a\n```\nb\n</code></pre>");
        assert_round_trips("<pre><code>a\n`````\nb\n~~~\n</code></pre>");
        assert_round_trips(concat!(
            r#"<pre><code class="language-md">"#,
            "```rust\nfn x() {}\n```\n</code></pre>",
        ));
    }

    #[test]
    fn malformed_storage_keeps_its_confluence_markup() {
        let markdown = storage_to_markdown(concat!(
            r#"<p>start <ri:user ri:account-id="1" /> end<br></p>"#,
            r#"<p><ri:user ri:account-id="2" /></p>"#,
        ));
        assert!(!markdown.contains("PLACEHOLDER"), "{markdown}");
        let storage = apply_storage(&markdown).unwrap();
        assert!(
            storage.contains(r#"start <ri:user ri:account-id="1" /> end"#),
            "{storage}"
        );
        assert!(
            storage.contains(r#"<ri:user ri:account-id="2" />"#),
            "{storage}"
        );
    }

    #[test]
    fn malformed_storage_keeps_confluence_markup_that_holds_more_of_it() {
        for element in [
            r#"<ac:link><ri:page ri:content-title="P" /><ac:plain-text-link-body><![CDATA[X]]></ac:plain-text-link-body></ac:link>"#,
            r#"<ac:structured-macro ac:name="info"><ac:parameter ac:name="title">T</ac:parameter><ac:rich-text-body><p>body</p></ac:rich-text-body></ac:structured-macro>"#,
            r#"<ac:structured-macro ac:name="expand"><ac:rich-text-body><ac:structured-macro ac:name="info"><ac:rich-text-body><p>in</p></ac:rich-text-body></ac:structured-macro></ac:rich-text-body></ac:structured-macro>"#,
            r#"<ri:attachment ri:filename="f"><ri:page ri:content-title="P" /></ri:attachment>"#,
            r#"<ac:emoticon ac:name="smile" />"#,
            r#"<ac:structured-macro ac:name="code"><ac:plain-text-body><![CDATA[</ac:structured-macro> <ac:x>]]></ac:plain-text-body></ac:structured-macro>"#,
            r#"<ri:attachment ri:filename="x/>y" />"#,
            r#"<ac:link><!-- </ac:link> --><ri:page ri:content-title="P" /></ac:link>"#,
        ] {
            let markdown = storage_to_markdown(&format!("<p>A{element}B<br></p>"));
            let storage = apply_storage(&markdown)
                .unwrap_or_else(|error| panic!("apply failed for {markdown:?}: {error:#}"));
            assert!(
                storage.contains(element),
                "{markdown:?} applied as {storage}"
            );
        }
    }

    #[test]
    fn storage_fences_keep_the_blank_lines_they_hold() {
        let storage = apply_storage(
            "```confluence-storage\n<pre>A\n\n\n```\n\n```confluence-storage\nB</pre>\n```",
        )
        .unwrap();
        assert_eq!(storage, "<pre>A\n\n\nB</pre>");
    }

    #[test]
    fn storage_fences_end_with_their_container() {
        let apply = |markdown: &str| apply_storage(markdown).unwrap();

        // The fence is never closed inside its quote, so the paragraph after
        // the quote is Markdown and the later backtick line opens new code.
        let storage = apply("> ```confluence-storage\n> <p>x</p>\n\n*middle*\n\n> ```\n> tail");
        assert!(storage.contains("<p><em>middle</em></p>"), "{storage}");
        assert!(storage.contains("&lt;p&gt;x&lt;/p&gt;"), "{storage}");
    }

    #[test]
    fn storage_fences_shown_as_code_stay_text() {
        let apply = |markdown: &str| apply_storage(markdown).unwrap();
        let shown = "&lt;ac:emoticon ac:name=\"smile\" /&gt;";

        for markdown in [
            "~~~markdown\n> ```confluence-storage\n> <ac:emoticon ac:name=\"smile\" />\n> ```\n~~~",
            "````markdown\n```confluence-storage\n<ac:emoticon ac:name=\"smile\" />\n```\n````",
            "    ```confluence-storage\n    <ac:emoticon ac:name=\"smile\" />\n    ```",
            "~~~\ntext ```confluence-storage\n<ac:emoticon ac:name=\"smile\" />\n``` text\n~~~",
        ] {
            let storage = apply(markdown);
            assert!(storage.contains(shown), "{markdown:?} gave {storage}");
            assert!(
                !storage.contains("<ac:emoticon"),
                "{markdown:?} gave {storage}"
            );
        }
    }

    #[test]
    fn list_items_keep_their_paragraphs_apart() {
        assert_round_trips("<ul><li><p>para a</p><p>para b</p></li><li><p>single</p></li></ul>");
        assert_round_trips("<ol><li><p>one</p><p>more</p></li><li><p>two</p></li></ol>");
        assert_round_trips("<ul><li><p>a</p><p>b</p><ul><li>nested</li></ul></li></ul>");
        assert_round_trips("<ul><li><p>a</p><p>- not a list</p></li></ul>");
        assert_round_trips("<ul><li><p>a</p><p><strong>b</strong> and c</p><p>d</p></li></ul>");

        let markdown = storage_to_markdown("<ul><li><p>para a</p><p>para b</p></li></ul>");
        assert_eq!(markdown, "- para a\n\n  para b");
    }

    #[test]
    fn list_items_markdown_cannot_express_keep_their_storage() {
        for storage in [
            "<ul><li><p>a</p><ul><li>nested</li></ul><p>b</p></li></ul>",
            "<ul><li>a<ul><li>nested</li></ul>b</li></ul>",
            "<ul><li><p>a</p><p></p></li></ul>",
            "<ul><li><p>a</p><p>b</p></li><li>c</li></ul>",
            "<ul><li>c</li><li><p>a</p><p>b</p></li></ul>",
            "<ul><li>a<p>b</p></li></ul>",
            "<ul><li><p>a</p>b</li></ul>",
        ] {
            let markdown = storage_to_markdown(storage);
            assert_eq!(
                canonical(&apply_storage(&markdown).unwrap()),
                canonical(storage),
                "pulled as {markdown:?}"
            );
        }
    }

    #[test]
    fn task_lists_that_follow_each_other_stay_separate() {
        assert_round_trips(
            "<ac:task-list><ac:task><ac:task-status>incomplete</ac:task-status><ac:task-body>one</ac:task-body></ac:task></ac:task-list><ac:task-list><ac:task><ac:task-status>incomplete</ac:task-status><ac:task-body>two</ac:task-body></ac:task></ac:task-list>",
        );
        assert_round_trips(
            "<ul><li>a</li></ul><ac:task-list><ac:task><ac:task-status>incomplete</ac:task-status><ac:task-body>one</ac:task-body></ac:task></ac:task-list><ul><li>b</li></ul>",
        );
    }

    #[test]
    fn lists_that_follow_each_other_stay_separate() {
        assert_round_trips(r#"<ol><li>one</li><li>two</li></ol><ol start="5"><li>five</li></ol>"#);
        assert_round_trips("<ol><li>one</li></ol><ol><li>again</li></ol><ol><li>third</li></ol>");
        assert_round_trips("<ul><li>a</li></ul><ul><li>b</li></ul><ul><li>c</li></ul>");
        assert_round_trips("<ul><li>a</li></ul><ol><li>b</li></ol><ul><li>c</li></ul>");
        assert_round_trips("<ul><li>parent<ol><li>a</li></ol><ol><li>b</li></ol></li></ul>");
        assert_round_trips(
            "<blockquote><ul><li>a</li></ul><ul><li>b<ul><li>c</li></ul></li></ul></blockquote>",
        );
    }

    #[test]
    fn nested_lists_keep_their_structure() {
        assert_round_trips("<ol><li>one<ol><li>sub</li></ol></li><li>two</li></ol>");
        assert_round_trips("<ol><li>one<ul><li>sub</li></ul></li><li>two</li></ol>");
        assert_round_trips("<ul><li>one<ol><li>sub</li></ol></li><li>two</li></ul>");
        assert_round_trips("<ul><li>one<ul><li>sub</li></ul></li><li>two</li></ul>");
        assert_round_trips(
            "<ol><li>one<ol><li>sub<ul><li>deep<ol><li>deeper</li></ol></li></ul></li></ol></li></ol>",
        );
    }

    #[test]
    fn ordered_list_start_number_survives() {
        assert_round_trips(r#"<ol start="3"><li>three</li><li>four</li></ol>"#);
        assert_round_trips(
            r#"<ol start="10"><li>ten<ol><li>sub</li></ol></li><li>eleven</li></ol>"#,
        );
    }

    #[test]
    fn mention_without_link_body_gains_none() {
        assert_round_trips(
            r#"<p>ask <ac:link><ri:user ri:userkey="8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a" /></ac:link> now</p>"#,
        );
        assert_round_trips(
            r#"<p>ask <ac:link><ri:user ri:account-id="557058:abc" /></ac:link> now</p>"#,
        );
        assert_round_trips(r#"<p>ask <ac:link><ri:user ri:username="jdoe" /></ac:link> now</p>"#);
    }

    #[test]
    fn mention_with_link_body_keeps_it() {
        assert_round_trips(
            r#"<p>ask <ac:link><ri:user ri:userkey="8a8a" /><ac:plain-text-link-body><![CDATA[Jane Doe]]></ac:plain-text-link-body></ac:link> now</p>"#,
        );
    }

    #[test]
    fn attachment_image_stays_an_attachment_reference() {
        assert_round_trips(r#"<p><ac:image><ri:attachment ri:filename="x.png" /></ac:image></p>"#);
        assert_round_trips(
            r#"<p>see <ac:image ac:alt="A &quot;diagram&quot;"><ri:attachment ri:filename="x.png" /></ac:image> here</p>"#,
        );
        assert_round_trips(
            r#"<p><ac:image ac:alt="Diagram" ac:title="Hover text"><ri:attachment ri:filename="x.png" /></ac:image></p>"#,
        );
        assert_round_trips(
            r#"<p><ac:image><ri:attachment ri:filename="my file (1) &amp; more.png" /></ac:image></p>"#,
        );
    }

    #[test]
    fn image_layout_attributes_survive() {
        assert_round_trips(
            r#"<p><ac:image ac:width="300" ac:align="center"><ri:attachment ri:filename="x.png" /></ac:image></p>"#,
        );
        assert_round_trips(
            r#"<p>inline <ac:image ac:thumbnail="true"><ri:attachment ri:filename="x.png" /></ac:image> text</p>"#,
        );
    }

    #[test]
    fn image_attached_to_another_page_keeps_its_owner() {
        assert_round_trips(
            r#"<p><ac:image><ri:attachment ri:filename="x.png"><ri:page ri:content-title="Other" ri:space-key="DOCS" /></ri:attachment></ac:image></p>"#,
        );
    }

    #[test]
    fn external_image_stays_a_url_reference() {
        assert_round_trips(
            r#"<p><ac:image ac:alt="Logo"><ri:url ri:value="https://example.com/a.png?x=1&amp;y=2" /></ac:image></p>"#,
        );
    }

    #[test]
    fn attachment_link_stays_an_attachment_reference() {
        assert_round_trips(
            r#"<p><ac:link><ri:attachment ri:filename="manual.pdf" /><ac:plain-text-link-body><![CDATA[the manual]]></ac:plain-text-link-body></ac:link></p>"#,
        );
    }

    #[test]
    fn link_without_body_gains_none() {
        assert_round_trips(
            r#"<p><ac:link><ri:attachment ri:filename="manual v2.pdf" /></ac:link></p>"#,
        );
        assert_round_trips(
            r#"<p><ac:link><ri:page ri:content-title="Release_notes *draft*" ri:space-key="DOCS" /></ac:link></p>"#,
        );
    }

    fn pulled_document(content_id: &str, title: &str, markdown: &str) -> LocalDocument {
        let directory = PathBuf::from(format!("/tmp/root/page--{content_id}"));
        LocalDocument {
            directory: directory.clone(),
            markdown_path: directory.join("index.md"),
            sidecar_path: directory.join(".confluence.json"),
            frontmatter: Frontmatter {
                title: title.to_string(),
                kind: "page".to_string(),
                labels: vec![],
                status: "current".to_string(),
                parent: None,
                properties: BTreeMap::new(),
            },
            body_markdown: markdown.to_string(),
            sidecar: Sidecar {
                content_id: Some(content_id.to_string()),
                provider: Some(ProviderKind::DataCenter),
                ..Sidecar::default()
            },
        }
    }

    /// A page as a pull fetched it, placed in `root`.
    fn fetched_page(root: &Path, content_id: &str, title: &str, markdown: &str) -> FetchedPage {
        let mut document = pulled_document(content_id, title, markdown);
        document.directory = root.join(document_dir_name(title, Some(content_id)));
        document.markdown_path = document.directory.join("index.md");
        document.sidecar_path = document.directory.join(".confluence.json");
        FetchedPage {
            document,
            remote_storage: format!("<p>{markdown}</p>"),
        }
    }

    /// Stores the pages a pull fetched, as `write_items` does.
    fn store_pages(
        root: &Path,
        fetched: impl Fn() -> Vec<FetchedPage>,
    ) -> (Vec<PulledPage>, Vec<UnconvertiblePage>) {
        let (documents, left_out) = convertible_pages(|left_out| {
            fetched()
                .into_iter()
                .filter(|page| !left_out.contains(&page.document.directory))
                .collect()
        });
        store_fetched_pages(root, documents, left_out).expect("store")
    }

    #[test]
    fn pull_writes_the_pages_that_convert_and_keeps_the_rest_for_inspection() {
        let temp = tempfile::tempdir().expect("tempdir");
        let staged = temp.path().join("staged");
        let root = temp.path().join("pulled");
        let mut child = fetched_page(&staged, "404", "Child", "child text");
        child.document.directory = staged.join("broken-one--202").join("child--404");
        child.document.markdown_path = child.document.directory.join("index.md");
        child.document.sidecar_path = child.document.directory.join(".confluence.json");
        child.document.sidecar.remote_parent_id = Some("202".to_string());
        let fetched = || {
            vec![
                fetched_page(&staged, "101", "Fine", "plain text"),
                fetched_page(&staged, "202", "Broken one", "an <unclosed> tag"),
                fetched_page(&staged, "303", "Broken two", "another <open attr> tag"),
                child.clone(),
            ]
        };
        for page in fetched() {
            fs::create_dir_all(page.document.directory.join("attachments")).expect("page dir");
        }
        fs::write(staged.join("broken-two--303/attachments/a.txt"), "a").expect("attachment");

        let (written, unconvertible) = store_pages(&staged, fetched);
        let error = install_pulled_pages(&staged, &root, written, unconvertible, Vec::new())
            .expect_err("pages that do not convert fail the pull");
        let error = error
            .downcast::<crate::output::CliError>()
            .expect("typed error");

        assert_eq!(error.kind, crate::output::ErrorKind::InvalidInput);
        assert!(error.message.contains("2 of 4"), "{}", error.message);
        assert!(
            error.message.contains("202 (Broken one)"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("303 (Broken two)"),
            "{}",
            error.message
        );
        assert!(!error.message.contains("101"), "{}", error.message);
        let details = error.details.expect("details");
        assert_eq!(
            details["written"],
            serde_json::json!([
                {"path": root.join("fine--101"), "storage_differs_from_remote": false},
                {
                    "path": root.join("broken-one--202/child--404"),
                    "storage_differs_from_remote": false,
                },
            ])
        );
        let pages = &details["pages"];
        assert_eq!(pages[0]["content_id"], "202");
        assert_eq!(pages[1]["title"], "Broken two");
        assert!(
            pages[0]["error"]
                .as_str()
                .is_some_and(|message| message.contains("unclosed"))
        );
        let artifacts = root.join(".confluence-pull-failures/broken-one--202");
        assert_eq!(pages[0]["artifacts"], serde_json::json!(artifacts));
        assert!(
            fs::read_to_string(artifacts.join("generated.md"))
                .expect("generated markdown")
                .ends_with("an <unclosed> tag")
        );
        assert_eq!(
            fs::read_to_string(artifacts.join("generated.xml")).expect("generated storage"),
            "<p>an <unclosed> tag</p>"
        );
        assert_eq!(
            fs::read_to_string(artifacts.join("remote.xml")).expect("remote storage"),
            "<p>an <unclosed> tag</p>"
        );

        assert!(root.join("fine--101/index.md").is_file());
        assert!(root.join("broken-one--202/child--404/index.md").is_file());
        assert!(!root.join("broken-one--202/index.md").exists());
        assert!(!root.join("broken-one--202/attachments").exists());
        assert!(!root.join("broken-two--303").exists());

        // The pages that were left out are not documents, so the rest of the
        // snapshot plans as unchanged, the child keeps its remote parent, and
        // a later pull may replace the snapshot.
        let plan = plan_path(&root, false, true, false).expect("plan");
        assert_eq!(plan.items.len(), 1, "{:?}", plan.items);
        assert_eq!(plan.items[0].action, PlanActionKind::Noop);
        validate_pull_destination(&root, false).expect("snapshot can be pulled again");
    }

    #[test]
    fn page_left_out_keeps_the_storage_that_did_not_validate_when_there_is_one() {
        let temp = tempfile::tempdir().expect("tempdir");
        let staged = temp.path().join("staged");
        let root = temp.path().join("pulled");
        let fetched = || {
            let mut fine = fetched_page(&staged, "101", "Fine", "plain text");
            fine.document.sidecar.space_key = Some("DOCS".to_string());
            vec![
                fine,
                fetched_page(
                    &staged,
                    "202",
                    "Links",
                    "see [Fine](../fine--101/index.md) and an <unclosed> tag",
                ),
                fetched_page(
                    &staged,
                    "303",
                    "Open block",
                    ":::confluence-macro name=\"info\"\nnever closed",
                ),
            ]
        };
        let (written, unconvertible) = store_pages(&staged, fetched);
        let error = install_pulled_pages(&staged, &root, written, unconvertible, Vec::new())
            .expect_err("left out")
            .downcast::<crate::output::CliError>()
            .expect("typed error");
        let details = error.details.expect("details");
        let failures = root.join(PULL_FAILURES_DIR);

        // The storage kept is the one apply would have sent, links included.
        let generated =
            fs::read_to_string(failures.join("links--202/generated.xml")).expect("storage kept");
        assert!(generated.contains("<unclosed>"), "{generated}");
        assert!(generated.contains("pageId=101"), "{generated}");
        assert!(!generated.contains("index.md"), "{generated}");
        assert_eq!(
            details["pages"][0]["files"],
            serde_json::json!(["generated.md", "generated.xml", "remote.xml"])
        );

        // Markdown that does not convert at all leaves no storage to keep,
        // and the report does not name a file that is not there.
        assert_eq!(details["pages"][1]["content_id"], "303", "{details}");
        assert!(!failures.join("open-block--303/generated.xml").exists());
        assert!(failures.join("open-block--303/generated.md").is_file());
        assert_eq!(
            details["pages"][1]["files"],
            serde_json::json!(["generated.md", "remote.xml"])
        );
        let hint = error.hint.expect("hint");
        assert!(
            hint.contains("when that Markdown converts at all"),
            "{hint}"
        );
    }

    #[test]
    fn pull_that_leaves_a_page_out_reports_its_warnings_in_the_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let staged = temp.path().join("staged");
        let root = temp.path().join("pulled");
        let fetched = || vec![fetched_page(&staged, "202", "Broken", "an <unclosed> tag")];
        let (written, unconvertible) = store_pages(&staged, fetched);
        let warnings = vec!["names could not be looked up".to_string()];
        let error = install_pulled_pages(&staged, &root, written, unconvertible, warnings)
            .expect_err("left out")
            .downcast::<crate::output::CliError>()
            .expect("typed error");
        assert_eq!(
            error.details.expect("details")["warnings"],
            serde_json::json!(["names could not be looked up"])
        );
        let hint = error.hint.expect("hint");
        assert!(
            hint.ends_with("Also: names could not be looked up."),
            "{hint}"
        );
    }

    #[test]
    fn pull_that_leaves_a_page_out_still_names_the_pages_whose_markup_differs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let staged = temp.path().join("staged");
        let root = temp.path().join("pulled");
        let fetched = || {
            let mut differing = fetched_page(&staged, "101", "Differs", "plain text");
            differing.remote_storage = "<p>other text</p>".to_string();
            vec![
                differing,
                fetched_page(&staged, "202", "Broken", "an <unclosed> tag"),
                fetched_page(&staged, "303", "Same", "plain text"),
            ]
        };
        let (written, unconvertible) = store_pages(&staged, fetched);
        let error = install_pulled_pages(&staged, &root, written, unconvertible, Vec::new())
            .expect_err("left out")
            .downcast::<crate::output::CliError>()
            .expect("typed error");
        assert_eq!(
            error.details.expect("details")["written"],
            serde_json::json!([
                {"path": root.join("differs--101"), "storage_differs_from_remote": true},
                {"path": root.join("same--303"), "storage_differs_from_remote": false},
            ])
        );
        let hint = error.hint.expect("hint");
        assert!(
            hint.contains("1 of 2 pages written hold markup that Markdown does not carry"),
            "{hint}"
        );
    }

    #[test]
    fn pull_that_leaves_out_every_blog_post_leaves_nothing_in_the_way_of_the_next() {
        let temp = tempfile::tempdir().expect("tempdir");
        let staged = temp.path().join("staged");
        let root = temp.path().join("pulled");
        let fetched = || {
            let mut post = fetched_page(&staged, "202", "Broken", "an <unclosed> tag");
            post.document.directory = staged.join("blog-posts").join("broken--202");
            post.document.markdown_path = post.document.directory.join("index.md");
            post.document.sidecar_path = post.document.directory.join(".confluence.json");
            vec![fetched_page(&staged, "101", "Fine", "plain text"), post]
        };
        for page in fetched() {
            fs::create_dir_all(&page.document.directory).expect("page dir");
        }
        let (written, unconvertible) = store_pages(&staged, fetched);
        install_pulled_pages(&staged, &root, written, unconvertible, Vec::new())
            .expect_err("left out");

        assert!(!root.join("blog-posts").exists());
        validate_pull_destination(&root, false).expect("nothing of the user's is in the way");
    }

    #[test]
    fn page_copied_into_the_failures_directory_is_not_a_document() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let mut page = fetched_page(root, "101", "Fine", "plain text").document;
        save_document(&page).expect("page");
        page.directory = root.join(PULL_FAILURES_DIR).join("copy--101");
        page.markdown_path = page.directory.join("index.md");
        page.sidecar_path = page.directory.join(".confluence.json");
        save_document(&page).expect("copy");
        fs::write(
            root.join(PULL_FAILURES_DIR).join(PULL_FAILURES_MANIFEST),
            "{}",
        )
        .expect("manifest");
        // A directory of that name that no pull wrote holds documents.
        page.directory = root.join("team").join(PULL_FAILURES_DIR).join("draft--101");
        page.markdown_path = page.directory.join("index.md");
        page.sidecar_path = page.directory.join(".confluence.json");
        save_document(&page).expect("draft");

        let documents = load_local_documents(root).expect("documents");
        assert_eq!(
            documents
                .iter()
                .map(|document| document.directory.as_path())
                .collect::<Vec<_>>(),
            [
                root.join("fine--101"),
                root.join("team").join(PULL_FAILURES_DIR).join("draft--101"),
            ]
        );
    }

    #[test]
    fn pull_does_not_replace_a_failures_directory_that_gained_a_directory() {
        type Change = fn(&Path);
        let changes: [(Change, bool); 4] = [
            (
                |failures| fs::create_dir(failures.join("mine")).expect("directory of the user"),
                false,
            ),
            (
                |failures| fs::create_dir(failures.join("broken--202/mine")).expect("directory"),
                false,
            ),
            // What was removed leaves nothing of the user's to keep.
            (
                |failures| fs::remove_file(failures.join("broken--202/remote.xml")).expect("file"),
                true,
            ),
            (
                |failures| fs::remove_dir_all(failures.join("broken--202")).expect("directory"),
                true,
            ),
        ];
        for (change, replaceable) in changes {
            let temp = tempfile::tempdir().expect("tempdir");
            let staged = temp.path().join("staged");
            let root = temp.path().join("pulled");
            let fetched = || vec![fetched_page(&staged, "202", "Broken", "an <unclosed> tag")];
            let (written, unconvertible) = store_pages(&staged, fetched);
            install_pulled_pages(&staged, &root, written, unconvertible, Vec::new())
                .expect_err("left out");
            validate_pull_destination(&root, false).expect("only what the pull wrote");

            change(&root.join(PULL_FAILURES_DIR));
            assert_eq!(validate_pull_destination(&root, false).is_ok(), replaceable);
        }
    }

    #[test]
    fn pull_destination_that_holds_only_pages_left_out_can_be_pulled_again() {
        let temp = tempfile::tempdir().expect("tempdir");
        let staged = temp.path().join("staged");
        let root = temp.path().join("pulled");
        let fetched = || vec![fetched_page(&staged, "202", "Broken", "an <unclosed> tag")];
        let (written, unconvertible) = store_pages(&staged, fetched);
        install_pulled_pages(&staged, &root, written, unconvertible, Vec::new())
            .expect_err("left out");

        validate_pull_destination(&root, false).expect("nothing of the user's is in the way");
        fs::write(root.join("notes.txt"), "mine").expect("unmanaged file");
        validate_pull_destination(&root, false).expect_err("an unmanaged file is in the way");
    }

    #[test]
    fn pull_replaces_in_the_failures_directory_only_what_a_pull_wrote_there() {
        let failures = Path::new(PULL_FAILURES_DIR);
        for (kept, in_the_way) in [
            (None, failures.join("notes.txt")),
            (None, failures.join("broken--202/notes.txt")),
            (None, failures.join("broken--202/extra/remote.xml")),
            (None, failures.join("mine/deeper/remote.xml")),
            // A file named as one a pull writes, where no pull wrote it, and
            // one a pull wrote that was edited since.
            (None, failures.join("notes/generated.md")),
            (None, failures.join("broken--202/generated.md")),
            (None, failures.join("manifest.json")),
            (Some("202"), failures.join("notes.txt")),
            (Some("202"), failures.join("broken--202/notes.txt")),
        ] {
            let temp = tempfile::tempdir().expect("tempdir");
            let staged = temp.path().join("staged");
            let root = temp.path().join("pulled");
            let fetched = || {
                vec![
                    fetched_page(&staged, "101", "Fine", "plain text"),
                    fetched_page(&staged, "202", "Broken", "an <unclosed> tag"),
                ]
                .into_iter()
                .filter(|page| {
                    kept.is_none_or(|kept| {
                        page.document.sidecar.content_id.as_deref() == Some(kept)
                    })
                })
                .collect::<Vec<_>>()
            };
            let (written, unconvertible) = store_pages(&staged, fetched);
            install_pulled_pages(&staged, &root, written, unconvertible, Vec::new())
                .expect_err("left out");
            validate_pull_destination(&root, false).expect("only what the pull wrote");

            let in_the_way = root.join(in_the_way);
            fs::create_dir_all(in_the_way.parent().expect("parent")).expect("directories");
            fs::write(&in_the_way, "mine").expect("file of the user");
            validate_pull_destination(&root, false)
                .expect_err(&format!("{} is in the way", in_the_way.display()));
        }
    }

    #[test]
    fn pull_does_not_replace_a_file_named_as_the_failures_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join(PULL_FAILURES_DIR), "mine").expect("file of the user");
        validate_pull_destination(temp.path(), false).expect_err("the file is in the way");
    }

    #[test]
    fn page_that_is_written_links_to_a_page_left_out_by_its_remote_address() {
        let temp = tempfile::tempdir().expect("tempdir");
        let staged = temp.path().join("staged");
        let root = temp.path().join("pulled");
        let broken = staged.join(document_dir_name("Broken", Some("202")));
        let (written, unconvertible) = {
            let (documents, left_out) = convertible_pages(|left_out| {
                let link = if left_out.contains(&broken) {
                    "https://example.atlassian.net/wiki/spaces/DOCS/pages/202/Broken"
                } else {
                    "../broken--202/index.md"
                };
                vec![
                    fetched_page(&staged, "101", "Fine", &format!("see [Broken]({link})")),
                    fetched_page(&staged, "202", "Broken", "an <unclosed> tag"),
                ]
                .into_iter()
                .filter(|page| !left_out.contains(&page.document.directory))
                .collect()
            });
            store_fetched_pages(&staged, documents, left_out).expect("store")
        };
        install_pulled_pages(&staged, &root, written, unconvertible, Vec::new())
            .expect_err("left out");

        let fine = fs::read_to_string(root.join("fine--101/index.md")).expect("page written");
        assert!(
            fine.contains("(https://example.atlassian.net/wiki/spaces/DOCS/pages/202/Broken)"),
            "{fine}"
        );
        let plan = plan_path(&root, false, true, false).expect("plan");
        assert_eq!(plan.items.len(), 1, "{:?}", plan.items);
        assert_eq!(plan.items[0].action, PlanActionKind::Noop);
    }

    #[test]
    fn pull_records_the_storage_hash_of_every_convertible_page() {
        let fetched = vec![fetched_page(
            Path::new("/tmp/root"),
            "101",
            "Fine",
            "plain text",
        )];
        let (stamped, failures) = stamp_storage_hashes(fetched);
        assert!(failures.is_empty());
        assert_eq!(
            stamped[0].sidecar.storage_hash.as_deref(),
            Some(sha256_hex("<p>plain text</p>".as_bytes()).as_str())
        );
        assert!(!stamped[0].sidecar.storage_differs_from_remote);
    }

    /// Pulls one page with the given remote storage and returns its sidecar.
    fn pulled_sidecar(remote_storage: &str) -> Sidecar {
        let mut page = fetched_page(
            Path::new("/tmp/root"),
            "101",
            "Page",
            &storage_to_markdown(remote_storage),
        );
        page.remote_storage = remote_storage.to_string();
        let (mut stamped, failures) = stamp_storage_hashes(vec![page]);
        assert!(failures.is_empty(), "{remote_storage} does not convert");
        stamped.remove(0).sidecar
    }

    #[test]
    fn pull_records_whether_the_converted_storage_still_says_what_the_remote_says() {
        for same in [
            "<p>plain <strong>text</strong></p>",
            "<p>a <b>b</b> <i>c</i></p>",
            r#"<p local-id="abc">a</p><ul><li><p>one</p></li></ul>"#,
            "<pre><code>code</code></pre>",
            r#"<table class="wrapped"><tbody><tr><th>H</th></tr></tbody></table>"#,
            "<p>a&nbsp;b &eacute;</p>",
            r#"<ac:structured-macro ac:name="details" ac:schema-version="1" ac:macro-id="7f"><ac:rich-text-body><p>x</p></ac:rich-text-body></ac:structured-macro>"#,
            r#"<p><ac:link><ri:user ri:userkey="abc" /></ac:link></p>"#,
        ] {
            assert!(
                !pulled_sidecar(same).storage_differs_from_remote,
                "{same} pulled as {:?}",
                storage_to_markdown(same)
            );
        }
        for different in [
            "<div><p>a</p></div>",
            "<p><span>a</span> b</p>",
            r#"<p><img src="https://example.com/a.png" /></p>"#,
        ] {
            assert!(
                pulled_sidecar(different).storage_differs_from_remote,
                "{different} pulled as {:?}",
                storage_to_markdown(different)
            );
        }
    }

    #[test]
    fn sidecar_of_a_page_that_converts_exactly_does_not_mention_it() {
        let sidecar = serde_json::to_value(pulled_sidecar("<p>a</p>")).expect("sidecar");
        assert!(sidecar.get("storage_differs_from_remote").is_none());
        let sidecar = serde_json::to_value(pulled_sidecar("<div><p>a</p></div>")).expect("sidecar");
        assert_eq!(sidecar["storage_differs_from_remote"], true);
    }

    #[test]
    fn link_body_that_matches_the_generated_label_is_kept() {
        assert_round_trips(
            r#"<p>ask <ac:link><ri:user ri:account-id="557058:abc" /><ac:plain-text-link-body><![CDATA[557058:abc]]></ac:plain-text-link-body></ac:link> now</p>"#,
        );
        assert_round_trips(
            r#"<p>see <ac:link><ri:page ri:content-title="Roadmap" /><ac:plain-text-link-body><![CDATA[Roadmap]]></ac:plain-text-link-body></ac:link> now</p>"#,
        );
    }

    /// Converts storage to Markdown as a pull that also holds the page
    /// titled "Sibling" does, and returns the Markdown written for it.
    fn pull_next_to_sibling(storage: &str) -> String {
        let paths = BTreeMap::from([(
            "456".to_string(),
            PathBuf::from("/tmp/root/sibling-page--456"),
        )]);
        let items = BTreeMap::from([(
            "456".to_string(),
            ContentItem {
                id: "456".to_string(),
                kind: ContentKind::Page,
                title: "Sibling".to_string(),
                status: "current".to_string(),
                space_id: None,
                space_key: Some("MFS".to_string()),
                parent_id: None,
                version: None,
                body_storage: None,
                labels: vec![],
                properties: BTreeMap::new(),
                web_url: None,
                created_at: None,
                updated_at: None,
            },
        )]);
        rewrite_page_links(
            &storage_to_markdown(storage),
            Path::new("/tmp/root/current-page--123"),
            &paths,
            &items,
        )
    }

    #[test]
    fn bodiless_link_to_a_page_of_the_same_pull_stays_bodiless() {
        for storage in [
            r#"<p>see <ac:link><ri:page ri:content-title="Sibling" ri:space-key="MFS" /></ac:link> now</p>"#,
            r#"<p>see <ac:link ac:anchor="intro"><ri:page ri:content-title="Sibling" ri:space-key="MFS" /></ac:link> now</p>"#,
        ] {
            let applied = apply_storage(&pull_next_to_sibling(storage)).expect("apply");

            assert_eq!(canonical(&applied), canonical(storage));
        }
    }

    #[test]
    fn links_keep_their_display_attributes() {
        for storage in [
            r#"<p>see <a href="https://x.example/a" data-card-appearance="inline">https://x.example/a</a> now</p>"#,
            r#"<p><a href="https://x.example/a" data-card-appearance="block">https://x.example/a</a></p>"#,
            r#"<p>see <a href="https://x.example/a?b=1&amp;c=2" data-card-appearance="inline">https://x.example/a?b=1&amp;c=2</a> now</p>"#,
            r#"<p>see <a href="https://x.example/a" title="a &quot;b&quot; &lt;c&gt;">the <strong>page</strong></a> now</p>"#,
            r#"<p>see <a href="https://x.example/a" data-card-appearance="inline">a *draft* of [x](y) &amp; `z`</a> now</p>"#,
            r#"<p>see <a href="https://x.example/a" data-card-appearance="inline"></a> now</p>"#,
            r#"<table><tbody><tr><td><p>a | b <a href="https://x.example/a|b" data-card-appearance="inline">c | d</a></p></td></tr></tbody></table>"#,
            r#"<ul><li><a href="https://x.example/a" data-card-appearance="inline">https://x.example/a</a></li></ul>"#,
            r#"<p><a href="https://x.example/a" data-card-appearance="inline"></a></p>"#,
            r#"<blockquote><p><a href="https://x.example/a" data-card-appearance="inline">a</a></p></blockquote>"#,
            r#"<p><a href="https://x.example/a" data-card-appearance="inline">a</a><a href="https://x.example/b" data-card-appearance="inline">b</a></p>"#,
            r#"<p><a href="https://x.example/a" data-card-appearance="inline">one<br />two</a></p>"#,
            r#"<p><a href="https://x.example/a" data-card-appearance="inline">one <ac:emoticon ac:name="smile" /></a></p>"#,
            r#"<h2><a href="https://x.example/a" data-card-appearance="inline">https://x.example/a</a></h2>"#,
        ] {
            assert_round_trips(storage);
        }
    }

    #[test]
    fn block_after_a_layout_stays_outside_it() {
        assert_keeps(
            r#"<ac:layout><ac:layout-section ac:type="single"><ac:layout-cell><p>in</p></ac:layout-cell></ac:layout-section></ac:layout><p>after</p>"#,
        );
        assert_keeps(
            r#"<p>before</p><ac:layout><ac:layout-section ac:type="single"><ac:layout-cell><p>in</p></ac:layout-cell></ac:layout-section></ac:layout><h2>after</h2><p>more</p>"#,
        );
    }

    #[test]
    fn link_with_display_attributes_keeps_a_bar_in_its_address_inside_a_table() {
        assert_round_trips(
            r#"<table><thead><tr><th>h</th></tr></thead><tbody><tr><td><a href="https://x.example/a|b" title="a | b">c | d</a></td></tr></tbody></table>"#,
        );
    }

    #[test]
    fn link_attribute_keeps_a_tab() {
        let storage = r#"<p><a href="https://x.example" title="a&#9;b">x</a></p>"#;
        let applied = apply_storage(&storage_to_markdown(storage)).expect("apply");
        assert!(applied.contains("a&#9;b"), "{applied}");
    }

    #[test]
    fn link_with_display_attributes_is_written_as_an_inline_tag() {
        assert_eq!(
            storage_to_markdown(
                r#"<p>see <a href="https://x.example/a" data-card-appearance="inline">https://x.example/a</a> now</p>"#
            )
            .trim(),
            r#"see <a href="https://x.example/a" data-card-appearance="inline">https://x.example/a</a> now"#
        );
    }

    #[test]
    fn link_without_display_attributes_stays_a_markdown_link() {
        for storage in [
            r#"<p>see <a href="https://x.example/a">label</a> now</p>"#,
            r#"<p>see <a href="https://x.example/a" local-id="1f2e">label</a> now</p>"#,
        ] {
            assert_eq!(
                storage_to_markdown(storage).trim(),
                "see [label](https://x.example/a) now"
            );
        }
    }

    #[test]
    fn link_with_an_attribute_that_markdown_does_not_read_survives() {
        assert_round_trips(r#"<p>see <a href="https://x.example/" café="1">x</a> now</p>"#);
    }

    #[test]
    fn link_keeps_its_address_next_to_an_attribute_that_ends_in_href() {
        assert_round_trips(
            r#"<p><a data-href="confluence-page://page?content-id=7" href="https://x.example/">x</a></p>"#,
        );
    }

    #[test]
    fn raw_storage_next_to_a_link_to_a_page_of_the_same_pull_stays_whole() {
        for storage in [
            r#"<p><ac:link><ri:page /></ac:link> <ac:link><ri:url ri:value="https://example.com/wiki/pages/456/Sibling" /><ac:plain-text-link-body><![CDATA[Sibling]]></ac:plain-text-link-body></ac:link></p>"#,
            r#"<p>a <ac:link ac:unknown="1"><ri:url ri:value="https://example.com/wiki/pages/456/Sibling" /></ac:link> b <a href="https://example.com/wiki/pages/456/Sibling">c</a></p>"#,
        ] {
            let applied = apply_storage(&pull_next_to_sibling(storage)).expect("apply");

            assert_eq!(canonical(&applied), canonical(storage));
        }
    }

    #[test]
    fn link_spread_over_lines_to_a_page_of_the_same_pull_survives() {
        let storage = "<p>See <a title=\"tip\"\nhref=\"https://example.com/wiki/pages/456/Sibling\">one<br />two</a>.</p>";
        let applied = apply_storage(&pull_next_to_sibling(storage)).expect("apply");
        assert_eq!(canonical(&applied), canonical(storage));
    }

    #[test]
    fn raw_storage_after_a_quoted_raw_block_stays_whole() {
        let storage = r#"<blockquote><ac:emoticon ac:name="smile" /></blockquote><ac:structured-macro ac:name="html"><ac:plain-text-body><![CDATA[see https://example.com/wiki/pages/456/Sibling now]]></ac:plain-text-body></ac:structured-macro>"#;
        let applied = apply_storage(&pull_next_to_sibling(storage)).expect("apply");
        assert_eq!(canonical(&applied), canonical(storage));
    }

    #[test]
    fn link_drops_only_its_local_id() {
        let applied = apply_storage(&storage_to_markdown(
            r#"<p>see <a local-id="1f2e" href="https://x.example/a" data-card-appearance="inline">label</a> now</p>"#,
        ))
        .expect("apply");

        assert_eq!(
            canonical(&applied),
            canonical(
                r#"<p>see <a href="https://x.example/a" data-card-appearance="inline">label</a> now</p>"#
            )
        );
    }

    #[test]
    fn confluence_links_keep_their_display_attributes() {
        for storage in [
            r#"<p>see <ac:link ac:card-appearance="inline"><ri:page ri:content-title="Roadmap" /></ac:link> now</p>"#,
            r#"<p>see <ac:link ac:card-appearance="inline" ac:tooltip="the &quot;plan&quot; &amp; more (v2)"><ri:page ri:content-title="Roadmap" ri:space-key="MFS" /></ac:link> now</p>"#,
            r#"<p>see <ac:link ac:anchor="intro" ac:card-appearance="block"><ri:page ri:content-title="Roadmap" /></ac:link> now</p>"#,
            r#"<p>see <ac:link ac:tooltip=""><ri:page ri:content-id="987" /></ac:link> now</p>"#,
            r#"<p>see <ac:link ac:card-appearance="inline"><ri:page ri:content-title="Roadmap" /><ac:plain-text-link-body><![CDATA[the plan]]></ac:plain-text-link-body></ac:link> now</p>"#,
            r#"<p>see <ac:link ac:card-appearance="inline"><ri:page ri:content-title="Roadmap" /><ac:link-body>the <strong>plan</strong></ac:link-body></ac:link> now</p>"#,
            r#"<p>ask <ac:link ac:card-appearance="inline"><ri:user ri:account-id="557058:abc" /></ac:link> now</p>"#,
            r#"<p>ask <ac:link ac:tooltip="owner"><ri:user ri:account-id="557058:abc" /><ac:plain-text-link-body><![CDATA[Alex]]></ac:plain-text-link-body></ac:link> now</p>"#,
            r#"<p>see <ac:link ac:card-appearance="inline"><ri:attachment ri:filename="plan.pdf" /></ac:link> now</p>"#,
            r#"<p>see <ac:link ac:card-appearance="inline"><ri:url ri:value="https://x.example/a" /></ac:link> now</p>"#,
            r#"<p>see <ac:link ac:anchor="intro" ac:tooltip="t" /> now</p>"#,
            r#"<p>see <ac:link ac:unknown="1"><ri:page ri:content-title="Roadmap" /></ac:link> now</p>"#,
        ] {
            assert_round_trips(storage);
        }
    }

    #[test]
    fn display_attribute_given_twice_is_written_once() {
        let applied = apply_storage(
            "[x](confluence-page://page?content-id=7&tooltip=a&tooltip=b&card-appearance=inline)",
        )
        .expect("apply");

        assert_eq!(
            canonical(&applied),
            canonical(
                r#"<p><ac:link ac:tooltip="a" ac:card-appearance="inline"><ri:page ri:content-id="7" /><ac:plain-text-link-body><![CDATA[x]]></ac:plain-text-link-body></ac:link></p>"#
            )
        );
    }

    #[test]
    fn link_to_a_page_it_does_not_name_survives() {
        for storage in [
            r#"<p>see <ac:link><ri:page /></ac:link> now</p>"#,
            r#"<p>see <ac:link ac:card-appearance="inline"><ri:page /><ac:link-body>home <strong>page</strong></ac:link-body></ac:link> now</p>"#,
            r#"<p>see <ac:link ac:anchor="intro"><ri:page /></ac:link> now</p>"#,
            r#"<p>see <ac:link><ri:page ri:space-key="MFS" /></ac:link> now</p>"#,
        ] {
            assert_round_trips(storage);
        }
    }

    #[test]
    fn confluence_link_with_display_attributes_stays_a_markdown_link() {
        assert_eq!(
            storage_to_markdown(
                r#"<p>see <ac:link ac:card-appearance="inline"><ri:page ri:content-title="Roadmap" /><ac:plain-text-link-body><![CDATA[the plan]]></ac:plain-text-link-body></ac:link> now</p>"#
            )
            .trim(),
            "see [the plan](confluence-page://page?content-title=Roadmap&card-appearance=inline) now"
        );
    }

    #[test]
    fn link_to_a_page_of_the_same_pull_keeps_its_display_attributes() {
        for storage in [
            r#"<p>see <ac:link ac:card-appearance="inline"><ri:page ri:content-title="Sibling" ri:space-key="MFS" /><ac:plain-text-link-body><![CDATA[the sibling]]></ac:plain-text-link-body></ac:link> now</p>"#,
            r#"<p>see <ac:link ac:tooltip="t"><ri:page ri:content-title="Sibling" ri:space-key="MFS" /></ac:link> now</p>"#,
            r#"<p>see <a href="https://example.com/wiki/spaces/MFS/pages/456/Sibling" data-card-appearance="inline">https://example.com/wiki/spaces/MFS/pages/456/Sibling</a> now</p>"#,
            r#"<p>see <a href="https://example.com/wiki/pages/viewpage.action?pageId=456" data-card-appearance="inline">Sibling</a> now</p>"#,
        ] {
            let applied = apply_storage(&pull_next_to_sibling(storage)).expect("apply");

            assert_eq!(canonical(&applied), canonical(storage));
        }
    }

    #[test]
    fn link_to_a_page_of_the_same_pull_is_still_local_next_to_one_with_display_attributes() {
        let markdown = pull_next_to_sibling(
            r#"<p><a href="https://example.com/wiki/spaces/MFS/pages/456/Sibling" data-card-appearance="inline">card</a> and <a href="https://example.com/wiki/spaces/MFS/pages/456/Sibling">plain</a></p>"#,
        );

        assert_eq!(
            markdown.trim(),
            r#"<a href="https://example.com/wiki/spaces/MFS/pages/456/Sibling" data-card-appearance="inline">card</a> and [plain](../sibling-page--456/index.md)"#
        );
    }

    #[test]
    fn link_with_display_attributes_to_an_attachment_keeps_its_address() {
        let url = "https://example.com/download/attachments/123/plan.pdf";
        let storage = format!(
            r#"<p><a href="{url}" data-card-appearance="inline">{url}</a> and <a href="{url}">plain</a></p>"#
        );
        let pulled = rewrite_attachment_links(
            &storage_to_markdown(&storage),
            &[crate::model::AttachmentInfo {
                id: "att1".to_string(),
                title: "plan.pdf".to_string(),
                version: None,
                media_type: None,
                file_size: None,
                download_url: Some(url.to_string()),
                comment: None,
            }],
        );

        assert_eq!(
            pulled.trim(),
            format!(
                r#"<a href="{url}" data-card-appearance="inline">{url}</a> and [plain](attachments/plan.pdf)"#
            )
        );
    }

    #[test]
    fn edited_label_of_a_link_to_a_page_of_the_same_pull_becomes_its_body() {
        let markdown = pull_next_to_sibling(
            r#"<p>see <ac:link><ri:page ri:content-title="Sibling" ri:space-key="MFS" /></ac:link> now</p>"#,
        )
        .replace("[Sibling]", "[the sibling]");

        let applied = apply_storage(&markdown).expect("apply");

        assert_eq!(
            canonical(&applied),
            canonical(
                r#"<p>see <ac:link><ri:page ri:content-title="Sibling" ri:space-key="MFS" /><ac:plain-text-link-body><![CDATA[the sibling]]></ac:plain-text-link-body></ac:link> now</p>"#
            )
        );
    }

    #[test]
    fn link_with_a_body_to_a_page_of_the_same_pull_points_at_its_local_file() {
        let markdown = pull_next_to_sibling(
            r#"<p><ac:link><ri:page ri:content-title="Sibling" ri:space-key="MFS" /><ac:plain-text-link-body><![CDATA[over there]]></ac:plain-text-link-body></ac:link></p>"#,
        );

        assert_eq!(
            markdown.trim(),
            "[over there](../sibling-page--456/index.md)"
        );
    }

    #[test]
    fn link_whose_generated_label_was_edited_gains_that_text_as_its_body() {
        let markdown = storage_to_markdown(
            r#"<p>ask <ac:link><ri:user ri:username="jdoe" /></ac:link> now</p>"#,
        )
        .replace("[@jdoe]", "[Jane]");

        let applied = apply_storage(&markdown).expect("apply");

        assert_eq!(
            canonical(&applied),
            canonical(
                r#"<p>ask <ac:link><ri:user ri:username="jdoe" /><ac:plain-text-link-body><![CDATA[Jane]]></ac:plain-text-link-body></ac:link> now</p>"#
            )
        );
    }

    #[test]
    fn link_body_with_an_ampersand_survives() {
        assert_round_trips(
            r#"<p><ac:link><ri:page ri:content-title="Lab" /><ac:plain-text-link-body><![CDATA[R&D lab]]></ac:plain-text-link-body></ac:link></p>"#,
        );
        assert_round_trips(
            r#"<p><ac:link><ri:attachment ri:filename="manual.pdf" /><ac:plain-text-link-body><![CDATA[R&D "manual" <draft>]]></ac:plain-text-link-body></ac:link></p>"#,
        );
    }

    #[test]
    fn attachment_file_names_with_url_characters_survive() {
        for file_name in [
            "budget%20final.pdf",
            "report#2.pdf",
            "O'Brien.pdf",
            "my file (1).png",
            "what?.png",
            "a&amp;b.png",
            "r\u{e9}sum\u{e9}.pdf",
        ] {
            assert_round_trips(&format!(
                r#"<p><ac:image><ri:attachment ri:filename="{file_name}" /></ac:image></p>"#
            ));
            assert_round_trips(&format!(
                r#"<p>see <ac:link><ri:attachment ri:filename="{file_name}" /></ac:link></p>"#
            ));
            assert_round_trips(&format!(
                r#"<p>see <ac:link ac:anchor="page=2"><ri:attachment ri:filename="{file_name}" /><ac:plain-text-link-body><![CDATA[the file]]></ac:plain-text-link-body></ac:link></p>"#
            ));
        }
    }

    #[test]
    fn attachment_file_names_stay_readable_in_markdown() {
        let markdown = storage_to_markdown(
            "<p><ac:link><ri:attachment ri:filename=\"r\u{e9}sum\u{e9}-v2.pdf\" /></ac:link></p>",
        );

        assert!(
            markdown.contains("(attachments/r\u{e9}sum\u{e9}-v2.pdf)"),
            "{markdown}"
        );
    }

    #[test]
    fn markup_quoted_inside_code_is_left_alone() {
        let storage = r#"<ac:structured-macro ac:name="code"><ac:plain-text-body><![CDATA[<img src="https://example.org/logo.png" alt="Logo" />
<a href="attachments/manual.pdf">manual.pdf</a>
<a href="confluence-user://user?username=jdoe">@jdoe</a>]]></ac:plain-text-body></ac:structured-macro>"#;

        assert_round_trips(storage);
    }

    #[test]
    fn raw_html_image_with_sizing_keeps_its_attributes() {
        let applied =
            apply_storage(r#"<img src="https://example.org/chart.png" width="300" alt="Chart" />"#)
                .expect("apply");

        assert!(applied.contains(r#"width="300""#), "{applied}");
        assert!(
            applied.contains(r#"src="https://example.org/chart.png""#),
            "{applied}"
        );
    }

    #[test]
    fn attachment_references_do_not_depend_on_how_the_root_is_spelled() {
        let markdown = "![Logo](attachments/logo.png) and [the manual](attachments/manual.pdf)";
        let expected = apply_storage_in("/tmp/root/page--123", markdown).expect("apply");
        assert!(
            expected.contains(r#"<ri:attachment ri:filename="logo.png" />"#),
            "{expected}"
        );

        for directory in [".", "./page--123", "page--123", "root/../page--123"] {
            assert_eq!(
                apply_storage_in(directory, markdown).expect("apply"),
                expected,
                "document directory {directory:?}"
            );
        }
    }

    #[test]
    fn attachment_link_with_a_title_keeps_it() {
        let applied = apply_storage(r#"[Manual](attachments/my%20file.pdf "Download the PDF")"#)
            .expect("apply");

        assert!(applied.contains(r#"title="Download the PDF""#), "{applied}");
        assert!(
            applied.contains(r#"href="/download/attachments/123/my%20file.pdf""#),
            "{applied}"
        );
    }

    #[test]
    fn markup_quoted_in_a_plain_text_link_body_is_left_alone() {
        for resource in [
            r#"<ri:page ri:content-title="Icons" />"#,
            r#"<ri:user ri:username="jdoe" />"#,
            r#"<ri:attachment ri:filename="icons.zip" />"#,
        ] {
            for body in [
                r#"<img src="https://example.org/icon.png" />"#,
                r#"<a href="attachments/icons.zip">icons.zip</a>"#,
                r#"<a href="confluence-user://user?username=jdoe">@jdoe</a>"#,
            ] {
                assert_round_trips(&format!(
                    "<p><ac:link>{resource}<ac:plain-text-link-body><![CDATA[{body}]]></ac:plain-text-link-body></ac:link></p>"
                ));
            }
        }
    }

    #[test]
    fn explicitly_closed_image_in_raw_storage_stays_well_formed() {
        let applied = apply_storage(
            "```confluence-storage\n<p><img src=\"https://example.com/x.png\"></img></p>\n```",
        )
        .expect("apply");

        assert_eq!(
            canonical(&applied),
            canonical(r#"<p><img src="https://example.com/x.png"></img></p>"#)
        );
    }

    #[test]
    fn heading_with_an_explicit_id_keeps_it() {
        let applied = apply_storage("## Setup {#install}").expect("apply");

        assert_eq!(
            canonical(&applied),
            canonical(r#"<h2 id="install">Setup</h2>"#)
        );
    }

    #[test]
    fn heading_text_in_braces_survives() {
        for heading in [
            "Setup {#install}",
            "Config {.wide}",
            "Empty {}",
            "a {b}",
            "{#only}",
            "Use <code>{#x}</code>",
            "Set <strong>{#y}</strong>",
            r"Path C:\{#z}",
            "a {b} c",
            r"Ends with a backslash {#w}\",
        ] {
            assert_round_trips(&format!("<h2>{heading}</h2>"));
        }
    }

    #[test]
    fn heading_text_ending_in_hashes_survives() {
        for heading in ["Topic #", "Topic ##", "C#", "#", "Pick a # or a #"] {
            assert_round_trips(&format!("<h2>{heading}</h2>"));
        }
    }

    #[test]
    fn ordered_list_start_number_survives_at_the_marker_limit() {
        assert_round_trips(r#"<ol start="999999998"><li>one</li><li>two</li><li>three</li></ol>"#);
        assert_round_trips(r#"<ol start="1000000000"><li>one</li><li>two</li></ol>"#);
    }

    #[test]
    fn images_and_attachment_links_in_raw_storage_keep_their_elements() {
        for (raw, expected) in [
            (
                r#"<p><img src="https://example.com/x.png" /></p>"#,
                r#"<p><img src="https://example.com/x.png" /></p>"#,
            ),
            (
                r#"<p><img src="attachments/x.png" alt="x" /></p>"#,
                r#"<p><img src="/download/attachments/123/x.png" alt="x" /></p>"#,
            ),
            (
                r#"<p><a href="attachments/report.pdf">report.pdf</a></p>"#,
                r#"<p><a href="/download/attachments/123/report.pdf">report.pdf</a></p>"#,
            ),
        ] {
            let applied =
                apply_storage(&format!("```confluence-storage\n{raw}\n```")).expect("apply");

            assert_eq!(canonical(&applied), canonical(expected));
        }
    }

    #[test]
    fn local_target_is_resolved_next_to_a_similarly_named_attribute() {
        for (raw, expected) in [
            (
                r#"<p><a data-src="preview" href="attachments/report.pdf">report</a></p>"#,
                r#"<p><a data-src="preview" href="/download/attachments/123/report.pdf">report</a></p>"#,
            ),
            (
                r#"<p><img data-href="attachments/y.png" src="attachments/x.png" /></p>"#,
                r#"<p><img data-href="attachments/y.png" src="/download/attachments/123/x.png" /></p>"#,
            ),
        ] {
            let applied =
                apply_storage(&format!("```confluence-storage\n{raw}\n```")).expect("apply");

            assert_eq!(canonical(&applied), canonical(expected));
        }
    }

    #[test]
    fn literal_confluence_tag_names_in_text_survive() {
        assert_round_trips("<p>Write &lt;ac:link&gt; here</p>");
        assert_round_trips("<p>Use <code>&lt;ri:page&gt;</code> for pages</p>");
    }

    #[test]
    fn confluence_markup_outside_a_storage_block_is_refused() {
        let error = apply_storage(r#"Write <ac:structured-macro ac:name="info" /> here"#)
            .expect_err("markup that would be escaped");

        assert!(format!("{error:#}").contains("raw Confluence XML was escaped"));
    }

    #[test]
    fn attachment_link_with_a_query_stays_a_url() {
        let applied =
            apply_storage("[download](attachments/manual.pdf?download=true)").expect("apply");

        assert_eq!(
            canonical(&applied),
            canonical(
                r#"<p><a href="/download/attachments/123/manual.pdf?download=true">download</a></p>"#
            )
        );
    }

    #[test]
    fn attachment_link_to_a_file_name_with_a_question_mark_names_that_file() {
        let applied = apply_storage("[what?.pdf](attachments/what%3F.pdf)").expect("apply");

        assert_eq!(
            canonical(&applied),
            canonical(r#"<p><ac:link><ri:attachment ri:filename="what?.pdf" /></ac:link></p>"#)
        );
    }

    #[test]
    fn checkbox_markup_quoted_in_an_attachment_link_label_stays_text() {
        let applied = apply_storage(
            r#"[\<ul>\<li>\<input type="checkbox" /> example\</li>\</ul>](attachments/a.pdf)"#,
        )
        .expect("apply");

        assert_eq!(
            canonical(&applied),
            canonical(concat!(
                r#"<p><ac:link><ri:attachment ri:filename="a.pdf" />"#,
                r#"<ac:plain-text-link-body><![CDATA[<ul><li><input type="checkbox" /> example</li></ul>]]>"#,
                "</ac:plain-text-link-body></ac:link></p>"
            ))
        );
    }

    #[test]
    fn task_holding_a_code_block_is_still_a_task() {
        let applied = apply_storage("- [ ] run\n  ```\n  make\n  ```").expect("apply");

        assert!(applied.contains("<ac:task-list>"), "{applied}");
        assert!(!applied.contains("<input"), "{applied}");
    }

    #[test]
    fn leading_metadata_block_is_not_published() {
        for markdown in [
            "---\ninternal-id: 123\n---\n\nBody",
            "+++\ninternal-id = 123\n+++\n\nBody",
        ] {
            let applied = apply_storage(markdown).expect("apply");

            assert_eq!(canonical(&applied), canonical("<p>Body</p>"));
        }
    }

    #[test]
    fn text_between_rules_or_plus_signs_at_the_top_of_a_page_survives() {
        assert_round_trips("<hr /><p>internal-id: 123</p><hr /><p>Body</p>");
        assert_round_trips("<p>+++</p><p>internal-id = 123</p><p>+++</p>");
        assert_round_trips("<p>---<br />internal-id: 123<br />---</p>");
        assert_round_trips("<p>+++<br />internal-id = 123<br />+++</p>");
    }

    #[test]
    fn definition_list_renders_as_one() {
        let applied = apply_storage("Term\n: Definition").expect("apply");

        assert_eq!(
            canonical(&applied),
            canonical("<dl><dt>Term</dt><dd>Definition</dd></dl>")
        );
    }

    #[test]
    fn text_starting_with_a_colon_survives() {
        assert_round_trips("<p>Term</p><p>: text</p>");
        assert_round_trips("<p>Term<br />: text</p>");
        assert_round_trips("<p>Term</p><p>:</p>");
        assert_round_trips("<p>:colon</p>");
        assert_round_trips("<p>Term</p><p>:text</p>");
        assert_round_trips("<p>Term</p><p>:::</p>");
        assert_round_trips("<p>Term<br />:text</p>");
        assert_round_trips("<p>Term</p><p>:smile: done</p>");
        assert_round_trips("<ul><li><p>Term</p><p>:text</p></li></ul>");
    }

    #[test]
    fn attachment_link_whose_body_repeats_the_file_name_keeps_the_body() {
        assert_round_trips(concat!(
            r#"<p><ac:link><ri:attachment ri:filename="report.pdf" />"#,
            "<ac:plain-text-link-body><![CDATA[report.pdf]]></ac:plain-text-link-body>",
            "</ac:link></p>"
        ));
    }

    #[test]
    fn literal_angle_bracket_text_survives() {
        assert_round_trips("<p>use the &lt;medallion&gt; layer</p>");
        assert_round_trips("<p>set &lt;env&gt; and &lt;TBD&gt; and a &lt; b and x &amp; y</p>");
        assert_round_trips("<p>call &lt;foo bar&gt; then</p>");
        assert_round_trips("<p>type &lt;br&gt; or &lt;/div&gt; or &lt;!-- note --&gt; here</p>");
        assert_round_trips("<p>&lt;div&gt; opens a block</p>");
    }

    #[test]
    fn literal_entity_text_survives() {
        assert_round_trips(
            "<p>R&amp;D writes &amp;lt; and &amp;copy; and &amp;#169; literally</p>",
        );
    }

    #[test]
    fn literal_tildes_survive() {
        assert_round_trips("<p>a ~b~ c and ~~d~~ e in ~/path</p>");
    }

    #[test]
    fn text_that_looks_like_block_syntax_stays_a_paragraph() {
        for text in [
            "# not a heading",
            "###### not a heading",
            "#hashtag",
            "1. not a list",
            "12) not a list",
            "- not a list",
            "+ not a list",
            "&gt; not a quote",
            "---",
            "===",
            "1.5 is a number",
            "-5 degrees",
        ] {
            assert_round_trips(&format!("<p>{text}</p>"));
            assert_round_trips(&format!("<p>intro<br />{text}</p>"));
            assert_round_trips(&format!("<ul><li>{text}</li></ul>"));
            assert_round_trips(&format!("<ol><li>{text}</li></ol>"));
        }
    }
}
