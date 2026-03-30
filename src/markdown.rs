use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use pulldown_cmark::{Options, Parser, html};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use slug::slugify;
use walkdir::WalkDir;

use crate::config::ensure_parent_dir;
use crate::model::{AttachmentState, ContentKind, LocalContentIndex, ProviderKind};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Frontmatter {
    pub title: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub status: String,
    pub parent: Option<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, Value>,
}

impl Frontmatter {
    pub fn from_kind(kind: ContentKind, title: String) -> Self {
        Self {
            title,
            kind: kind.file_type().to_string(),
            labels: Vec::new(),
            status: "current".to_string(),
            parent: None,
            properties: BTreeMap::new(),
        }
    }

    pub fn content_kind(&self) -> ContentKind {
        match self.kind.as_str() {
            "blog" | "blogpost" => ContentKind::BlogPost,
            _ => ContentKind::Page,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Sidecar {
    pub content_id: Option<String>,
    pub space_key: Option<String>,
    pub provider: Option<ProviderKind>,
    pub remote_version: Option<u64>,
    pub remote_parent_id: Option<String>,
    pub last_pulled_hash: Option<String>,
    pub storage_hash: Option<String>,
    #[serde(default)]
    pub attachment_map: BTreeMap<String, AttachmentState>,
    pub last_sync_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct LocalDocument {
    pub directory: PathBuf,
    pub markdown_path: PathBuf,
    pub sidecar_path: PathBuf,
    pub frontmatter: Frontmatter,
    pub body_markdown: String,
    pub sidecar: Sidecar,
}

#[derive(Debug, Clone, Default)]
pub struct ConversionOutput {
    pub storage: String,
    pub lossy: Vec<String>,
}

pub fn storage_to_markdown(storage: &str) -> String {
    let mut xml_fragments = Vec::new();
    let mut normalized = storage.to_string();
    let contains_confluence_xml = storage.contains("<ac:") || storage.contains("<ri:");

    if contains_confluence_xml {
        return format!("```confluence-storage\n{}\n```", storage.trim());
    }

    for pattern in [
        r"(?s)<ac:[\w-]+(?:\s[^>]*)?>.*?</ac:[\w-]+>",
        r"(?s)<ri:[\w-]+(?:\s[^>]*)?/>",
        r"(?s)<ri:[\w-]+(?:\s[^>]*)?>.*?</ri:[\w-]+>",
    ] {
        let re = Regex::new(pattern).expect("valid regex");
        normalized = re
            .replace_all(&normalized, |captures: &regex::Captures<'_>| {
                let idx = xml_fragments.len();
                xml_fragments.push(captures[0].to_string());
                format!("CONFLUENCE_XML_PLACEHOLDER_{idx}")
            })
            .to_string();
    }

    let mut markdown = html2md::parse_html(&normalized);
    for (idx, fragment) in xml_fragments.into_iter().enumerate() {
        let block = format!("```confluence-storage\n{fragment}\n```");
        markdown = markdown.replace(&format!("CONFLUENCE_XML_PLACEHOLDER_{idx}"), &block);
    }
    markdown.trim().to_string()
}

pub fn markdown_to_storage(markdown: &str, allow_lossy: bool) -> Result<ConversionOutput> {
    let block_re = Regex::new(r"(?s)```confluence-storage\s*\n(.*?)\n```")?;
    let mut raw_fragments = Vec::new();
    let normalized = block_re
        .replace_all(markdown, |captures: &regex::Captures<'_>| {
            let idx = raw_fragments.len();
            raw_fragments.push(captures[1].to_string());
            format!("CONFLUENCE_XML_PLACEHOLDER_{idx}")
        })
        .to_string();

    let parser = Parser::new_ext(&normalized, Options::all());
    let mut html_output = String::new();
    html::push_html(&mut html_output, parser);
    for (idx, fragment) in raw_fragments.into_iter().enumerate() {
        html_output = html_output.replace(
            &format!("<p>CONFLUENCE_XML_PLACEHOLDER_{idx}</p>"),
            &fragment,
        );
        html_output = html_output.replace(&format!("CONFLUENCE_XML_PLACEHOLDER_{idx}"), &fragment);
    }

    let mut lossy = Vec::new();
    if html_output.contains("&lt;ac:") || html_output.contains("&lt;ri:") {
        lossy.push("raw Confluence XML was escaped during Markdown conversion".to_string());
    }
    if !allow_lossy && !lossy.is_empty() {
        bail!(lossy.join("; "));
    }

    Ok(ConversionOutput {
        storage: html_output.trim().to_string(),
        lossy,
    })
}

pub fn split_frontmatter(content: &str) -> Result<(Frontmatter, String)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---\n") {
        bail!("missing YAML frontmatter");
    }
    let mut parts = trimmed.splitn(3, "---\n");
    let _ = parts.next();
    let fm = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing frontmatter body"))?;
    let body = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing markdown body"))?;
    let frontmatter: Frontmatter = serde_yaml::from_str(fm)?;
    Ok((frontmatter, body.trim_start().to_string()))
}

