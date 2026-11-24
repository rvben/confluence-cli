use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind as IoErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{
    ConfluenceProvider, LocalDocument, attachment_file_name_is_safe, provider_site_fingerprint,
};
use crate::markdown::{PULL_FAILURES_DIR, document_dir_name, safe_slug};
use crate::model::ContentItem;
use crate::output::{ErrorKind, typed_error, typed_error_with_details, typed_error_with_hint};

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub(crate) enum TreeLayout {
    SlugId,
    Slug,
}

#[derive(Clone, Debug)]
pub(crate) struct TreeMove {
    page_id: String,
    parent_id: String,
}

impl std::str::FromStr for TreeMove {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (page, parent) = value.split_once('=').ok_or("expected PAGE_ID=PARENT_ID")?;
        if page.is_empty() || parent.is_empty() || parent.contains('=') {
            return Err("expected PAGE_ID=PARENT_ID");
        }
        Ok(Self {
            page_id: page.to_owned(),
            parent_id: parent.to_owned(),
        })
    }
}

pub(super) struct TreePull {
    pub(super) root_id: String,
    pub(super) layout: TreeLayout,
    pub(super) existing: Option<ExistingTree>,
}

pub(super) struct TreePathPlan {
    pub(super) paths: BTreeMap<String, PathBuf>,
    pub(super) changes: Vec<TreePathChange>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct TreePathChange {
    pub(crate) content_id: String,
    pub(crate) title: String,
    pub(crate) change: TreeChange,
    pub(crate) previous_path: Option<PathBuf>,
    pub(crate) path: Option<PathBuf>,
    pub(crate) previous_parent_id: Option<String>,
    pub(crate) parent_id: Option<String>,
}

impl TreePathChange {
    pub(crate) fn move_argument(&self) -> Option<String> {
        match (self.change, &self.parent_id) {
            (TreeChange::Move, Some(parent)) => Some(format!("{}={parent}", self.content_id)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TreeChange {
    Unchanged,
    New,
    Move,
    Relocated,
    Missing,
}

impl TreeChange {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::New => "new",
            Self::Move => "move",
            Self::Relocated => "relocated",
            Self::Missing => "missing",
        }
    }
}

impl TreePathPlan {
    pub(super) fn check_acceptance(&self, accepted: &[TreeMove]) -> Result<()> {
        if let Some(missing) = self
            .changes
            .iter()
            .find(|row| row.change == TreeChange::Missing)
        {
            return Err(refresh_conflict(format!(
                "page {} at `{}` is no longer in the remote tree",
                missing.content_id,
                missing
                    .previous_path
                    .as_deref()
                    .expect("missing page has a local path")
                    .display()
            )));
        }

        let moves: BTreeMap<_, _> = self
            .changes
            .iter()
            .filter(|row| row.change == TreeChange::Move)
            .map(|row| (row.content_id.as_str(), row))
            .collect();
        let mut remaining = moves;
        for approval in accepted {
            let id = approval.page_id.as_str();
            let Some(row) = remaining.remove(id) else {
                return Err(typed_error(
                    ErrorKind::InvalidInput,
                    format!(
                        "page {id} does not have a remote parent move in this pull or was already accepted"
                    ),
                ));
            };
            if row.parent_id.as_deref() != Some(approval.parent_id.as_str()) {
                return Err(typed_error_with_details(
                    ErrorKind::Conflict,
                    format!(
                        "page {id} has a different current parent than the accepted parent {}",
                        approval.parent_id
                    ),
                    Some(
                        "run `confluence pull preview-tree` to inspect the current move"
                            .to_string(),
                    ),
                    serde_json::json!({"move": row}),
                ));
            }
        }

        let pending: Vec<_> = remaining.into_values().collect();
        if !pending.is_empty() {
            let pages = pending
                .iter()
                .map(|row| row.content_id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(typed_error_with_details(
                ErrorKind::Conflict,
                format!("remote pages {pages} changed parent"),
                Some("run `confluence pull preview-tree` to inspect the proposed paths. Pass --accept-move PAGE_ID=PARENT_ID for each move to install the refresh".to_string()),
                serde_json::json!({"moves": pending}),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ExistingTree {
    pages: BTreeMap<String, ExistingPage>,
    failures: BTreeMap<String, FailedPagePlacement>,
}

#[derive(Debug, PartialEq, Eq)]
struct ExistingPage {
    path: PathBuf,
    title: String,
    parent_id: Option<String>,
    version: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
struct RetainedFolder {
    path: PathBuf,
    parent_id: Option<String>,
}

pub(super) const FAILED_PAGE_PLACEMENT_FILE: &str = "placement.json";

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct FailedPagePlacement {
    pub(super) content_id: String,
    pub(super) parent_id: Option<String>,
    pub(super) site_fingerprint: Option<String>,
}

fn failed_page_placements(
    root: &Path,
    site: &str,
) -> Result<BTreeMap<String, FailedPagePlacement>> {
    let directory = root.join(PULL_FAILURES_DIR);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == IoErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read {}", directory.display()));
        }
    };
    let mut placements = BTreeMap::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join(FAILED_PAGE_PLACEMENT_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == IoErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        let placement: FailedPagePlacement = serde_json::from_slice(&bytes)
            .with_context(|| format!("cannot read placement in {}", path.display()))?;
        if placement.site_fingerprint.as_deref() == Some(site) {
            let id = placement.content_id.clone();
            if placements.insert(id.clone(), placement).is_some() {
                return Err(refresh_conflict(format!(
                    "multiple pull failure placements claim page {id}"
                )));
            }
        }
    }
    Ok(placements)
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
                title: doc.frontmatter.title.clone(),
                parent_id: doc.sidecar.remote_parent_id.clone(),
                version: doc.sidecar.remote_version,
            };
            if pages.insert(id.clone(), page).is_some() {
                return Err(refresh_conflict(format!(
                    "multiple local folders claim page {id}"
                )));
            }
        }
        let failures = failed_page_placements(root, &site)?;
        Ok(Self { pages, failures })
    }

    fn retained_paths(
        &self,
        root_id: &str,
        items: &BTreeMap<String, ContentItem>,
    ) -> Result<BTreeMap<String, RetainedFolder>> {
        let mut paths: BTreeMap<String, RetainedFolder> = BTreeMap::new();
        let local_ids: BTreeMap<_, _> = self
            .pages
            .iter()
            .map(|(id, page)| (page.path.as_path(), id))
            .collect();
        for (id, local) in self.pages.iter().filter(|(id, _)| items.contains_key(*id)) {
            // A prior pull can leave out an unconvertible page but still write
            // its descendants. Their paths also reserve that ancestor's folder.
            let mut ancestor = id.clone();
            let mut path = local.path.as_path();
            loop {
                // The destination itself is only a page folder if it already
                // contains that page's document. A narrower export cannot
                // implicitly claim the destination for a different root.
                if path.as_os_str().is_empty()
                    && self
                        .pages
                        .get(&ancestor)
                        .is_none_or(|page| page.path != path)
                {
                    return Err(hierarchy_conflict(id, &local.path));
                }
                if let Some(previous) = paths.get(&ancestor) {
                    if previous.path != path {
                        return Err(hierarchy_conflict(id, &local.path));
                    }
                    break;
                }
                let parent_id = if let Some(page) = self.pages.get(&ancestor) {
                    page.parent_id.clone()
                } else if let Some(placement) = self.failures.get(&ancestor) {
                    placement.parent_id.clone()
                } else {
                    // Older partial pulls can establish the old parent if
                    // the directory directly above still has its document.
                    path.parent()
                        .and_then(|parent| local_ids.get(parent))
                        .map(|id| (*id).clone())
                };
                let is_root = ancestor == root_id;
                paths.insert(
                    ancestor,
                    RetainedFolder {
                        path: path.to_path_buf(),
                        parent_id: parent_id.clone(),
                    },
                );
                if is_root {
                    break;
                }
                if path
                    .file_name()
                    .is_some_and(|name| page_uses_name(&name.to_string_lossy()))
                {
                    return Err(hierarchy_conflict(id, &local.path));
                }
                ancestor = parent_id
                    .filter(|parent| {
                        items.contains_key(parent)
                            || self.pages.contains_key(parent)
                            || self.failures.contains_key(parent)
                    })
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
    pub(super) fn plan(&self, items: &BTreeMap<String, ContentItem>) -> Result<TreePathPlan> {
        let empty = ExistingTree::default();
        let existing = self.existing.as_ref().unwrap_or(&empty);
        let retained = existing.retained_paths(&self.root_id, items)?;
        let root_item = items
            .get(&self.root_id)
            .context("remote tree has no root page")?;
        let root_path = retained
            .get(&self.root_id)
            .map(|folder| folder.path.clone())
            .unwrap_or_else(|| {
                self.layout
                    .allocate(root_item, Path::new(""), false, &mut BTreeSet::new())
            });
        let moved = |item: &ContentItem| {
            item.id != self.root_id
                && retained
                    .get(&item.id)
                    .is_some_and(|folder| folder.parent_id != item.parent_id)
        };
        let mut children: BTreeMap<&str, Vec<&ContentItem>> = BTreeMap::new();
        for item in items.values().filter(|item| item.id != self.root_id) {
            if let Some(parent) = item.parent_id.as_deref() {
                children.entry(parent).or_default().push(item);
            }
        }
        // Reserve names from the current snapshot through this refresh so
        // moving a page and importing another cannot reuse its path together.
        let mut previous_names: BTreeMap<&str, Vec<&OsStr>> = BTreeMap::new();
        for (_, folder) in retained.iter().filter(|(id, _)| *id != &self.root_id) {
            if let Some(parent) = folder.parent_id.as_deref()
                && let Some(name) = folder.path.file_name()
            {
                previous_names.entry(parent).or_default().push(name);
            }
        }

        let mut paths = BTreeMap::from([(self.root_id.clone(), root_path)]);
        let mut pending = vec![self.root_id.as_str()];
        let mut seen: BTreeSet<_> = paths.values().cloned().collect();
        while let Some(id) = pending.pop() {
            let Some(mut siblings) = children.remove(id) else {
                continue;
            };
            let parent = paths[id].clone();
            let mut occupied: BTreeSet<_> = previous_names
                .get(id)
                .into_iter()
                .flatten()
                .map(|name| path_key(&parent.join(name)))
                .collect();
            // Reserve existing sibling names first, then allocate for incoming
            // moves and new pages. Equal priorities follow content ID order.
            siblings.sort_by_key(|child| (!retained.contains_key(&child.id), moved(child)));
            for child in siblings {
                let path = if let Some(old) = retained.get(&child.id) {
                    let name = old
                        .path
                        .file_name()
                        .context("retained child has no folder name")?;
                    let candidate = parent.join(name);
                    let available = occupied.insert(path_key(&candidate));
                    if moved(child) && !available {
                        let mut fallback = name.to_os_string();
                        fallback.push(format!("--{}", safe_slug(&child.id)));
                        unique_path(&parent, &fallback, &mut occupied)
                    } else {
                        candidate
                    }
                } else {
                    self.layout.allocate(child, &parent, true, &mut occupied)
                };
                if !seen.insert(path.clone()) {
                    return Err(refresh_conflict(format!(
                        "multiple pages map to folder `{}`",
                        path.display()
                    )));
                }
                paths.insert(child.id.clone(), path);
                pending.push(child.id.as_str());
            }
        }
        if paths.len() != items.len() {
            return Err(typed_error(
                ErrorKind::Api,
                format!(
                    "the remote tree rooted at {} contains pages that cannot be placed beneath its root",
                    self.root_id
                ),
            ));
        }
        let mut changes = Vec::with_capacity(paths.len());
        for (id, path) in &paths {
            let remote = &items[id];
            let local = retained.get(id);
            let previous_path = local.map(|folder| &folder.path);
            let change = match previous_path {
                _ if moved(remote) => TreeChange::Move,
                Some(previous) if previous != path => TreeChange::Relocated,
                Some(_) => TreeChange::Unchanged,
                None => TreeChange::New,
            };
            changes.push(TreePathChange {
                content_id: id.clone(),
                title: remote.title.clone(),
                change,
                previous_path: previous_path.cloned(),
                path: Some(path.clone()),
                previous_parent_id: local.and_then(|page| page.parent_id.clone()),
                parent_id: remote.parent_id.clone(),
            });
        }
        for (id, local) in existing
            .pages
            .iter()
            .filter(|(id, _)| !items.contains_key(*id))
        {
            changes.push(TreePathChange {
                content_id: id.clone(),
                title: local.title.clone(),
                change: TreeChange::Missing,
                previous_path: Some(local.path.clone()),
                path: None,
                previous_parent_id: local.parent_id.clone(),
                parent_id: None,
            });
        }
        Ok(TreePathPlan { paths, changes })
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
        unique_path(parent, OsStr::new(&name), occupied)
    }
}

fn unique_path(parent: &Path, name: &OsStr, occupied: &mut BTreeSet<String>) -> PathBuf {
    let mut candidate = parent.join(name);
    let mut suffix = 2;
    while !occupied.insert(path_key(&candidate)) {
        let mut numbered = name.to_os_string();
        numbered.push(format!("-{suffix}"));
        candidate = parent.join(numbered);
        suffix += 1;
    }
    candidate
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
