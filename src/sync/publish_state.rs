use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tempfile::Builder;

use super::{ConfluenceProvider, ContentItem, LocalDocument, Sidecar, provider_site_fingerprint};

#[derive(Deserialize, Serialize)]
pub(super) struct PublishState {
    format: u32,
    site: String,
    pub(super) pending: bool,
    pages: BTreeMap<PathBuf, PublishedPage>,
}

#[derive(Deserialize, Serialize)]
struct PublishedPage {
    sidecar: Sidecar,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bootstrap_space: Option<serde_json::Value>,
}

impl PublishState {
    pub(super) fn initialize(path: &Path, site: String, docs: &[LocalDocument]) -> Result<Self> {
        if path.symlink_metadata().is_ok() {
            anyhow::bail!("publish state {} already exists", path.display());
        }
        let state = Self {
            format: 1,
            site,
            pending: false,
            pages: BTreeMap::new(),
        };
        state.validate_checkout(docs)?;
        Ok(state)
    }

    fn read(path: &Path) -> Result<Self> {
        let state: Self = serde_json::from_reader(fs::File::open(path).with_context(|| {
            format!(
                "cannot open publish state {}. Restore the last publish state before proceeding",
                path.display()
            )
        })?)
        .with_context(|| format!("cannot read publish state {}", path.display()))?;
        if state.format != 1 {
            anyhow::bail!("unsupported publish state format {}", state.format);
        }
        Ok(state)
    }

    pub(super) fn read_for_apply(path: &Path) -> Result<Self> {
        Self::read(path)?.require_complete(path)
    }

    pub(super) fn read_for_preview(path: &Path) -> Result<Self> {
        let state = Self::read(path)?;
        if !state.pending {
            return Ok(state);
        }

        // A preview can use a read-only copy of a completed state file.
        // Consult the lock only when the snapshot records a pending publish.
        let lock = match fs::File::open(lock_path(path)) {
            Ok(lock) => lock,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return state.require_complete(path);
            }
            Err(error) => return Err(error.into()),
        };
        match lock.try_lock_shared() {
            Ok(()) => Self::read(path)?.require_complete(path),
            Err(std::fs::TryLockError::WouldBlock) => Err(crate::output::typed_error_with_hint(
                crate::output::ErrorKind::Conflict,
                format!("publish is in progress for {}", path.display()),
                "rerun the preview after the active apply finishes",
            )),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }

    fn require_complete(self, path: &Path) -> Result<Self> {
        if self.pending {
            return Err(crate::output::typed_error_with_hint(
                crate::output::ErrorKind::Conflict,
                format!(
                    "publish state {} records an unfinished apply",
                    path.display()
                ),
                "reconcile the remote pages and local source, then initialize a new state file at a different path",
            ));
        }
        Ok(self)
    }

    pub(super) fn write(&self, path: &Path) -> Result<()> {
        let parent = path.parent().expect("absolute state file path");
        let mut file = Builder::new()
            .prefix(".confluence-state-")
            .tempfile_in(parent)?;
        serde_json::to_writer(file.as_file_mut(), self)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(path)
            .map_err(|error| error.error)
            .with_context(|| format!("failed to install publish state {}", path.display()))?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    pub(super) fn restore(
        &self,
        root: &Path,
        docs: &mut [LocalDocument],
        provider: Option<&dyn ConfluenceProvider>,
    ) -> Result<()> {
        if provider.is_some_and(|provider| self.site != provider_site_fingerprint(provider)) {
            anyhow::bail!("publish state belongs to a different Confluence site");
        }
        self.validate_checkout(docs)?;
        let mut remaining: BTreeSet<_> = self.pages.keys().map(PathBuf::as_path).collect();
        for doc in &mut *docs {
            let key = publish_page_key(root, &doc.markdown_path);
            if let Some(page) = self.pages.get(key) {
                remaining.remove(key);
                if doc.sidecar.content_id.is_some()
                    && doc.sidecar.content_id != page.sidecar.content_id
                {
                    anyhow::bail!(
                        "checkout and publish state disagree on page ID for {}",
                        key.display()
                    );
                }
                if let Some(expected) = &page.bootstrap_space
                    && let Some(actual) = doc.frontmatter.properties.remove("space_key")
                    && &actual != expected
                {
                    anyhow::bail!(
                        "cannot change the bootstrap space_key for {}",
                        key.display()
                    );
                }
                doc.sidecar = page.sidecar.clone();
            }
        }
        if let Some(missing) = remaining.first() {
            anyhow::bail!(
                "publish state refers to missing document {}. Use the complete publishing tree and reconcile path changes before publishing",
                missing.display()
            );
        }
        self.validate_checkout(docs)
    }

    fn validate_checkout(&self, docs: &[LocalDocument]) -> Result<()> {
        let mut ids = BTreeSet::new();
        for doc in docs {
            if doc
                .sidecar
                .site_fingerprint
                .as_ref()
                .is_some_and(|site| *site != self.site)
            {
                anyhow::bail!(
                    "{} belongs to a different Confluence site",
                    doc.markdown_path.display()
                );
            }
            if let Some(id) = &doc.sidecar.content_id
                && !ids.insert(id)
            {
                anyhow::bail!("publish tree contains multiple documents for Confluence page {id}");
            }
        }
        Ok(())
    }

    pub(super) fn capture(&mut self, root: &Path, docs: &[LocalDocument]) {
        for doc in docs {
            let key = publish_page_key(root, &doc.markdown_path);
            let bootstrap_space = if doc.sidecar.content_id.is_none() {
                doc.frontmatter.properties.get("space_key").cloned()
            } else {
                self.pages
                    .get(key)
                    .and_then(|page| page.bootstrap_space.clone())
            };
            self.pages.insert(
                key.to_path_buf(),
                PublishedPage {
                    sidecar: doc.sidecar.clone(),
                    bootstrap_space,
                },
            );
        }
    }

    pub(super) fn record_creation(
        &mut self,
        root: &Path,
        path: &Path,
        doc: &LocalDocument,
        created: &ContentItem,
    ) -> Result<()> {
        let key = publish_page_key(root, &doc.markdown_path);
        let page = self
            .pages
            .get_mut(key)
            .context("new page missing from pending state")?;
        page.sidecar.content_id = Some(created.id.clone());
        page.sidecar.remote_version = created.version;
        self.write(path)
    }
}

fn publish_page_key<'a>(root: &Path, markdown: &'a Path) -> &'a Path {
    markdown
        .strip_prefix(root)
        .expect("document scanned below publish root")
}

pub(super) fn state_path(root: &Path, path: &Path) -> Result<PathBuf> {
    let name = path.file_name().context("publish state must name a file")?;
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("lock"))
    {
        anyhow::bail!("the .lock extension is reserved for publish state locks");
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let directory = parent
        .canonicalize()
        .with_context(|| format!("cannot open publish state directory {}", parent.display()))?;
    if directory.starts_with(root.canonicalize()?) {
        anyhow::bail!("publish state must be outside the Markdown tree");
    }
    let path = directory.join(name);
    match path.symlink_metadata() {
        Ok(metadata) if !metadata.is_file() => {
            anyhow::bail!("publish state {} must be a regular file", path.display())
        }
        Err(error) if error.kind() != ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    Ok(path)
}

fn lock_path(path: &Path) -> PathBuf {
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    lock_path.into()
}

pub(super) fn lock_state(path: &Path) -> Result<fs::File> {
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path(path))?;
    lock.try_lock()
        .with_context(|| format!("cannot lock publish state {}", path.display()))?;
    Ok(lock)
}
