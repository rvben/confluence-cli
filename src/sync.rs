use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use pathdiff::diff_paths;
use regex::Regex;
use walkdir::WalkDir;

use crate::markdown::{
    Frontmatter, LocalDocument, PageLinkPlaceholder, Sidecar, document_dir_name, load_document,
    markdown_to_storage, parse_page_placeholder_url, save_document, scan_local_documents,
    sha256_hex, storage_to_markdown,
};
use crate::model::{AttachmentState, ContentItem, ContentKind, PlanActionKind, PlanItem, SyncPlan};
use crate::provider::ConfluenceProvider;

#[derive(Clone, Debug)]
struct LinkTarget {
    markdown_path: PathBuf,
    directory: PathBuf,
    content_id: Option<String>,
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
) -> Result<Vec<PathBuf>> {
    let root_id = provider.resolve_page_ref(reference).await?;
    let root_item = provider
        .get_content(ContentKind::Page, &root_id, true)
        .await?;
    let mut items = vec![root_item];
    if recursive {
        items.extend(provider.list_children(&root_id, true).await?);
    }
    pull_items(provider, root, items).await
}

pub async fn pull_space(
    provider: &dyn ConfluenceProvider,
    space: &str,
    root: &Path,
) -> Result<Vec<PathBuf>> {
    let mut items = provider
        .list_space_content(ContentKind::Page, space, true)
        .await?;
    items.extend(
        provider
            .list_space_content(ContentKind::BlogPost, space, false)
            .await?,
    );
    pull_items(provider, root, items).await
}

pub fn plan_path(root: &Path, allow_lossy: bool, delete_remote: bool) -> Result<SyncPlan> {
    let docs = load_local_documents(root)?;
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
        let storage = render_body_storage(
            &doc,
            &link_index,
            allow_lossy,
            effective_web_path_prefix(&doc.sidecar, None),
        )?;
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
            });
        } else {
            let mut changes = Vec::new();
            if body_changed(&doc.sidecar, &current_hash, &storage_hash) {
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
                });
            }
            if !changes.is_empty() {
                plan.items.push(PlanItem {
                    action: PlanActionKind::UpdateContent,
                    title: doc.frontmatter.title.clone(),
                    content_id: doc.sidecar.content_id.clone(),
                    path: doc.directory.clone(),
                    details: format!("changed: {}", changes.join(", ")),
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
                    });
                }
            }
        }
    }

    if plan.items.is_empty() {
        plan.items.push(PlanItem {
            action: PlanActionKind::Noop,
            title: root.display().to_string(),
            content_id: None,
            path: root.to_path_buf(),
            details: "no changes detected".to_string(),
        });
    }

    Ok(plan)
}

