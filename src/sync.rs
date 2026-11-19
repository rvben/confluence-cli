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
    Frontmatter, LocalDocument, PageLinkPlaceholder, Sidecar, UserMentionPlaceholder,
    build_page_placeholder_url, build_storage_link_body, decode_character_references,
    document_dir_name, encode_target_segment, has_generated_label, is_local_target,
    link_body_unless_default, load_document, markdown_to_storage, parse_page_placeholder_url,
    parse_status_placeholder_url, parse_user_placeholder_url, rewrite_outside_cdata,
    rewrite_outside_literal_sections, save_document, scan_local_documents, sha256_hex,
    split_target_fragment, storage_to_markdown,
};
use crate::model::{AttachmentState, ContentItem, ContentKind, PlanActionKind, PlanItem, SyncPlan};
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

pub async fn pull_page(
    provider: &dyn ConfluenceProvider,
    reference: &str,
    root: &Path,
    recursive: bool,
    force: bool,
) -> Result<Vec<PathBuf>> {
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
) -> Result<Vec<PathBuf>> {
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
) -> Result<Vec<PathBuf>> {
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
                plan.items.push(PlanItem {
                    action: PlanActionKind::UpdateContent,
                    title: doc.frontmatter.title.clone(),
                    content_id: doc.sidecar.content_id.clone(),
                    path: doc.directory.clone(),
                    details: format!("changed: {}", changes.join(", ")),
                    diff,
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
) -> Result<Vec<PathBuf>> {
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
    let staged_paths = write_items(provider, &staged_root, items).await?;
    install_staged_snapshot(&staged_root, root)?;

    staged_paths
        .into_iter()
        .map(|path| {
            let relative = path.strip_prefix(&staged_root).with_context(|| {
                format!(
                    "staged path {} escaped snapshot {}",
                    path.display(),
                    staged_root.display()
                )
            })?;
            Ok(root.join(relative))
        })
        .collect()
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

    let docs = load_local_documents(root)?;
    if docs.is_empty() {
        if fs::read_dir(root)?.next().is_none() {
            return Ok(());
        }
        return Err(dirty_pull_destination_error(root));
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
        if entry.path() == root {
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
) -> Result<Vec<PathBuf>> {
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
    let mut docs_to_write = Vec::new();
    let mut written = Vec::new();
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

        let mut body_markdown =
            storage_to_markdown(item.body_storage.as_deref().unwrap_or_default());
        body_markdown = rewrite_page_links(&body_markdown, dir, &paths, &item_map);
        body_markdown =
            rewrite_generic_macro_page_placeholders(&body_markdown, dir, &paths, &item_map);
        body_markdown = rewrite_attachment_links(&body_markdown, &attachments);

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
            provider: Some(provider.kind()),
            web_path_prefix: Some(provider.web_path_prefix()),
            remote_version: item.version,
            remote_parent_id: item.parent_id.clone(),
            last_pulled_hash: Some(markdown_body_hash(&body_markdown)),
            storage_hash: None,
            remote_storage_hash: Some(sha256_hex(
                item.body_storage.as_deref().unwrap_or_default().as_bytes(),
            )),
            attachment_map,
            last_sync_at: Some(Utc::now()),
            last_pulled_title: Some(item.title.clone()),
            last_pulled_labels: item.labels.clone(),
            last_pulled_status: Some(item.status.clone()),
            last_pulled_properties_hash: Some(props_hash),
            last_pulled_parent: Some(frontmatter.parent.clone()),
            last_pulled_body_markdown: Some(body_markdown.clone()),
        };
        docs_to_write.push(LocalDocument {
            directory: dir.clone(),
            markdown_path: dir.join("index.md"),
            sidecar_path: dir.join(".confluence.json"),
            frontmatter,
            body_markdown,
            sidecar,
        });
    }

    for doc in stamp_storage_hashes(docs_to_write)? {
        save_document(&doc)?;
        written.push(doc.directory.clone());
    }

    Ok(written)
}

/// Records on each pulled document the hash of the storage its Markdown
/// converts back to. Every document is converted before any is returned, so
/// a failure reports all affected pages rather than only the first.
fn stamp_storage_hashes(docs: Vec<LocalDocument>) -> Result<Vec<LocalDocument>> {
    let link_index = build_link_index(&docs);
    let total = docs.len();
    let mut stamped = Vec::with_capacity(total);
    let mut failures = Vec::new();
    for mut doc in docs {
        match render_body_storage(
            &doc,
            &link_index,
            false,
            effective_web_path_prefix(&doc.sidecar, None),
        ) {
            Ok(storage) => {
                doc.sidecar.storage_hash = Some(sha256_hex(storage.as_bytes()));
                stamped.push(doc);
            }
            Err(error) => failures.push(serde_json::json!({
                "content_id": doc.sidecar.content_id,
                "title": doc.frontmatter.title,
                "error": format!("{error:#}"),
            })),
        }
    }
    if failures.is_empty() {
        return Ok(stamped);
    }

    let summary = failures
        .iter()
        .map(|failure| {
            format!(
                "{} ({}): {}",
                failure["content_id"].as_str().unwrap_or("unknown ID"),
                failure["title"].as_str().unwrap_or_default(),
                failure["error"].as_str().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Err(crate::output::typed_error_with_details(
        crate::output::ErrorKind::Unexpected,
        format!(
            "{} of {total} pulled pages do not convert back to valid storage: {summary}",
            failures.len()
        ),
        Some(
            "Nothing was written. Pull the remaining pages individually, and report the listed pages as a conversion bug."
                .to_string(),
        ),
        serde_json::json!({ "operation": "pull", "pages": failures }),
    ))
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

fn build_link_index(docs: &[LocalDocument]) -> LinkIndex {
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

fn render_body_storage(
    doc: &LocalDocument,
    link_index: &LinkIndex,
    allow_lossy: bool,
    web_path_prefix: impl AsRef<str>,
) -> Result<String> {
    let rewritten_markdown =
        rewrite_macro_page_references(&doc.body_markdown, &doc.directory, link_index);
    let converted = markdown_to_storage(&rewritten_markdown, allow_lossy)?;
    let storage = rewrite_local_links_to_remote(
        &converted.storage,
        &doc.directory,
        link_index,
        web_path_prefix.as_ref(),
    );
    crate::storage::validate_generated_storage(&storage)?;
    Ok(storage)
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

    let storage = rewrite_outside_cdata(storage, rewrite_placeholder_links_to_storage);
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
    let mut rewritten = markdown.to_string();
    for (content_id, target_dir) in paths {
        let target_index = target_dir.join("index.md");
        let relative = diff_paths(&target_index, current_dir).unwrap_or(target_index);
        let replacement = relative.to_string_lossy().replace('\\', "/");
        let patterns = [
            format!(r#"https?://[^\s\)]+/pages/{content_id}(?:/[^\s\)]*)?"#),
            format!(r#"/pages/{content_id}(?:/[^\s\)]*)?"#),
            format!(r#"viewpage\.action\?pageId={content_id}"#),
        ];
        for pattern in patterns {
            let re = Regex::new(&pattern).expect("valid page link regex");
            rewritten = re.replace_all(&rewritten, replacement.as_str()).to_string();
        }
    }
    let placeholder_re =
        Regex::new(r#"confluence-page://page[^\s\)"]+"#).expect("valid page placeholder regex");
    placeholder_re
        .replace_all(&rewritten, |captures: &regex::Captures<'_>| {
            let target = captures.get(0).map(|m| m.as_str()).unwrap_or_default();
            // A link pulled without a body keeps its placeholder, which is
            // what lets apply write it without a body again.
            if has_generated_label(target) {
                return target.to_string();
            }
            resolve_placeholder_to_local_path(target, current_dir, paths, items)
                .unwrap_or_else(|| target.to_string())
        })
        .to_string()
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
        if let Some(url) = &attachment.download_url {
            rewritten = rewritten.replace(url, &replacement);
        }
        let escaped = regex::escape(&attachment.title);
        let re = Regex::new(&format!(r#"download/attachments/[^\s\)]+/{escaped}"#))
            .expect("valid attachment regex");
        rewritten = re.replace_all(&rewritten, replacement.as_str()).to_string();
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

fn rewrite_placeholder_links_to_storage(storage: &str) -> String {
    static LINK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?s)<a([^>]*?)href="(confluence-(?:page|user|status)://[^"]+)"([^>]*)>(.*?)</a>"#,
        )
        .expect("valid placeholder link regex")
    });
    LINK.replace_all(storage, |captures: &regex::Captures<'_>| {
        let target = captures.get(2).map(|m| m.as_str()).unwrap_or_default();
        let body = captures.get(4).map(|m| m.as_str()).unwrap_or_default();
        placeholder_to_storage_macro(target, body).unwrap_or_else(|| captures[0].to_string())
    })
    .to_string()
}

fn placeholder_to_storage_macro(target: &str, body_html: &str) -> Option<String> {
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
            r#"<ac:link{anchor_attr}>{resource}{body}</ac:link>"#
        ));
    }

    if let Some(placeholder) = parse_user_placeholder_url(target) {
        let resource = user_placeholder_to_storage_resource(&placeholder)?;
        let body = placeholder_link_body(target, body_html, &placeholder.default_label());
        return Some(format!(r#"<ac:link>{resource}{body}</ac:link>"#));
    }

    status_placeholder_to_storage_macro(target, body_html)
}

fn status_placeholder_to_storage_macro(target: &str, body_html: &str) -> Option<String> {
    let placeholder = parse_status_placeholder_url(target)?;
    let title = if !body_html.trim().is_empty() && !body_html.contains('<') {
        body_html.trim().to_string()
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

    #[test]
    fn pull_names_every_page_it_cannot_convert() {
        let docs = vec![
            pulled_document("101", "Fine", "plain text"),
            pulled_document("202", "Broken one", "an <unclosed> tag"),
            pulled_document("303", "Broken two", "another <open attr> tag"),
        ];

        let error = stamp_storage_hashes(docs).expect_err("unconvertible pages fail the pull");
        let error = error
            .downcast::<crate::output::CliError>()
            .expect("typed error");

        assert!(error.message.contains("2 of 3"), "{}", error.message);
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
        let pages = error.details.expect("details")["pages"].clone();
        assert_eq!(pages[0]["content_id"], "202");
        assert_eq!(pages[1]["title"], "Broken two");
        assert!(
            pages[0]["error"]
                .as_str()
                .is_some_and(|message| message.contains("unclosed"))
        );
    }

    #[test]
    fn pull_records_the_storage_hash_of_every_convertible_page() {
        let docs = vec![pulled_document("101", "Fine", "plain text")];
        let stamped = stamp_storage_hashes(docs).expect("convertible pages succeed");
        assert_eq!(
            stamped[0].sidecar.storage_hash.as_deref(),
            Some(sha256_hex("<p>plain text</p>".as_bytes()).as_str())
        );
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