pub fn render_document(frontmatter: &Frontmatter, body_markdown: &str) -> Result<String> {
    Ok(format!(
        "---\n{}---\n\n{}",
        serde_yaml::to_string(frontmatter)?,
        body_markdown.trim()
    ))
}

pub fn sha256_hex(content: impl AsRef<[u8]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

pub fn safe_slug(value: &str) -> String {
    let slugged = slugify(value);
    if slugged.is_empty() {
        "untitled".to_string()
    } else {
        slugged
    }
}

pub fn document_dir_name(title: &str, content_id: Option<&str>) -> String {
    match content_id {
        Some(content_id) => format!("{}--{}", safe_slug(title), content_id),
        None => safe_slug(title),
    }
}

pub fn load_document(dir: &Path) -> Result<LocalDocument> {
    let markdown_path = dir.join("index.md");
    let sidecar_path = dir.join(".confluence.json");
    let markdown = fs::read_to_string(&markdown_path)
        .with_context(|| format!("failed to read {}", markdown_path.display()))?;
    let (frontmatter, body_markdown) = split_frontmatter(&markdown)?;
    let sidecar = if sidecar_path.exists() {
        let raw = fs::read_to_string(&sidecar_path)
            .with_context(|| format!("failed to read {}", sidecar_path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", sidecar_path.display()))?
    } else {
        Sidecar::default()
    };
    Ok(LocalDocument {
        directory: dir.to_path_buf(),
        markdown_path,
        sidecar_path,
        frontmatter,
        body_markdown,
        sidecar,
    })
}

pub fn save_document(doc: &LocalDocument) -> Result<()> {
    ensure_parent_dir(&doc.markdown_path)?;
    ensure_parent_dir(&doc.sidecar_path)?;
    fs::create_dir_all(&doc.directory)
        .with_context(|| format!("failed to create {}", doc.directory.display()))?;
    fs::write(
        &doc.markdown_path,
        render_document(&doc.frontmatter, &doc.body_markdown)?,
    )
    .with_context(|| format!("failed to write {}", doc.markdown_path.display()))?;
    fs::write(
        &doc.sidecar_path,
        serde_json::to_string_pretty(&doc.sidecar)?,
    )
    .with_context(|| format!("failed to write {}", doc.sidecar_path.display()))?;
    Ok(())
}

pub fn scan_local_documents(root: &Path) -> Result<Vec<LocalContentIndex>> {
    let mut docs = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() || entry.file_name() != "index.md" {
            continue;
        }
        let directory = entry
            .path()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("index.md without parent directory"))?
            .to_path_buf();
        let document = load_document(&directory)?;
        docs.push(LocalContentIndex {
            directory: directory.clone(),
            markdown_path: directory.join("index.md"),
            sidecar_path: directory.join(".confluence.json"),
            title: document.frontmatter.title.clone(),
            kind: document.frontmatter.content_kind(),
            parent_directory: directory.parent().map(|path| path.to_path_buf()),
            content_id: document.sidecar.content_id.clone(),
        });
    }
    docs.sort_by(|a, b| a.directory.cmp(&b.directory));
    Ok(docs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_macro_round_trips_through_sentinel_block() {
        let storage = r#"<p>Hello</p><ac:structured-macro ac:name="info"><ac:rich-text-body><p>world</p></ac:rich-text-body></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains("```confluence-storage"));
        let rendered = markdown_to_storage(&markdown, false).expect("conversion succeeds");
        assert!(rendered.storage.contains("<ac:structured-macro"));
    }

    #[test]
    fn frontmatter_round_trip() {
        let doc = Frontmatter {
            title: "Docs".to_string(),
            kind: "page".to_string(),
            labels: vec!["docs".to_string()],
            status: "current".to_string(),
            parent: Some("root".to_string()),
            properties: BTreeMap::new(),
        };
        let rendered = render_document(&doc, "# Hello").expect("rendered");
        let (parsed, body) = split_frontmatter(&rendered).expect("parsed");
        assert_eq!(parsed.title, "Docs");
        assert_eq!(body, "# Hello");
    }
}
