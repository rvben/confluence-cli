use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{
    ConfluenceProvider, LocalDocument, attachment_file_name_is_safe, provider_site_fingerprint,
};
use crate::markdown::document_dir_name;
use crate::model::ContentItem;
use crate::output::{ErrorKind, typed_error, typed_error_with_hint};

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub(crate) enum TreeLayout {
    SlugId,
    Slug,
}

pub(super) struct TreePull {
    pub(super) root_id: String,
    pub(super) layout: TreeLayout,
    pub(super) existing: Option<ExistingTree>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ExistingTree(BTreeMap<String, ExistingPage>);

#[derive(Debug, PartialEq, Eq)]
struct ExistingPage {
    path: PathBuf,
    parent_id: Option<String>,
    version: Option<u64>,
}

impl ExistingTree {
    pub(super) fn from_documents(
        root: &Path,
        docs: &[LocalDocument],
        provider: &dyn ConfluenceProvider,
    ) -> Result<Self> {
        let site = provider_site_fingerprint(provider);
        let mut pages = BTreeMap::new();
        for doc in docs {
            if doc
                .sidecar
                .site_fingerprint
                .as_ref()
                .is_some_and(|value| value != &site)
                || doc
                    .sidecar
                    .provider
                    .is_some_and(|kind| kind != provider.kind())
            {
                return Err(refresh_conflict(format!(
                    "page at `{}` belongs to a different Confluence site or provider",
                    doc.directory.display()
                )));
            }
            let id = doc
                .sidecar
                .content_id
                .as_ref()
                .context("clean tree document has no content ID")?;
            let path = doc.directory.strip_prefix(root)?.to_path_buf();
            let page = ExistingPage {
                path,
                parent_id: doc.sidecar.remote_parent_id.clone(),
                version: doc.sidecar.remote_version,
            };
            if pages.insert(id.clone(), page).is_some() {
                return Err(refresh_conflict(format!(
                    "multiple local folders claim page {id}"
                )));
            }
        }
        Ok(Self(pages))
    }

    fn retained_paths(
        &self,
        root_id: &str,
        items: &BTreeMap<String, ContentItem>,
    ) -> Result<BTreeMap<String, PathBuf>> {
        let mut paths = BTreeMap::new();
        for (id, local) in &self.0 {
            let remote = items.get(id).ok_or_else(|| {
                refresh_conflict(format!(
                    "page {id} at `{}` is no longer in the remote tree rooted at {root_id}",
                    local.path.display()
                ))
            })?;
            if id != root_id && local.parent_id != remote.parent_id {
                let before = local.parent_id.as_deref().unwrap_or("none");
                let after = remote.parent_id.as_deref().unwrap_or("none");
                return Err(refresh_conflict(format!(
                    "remote page {id} ({}) changed parent from {before} to {after}",
                    remote.title
                )));
            }
            // A prior pull can leave out an unconvertible page but still write
            // its descendants. Their paths also reserve that ancestor's folder.
            let mut ancestor = id.as_str();
            let mut path = local.path.as_path();
            loop {
                // The destination itself is only a page folder if it already
                // contains that page's document. A narrower export cannot
                // implicitly claim the destination for a different root.
                if path.as_os_str().is_empty()
                    && self.0.get(ancestor).is_none_or(|page| page.path != path)
                {
                    return Err(hierarchy_conflict(id, &local.path));
                }
                if let Some(previous) = paths.get(ancestor) {
                    if previous != path {
                        return Err(hierarchy_conflict(id, &local.path));
                    }
                    break;
                }
                paths.insert(ancestor.to_owned(), path.to_path_buf());
                if ancestor == root_id {
                    break;
                }
                if path
                    .file_name()
                    .is_some_and(|name| page_uses_name(&name.to_string_lossy()))
                {
                    return Err(hierarchy_conflict(id, &local.path));
                }
                ancestor = items
                    .get(ancestor)
                    .and_then(|item| item.parent_id.as_deref())
                    .filter(|parent| items.contains_key(*parent))
                    .ok_or_else(|| hierarchy_conflict(id, &local.path))?;
                path = path
                    .parent()
                    .ok_or_else(|| hierarchy_conflict(id, &local.path))?;
            }
        }
        Ok(paths)
    }
}

impl TreePull {
    pub(super) fn paths(
        &self,
        root: &Path,
        items: &BTreeMap<String, ContentItem>,
    ) -> Result<BTreeMap<String, PathBuf>> {
        let mut paths = match &self.existing {
            Some(existing) => existing.retained_paths(&self.root_id, items)?,
            None => BTreeMap::new(),
        };
        // Existing case variants can coexist on a case-sensitive filesystem.
        // Allocate new names without relying on that distinction.
        let mut occupied: BTreeSet<_> = paths.values().map(|path| path_key(path)).collect();

        let root_item = items
            .get(&self.root_id)
            .context("remote tree has no root page")?;
        let mut children: BTreeMap<&str, Vec<&ContentItem>> = BTreeMap::new();
        for item in items.values().filter(|item| item.id != self.root_id) {
            if let Some(parent) = item.parent_id.as_deref() {
                children.entry(parent).or_default().push(item);
            }
        }

        let mut pending = vec![(root_item, PathBuf::new())];
        let mut visited = 0;
        while let Some((item, parent)) = pending.pop() {
            let path = paths.entry(item.id.clone()).or_insert_with(|| {
                self.layout
                    .allocate(item, &parent, item.id != self.root_id, &mut occupied)
            });
            visited += 1;
            if let Some(children) = children.remove(item.id.as_str()) {
                pending.extend(
                    children
                        .into_iter()
                        .rev()
                        .map(|child| (child, path.clone())),
                );
            }
        }
        if visited != items.len() {
            return Err(typed_error(
                ErrorKind::Api,
                format!(
                    "the remote tree rooted at {} contains pages that cannot be placed beneath its root",
                    self.root_id
                ),
            ));
        }
        for path in paths.values_mut() {
            *path = root.join(&*path);
        }
        Ok(paths)
    }
}

impl TreeLayout {
    fn allocate(
        self,
        item: &ContentItem,
        parent: &Path,
        is_child: bool,
        occupied: &mut BTreeSet<String>,
    ) -> PathBuf {
        if matches!(self, Self::Slug) {
            let slug = document_dir_name(&item.title, None);
            let candidate = parent.join(&slug);
            if attachment_file_name_is_safe(&slug)
                && !(is_child && page_uses_name(&slug))
                && occupied.insert(path_key(&candidate))
            {
                return candidate;
            }
        }
        let name = document_dir_name(&item.title, Some(&item.id));
        let mut candidate = parent.join(&name);
        let mut suffix = 2;
        while !occupied.insert(path_key(&candidate)) {
            candidate = parent.join(format!("{name}-{suffix}"));
            suffix += 1;
        }
        candidate
    }
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

fn page_uses_name(name: &str) -> bool {
    ["attachments", "index.md", ".confluence.json"]
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

fn hierarchy_conflict(id: &str, path: &Path) -> anyhow::Error {
    refresh_conflict(format!(
        "local folder `{}` for page {id} does not follow the remote tree hierarchy",
        path.display()
    ))
}

fn refresh_conflict(message: String) -> anyhow::Error {
    typed_error_with_hint(
        ErrorKind::Conflict,
        message,
        "inspect the remote tree in a new destination, or use --force to replace this snapshot and regenerate its folder names",
    )
}