pub async fn apply_path(
    provider: &dyn ConfluenceProvider,
    root: &Path,
    allow_lossy: bool,
    delete_remote: bool,
    force: bool,
) -> Result<SyncPlan> {
    let mut docs = load_local_documents(root)?;
    docs.sort_by_key(|doc| doc.directory.components().count());
    let mut link_index = build_link_index(&docs);
    let web_path_prefix = provider.web_path_prefix();

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
            let remote = provider.get_content(kind, &content_id, false).await?;
            if !force && doc.sidecar.remote_version != remote.version {
                bail!(
                    "remote version drift for {}: local {:?}, remote {:?}",
                    title,
                    doc.sidecar.remote_version,
                    remote.version
                );
            }
            let needs_update = body_changed(&doc.sidecar, &markdown_hash, &storage_hash)
                || doc.sidecar.remote_parent_id != derived_parent_id
                || remote.title != title
                || remote.labels != doc.frontmatter.labels
                || remote.properties != doc.frontmatter.properties;
            if needs_update {
                let version = remote
                    .version
                    .ok_or_else(|| anyhow!("remote content {content_id} has no version"))?;
                let updated = provider
                    .update_content(&crate::model::UpdateContentRequest {
                        id: content_id.clone(),
                        kind,
                        title: title.clone(),
                        parent_id: derived_parent_id.clone(),
                        body_storage: body_storage.clone(),
                        version,
                        message: Some("Updated via confluence-cli".to_string()),
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
                    title: title.clone(),
                    content_id: Some(updated.id.clone()),
                    path: doc.directory.clone(),
                    details: "remote content updated".to_string(),
                });
                updated
            } else {
                remote
            }
        } else {
            let space_key = space_key.clone().ok_or_else(|| {
                anyhow!(
                    "document {} is missing space_key metadata",
                    doc.directory.display()
                )
            })?;
            let created = provider
                .create_content(&crate::model::CreateContentRequest {
                    kind,
                    title: title.clone(),
                    space: space_key.clone(),
                    parent_id: derived_parent_id.clone(),
                    body_storage: body_storage.clone(),
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
                action: PlanActionKind::CreateContent,
                title: title.clone(),
                content_id: Some(created.id.clone()),
                path: doc.directory.clone(),
                details: "remote content created".to_string(),
            });
            created
        };

        doc.sidecar.content_id = Some(content.id.clone());
        link_index.set_content_id(&doc.directory, &content.id);
        sync_attachments(provider, doc, &content.id, delete_remote, &mut applied).await?;

        doc.sidecar.provider = Some(provider.kind());
        doc.sidecar.web_path_prefix = Some(web_path_prefix.clone());
        doc.sidecar.space_key = content.space_key.clone().or(space_key);
        doc.sidecar.remote_version = content.version;
        doc.sidecar.remote_parent_id = derived_parent_id;
        doc.sidecar.last_pulled_hash = Some(markdown_hash);
        doc.sidecar.storage_hash = Some(storage_hash.clone());
        doc.sidecar.remote_storage_hash = Some(storage_hash);
        doc.sidecar.last_sync_at = Some(Utc::now());
        save_document(&doc)?;
    }

    reconcile_local_link_updates(
        provider,
        &mut docs,
        &link_index,
        allow_lossy,
        web_path_prefix.as_str(),
        &mut applied,
    )
    .await?;

    if applied.items.is_empty() {
        applied.items.push(PlanItem {
            action: PlanActionKind::Noop,
            title: root.display().to_string(),
            content_id: None,
            path: root.to_path_buf(),
            details: "no remote changes applied".to_string(),
        });
    }

    Ok(applied)
}

async fn reconcile_local_link_updates(
    provider: &dyn ConfluenceProvider,
    docs: &mut [LocalDocument],
    link_index: &LinkIndex,
    allow_lossy: bool,
    web_path_prefix: &str,
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
        let version = remote
            .version
            .ok_or_else(|| anyhow!("remote content {content_id} has no version"))?;
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
        });
        doc.sidecar.remote_version = updated.version;
        doc.sidecar.storage_hash = Some(final_hash.clone());
        doc.sidecar.remote_storage_hash = Some(final_hash);
        doc.sidecar.last_pulled_hash = Some(markdown_body_hash(&doc.body_markdown));
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
    let mut item_map = BTreeMap::new();
    for item in items {
        item_map.insert(item.id.clone(), item);
    }

    let paths = compute_paths(root, &item_map);
    let mut docs_to_write = Vec::new();
    let mut written = Vec::new();
    for item in item_map.values() {
        let Some(dir) = paths.get(&item.id) else {
            continue;
        };
        let attachments = provider
            .list_attachments(&item.id)
            .await
            .unwrap_or_default();
        let attachments_dir = dir.join("attachments");
        fs::create_dir_all(&attachments_dir)
            .with_context(|| format!("failed to create {}", attachments_dir.display()))?;

        let mut attachment_map = BTreeMap::new();
        for attachment in &attachments {
            let attachment_path = attachments_dir.join(&attachment.title);
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
                    media_type: attachment.media_type.clone(),
                    sha256: Some(sha256_hex(&bytes)),
                },
            );
        }

        let mut body_markdown =
            storage_to_markdown(item.body_storage.as_deref().unwrap_or_default());
        body_markdown = rewrite_page_links(&body_markdown, dir, &paths, &item_map);
        body_markdown = rewrite_attachment_links(&body_markdown, dir, &attachments);

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

    let link_index = build_link_index(&docs_to_write);
    for mut doc in docs_to_write {
        let storage = render_body_storage(
            &doc,
            &link_index,
            false,
            effective_web_path_prefix(&doc.sidecar, None),
        )?;
        doc.sidecar.storage_hash = Some(sha256_hex(storage.as_bytes()));
        save_document(&doc)?;
        written.push(doc.directory.clone());
    }

    Ok(written)
}

