use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use pathdiff::diff_paths;
use regex::Regex;
use walkdir::WalkDir;

use crate::markdown::{
    Frontmatter, LocalDocument, Sidecar, document_dir_name, load_document, markdown_to_storage,
    save_document, scan_local_documents, sha256_hex, storage_to_markdown,
};
use crate::model::{AttachmentState, ContentItem, ContentKind, PlanActionKind, PlanItem, SyncPlan};
use crate::provider::ConfluenceProvider;

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
    let docs = scan_local_documents(root)?;
    let mut parent_ids = BTreeMap::new();
    for doc in &docs {
        if let Some(content_id) = &doc.content_id {
            parent_ids.insert(doc.directory.clone(), content_id.clone());
        }
    }

    let mut plan = SyncPlan::default();
    for index in docs {
        let doc = load_document(&index.directory)?;
        let converted = markdown_to_storage(&doc.body_markdown, allow_lossy)?;
        let storage_hash = sha256_hex(&converted.storage);
        let current_hash = sha256_hex(doc.body_markdown.as_bytes());
        let derived_parent_id = doc
            .directory
            .parent()
            .and_then(|parent| parent_ids.get(parent))
            .cloned();

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
            if doc.sidecar.storage_hash.as_deref() != Some(storage_hash.as_str()) {
                changes.push("body");
            }
            if doc.sidecar.last_pulled_hash.as_deref() != Some(current_hash.as_str()) {
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
    let docs = scan_local_documents(root)?;
    let mut doc_dirs: Vec<_> = docs.into_iter().map(|item| item.directory).collect();
    doc_dirs.sort_by_key(|path| path.components().count());

    let mut applied = SyncPlan::default();
    for dir in doc_dirs {
        let mut doc = load_document(&dir)?;
        let space_key = doc.sidecar.space_key.clone().or_else(|| {
            doc.frontmatter
                .properties
                .get("space_key")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned)
        });
        let derived_parent_id = if let Some(parent) = doc.directory.parent() {
            let parent_sidecar = parent.join(".confluence.json");
            if parent_sidecar.exists() {
                let sidecar: Sidecar = serde_json::from_str(&fs::read_to_string(parent_sidecar)?)?;
                sidecar.content_id
            } else {
                None
            }
        } else {
            None
        };

        let converted = markdown_to_storage(&doc.body_markdown, allow_lossy)?;
        let body_storage = converted.storage;
        let storage_hash = sha256_hex(body_storage.as_bytes());
        let markdown_hash = sha256_hex(doc.body_markdown.as_bytes());
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
            let needs_update = doc.sidecar.storage_hash.as_deref() != Some(storage_hash.as_str())
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
                    path: dir.clone(),
                    details: "remote content updated".to_string(),
                });
                updated
            } else {
                remote
            }
        } else {
            let space_key = space_key.clone().ok_or_else(|| {
                anyhow!("document {} is missing space_key metadata", dir.display())
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
                path: dir.clone(),
                details: "remote content created".to_string(),
            });
            created
        };

        sync_attachments(provider, &mut doc, &content.id, delete_remote, &mut applied).await?;

        doc.sidecar.content_id = Some(content.id.clone());
        doc.sidecar.provider = Some(provider.kind());
        doc.sidecar.space_key = content.space_key.clone().or(space_key);
        doc.sidecar.remote_version = content.version;
        doc.sidecar.remote_parent_id = derived_parent_id;
        doc.sidecar.last_pulled_hash = Some(markdown_hash);
        doc.sidecar.storage_hash = Some(storage_hash);
        doc.sidecar.last_sync_at = Some(Utc::now());
        save_document(&doc)?;
    }

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
        body_markdown = rewrite_page_links(&body_markdown, dir, &paths);
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
            remote_version: item.version,
            remote_parent_id: item.parent_id.clone(),
            last_pulled_hash: Some(sha256_hex(body_markdown.as_bytes())),
            storage_hash: Some(sha256_hex(
                item.body_storage.as_deref().unwrap_or_default().as_bytes(),
            )),
            attachment_map,
            last_sync_at: Some(Utc::now()),
        };
        let doc = LocalDocument {
            directory: dir.clone(),
            markdown_path: dir.join("index.md"),
            sidecar_path: dir.join(".confluence.json"),
            frontmatter,
            body_markdown,
            sidecar,
        };
        save_document(&doc)?;
        written.push(dir.clone());
    }

    Ok(written)
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
    rewritten
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
    use crate::markdown::{Frontmatter, Sidecar, render_document};
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
}