fn load_local_documents(root: &Path) -> Result<Vec<LocalDocument>> {
    let indexes = scan_local_documents(root)?;
    indexes
        .into_iter()
        .map(|index| load_document(&index.directory))
        .collect()
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
    let converted = markdown_to_storage(&doc.body_markdown, allow_lossy)?;
    Ok(rewrite_local_links_to_remote(
        &converted.storage,
        &doc.directory,
        link_index,
        web_path_prefix.as_ref(),
    ))
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

fn rewrite_local_links_to_remote(
    storage: &str,
    current_dir: &Path,
    link_index: &LinkIndex,
    web_path_prefix: &str,
) -> String {
    let rewritten_placeholders = rewrite_placeholder_page_links_to_storage(storage);
    let link_re = Regex::new(r#"<a([^>]*?)href="([^"]+)"([^>]*)>"#).expect("valid anchor regex");
    let image_re = Regex::new(r#"<img([^>]*?)src="([^"]+)"([^>]*)>"#).expect("valid image regex");

    let rewritten =
        link_re.replace_all(&rewritten_placeholders, |captures: &regex::Captures<'_>| {
            let original = captures.get(2).map(|m| m.as_str()).unwrap_or_default();
            let href = rewrite_local_target_url(current_dir, original, link_index, web_path_prefix)
                .unwrap_or_else(|| original.to_string());
            format!(r#"<a{}href="{}"{}>"#, &captures[1], href, &captures[3])
        });

    image_re
        .replace_all(&rewritten, |captures: &regex::Captures<'_>| {
            let original = captures.get(2).map(|m| m.as_str()).unwrap_or_default();
            let src = rewrite_local_target_url(current_dir, original, link_index, web_path_prefix)
                .unwrap_or_else(|| original.to_string());
            format!(r#"<img{}src="{}"{}>"#, &captures[1], src, &captures[3])
        })
        .to_string()
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
    if !is_local_target(target) {
        return None;
    }

    let (path_part, fragment) = split_target_fragment(target);
    if path_part.is_empty() {
        return None;
    }

    let resolved = normalize_path(current_dir.join(path_part));
    let markdown_target = if resolved.file_name().and_then(|name| name.to_str()) == Some("index.md")
    {
        resolved.clone()
    } else if resolved.extension().is_none() {
        resolved.join("index.md")
    } else {
        resolved.clone()
    };

    if let Some(link) = link_index.by_markdown_path.get(&markdown_target) {
        if let Some(content_id) = &link.content_id {
            let mut url = format!(
                "{}{}",
                web_path_prefix.trim_end_matches('/'),
                format!("/pages/viewpage.action?pageId={content_id}")
            );
            if let Some(fragment) = fragment {
                url.push('#');
                url.push_str(fragment);
            }
            return Some(url);
        }
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
        "{}{}",
        web_path_prefix.trim_end_matches('/'),
        format!(
            "/download/attachments/{content_id}/{}",
            urlencoding::encode(file_name)
        )
    );
    if let Some(fragment) = fragment {
        url.push('#');
        url.push_str(fragment);
    }
    Some(url)
}

fn is_local_target(target: &str) -> bool {
    !(target.is_empty()
        || target.starts_with('#')
        || target.starts_with('/')
        || target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
        || target.starts_with("tel:")
        || target.starts_with("data:"))
}

fn split_target_fragment(target: &str) -> (&str, Option<&str>) {
    if let Some((path, fragment)) = target.split_once('#') {
        (path, Some(fragment))
    } else {
        (target, None)
    }
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
    current_dir: &Path,
    attachments: &[crate::model::AttachmentInfo],
) -> String {
    let mut rewritten = markdown.to_string();
    for attachment in attachments {
        let target = diff_paths(
            current_dir.join("attachments").join(&attachment.title),
            current_dir,
        )
        .unwrap_or_else(|| PathBuf::from("attachments").join(&attachment.title));
        let replacement = target.to_string_lossy().replace('\\', "/");
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

fn rewrite_placeholder_page_links_to_storage(storage: &str) -> String {
    let re = Regex::new(r#"(?s)<a([^>]*?)href="(confluence-page://page[^"]+)"([^>]*)>(.*?)</a>"#)
        .expect("valid placeholder link regex");
    re.replace_all(storage, |captures: &regex::Captures<'_>| {
        let target = captures.get(2).map(|m| m.as_str()).unwrap_or_default();
        let body = captures.get(4).map(|m| m.as_str()).unwrap_or_default();
        placeholder_to_storage_macro(target, body).unwrap_or_else(|| captures[0].to_string())
    })
    .to_string()
}

fn placeholder_to_storage_macro(target: &str, body_html: &str) -> Option<String> {
    let placeholder = parse_page_placeholder_url(target)?;
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
    } else if let Some(content_id) = placeholder.content_id.as_deref() {
        format!(
            r#"<ri:page ri:content-id="{}" />"#,
            escape_xml_attr(content_id)
        )
    } else {
        return None;
    };
    let body = build_storage_link_body(body_html);
    Some(format!(
        r#"<ac:link{anchor_attr}>{resource}{body}</ac:link>"#
    ))
}

fn build_storage_link_body(body_html: &str) -> String {
    let trimmed = body_html.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.contains('<') {
        format!("<ac:link-body>{trimmed}</ac:link-body>")
    } else {
        format!("<ac:plain-text-link-body><![CDATA[{trimmed}]]></ac:plain-text-link-body>")
    }
}

fn placeholder_to_remote_url(link: &PageLinkPlaceholder, web_path_prefix: &str) -> Option<String> {
    let content_id = link.content_id.as_deref()?;
    let mut url = format!(
        "{}{}",
        web_path_prefix.trim_end_matches('/'),
        format!("/pages/viewpage.action?pageId={content_id}")
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
            continue;
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
    let known = doc.sidecar.attachment_map.clone();

    for (file_name, hash) in &local_hashes {
        let current = known
            .get(file_name)
            .and_then(|attachment| attachment.sha256.as_deref());
        if current != Some(hash.as_str()) {
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
            });
        }
    }

    if delete_remote {
        let local_names: BTreeSet<_> = local_hashes.keys().cloned().collect();
        for (file_name, attachment) in known {
            if !local_names.contains(&file_name) {
                provider
                    .delete_attachment(content_id, &attachment.id)
                    .await?;
                applied.items.push(PlanItem {
                    action: PlanActionKind::DeleteAttachment,
                    title: file_name,
                    content_id: Some(attachment.id),
                    path: attachments_dir.clone(),
                    details: "attachment deleted remotely".to_string(),
                });
            }
        }
    }

    let refreshed = provider.list_attachments(content_id).await?;
    let mut attachment_map = BTreeMap::new();
    for attachment in refreshed {
        let path = attachments_dir.join(&attachment.title);
        let sha = if path.exists() {
            Some(sha256_hex(fs::read(&path)?))
        } else {
            None
        };
        attachment_map.insert(
            attachment.title.clone(),
            AttachmentState {
                id: attachment.id,
                file_name: attachment.title,
                media_type: attachment.media_type,
                sha256: sha,
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
        Frontmatter, PageLinkPlaceholder, Sidecar, build_page_placeholder_url, render_document,
    };
    use crate::model::ProviderKind;

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

        let plan = plan_path(dir.path(), false, false).expect("plan");
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

        let plan = plan_path(dir.path(), false, false).expect("plan");
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

        let plan = plan_path(dir.path(), false, false).expect("plan");
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
                "[Sibling](../sibling-page--456/index.md#intro)\n\n![Logo](attachments/logo.png)\n"
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
        assert!(storage.contains(r#"src="/wiki/download/attachments/123/logo.png""#));
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

        let index = build_link_index(&[current.clone()]);
        let storage =
            render_body_storage(&current, &index, false, "/wiki").expect("render body storage");

        assert!(storage.contains(r#"<ac:link ac:anchor="intro">"#));
        assert!(storage.contains(r#"ri:content-title="Docs Home""#));
        assert!(storage.contains(r#"ri:space-key="MFS""#));
        assert!(
            storage.contains("<ac:plain-text-link-body><![CDATA[Docs]]></ac:plain-text-link-body>")
        );
    }
}
