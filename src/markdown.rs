use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use pulldown_cmark::{Options, Parser, html};
use regex::Regex;
use roxmltree::{Document, Node, NodeType};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use slug::slugify;
use url::Url;
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
    pub fn content_kind(&self) -> ContentKind {
        match self.kind.as_str() {
            "blog" | "blogpost" => ContentKind::BlogPost,
            _ => ContentKind::Page,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Sidecar {
    pub content_id: Option<String>,
    pub space_key: Option<String>,
    pub provider: Option<ProviderKind>,
    pub web_path_prefix: Option<String>,
    pub remote_version: Option<u64>,
    pub remote_parent_id: Option<String>,
    pub last_pulled_hash: Option<String>,
    pub storage_hash: Option<String>,
    pub remote_storage_hash: Option<String>,
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

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct ConversionOutput {
    pub storage: String,
    pub lossy: Vec<String>,
}

pub fn storage_to_markdown(storage: &str) -> String {
    let trimmed = storage.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    storage_to_markdown_xml(trimmed).unwrap_or_else(|| storage_to_markdown_fallback(trimmed))
}

const AC_NS: &str = "urn:confluence-ac";
const RI_NS: &str = "urn:confluence-ri";
pub(crate) const CONFLUENCE_PAGE_SCHEME: &str = "confluence-page";
pub(crate) const CONFLUENCE_USER_SCHEME: &str = "confluence-user";
pub(crate) const CONFLUENCE_STATUS_SCHEME: &str = "confluence-status";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PageLinkPlaceholder {
    pub content_id: Option<String>,
    pub space_key: Option<String>,
    pub content_title: Option<String>,
    pub anchor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct UserMentionPlaceholder {
    pub account_id: Option<String>,
    pub user_key: Option<String>,
    pub username: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct StatusMacroPlaceholder {
    pub title: String,
    pub colour: Option<String>,
}

fn storage_to_markdown_xml(storage: &str) -> Option<String> {
    let wrapped = wrap_storage_fragment(storage);
    let document = Document::parse(&wrapped).ok()?;
    let mut blocks = Vec::new();
    for child in document.root_element().children() {
        if let Some(block) = render_top_level_block(child, &wrapped) {
            if !block.trim().is_empty() {
                blocks.push(block.trim().to_string());
            }
        }
    }
    Some(blocks.join("\n\n"))
}

fn storage_to_markdown_fallback(storage: &str) -> String {
    let mut xml_fragments = Vec::new();
    let mut normalized = storage.to_string();

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
    let (normalized_layouts, layout_fragments) = replace_layout_blocks(markdown, allow_lossy)?;
    let (normalized_macros, macro_fragments) =
        replace_confluence_macro_blocks(&normalized_layouts, allow_lossy)?;
    let block_re = Regex::new(r"(?s)```confluence-storage\s*\n(.*?)\n```")?;
    let mut raw_fragments = Vec::new();
    let normalized = block_re
        .replace_all(&normalized_macros, |captures: &regex::Captures<'_>| {
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
    for (idx, fragment) in layout_fragments.into_iter().enumerate() {
        html_output = html_output.replace(
            &format!("<p>CONFLUENCE_LAYOUT_PLACEHOLDER_{idx}</p>"),
            &fragment,
        );
        html_output =
            html_output.replace(&format!("CONFLUENCE_LAYOUT_PLACEHOLDER_{idx}"), &fragment);
    }
    for (idx, fragment) in macro_fragments.into_iter().enumerate() {
        html_output = html_output.replace(
            &format!("<p>CONFLUENCE_MACRO_PLACEHOLDER_{idx}</p>"),
            &fragment,
        );
        html_output =
            html_output.replace(&format!("CONFLUENCE_MACRO_PLACEHOLDER_{idx}"), &fragment);
    }
    html_output = convert_checkbox_lists_to_task_lists(&html_output);

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

fn wrap_storage_fragment(storage: &str) -> String {
    format!(r#"<root xmlns:ac="{AC_NS}" xmlns:ri="{RI_NS}">{storage}</root>"#)
}

fn render_top_level_block(node: Node<'_, '_>, source: &str) -> Option<String> {
    match node.node_type() {
        NodeType::Text => {
            let text = normalize_text_node(node.text().unwrap_or_default());
            if text.trim().is_empty() {
                None
            } else {
                Some(text.trim().to_string())
            }
        }
        NodeType::Element => Some(
            render_block_element(node, source)
                .unwrap_or_else(|| fallback_block_markdown(node, source)),
        ),
        _ => None,
    }
}

fn render_block_element(node: Node<'_, '_>, source: &str) -> Option<String> {
    if is_confluence_node(node) {
        return render_confluence_block(node, source);
    }

    match node.tag_name().name() {
        "p" => {
            let inline = render_inline_children(node, source)?;
            if inline.trim().is_empty() {
                None
            } else {
                Some(inline.trim().to_string())
            }
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = node.tag_name().name()[1..].parse::<usize>().ok()?;
            let inline = render_inline_children(node, source)?;
            Some(format!("{} {}", "#".repeat(level), inline.trim()))
        }
        "pre" => render_preformatted(node),
        "blockquote" => {
            let rendered = render_container_blocks(node, source)?;
            Some(prefix_markdown_block(&rendered, "> "))
        }
        "ul" => render_list(node, source, 0, false),
        "ol" => render_list(node, source, 0, true),
        "table" => render_table(node, source),
        "hr" => Some("---".to_string()),
        "div" | "section" | "article" | "main" => render_container_blocks(node, source),
        "img" => render_inline_node(node, source).map(|value| value.trim().to_string()),
        _ => {
            if has_block_children(node) {
                None
            } else {
                render_inline_node(node, source).map(|value| value.trim().to_string())
            }
        }
    }
}

fn render_confluence_block(node: Node<'_, '_>, source: &str) -> Option<String> {
    if node.tag_name().namespace() != Some(AC_NS) {
        return Some(confluence_raw_block(raw_xml_fragment(node, source)));
    }

    match node.tag_name().name() {
        "layout" => render_layout(node, source),
        "task-list" => render_task_list(node, source),
        "structured-macro" => render_supported_macro_block(node, source),
        "image" | "link" => render_inline_node(node, source).map(|value| value.trim().to_string()),
        _ => Some(confluence_raw_block(raw_xml_fragment(node, source))),
    }
}

fn render_layout(node: Node<'_, '_>, source: &str) -> Option<String> {
    let sections: Vec<_> = node
        .children()
        .filter(|child| {
            child.is_element()
                && child.tag_name().namespace() == Some(AC_NS)
                && child.tag_name().name() == "layout-section"
        })
        .collect();
    if sections.is_empty() {
        return None;
    }
    if node.children().any(|child| {
        child.is_element()
            && (child.tag_name().namespace() != Some(AC_NS)
                || child.tag_name().name() != "layout-section")
    }) {
        return Some(confluence_raw_block(raw_xml_fragment(node, source)));
    }

    let mut rendered_sections = Vec::new();
    for section in sections {
        let mut attrs = section.attributes();
        let section_type = section.attribute((AC_NS, "type"))?;
        let breakout_mode = section.attribute((AC_NS, "breakout-mode"));
        if attrs.any(|attr| {
            attr.namespace() != Some(AC_NS) || !matches!(attr.name(), "type" | "breakout-mode")
        }) {
            return Some(confluence_raw_block(raw_xml_fragment(node, source)));
        }
        let cells: Vec<_> = section
            .children()
            .filter(|child| {
                child.is_element()
                    && child.tag_name().namespace() == Some(AC_NS)
                    && child.tag_name().name() == "layout-cell"
            })
            .collect();
        if cells.is_empty()
            || section.children().any(|child| {
                child.is_element()
                    && (child.tag_name().namespace() != Some(AC_NS)
                        || child.tag_name().name() != "layout-cell")
            })
        {
            return Some(confluence_raw_block(raw_xml_fragment(node, source)));
        }

        let mut block = format!("~~~~confluence-layout-section {section_type}\n");
        if let Some(breakout_mode) = breakout_mode.filter(|value| !value.is_empty()) {
            block.push_str(&format!("breakout-mode: {breakout_mode}\n"));
        }
        for cell in cells {
            block.push_str("--- cell ---\n");
            let cell_markdown = storage_to_markdown(&inner_xml_fragment(cell, source));
            if !cell_markdown.trim().is_empty() {
                block.push_str(cell_markdown.trim());
                block.push('\n');
            }
        }
        block.push_str("~~~~");
        rendered_sections.push(block);
    }

    Some(rendered_sections.join("\n\n"))
}

fn render_container_blocks(node: Node<'_, '_>, source: &str) -> Option<String> {
    let mut blocks = Vec::new();
    let mut inline_buffer = String::new();
    for child in node.children() {
        match child.node_type() {
            NodeType::Text => {
                let text = normalize_text_node(child.text().unwrap_or_default());
                if !text.is_empty() {
                    inline_buffer.push_str(&text);
                }
            }
            NodeType::Element => {
                if is_inlineish_tag(child) || is_confluence_inline(child) {
                    inline_buffer.push_str(&render_inline_node(child, source)?);
                    continue;
                }
                if !inline_buffer.trim().is_empty() {
                    blocks.push(inline_buffer.trim().to_string());
                    inline_buffer.clear();
                }
                let rendered = render_block_element(child, source)
                    .unwrap_or_else(|| fallback_block_markdown(child, source));
                if !rendered.trim().is_empty() {
                    blocks.push(rendered.trim().to_string());
                }
            }
            _ => {}
        }
    }
    if !inline_buffer.trim().is_empty() {
        blocks.push(inline_buffer.trim().to_string());
    }
    if blocks.is_empty() {
        None
    } else {
        Some(blocks.join("\n\n"))
    }
}

fn render_preformatted(node: Node<'_, '_>) -> Option<String> {
    if contains_confluence_markup(node) {
        return None;
    }
    let code_child = node
        .children()
        .find(|child| child.is_element() && child.tag_name().name() == "code");
    let (language, content) = if let Some(code) = code_child {
        (
            code.attribute("class")
                .and_then(language_from_class)
                .unwrap_or_default(),
            code.text()
                .unwrap_or_default()
                .trim_end_matches('\n')
                .to_string(),
        )
    } else {
        (
            String::new(),
            node.text()
                .unwrap_or_default()
                .trim_end_matches('\n')
                .to_string(),
        )
    };
    Some(format!("```{language}\n{content}\n```"))
}

fn language_from_class(class_name: &str) -> Option<String> {
    class_name
        .split_whitespace()
        .find_map(|part| part.strip_prefix("language-"))
        .map(ToOwned::to_owned)
}

fn render_list(node: Node<'_, '_>, source: &str, indent: usize, ordered: bool) -> Option<String> {
    let items: Vec<_> = node
        .children()
        .filter(|child| child.is_element() && child.tag_name().name() == "li")
        .collect();
    if items.is_empty() {
        return None;
    }

    let mut rendered = Vec::new();
    for item in items {
        rendered.push(render_list_item(item, source, indent, ordered)?);
    }
    Some(rendered.join("\n"))
}

fn render_list_item(
    node: Node<'_, '_>,
    source: &str,
    indent: usize,
    ordered: bool,
) -> Option<String> {
    let marker = if ordered { "1." } else { "-" };
    let prefix = format!("{}{} ", " ".repeat(indent), marker);
    let mut inline = String::new();
    let mut nested = Vec::new();

    for child in node.children() {
        match child.node_type() {
            NodeType::Text => {
                inline.push_str(&normalize_text_node(child.text().unwrap_or_default()))
            }
            NodeType::Element => {
                if child.tag_name().name() == "ul" {
                    nested.push(render_list(child, source, indent + 2, false)?);
                    continue;
                }
                if child.tag_name().name() == "ol" {
                    nested.push(render_list(child, source, indent + 2, true)?);
                    continue;
                }
                if child.tag_name().name() == "p" {
                    inline.push_str(render_inline_children(child, source)?.trim());
                    continue;
                }
                if is_inlineish_tag(child) || is_confluence_inline(child) {
                    inline.push_str(&render_inline_node(child, source)?);
                    continue;
                }
                return None;
            }
            _ => {}
        }
    }

    let mut out = format!("{prefix}{}", inline.trim());
    for block in nested {
        if !block.trim().is_empty() {
            out.push('\n');
            out.push_str(&block);
        }
    }
    Some(out)
}

fn render_table(node: Node<'_, '_>, source: &str) -> Option<String> {
    let rows = collect_table_rows(node);
    if rows.is_empty() {
        return None;
    }

    let mut markdown_rows = Vec::new();
    for row in rows {
        let mut cells = Vec::new();
        let mut header_row = false;
        for cell in row
            .children()
            .filter(|child| child.is_element() && matches!(child.tag_name().name(), "th" | "td"))
        {
            if cell.attribute("rowspan").is_some() || cell.attribute("colspan").is_some() {
                return None;
            }
            if cell.tag_name().name() == "th" {
                header_row = true;
            }
            let value = render_inline_children(cell, source)?;
            let value = value.trim().replace('\n', " ").replace('|', r"\|");
            cells.push(value);
        }
        if cells.is_empty() {
            continue;
        }
        markdown_rows.push((header_row, cells));
    }

    if markdown_rows.is_empty() || !markdown_rows.first()?.0 {
        return None;
    }

    let headers = &markdown_rows[0].1;
    let mut lines = Vec::new();
    lines.push(format!("| {} |", headers.join(" | ")));
    lines.push(format!(
        "| {} |",
        headers
            .iter()
            .map(|_| "---")
            .collect::<Vec<_>>()
            .join(" | ")
    ));
    for (_, row) in markdown_rows.into_iter().skip(1) {
        lines.push(format!("| {} |", row.join(" | ")));
    }
    Some(lines.join("\n"))
}

fn collect_table_rows<'a, 'input>(node: Node<'a, 'input>) -> Vec<Node<'a, 'input>> {
    node.children()
        .filter(|child| child.is_element())
        .flat_map(|child| match child.tag_name().name() {
            "thead" | "tbody" | "tfoot" => child
                .children()
                .filter(|row| row.is_element() && row.tag_name().name() == "tr")
                .collect::<Vec<_>>(),
            "tr" => vec![child],
            _ => Vec::new(),
        })
        .collect()
}

fn render_task_list(node: Node<'_, '_>, source: &str) -> Option<String> {
    let tasks: Vec<_> = node
        .children()
        .filter(|child| {
            child.is_element()
                && child.tag_name().namespace() == Some(AC_NS)
                && child.tag_name().name() == "task"
        })
        .collect();
    if tasks.is_empty() {
        return None;
    }

    let mut lines = Vec::new();
    for task in tasks {
        let status = namespaced_child(task, AC_NS, "task-status")?
            .text()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let body = namespaced_child(task, AC_NS, "task-body")?;
        let text = render_inline_children(body, source)?;
        let marker = if status == "complete" { "[x]" } else { "[ ]" };
        lines.push(format!("- {marker} {}", text.trim()));
    }
    Some(lines.join("\n"))
}

fn render_supported_macro_block(node: Node<'_, '_>, source: &str) -> Option<String> {
    let name = node.attribute((AC_NS, "name"))?;
    if name == "code" {
        return render_code_macro_block(node);
    }
    if name == "excerpt" {
        return render_excerpt_macro_block(node, source);
    }
    if name == "excerpt-include" {
        return render_page_reference_macro_block("excerpt-include", node);
    }
    if matches!(name, "include" | "include-page") {
        return render_include_page_macro_block(node);
    }
    if name == "pagetree" {
        return render_page_tree_macro_block(node);
    }
    if name == "pagetreesearch" {
        return render_page_tree_search_macro_block(node);
    }
    if name == "contentbylabel" {
        return render_parameter_only_macro_block("content-by-label", node);
    }
    if name == "recently-updated" || name == "recentlyupdated" {
        return render_recently_updated_macro_block(node);
    }
    if name == "toc" {
        return render_parameter_only_macro_block("toc", node);
    }
    if name == "children" {
        return render_parameter_only_macro_block("children", node);
    }
    if name == "expand" {
        return render_expand_macro_block(node, source);
    }
    if name == "status" {
        return render_status_macro(node);
    }
    if !matches!(name, "info" | "note" | "tip" | "warning") {
        return Some(confluence_raw_block(raw_xml_fragment(node, source)));
    }
    let body = namespaced_child(node, AC_NS, "rich-text-body")?;
    let body_markdown = storage_to_markdown(&inner_xml_fragment(body, source));
    if body_markdown.trim().is_empty() {
        Some(format!(":::confluence-{name}\n:::"))
    } else {
        Some(format!(
            ":::confluence-{name}\n{}\n:::",
            body_markdown.trim()
        ))
    }
}

fn render_excerpt_macro_block(node: Node<'_, '_>, source: &str) -> Option<String> {
    let body = namespaced_child(node, AC_NS, "rich-text-body")?;
    let body_markdown = storage_to_markdown(&inner_xml_fragment(body, source));
    let parameters = collect_macro_parameters(node);
    Some(render_rich_text_macro_block(
        "excerpt",
        &parameters,
        &body_markdown,
    ))
}

fn render_page_reference_macro_block(block_name: &str, node: Node<'_, '_>) -> Option<String> {
    let mut parameters = collect_macro_parameters(node);
    if let Some(target) = parameters.remove("default-parameter") {
        let placeholder = parse_default_parameter_page_target(&target);
        parameters.insert("page".to_string(), build_page_placeholder_url(&placeholder));
    } else if let Some(page) = namespaced_child(node, RI_NS, "page") {
        let placeholder = page_resource_placeholder(page);
        parameters.insert("page".to_string(), build_page_placeholder_url(&placeholder));
    }
    Some(render_parameter_only_macro_block_with_parameters(
        block_name,
        &parameters,
    ))
}

fn render_include_page_macro_block(node: Node<'_, '_>) -> Option<String> {
    let mut parameters = collect_macro_parameters(node);
    let placeholder = if let Some(parameter) = find_macro_parameter(node, "") {
        parameters.remove("");
        namespaced_child(parameter, AC_NS, "link")
            .and_then(|link| namespaced_child(link, RI_NS, "page"))
            .map(page_resource_placeholder)
    } else if let Some(target) = parameters.remove("default-parameter") {
        Some(parse_default_parameter_page_target(&target))
    } else {
        namespaced_child(node, RI_NS, "page").map(page_resource_placeholder)
    };

    if let Some(placeholder) = placeholder {
        parameters.insert("page".to_string(), build_page_placeholder_url(&placeholder));
    }
    Some(render_parameter_only_macro_block_with_parameters(
        "include-page",
        &parameters,
    ))
}

fn render_page_tree_macro_block(node: Node<'_, '_>) -> Option<String> {
    let mut parameters = collect_macro_parameters(node);
    let root_placeholder = if let Some(parameter) = find_macro_parameter(node, "root") {
        namespaced_child(parameter, AC_NS, "link")
            .and_then(|link| namespaced_child(link, RI_NS, "page"))
            .map(page_resource_placeholder)
            .or_else(|| {
                parameters.get("root").and_then(|root| {
                    let trimmed = root.trim();
                    if trimmed.is_empty() || trimmed.starts_with('@') {
                        None
                    } else {
                        Some(parse_default_parameter_page_target(trimmed))
                    }
                })
            })
    } else {
        None
    };
    if let Some(placeholder) = root_placeholder {
        parameters.insert("root".to_string(), build_page_placeholder_url(&placeholder));
    }
    Some(render_parameter_only_macro_block_with_parameters(
        "page-tree",
        &parameters,
    ))
}

fn render_page_tree_search_macro_block(node: Node<'_, '_>) -> Option<String> {
    let mut parameters = collect_macro_parameters(node);
    if let Some(root) = parameters.get("root").cloned() {
        let trimmed = root.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('@') {
            let placeholder = parse_default_parameter_page_target(trimmed);
            parameters.insert("root".to_string(), build_page_placeholder_url(&placeholder));
        }
    }
    Some(render_parameter_only_macro_block_with_parameters(
        "page-tree-search",
        &parameters,
    ))
}

fn render_recently_updated_macro_block(node: Node<'_, '_>) -> Option<String> {
    let mut parameters = collect_macro_parameters(node);
    if let Some(parameter) = find_macro_parameter(node, "spaces") {
        if let Some(space) = namespaced_child(parameter, RI_NS, "space") {
            let space_key = space
                .attribute((RI_NS, "space-key"))
                .or_else(|| space.attribute("ri:space-key"))
                .or_else(|| space.attribute("space-key"))?;
            parameters.insert("spaces".to_string(), space_key.to_string());
        }
    }
    Some(render_parameter_only_macro_block_with_parameters(
        "recently-updated",
        &parameters,
    ))
}

fn render_expand_macro_block(node: Node<'_, '_>, source: &str) -> Option<String> {
    let parameters = collect_macro_parameters(node);
    if parameters.keys().any(|name| name != "title") {
        return Some(confluence_raw_block(raw_xml_fragment(node, source)));
    }
    let title = parameters
        .get("title")
        .map(|value| value.trim())
        .unwrap_or("");
    let body = namespaced_child(node, AC_NS, "rich-text-body")?;
    let body_markdown = storage_to_markdown(&inner_xml_fragment(body, source));
    let header = if title.is_empty() {
        ":::confluence-expand".to_string()
    } else {
        format!(":::confluence-expand {title}")
    };
    if body_markdown.trim().is_empty() {
        Some(format!("{header}\n:::"))
    } else {
        Some(format!("{header}\n{}\n:::", body_markdown.trim()))
    }
}

fn render_parameter_only_macro_block(name: &str, node: Node<'_, '_>) -> Option<String> {
    let parameters = collect_macro_parameters(node);
    Some(render_parameter_only_macro_block_with_parameters(
        name,
        &parameters,
    ))
}

fn render_parameter_only_macro_block_with_parameters(
    name: &str,
    parameters: &BTreeMap<String, String>,
) -> String {
    let mut markdown = format!(":::confluence-{name}\n");
    for (parameter, value) in parameters {
        markdown.push_str(&format!("{parameter}: {value}\n"));
    }
    markdown.push_str(":::");
    markdown
}

fn page_resource_placeholder(node: Node<'_, '_>) -> PageLinkPlaceholder {
    PageLinkPlaceholder {
        content_id: node.attribute((RI_NS, "content-id")).map(ToOwned::to_owned),
        space_key: node.attribute((RI_NS, "space-key")).map(ToOwned::to_owned),
        content_title: node
            .attribute((RI_NS, "content-title"))
            .map(ToOwned::to_owned),
        anchor: None,
    }
}

fn render_rich_text_macro_block(
    name: &str,
    parameters: &BTreeMap<String, String>,
    body_markdown: &str,
) -> String {
    let mut markdown = format!(":::confluence-{name}\n");
    if !parameters.is_empty() {
        for (parameter, value) in parameters {
            markdown.push_str(&format!("{parameter}: {value}\n"));
        }
        markdown.push_str("---\n");
    }
    if !body_markdown.trim().is_empty() {
        markdown.push_str(body_markdown.trim());
        markdown.push('\n');
    }
    markdown.push_str(":::");
    markdown
}

fn render_code_macro_block(node: Node<'_, '_>) -> Option<String> {
    let code_body = namespaced_child(node, AC_NS, "plain-text-body")?;
    let code = code_body.text().unwrap_or_default().trim_end_matches('\n');
    let mut parameters = collect_macro_parameters(node);
    let language = parameters
        .remove("language")
        .filter(|value| !value.is_empty());

    let mut markdown = match language {
        Some(language) => format!("~~~confluence-code {language}\n"),
        None => "~~~confluence-code\n".to_string(),
    };
    if !parameters.is_empty() {
        for (name, value) in parameters {
            markdown.push_str(&format!("{name}: {value}\n"));
        }
        markdown.push_str("---\n");
    }
    markdown.push_str(code);
    if !code.is_empty() {
        markdown.push('\n');
    }
    markdown.push_str("~~~");
    Some(markdown)
}

fn render_inline_children(node: Node<'_, '_>, source: &str) -> Option<String> {
    let mut rendered = String::new();
    for child in node.children() {
        match child.node_type() {
            NodeType::Text => {
                rendered.push_str(&normalize_text_node(child.text().unwrap_or_default()))
            }
            NodeType::Element => rendered.push_str(&render_inline_node(child, source)?),
            _ => {}
        }
    }
    Some(rendered)
}

fn render_inline_node(node: Node<'_, '_>, source: &str) -> Option<String> {
    if is_confluence_node(node) {
        return render_confluence_inline(node, source);
    }
    if contains_confluence_markup(node) {
        return None;
    }

    match node.tag_name().name() {
        "strong" | "b" => wrap_markdown("**", render_inline_children(node, source)?),
        "em" | "i" => wrap_markdown("*", render_inline_children(node, source)?),
        "code" => Some(format!(
            "`{}`",
            node.text().unwrap_or_default().trim().replace('`', r"\`")
        )),
        "a" => {
            let href = node.attribute("href")?;
            let label = render_inline_children(node, source)?.trim().to_string();
            Some(format!(
                "[{}]({href})",
                if label.is_empty() { href } else { &label }
            ))
        }
        "img" => {
            let src = node.attribute("src")?;
            let alt = node.attribute("alt").unwrap_or_default();
            Some(format!("![{}]({src})", escape_markdown_text(alt)))
        }
        "br" => Some("  \n".to_string()),
        "span" => {
            if node.attributes().len() > 0 {
                Some(raw_xml_fragment(node, source).to_string())
            } else {
                render_inline_children(node, source)
            }
        }
        "small" | "big" | "u" | "sub" | "sup" => Some(raw_xml_fragment(node, source).to_string()),
        _ => {
            if node.attributes().len() > 0 || has_block_children(node) {
                Some(raw_xml_fragment(node, source).to_string())
            } else {
                render_inline_children(node, source)
            }
        }
    }
}

fn render_confluence_inline(node: Node<'_, '_>, source: &str) -> Option<String> {
    if node.tag_name().namespace() != Some(AC_NS) {
        return None;
    }

    match node.tag_name().name() {
        "image" => render_confluence_image(node),
        "link" => render_confluence_link(node, source),
        "structured-macro" => render_status_macro(node),
        _ => None,
    }
}

fn render_status_macro(node: Node<'_, '_>) -> Option<String> {
    let name = node.attribute((AC_NS, "name"))?;
    if name != "status" {
        return None;
    }
    let parameters = collect_macro_parameters(node);
    if parameters
        .keys()
        .any(|key| !matches!(key.as_str(), "title" | "colour"))
    {
        return None;
    }
    let title = parameters.get("title")?.trim().to_string();
    let placeholder = StatusMacroPlaceholder {
        title: title.clone(),
        colour: parameters
            .get("colour")
            .cloned()
            .filter(|value| !value.is_empty()),
    };
    let target = build_status_placeholder_url(&placeholder);
    Some(format!("[{}]({target})", escape_markdown_text(&title)))
}

fn render_confluence_image(node: Node<'_, '_>) -> Option<String> {
    let alt = node
        .attribute((AC_NS, "alt"))
        .or_else(|| node.attribute((AC_NS, "title")))
        .unwrap_or_default();

    if let Some(attachment) = namespaced_child(node, RI_NS, "attachment") {
        let file_name = attachment.attribute((RI_NS, "filename"))?;
        return Some(format!(
            "![{}](attachments/{file_name})",
            escape_markdown_text(alt)
        ));
    }

    let url = namespaced_child(node, RI_NS, "url")?.attribute((RI_NS, "value"))?;
    Some(format!("![{}]({url})", escape_markdown_text(alt)))
}

fn render_confluence_link(node: Node<'_, '_>, source: &str) -> Option<String> {
    let anchor = node.attribute((AC_NS, "anchor"));
    let label = link_label(node, source);

    if let Some(page) = namespaced_child(node, RI_NS, "page") {
        let placeholder = PageLinkPlaceholder {
            content_id: page.attribute((RI_NS, "content-id")).map(ToOwned::to_owned),
            space_key: page.attribute((RI_NS, "space-key")).map(ToOwned::to_owned),
            content_title: page
                .attribute((RI_NS, "content-title"))
                .map(ToOwned::to_owned),
            anchor: anchor.map(ToOwned::to_owned),
        };
        let target = build_page_placeholder_url(&placeholder);
        let label = label
            .or_else(|| placeholder.content_title.clone())
            .or_else(|| placeholder.content_id.clone())
            .or_else(|| placeholder.anchor.clone())
            .unwrap_or_else(|| "Confluence page".to_string());
        return Some(format!("[{}]({target})", label.trim()));
    }

    if let Some(user) = namespaced_child(node, RI_NS, "user") {
        let placeholder = UserMentionPlaceholder {
            account_id: user.attribute((RI_NS, "account-id")).map(ToOwned::to_owned),
            user_key: user.attribute((RI_NS, "userkey")).map(ToOwned::to_owned),
            username: user.attribute((RI_NS, "username")).map(ToOwned::to_owned),
        };
        let target = build_user_placeholder_url(&placeholder);
        let label = label
            .or_else(|| {
                placeholder
                    .username
                    .clone()
                    .map(|value| format!("@{value}"))
            })
            .or_else(|| placeholder.account_id.clone())
            .or_else(|| placeholder.user_key.clone())
            .unwrap_or_else(|| "@user".to_string());
        return Some(format!("[{}]({target})", label.trim()));
    }

    if let Some(attachment) = namespaced_child(node, RI_NS, "attachment") {
        let file_name = attachment.attribute((RI_NS, "filename"))?;
        let mut target = format!("attachments/{file_name}");
        if let Some(anchor) = anchor {
            target.push('#');
            target.push_str(anchor);
        }
        let label = label.unwrap_or_else(|| file_name.to_string());
        return Some(format!("[{}]({target})", label.trim()));
    }

    if let Some(url) =
        namespaced_child(node, RI_NS, "url").and_then(|url| url.attribute((RI_NS, "value")))
    {
        let mut target = url.to_string();
        if let Some(anchor) = anchor {
            target.push('#');
            target.push_str(anchor);
        }
        let label = label.unwrap_or_else(|| target.clone());
        return Some(format!("[{}]({target})", label.trim()));
    }

    if let Some(anchor) = anchor {
        let label = label.unwrap_or_else(|| anchor.to_string());
        return Some(format!("[{}](#{anchor})", label.trim()));
    }

    None
}

fn link_label(node: Node<'_, '_>, source: &str) -> Option<String> {
    if let Some(body) = namespaced_child(node, AC_NS, "plain-text-link-body") {
        return body.text().map(|text| escape_markdown_text(text.trim()));
    }
    if let Some(body) = namespaced_child(node, AC_NS, "link-body") {
        return render_inline_children(body, source).map(|text| text.trim().to_string());
    }
    None
}

fn namespaced_child<'a, 'input>(
    node: Node<'a, 'input>,
    namespace: &str,
    name: &str,
) -> Option<Node<'a, 'input>> {
    node.children().find(|child| {
        child.is_element()
            && child.tag_name().namespace() == Some(namespace)
            && child.tag_name().name() == name
    })
}

fn is_confluence_node(node: Node<'_, '_>) -> bool {
    node.tag_name()
        .namespace()
        .is_some_and(|namespace| namespace == AC_NS || namespace == RI_NS)
}

fn is_confluence_inline(node: Node<'_, '_>) -> bool {
    is_confluence_node(node) && matches!(node.tag_name().name(), "image" | "link")
}

fn contains_confluence_markup(node: Node<'_, '_>) -> bool {
    node.descendants()
        .any(|child| child.is_element() && is_confluence_node(child))
}

fn is_inlineish_tag(node: Node<'_, '_>) -> bool {
    matches!(
        node.tag_name().name(),
        "span"
            | "strong"
            | "b"
            | "em"
            | "i"
            | "code"
            | "a"
            | "img"
            | "br"
            | "small"
            | "big"
            | "u"
            | "sub"
            | "sup"
    )
}

fn has_block_children(node: Node<'_, '_>) -> bool {
    node.children().any(|child| {
        child.is_element()
            && !matches!(
                child.tag_name().name(),
                "span"
                    | "strong"
                    | "b"
                    | "em"
                    | "i"
                    | "code"
                    | "a"
                    | "img"
                    | "br"
                    | "small"
                    | "big"
                    | "u"
                    | "sub"
                    | "sup"
            )
    })
}

fn normalize_text_node(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        if text.chars().any(char::is_whitespace) {
            " ".to_string()
        } else {
            String::new()
        }
    } else {
        let leading = if text.chars().next().is_some_and(char::is_whitespace) {
            " "
        } else {
            ""
        };
        let trailing = if text.chars().last().is_some_and(char::is_whitespace) {
            " "
        } else {
            ""
        };
        format!("{leading}{}{trailing}", escape_markdown_text(&collapsed))
    }
}

fn escape_markdown_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' | '*' | '_' | '[' | ']' | '`' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn wrap_markdown(wrapper: &str, content: String) -> Option<String> {
    if content.trim().is_empty() {
        None
    } else {
        Some(format!("{wrapper}{}{wrapper}", content.trim()))
    }
}

fn prefix_markdown_block(block: &str, prefix: &str) -> String {
    block
        .lines()
        .map(|line| {
            if line.is_empty() {
                prefix.trim_end().to_string()
            } else {
                format!("{prefix}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn fallback_block_markdown(node: Node<'_, '_>, source: &str) -> String {
    if contains_confluence_markup(node) || is_confluence_node(node) {
        confluence_raw_block(raw_xml_fragment(node, source))
    } else {
        raw_xml_fragment(node, source).trim().to_string()
    }
}

fn confluence_raw_block(raw: &str) -> String {
    format!("```confluence-storage\n{}\n```", raw.trim())
}

fn collect_macro_parameters(node: Node<'_, '_>) -> BTreeMap<String, String> {
    node.children()
        .filter(|child| {
            child.is_element()
                && child.tag_name().namespace() == Some(AC_NS)
                && child.tag_name().name() == "parameter"
        })
        .filter_map(|parameter| {
            let name = parameter.attribute((AC_NS, "name"))?;
            Some((
                name.to_string(),
                parameter.text().unwrap_or_default().trim().to_string(),
            ))
        })
        .collect()
}

fn find_macro_parameter<'a>(node: Node<'a, 'a>, name: &str) -> Option<Node<'a, 'a>> {
    node.children().find(|child| {
        child.is_element()
            && child.tag_name().namespace() == Some(AC_NS)
            && child.tag_name().name() == "parameter"
            && child.attribute((AC_NS, "name")) == Some(name)
    })
}

pub(crate) fn build_page_placeholder_url(link: &PageLinkPlaceholder) -> String {
    let mut url = Url::parse(&format!("{CONFLUENCE_PAGE_SCHEME}://page"))
        .expect("valid confluence page placeholder base");
    {
        let mut pairs = url.query_pairs_mut();
        if let Some(content_id) = &link.content_id {
            pairs.append_pair("content-id", content_id);
        }
        if let Some(space_key) = &link.space_key {
            pairs.append_pair("space-key", space_key);
        }
        if let Some(content_title) = &link.content_title {
            pairs.append_pair("content-title", content_title);
        }
    }
    if let Some(anchor) = &link.anchor {
        url.set_fragment(Some(anchor));
    }
    url.to_string()
}

pub(crate) fn parse_page_placeholder_url(target: &str) -> Option<PageLinkPlaceholder> {
    let normalized = target.replace("&amp;", "&");
    let url = Url::parse(&normalized).ok()?;
    if url.scheme() != CONFLUENCE_PAGE_SCHEME || url.host_str() != Some("page") {
        return None;
    }
    let mut placeholder = PageLinkPlaceholder::default();
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "content-id" => placeholder.content_id = Some(value.into_owned()),
            "space-key" => placeholder.space_key = Some(value.into_owned()),
            "content-title" => placeholder.content_title = Some(value.into_owned()),
            _ => {}
        }
    }
    placeholder.anchor = url.fragment().map(ToOwned::to_owned);
    if placeholder.content_id.is_none()
        && placeholder.space_key.is_none()
        && placeholder.content_title.is_none()
        && placeholder.anchor.is_none()
    {
        None
    } else {
        Some(placeholder)
    }
}

pub(crate) fn build_user_placeholder_url(link: &UserMentionPlaceholder) -> String {
    let mut url = Url::parse(&format!("{CONFLUENCE_USER_SCHEME}://user"))
        .expect("valid confluence user placeholder base");
    {
        let mut pairs = url.query_pairs_mut();
        if let Some(account_id) = &link.account_id {
            pairs.append_pair("account-id", account_id);
        }
        if let Some(user_key) = &link.user_key {
            pairs.append_pair("userkey", user_key);
        }
        if let Some(username) = &link.username {
            pairs.append_pair("username", username);
        }
    }
    url.to_string()
}

pub(crate) fn parse_user_placeholder_url(target: &str) -> Option<UserMentionPlaceholder> {
    let normalized = target.replace("&amp;", "&");
    let url = Url::parse(&normalized).ok()?;
    if url.scheme() != CONFLUENCE_USER_SCHEME || url.host_str() != Some("user") {
        return None;
    }
    let mut placeholder = UserMentionPlaceholder::default();
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "account-id" => placeholder.account_id = Some(value.into_owned()),
            "userkey" => placeholder.user_key = Some(value.into_owned()),
            "username" => placeholder.username = Some(value.into_owned()),
            _ => {}
        }
    }
    if placeholder.account_id.is_none()
        && placeholder.user_key.is_none()
        && placeholder.username.is_none()
    {
        None
    } else {
        Some(placeholder)
    }
}

pub(crate) fn build_status_placeholder_url(status: &StatusMacroPlaceholder) -> String {
    let mut url = Url::parse(&format!("{CONFLUENCE_STATUS_SCHEME}://status"))
        .expect("valid confluence status placeholder base");
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("title", &status.title);
        if let Some(colour) = &status.colour {
            pairs.append_pair("colour", colour);
        }
    }
    url.to_string()
}

pub(crate) fn parse_status_placeholder_url(target: &str) -> Option<StatusMacroPlaceholder> {
    let normalized = target.replace("&amp;", "&");
    let url = Url::parse(&normalized).ok()?;
    if url.scheme() != CONFLUENCE_STATUS_SCHEME || url.host_str() != Some("status") {
        return None;
    }
    let mut title = None;
    let mut colour = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "title" => title = Some(value.into_owned()),
            "colour" => colour = Some(value.into_owned()),
            _ => {}
        }
    }
    Some(StatusMacroPlaceholder {
        title: title?,
        colour,
    })
}

fn raw_xml_fragment<'a>(node: Node<'_, 'a>, source: &'a str) -> &'a str {
    &source[node.range()]
}

fn inner_xml_fragment(node: Node<'_, '_>, source: &str) -> String {
    node.children()
        .map(|child| raw_xml_fragment(child, source))
        .collect::<String>()
}

fn replace_confluence_macro_blocks(
    markdown: &str,
    allow_lossy: bool,
) -> Result<(String, Vec<String>)> {
    let panel_macro_re =
        Regex::new(r"(?ms)^:::confluence-(info|note|tip|warning)[ \t]*\n(.*?)\n:::[ \t]*(?:\n|$)")?;
    let mut fragments = Vec::new();
    let mut normalized = String::with_capacity(markdown.len());
    let mut last = 0;
    for captures in panel_macro_re.captures_iter(markdown) {
        let Some(full_match) = captures.get(0) else {
            continue;
        };
        normalized.push_str(&markdown[last..full_match.start()]);
        let name = captures.get(1).map(|m| m.as_str()).unwrap_or_default();
        let body_markdown = captures.get(2).map(|m| m.as_str()).unwrap_or_default();
        let body_storage = markdown_to_storage(body_markdown, allow_lossy)?.storage;
        let idx = fragments.len();
        fragments.push(format!(
            r#"<ac:structured-macro ac:name="{name}"><ac:rich-text-body>{body_storage}</ac:rich-text-body></ac:structured-macro>"#
        ));
        normalized.push_str(&format!("CONFLUENCE_MACRO_PLACEHOLDER_{idx}"));
        last = full_match.end();
    }
    normalized.push_str(&markdown[last..]);

    let parameterized =
        replace_parameterized_colon_macro_blocks(&normalized, allow_lossy, &mut fragments)?;

    let expand_macro_re = Regex::new(
        r"(?ms)^:::confluence-expand(?:[ \t]+([^\n]+))?[ \t]*\n(.*?)\n:::[ \t]*(?:\n|$)",
    )?;
    let mut expanded = String::with_capacity(parameterized.len());
    last = 0;
    for captures in expand_macro_re.captures_iter(&parameterized) {
        let Some(full_match) = captures.get(0) else {
            continue;
        };
        expanded.push_str(&parameterized[last..full_match.start()]);
        let title = captures.get(1).map(|m| m.as_str().trim());
        let body_markdown = captures.get(2).map(|m| m.as_str()).unwrap_or_default();
        let body_storage = markdown_to_storage(body_markdown, allow_lossy)?.storage;
        let idx = fragments.len();
        let title_param = title
            .filter(|value| !value.is_empty())
            .map(|value| {
                format!(
                    r#"<ac:parameter ac:name="title">{}</ac:parameter>"#,
                    escape_xml(value)
                )
            })
            .unwrap_or_default();
        fragments.push(format!(
            r#"<ac:structured-macro ac:name="expand">{title_param}<ac:rich-text-body>{body_storage}</ac:rich-text-body></ac:structured-macro>"#
        ));
        expanded.push_str(&format!("CONFLUENCE_MACRO_PLACEHOLDER_{idx}"));
        last = full_match.end();
    }
    expanded.push_str(&parameterized[last..]);

    let code_macro_re = Regex::new(
        r"(?ms)^~~~confluence-code(?:[ \t]+([^\n~]+))?[ \t]*\n(.*?)\n~~~[ \t]*(?:\n|$)",
    )?;
    let mut code_normalized = String::with_capacity(expanded.len());
    last = 0;
    for captures in code_macro_re.captures_iter(&expanded) {
        let Some(full_match) = captures.get(0) else {
            continue;
        };
        code_normalized.push_str(&expanded[last..full_match.start()]);
        let language = captures.get(1).map(|m| m.as_str().trim());
        let block_body = captures.get(2).map(|m| m.as_str()).unwrap_or_default();
        let (parameters, code) = parse_code_macro_block(language, block_body)?;
        let idx = fragments.len();
        fragments.push(build_code_macro_storage(&parameters, &code));
        code_normalized.push_str(&format!("CONFLUENCE_MACRO_PLACEHOLDER_{idx}"));
        last = full_match.end();
    }
    code_normalized.push_str(&expanded[last..]);

    Ok((code_normalized, fragments))
}

fn replace_parameterized_colon_macro_blocks(
    markdown: &str,
    allow_lossy: bool,
    fragments: &mut Vec<String>,
) -> Result<String> {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut output = String::new();
    let mut index = 0;

    while index < lines.len() {
        let trimmed = lines[index].trim();
        let macro_name = match trimmed {
            ":::confluence-excerpt" => Some("excerpt"),
            ":::confluence-toc" => Some("toc"),
            ":::confluence-children" => Some("children"),
            ":::confluence-excerpt-include" => Some("excerpt-include"),
            ":::confluence-include-page" => Some("include-page"),
            ":::confluence-page-tree" => Some("page-tree"),
            ":::confluence-page-tree-search" => Some("page-tree-search"),
            ":::confluence-content-by-label" => Some("content-by-label"),
            ":::confluence-recently-updated" => Some("recently-updated"),
            _ => None,
        };

        let Some(macro_name) = macro_name else {
            output.push_str(lines[index]);
            output.push('\n');
            index += 1;
            continue;
        };

        let mut body_lines = Vec::new();
        index += 1;
        while index < lines.len() && lines[index].trim() != ":::" {
            body_lines.push(lines[index]);
            index += 1;
        }
        if index >= lines.len() {
            bail!("unterminated confluence {macro_name} block");
        }

        let body = body_lines.join("\n");
        let fragment = match macro_name {
            "excerpt" => {
                let (parameters, body_storage) =
                    parse_rich_text_macro_block("confluence excerpt macro", &body, allow_lossy)?;
                build_rich_text_macro_storage("excerpt", &parameters, &body_storage)
            }
            "toc" => {
                let parameters = parse_macro_parameter_lines(&body, "confluence toc macro")?;
                build_parameter_only_macro_storage("toc", &parameters)
            }
            "children" => {
                let parameters = parse_macro_parameter_lines(&body, "confluence children macro")?;
                build_parameter_only_macro_storage("children", &parameters)
            }
            "excerpt-include" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence excerpt-include macro")?;
                build_excerpt_include_macro_storage(&parameters)?
            }
            "include-page" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence include-page macro")?;
                build_include_page_macro_storage(&parameters)?
            }
            "page-tree" => {
                let parameters = parse_macro_parameter_lines(&body, "confluence page-tree macro")?;
                build_page_tree_macro_storage(&parameters)?
            }
            "page-tree-search" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence page-tree-search macro")?;
                build_page_tree_search_macro_storage(&parameters)
            }
            "content-by-label" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence content-by-label macro")?;
                build_parameter_only_macro_storage("contentbylabel", &parameters)
            }
            "recently-updated" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence recently-updated macro")?;
                build_recently_updated_macro_storage(&parameters)
            }
            _ => unreachable!(),
        };

        let placeholder_index = fragments.len();
        fragments.push(fragment);
        output.push_str(&format!(
            "CONFLUENCE_MACRO_PLACEHOLDER_{placeholder_index}\n"
        ));
        index += 1;
    }

    if !markdown.ends_with('\n') && output.ends_with('\n') {
        output.pop();
    }

    Ok(output)
}

fn parse_rich_text_macro_block(
    context: &str,
    block_body: &str,
    allow_lossy: bool,
) -> Result<(BTreeMap<String, String>, String)> {
    let trimmed_body = block_body.trim_end_matches('\n');
    if let Some((header, body_markdown)) = trimmed_body.split_once("\n---\n") {
        let parameters = parse_macro_parameter_lines(header, context)?;
        let body_storage = markdown_to_storage(body_markdown, allow_lossy)?.storage;
        Ok((parameters, body_storage))
    } else {
        Ok((
            BTreeMap::new(),
            markdown_to_storage(trimmed_body, allow_lossy)?.storage,
        ))
    }
}

fn parse_macro_parameter_lines(body: &str, context: &str) -> Result<BTreeMap<String, String>> {
    let mut parameters = BTreeMap::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            bail!("invalid {context} header line: {trimmed}");
        };
        let name = name.trim();
        if name.is_empty() {
            bail!("invalid {context} header line: {trimmed}");
        }
        parameters.insert(name.to_string(), value.trim().to_string());
    }
    Ok(parameters)
}

fn replace_layout_blocks(markdown: &str, allow_lossy: bool) -> Result<(String, Vec<String>)> {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut fragments = Vec::new();
    let mut output = String::new();
    let mut index = 0;

    while index < lines.len() {
        if !lines[index]
            .trim_start()
            .starts_with("~~~~confluence-layout-section")
        {
            output.push_str(lines[index]);
            output.push('\n');
            index += 1;
            continue;
        }

        let (next_index, layout_storage) = parse_layout_group(&lines, index, allow_lossy)?;
        let idx = fragments.len();
        fragments.push(layout_storage);
        output.push_str(&format!("CONFLUENCE_LAYOUT_PLACEHOLDER_{idx}\n"));
        index = next_index;
    }

    if !markdown.ends_with('\n') && output.ends_with('\n') {
        output.pop();
    }

    Ok((output, fragments))
}

fn parse_layout_group(lines: &[&str], start: usize, allow_lossy: bool) -> Result<(usize, String)> {
    let mut index = start;
    let mut sections = Vec::new();

    loop {
        let (next_index, section_storage) = parse_layout_section_block(lines, index, allow_lossy)?;
        sections.push(section_storage);
        index = next_index;

        let mut scan = index;
        while scan < lines.len() && lines[scan].trim().is_empty() {
            scan += 1;
        }
        if scan >= lines.len()
            || !lines[scan]
                .trim_start()
                .starts_with("~~~~confluence-layout-section")
        {
            index = scan;
            break;
        }
        index = scan;
    }

    Ok((
        index,
        format!("<ac:layout>{}</ac:layout>", sections.join("")),
    ))
}

fn parse_layout_section_block(
    lines: &[&str],
    start: usize,
    allow_lossy: bool,
) -> Result<(usize, String)> {
    let header = lines
        .get(start)
        .copied()
        .ok_or_else(|| anyhow::anyhow!("missing layout section header"))?;
    let section_re = Regex::new(r"^~~~~confluence-layout-section(?:[ \t]+([^\n]+))?[ \t]*$")?;
    let captures = section_re
        .captures(header.trim_end())
        .ok_or_else(|| anyhow::anyhow!("invalid confluence layout section header: {header}"))?;
    let section_type = captures
        .get(1)
        .map(|m| m.as_str().trim())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing layout section type in: {header}"))?;

    let mut body_lines = Vec::new();
    let mut index = start + 1;
    while index < lines.len() && lines[index].trim_end() != "~~~~" {
        body_lines.push(lines[index]);
        index += 1;
    }
    if index >= lines.len() {
        bail!("unterminated confluence layout section `{section_type}`");
    }

    let (metadata, cells) = parse_layout_section_body(&body_lines)?;
    let mut attrs = vec![format!(r#"ac:type="{}""#, escape_xml(section_type))];
    if let Some(breakout_mode) = metadata.get("breakout-mode") {
        attrs.push(format!(
            r#"ac:breakout-mode="{}""#,
            escape_xml(breakout_mode)
        ));
    }
    let cells_xml = cells
        .into_iter()
        .map(|cell| {
            let body_storage = markdown_to_storage(&cell, allow_lossy)?.storage;
            Ok(format!("<ac:layout-cell>{body_storage}</ac:layout-cell>"))
        })
        .collect::<Result<Vec<_>>>()?
        .join("");

    Ok((
        index + 1,
        format!(
            r#"<ac:layout-section {}>{}</ac:layout-section>"#,
            attrs.join(" "),
            cells_xml
        ),
    ))
}

fn parse_layout_section_body(lines: &[&str]) -> Result<(BTreeMap<String, String>, Vec<String>)> {
    let mut metadata = BTreeMap::new();
    let mut cells = Vec::new();
    let mut current_cell = Vec::new();
    let mut saw_cell = false;

    for line in lines {
        if line.trim_end() == "--- cell ---" {
            if saw_cell {
                cells.push(current_cell.join("\n").trim_end().to_string());
                current_cell.clear();
            } else {
                saw_cell = true;
            }
            continue;
        }

        if !saw_cell {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Some((name, value)) = trimmed.split_once(':') else {
                bail!("invalid confluence layout metadata line: {trimmed}");
            };
            let name = name.trim();
            if name != "breakout-mode" {
                bail!("unsupported confluence layout metadata key: {name}");
            }
            metadata.insert(name.to_string(), value.trim().to_string());
            continue;
        }

        current_cell.push(*line);
    }

    if !saw_cell {
        bail!("confluence layout section is missing `--- cell ---` delimiters");
    }

    cells.push(current_cell.join("\n").trim_end().to_string());
    Ok((metadata, cells))
}

fn parse_code_macro_block(
    language: Option<&str>,
    block_body: &str,
) -> Result<(BTreeMap<String, String>, String)> {
    let mut parameters = BTreeMap::new();
    if let Some(language) = language.filter(|value| !value.is_empty()) {
        parameters.insert("language".to_string(), language.to_string());
    }

    let trimmed_body = block_body.trim_end_matches('\n');
    if let Some((header, code)) = trimmed_body.split_once("\n---\n") {
        let parsed_header = parse_code_macro_header(header)?;
        for (name, value) in parsed_header {
            parameters.insert(name, value);
        }
        Ok((parameters, code.to_string()))
    } else {
        Ok((parameters, trimmed_body.to_string()))
    }
}

fn parse_code_macro_header(header: &str) -> Result<BTreeMap<String, String>> {
    parse_macro_parameter_lines(header, "confluence code macro")
}

fn build_code_macro_storage(parameters: &BTreeMap<String, String>, code: &str) -> String {
    let parameters_xml = build_macro_parameters_xml(parameters);
    let body = wrap_cdata(code);
    format!(
        r#"<ac:structured-macro ac:name="code">{parameters_xml}<ac:plain-text-body><![CDATA[{body}]]></ac:plain-text-body></ac:structured-macro>"#
    )
}

fn build_rich_text_macro_storage(
    name: &str,
    parameters: &BTreeMap<String, String>,
    body_storage: &str,
) -> String {
    let parameters_xml = build_macro_parameters_xml(parameters);
    format!(
        r#"<ac:structured-macro ac:name="{name}">{parameters_xml}<ac:rich-text-body>{body_storage}</ac:rich-text-body></ac:structured-macro>"#
    )
}

fn build_parameter_only_macro_storage(name: &str, parameters: &BTreeMap<String, String>) -> String {
    let parameters_xml = build_macro_parameters_xml(parameters);
    format!(r#"<ac:structured-macro ac:name="{name}">{parameters_xml}</ac:structured-macro>"#)
}

fn build_excerpt_include_macro_storage(parameters: &BTreeMap<String, String>) -> Result<String> {
    let mut parameters = parameters.clone();
    let page = parameters.remove("page").ok_or_else(|| {
        anyhow::anyhow!("confluence excerpt-include macro requires a `page` parameter")
    })?;
    let placeholder = parse_page_placeholder_url(&page).ok_or_else(|| {
        anyhow::anyhow!(
            "confluence excerpt-include `page` must be a confluence-page placeholder; local paths are resolved during sync apply"
        )
    })?;
    let excerpt_target = build_title_page_target(&placeholder)?;
    parameters.insert("default-parameter".to_string(), excerpt_target);
    Ok(build_parameter_only_macro_storage(
        "excerpt-include",
        &parameters,
    ))
}

fn build_include_page_macro_storage(parameters: &BTreeMap<String, String>) -> Result<String> {
    let mut parameters = parameters.clone();
    let page = parameters.remove("page").ok_or_else(|| {
        anyhow::anyhow!("confluence include-page macro requires a `page` parameter")
    })?;
    let placeholder = parse_page_placeholder_url(&page).ok_or_else(|| {
        anyhow::anyhow!(
            "confluence include-page `page` must be a confluence-page placeholder; local paths are resolved during sync apply"
        )
    })?;
    let page_xml = build_page_title_resource_xml(&placeholder)?;
    let parameters_xml = build_macro_parameters_xml(&parameters);
    Ok(format!(
        r#"<ac:structured-macro ac:name="include">{parameters_xml}<ac:parameter ac:name=""><ac:link>{page_xml}</ac:link></ac:parameter></ac:structured-macro>"#
    ))
}

fn build_page_tree_macro_storage(parameters: &BTreeMap<String, String>) -> Result<String> {
    let mut parameters = parameters.clone();
    let root_xml = if let Some(root) = parameters.remove("root") {
        if let Some(placeholder) = parse_page_placeholder_url(&root) {
            let page_xml = build_page_title_resource_xml(&placeholder)?;
            Some(format!(
                r#"<ac:parameter ac:name="root"><ac:link>{page_xml}</ac:link></ac:parameter>"#
            ))
        } else {
            parameters.insert("root".to_string(), root);
            None
        }
    } else {
        None
    };
    let parameters_xml = build_macro_parameters_xml(&parameters);
    Ok(format!(
        r#"<ac:structured-macro ac:name="pagetree">{parameters_xml}{}</ac:structured-macro>"#,
        root_xml.unwrap_or_default()
    ))
}

fn build_page_tree_search_macro_storage(parameters: &BTreeMap<String, String>) -> String {
    let mut parameters = parameters.clone();
    if let Some(root) = parameters.get("root").cloned() {
        if let Some(placeholder) = parse_page_placeholder_url(&root) {
            if let Ok(root_target) = build_title_page_target(&placeholder) {
                parameters.insert("root".to_string(), root_target);
            }
        }
    }
    build_parameter_only_macro_storage("pagetreesearch", &parameters)
}

fn build_recently_updated_macro_storage(parameters: &BTreeMap<String, String>) -> String {
    let mut parameters = parameters.clone();
    let spaces_xml = if let Some(spaces) = parameters.remove("spaces") {
        let trimmed = spaces.trim();
        if trimmed.is_empty() {
            None
        } else if trimmed.starts_with('@') || trimmed.contains(',') {
            Some(format!(
                r#"<ac:parameter ac:name="spaces">{}</ac:parameter>"#,
                escape_xml(trimmed)
            ))
        } else {
            Some(format!(
                r#"<ac:parameter ac:name="spaces"><ri:space ri:space-key="{}" /></ac:parameter>"#,
                escape_xml(trimmed)
            ))
        }
    } else {
        None
    };
    let parameters_xml = build_macro_parameters_xml(&parameters);
    format!(
        r#"<ac:structured-macro ac:name="recently-updated">{parameters_xml}{}</ac:structured-macro>"#,
        spaces_xml.unwrap_or_default()
    )
}

fn parse_default_parameter_page_target(target: &str) -> PageLinkPlaceholder {
    let trimmed = target.trim();
    let mut placeholder = PageLinkPlaceholder::default();
    if let Some((space_key, title)) = trimmed.split_once(':') {
        let space_key = space_key.trim();
        let title = title.trim();
        if !space_key.is_empty() && !title.is_empty() && looks_like_space_key(space_key) {
            placeholder.space_key = Some(space_key.to_string());
            placeholder.content_title = Some(title.to_string());
            return placeholder;
        }
    }
    placeholder.content_title = (!trimmed.is_empty()).then(|| trimmed.to_string());
    placeholder
}

fn looks_like_space_key(space_key: &str) -> bool {
    space_key.starts_with('~')
        || space_key
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

fn build_title_page_target(placeholder: &PageLinkPlaceholder) -> Result<String> {
    let title = placeholder
        .content_title
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("confluence page references require a title"))?;
    if let Some(space_key) = placeholder.space_key.as_deref() {
        Ok(format!("{space_key}:{title}"))
    } else {
        Ok(title.to_string())
    }
}

fn build_page_resource_xml(placeholder: &PageLinkPlaceholder) -> Result<String> {
    if let Some(title) = placeholder.content_title.as_deref() {
        let mut attrs = vec![format!(r#"ri:content-title="{}""#, escape_xml(title))];
        if let Some(space_key) = placeholder.space_key.as_deref() {
            attrs.push(format!(r#"ri:space-key="{}""#, escape_xml(space_key)));
        }
        return Ok(format!(r#"<ri:page {} />"#, attrs.join(" ")));
    }
    if let Some(content_id) = placeholder.content_id.as_deref() {
        return Ok(format!(
            r#"<ri:page ri:content-id="{}" />"#,
            escape_xml(content_id)
        ));
    }
    bail!("confluence page placeholder requires a content id or content title")
}

fn build_page_title_resource_xml(placeholder: &PageLinkPlaceholder) -> Result<String> {
    if placeholder.content_title.is_some() {
        return build_page_resource_xml(placeholder);
    }
    bail!("confluence include-page references require a page title")
}

fn build_macro_parameters_xml(parameters: &BTreeMap<String, String>) -> String {
    let parameters_xml = parameters
        .iter()
        .map(|(name, value)| {
            format!(
                r#"<ac:parameter ac:name="{}">{}</ac:parameter>"#,
                escape_xml(name),
                escape_xml(value)
            )
        })
        .collect::<String>();
    parameters_xml
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn wrap_cdata(value: &str) -> String {
    value.replace("]]>", "]]]]><![CDATA[>")
}

fn convert_checkbox_lists_to_task_lists(html: &str) -> String {
    let ul_re = Regex::new(
        r#"(?s)<ul>\s*((?:<li>\s*<input[^>]*type="checkbox"[^>]*/?>\s*.*?</li>\s*)+)</ul>"#,
    )
    .expect("valid checkbox list regex");
    let li_re = Regex::new(r#"(?s)<li>\s*<input([^>]*)type="checkbox"([^>]*)/?>\s*(.*?)\s*</li>"#)
        .expect("valid checkbox item regex");

    ul_re
        .replace_all(html, |captures: &regex::Captures<'_>| {
            let items = &captures[1];
            let mut tasks = Vec::new();
            for item in li_re.captures_iter(items) {
                let attrs = format!("{} {}", &item[1], &item[2]);
                let status = if attrs.contains("checked") {
                    "complete"
                } else {
                    "incomplete"
                };
                let body = item[3].trim();
                tasks.push(format!(
                    "<ac:task><ac:task-status>{status}</ac:task-status><ac:task-body>{body}</ac:task-body></ac:task>"
                ));
            }

            if tasks.is_empty() {
                captures[0].to_string()
            } else {
                format!("<ac:task-list>{}</ac:task-list>", tasks.join(""))
            }
        })
        .to_string()
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
        let storage = r#"<p>Hello</p><ac:structured-macro ac:name="view-file" />"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains("```confluence-storage"));
        let rendered = markdown_to_storage(&markdown, false).expect("conversion succeeds");
        assert!(rendered.storage.contains("<ac:structured-macro"));
    }

    #[test]
    fn mixed_content_preserves_only_unsupported_confluence_fragments() {
        let storage = r#"<h1>Title</h1><ac:structured-macro ac:name="view-file" /><p>After</p>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains("# Title"));
        assert!(markdown.contains("```confluence-storage"));
        assert!(markdown.contains("After"));
        assert!(
            !markdown
                .trim_start()
                .starts_with("```confluence-storage\n<h1>Title</h1>")
        );
    }

    #[test]
    fn task_lists_round_trip_between_storage_and_markdown() {
        let storage = r#"<ac:task-list><ac:task><ac:task-status>incomplete</ac:task-status><ac:task-body>Write docs</ac:task-body></ac:task><ac:task><ac:task-status>complete</ac:task-status><ac:task-body>Ship it</ac:task-body></ac:task></ac:task-list>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains("- [ ] Write docs"));
        assert!(markdown.contains("- [x] Ship it"));

        let rendered = markdown_to_storage(&markdown, false).expect("task list markdown converts");
        assert!(rendered.storage.contains("<ac:task-list>"));
        assert!(
            rendered
                .storage
                .contains("<ac:task-status>incomplete</ac:task-status>")
        );
        assert!(
            rendered
                .storage
                .contains("<ac:task-status>complete</ac:task-status>")
        );
    }

    #[test]
    fn attachment_image_and_link_macros_become_markdown_paths() {
        let storage = r#"<p><ac:image ac:alt="Logo"><ri:attachment ri:filename="logo.png" /></ac:image> <ac:link><ri:attachment ri:filename="manual.pdf" /><ac:plain-text-link-body><![CDATA[Manual]]></ac:plain-text-link-body></ac:link></p>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains("![Logo](attachments/logo.png)"));
        assert!(markdown.contains("[Manual](attachments/manual.pdf)"));
    }

    #[test]
    fn page_link_macros_export_to_placeholders() {
        let storage = r#"<p><ac:link ac:anchor="intro"><ri:page ri:space-key="TEST" ri:content-title="Docs Home" /><ac:plain-text-link-body><![CDATA[Docs]]></ac:plain-text-link-body></ac:link></p>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains("[Docs](confluence-page://page?"));
        assert!(markdown.contains("space-key=TEST"));
        assert!(markdown.contains("content-title=Docs+Home#intro"));
    }

    #[test]
    fn user_mentions_export_to_placeholders() {
        let storage = r#"<p><ac:link><ri:user ri:account-id="abc123" /><ac:plain-text-link-body><![CDATA[@Ruben]]></ac:plain-text-link-body></ac:link></p>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains("[@Ruben](confluence-user://user?account-id=abc123)"));
    }

    #[test]
    fn status_macros_export_to_placeholders() {
        let storage = r#"<p>State: <ac:structured-macro ac:name="status"><ac:parameter ac:name="title">Ready</ac:parameter><ac:parameter ac:name="colour">Green</ac:parameter></ac:structured-macro></p>"#;
        let markdown = storage_to_markdown(storage);
        assert!(
            markdown
                .contains("State: [Ready](confluence-status://status?title=Ready&colour=Green)")
        );
    }

    #[test]
    fn excerpt_macros_export_to_excerpt_blocks() {
        let storage = r#"<ac:structured-macro ac:name="excerpt"><ac:parameter ac:name="hidden">true</ac:parameter><ac:rich-text-body><p>Hello</p><ul><li>World</li></ul></ac:rich-text-body></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-excerpt"));
        assert!(markdown.contains("hidden: true"));
        assert!(markdown.contains("---"));
        assert!(markdown.contains("Hello"));
        assert!(markdown.contains("- World"));
    }

    #[test]
    fn toc_macros_export_to_toc_blocks() {
        let storage = r#"<ac:structured-macro ac:name="toc"><ac:parameter ac:name="maxLevel">3</ac:parameter><ac:parameter ac:name="style">square</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-toc"));
        assert!(markdown.contains("maxLevel: 3"));
        assert!(markdown.contains("style: square"));
    }

    #[test]
    fn excerpt_include_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="excerpt-include"><ac:parameter ac:name="nopanel">true</ac:parameter><ri:page ri:space-key="TEST" ri:content-title="Docs Home" /></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-excerpt-include"));
        assert!(markdown.contains("nopanel: true"));
        assert!(markdown.contains("page: confluence-page://page?"));
        assert!(markdown.contains("content-title=Docs+Home"));
    }

    #[test]
    fn excerpt_include_default_parameter_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="excerpt-include"><ac:parameter ac:name="default-parameter">TEST:Docs Home</ac:parameter><ac:parameter ac:name="nopanel">true</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-excerpt-include"));
        assert!(markdown.contains("nopanel: true"));
        assert!(markdown.contains("page: confluence-page://page?"));
        assert!(markdown.contains("content-title=Docs+Home"));
        assert!(!markdown.contains("default-parameter:"));
    }

    #[test]
    fn include_page_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="include"><ac:parameter ac:name=""><ac:link><ri:page ri:space-key="TEST" ri:content-title="Docs Home" /></ac:link></ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-include-page"));
        assert!(markdown.contains("page: confluence-page://page?"));
        assert!(markdown.contains("content-title=Docs+Home"));
    }

    #[test]
    fn page_tree_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="pagetree"><ac:parameter ac:name="root"><ac:link><ri:page ri:space-key="TEST" ri:content-title="Docs Home" /></ac:link></ac:parameter><ac:parameter ac:name="searchBox">true</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-page-tree"));
        assert!(markdown.contains("root: confluence-page://page?"));
        assert!(markdown.contains("content-title=Docs+Home"));
        assert!(markdown.contains("searchBox: true"));
    }

    #[test]
    fn page_tree_search_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="pagetreesearch"><ac:parameter ac:name="root">TEST:Docs Home</ac:parameter><ac:parameter ac:name="spaceKey">TEST</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-page-tree-search"));
        assert!(markdown.contains("root: confluence-page://page?"));
        assert!(markdown.contains("content-title=Docs+Home"));
        assert!(markdown.contains("spaceKey: TEST"));
    }

    #[test]
    fn content_by_label_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="contentbylabel"><ac:parameter ac:name="cql">label = "e2e-macro-target"</ac:parameter><ac:parameter ac:name="maxResults">5</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-content-by-label"));
        assert!(markdown.contains(r#"cql: label = "e2e-macro-target""#));
        assert!(markdown.contains("maxResults: 5"));
    }

    #[test]
    fn recently_updated_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="recently-updated"><ac:parameter ac:name="spaces"><ri:space ri:space-key="TEST" /></ac:parameter><ac:parameter ac:name="max">10</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-recently-updated"));
        assert!(markdown.contains("spaces: TEST"));
        assert!(markdown.contains("max: 10"));
    }

    #[test]
    fn whitespace_between_supported_blocks_does_not_force_fallback_export() {
        let storage = concat!(
            "<h1>Macro Source</h1>\n",
            "<ac:structured-macro ac:name=\"excerpt-include\">",
            "<ac:parameter ac:name=\"default-parameter\">TEST:Docs Home</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"include\">",
            "<ri:page ri:content-title=\"Shared Page\" />",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"pagetree\">",
            "<ac:parameter ac:name=\"root\"><ac:link><ri:page ri:space-key=\"TEST\" ri:content-title=\"Docs Home\" /></ac:link></ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"pagetreesearch\">",
            "<ac:parameter ac:name=\"root\">TEST:Docs Home</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"contentbylabel\">",
            "<ac:parameter ac:name=\"cql\">label = &quot;e2e-macro-target&quot;</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"recently-updated\">",
            "<ac:parameter ac:name=\"spaces\">TEST</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"children\">",
            "<ac:parameter ac:name=\"all\">true</ac:parameter>",
            "</ac:structured-macro>"
        );
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains("# Macro Source"));
        assert!(markdown.contains(":::confluence-excerpt-include"));
        assert!(markdown.contains(":::confluence-include-page"));
        assert!(markdown.contains(":::confluence-page-tree"));
        assert!(markdown.contains(":::confluence-page-tree-search"));
        assert!(markdown.contains(":::confluence-content-by-label"));
        assert!(markdown.contains(":::confluence-recently-updated"));
        assert!(markdown.contains(":::confluence-children"));
        assert!(!markdown.contains("CONFLUENCE_XML_PLACEHOLDER"));
    }

    #[test]
    fn children_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="children"><ac:parameter ac:name="all">true</ac:parameter><ac:parameter ac:name="sort">creation</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-children"));
        assert!(markdown.contains("all: true"));
        assert!(markdown.contains("sort: creation"));
    }

    #[test]
    fn layouts_export_to_section_blocks() {
        let storage = r#"<ac:layout><ac:layout-section ac:type="two_equal" ac:breakout-mode="default"><ac:layout-cell><h2>Left</h2><p>Alpha</p></ac:layout-cell><ac:layout-cell><p>Right</p></ac:layout-cell></ac:layout-section><ac:layout-section ac:type="single"><ac:layout-cell><p>Bottom</p></ac:layout-cell></ac:layout-section></ac:layout>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains("~~~~confluence-layout-section two_equal"));
        assert!(markdown.contains("breakout-mode: default"));
        assert!(markdown.contains("--- cell ---"));
        assert!(markdown.contains("## Left"));
        assert!(markdown.contains("Alpha"));
        assert!(markdown.contains("~~~~confluence-layout-section single"));
        assert!(markdown.contains("Bottom"));
    }

    #[test]
    fn supported_panel_macros_export_to_macro_blocks() {
        let storage = r#"<ac:structured-macro ac:name="info"><ac:rich-text-body><p>Hello</p><ul><li>World</li></ul></ac:rich-text-body></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-info"));
        assert!(markdown.contains("Hello"));
        assert!(markdown.contains("- World"));
    }

    #[test]
    fn expand_macros_export_to_expand_blocks() {
        let storage = r#"<ac:structured-macro ac:name="expand"><ac:parameter ac:name="title">Details</ac:parameter><ac:rich-text-body><p>Hello</p><ul><li>World</li></ul></ac:rich-text-body></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-expand Details"));
        assert!(markdown.contains("Hello"));
        assert!(markdown.contains("- World"));
    }

    #[test]
    fn macro_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-warning\n## Heads up\n\nBody text.\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("macro block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="warning">"#)
        );
        assert!(rendered.storage.contains("<ac:rich-text-body>"));
        assert!(rendered.storage.contains("<h2>Heads up</h2>"));
        assert!(rendered.storage.contains("<p>Body text.</p>"));
    }

    #[test]
    fn expand_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-expand Details\n## Heads up\n\nBody text.\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("expand block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="expand">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="title">Details</ac:parameter>"#)
        );
        assert!(rendered.storage.contains("<ac:rich-text-body>"));
        assert!(rendered.storage.contains("<h2>Heads up</h2>"));
        assert!(rendered.storage.contains("<p>Body text.</p>"));
    }

    #[test]
    fn excerpt_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-excerpt\nhidden: true\n---\n## Heads up\n\nBody text.\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("excerpt block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="excerpt">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="hidden">true</ac:parameter>"#)
        );
        assert!(rendered.storage.contains("<ac:rich-text-body>"));
        assert!(rendered.storage.contains("<h2>Heads up</h2>"));
        assert!(rendered.storage.contains("<p>Body text.</p>"));
    }

    #[test]
    fn toc_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-toc\nmaxLevel: 3\nstyle: square\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("toc block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="toc">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="maxLevel">3</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="style">square</ac:parameter>"#)
        );
    }

    #[test]
    fn excerpt_include_blocks_round_trip_back_to_structured_macros() {
        let page = build_page_placeholder_url(&PageLinkPlaceholder {
            space_key: Some("TEST".to_string()),
            content_title: Some("Docs Home".to_string()),
            ..PageLinkPlaceholder::default()
        });
        let markdown = format!(":::confluence-excerpt-include\nnopanel: true\npage: {page}\n:::");
        let rendered =
            markdown_to_storage(&markdown, false).expect("excerpt-include block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="excerpt-include">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="nopanel">true</ac:parameter>"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="default-parameter">TEST:Docs Home</ac:parameter>"#
        ));
    }

    #[test]
    fn include_page_blocks_round_trip_back_to_structured_macros() {
        let page = build_page_placeholder_url(&PageLinkPlaceholder {
            space_key: Some("TEST".to_string()),
            content_title: Some("Docs Home".to_string()),
            ..PageLinkPlaceholder::default()
        });
        let markdown = format!(":::confluence-include-page\npage: {page}\n:::");
        let rendered = markdown_to_storage(&markdown, false).expect("include-page block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="include">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name=""><ac:link>"#)
        );
        assert!(rendered.storage.contains(r#"ri:content-title="Docs Home""#));
        assert!(rendered.storage.contains(r#"ri:space-key="TEST""#));
    }

    #[test]
    fn page_tree_blocks_round_trip_back_to_structured_macros() {
        let page = build_page_placeholder_url(&PageLinkPlaceholder {
            space_key: Some("TEST".to_string()),
            content_title: Some("Docs Home".to_string()),
            ..PageLinkPlaceholder::default()
        });
        let markdown = format!(":::confluence-page-tree\nroot: {page}\nsearchBox: true\n:::");
        let rendered = markdown_to_storage(&markdown, false).expect("page-tree block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="pagetree">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="root"><ac:link><ri:page ri:content-title="Docs Home" ri:space-key="TEST" /></ac:link></ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="searchBox">true</ac:parameter>"#)
        );
    }

    #[test]
    fn page_tree_search_blocks_round_trip_back_to_structured_macros() {
        let page = build_page_placeholder_url(&PageLinkPlaceholder {
            space_key: Some("TEST".to_string()),
            content_title: Some("Docs Home".to_string()),
            ..PageLinkPlaceholder::default()
        });
        let markdown = format!(":::confluence-page-tree-search\nroot: {page}\nspaceKey: TEST\n:::");
        let rendered =
            markdown_to_storage(&markdown, false).expect("page-tree-search block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="pagetreesearch">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="root">TEST:Docs Home</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="spaceKey">TEST</ac:parameter>"#)
        );
    }

    #[test]
    fn content_by_label_blocks_round_trip_back_to_structured_macros() {
        let markdown =
            ":::confluence-content-by-label\ncql: label = \"e2e-macro-target\"\nmaxResults: 5\n:::";
        let rendered =
            markdown_to_storage(markdown, false).expect("content-by-label block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="contentbylabel">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="cql">label = &quot;e2e-macro-target&quot;</ac:parameter>"#
        ));
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="maxResults">5</ac:parameter>"#)
        );
    }

    #[test]
    fn recently_updated_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-recently-updated\nspaces: TEST\nmax: 10\n:::";
        let rendered =
            markdown_to_storage(markdown, false).expect("recently-updated block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="recently-updated">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="spaces"><ri:space ri:space-key="TEST" /></ac:parameter>"#
        ));
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="max">10</ac:parameter>"#)
        );
    }

    #[test]
    fn children_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-children\nall: true\nsort: creation\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("children block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="children">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="all">true</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="sort">creation</ac:parameter>"#)
        );
    }

    #[test]
    fn layout_section_blocks_round_trip_back_to_layout_storage() {
        let markdown = r#"~~~~confluence-layout-section two_equal
breakout-mode: default
--- cell ---
## Left

Alpha
--- cell ---
:::confluence-info
Right
:::
~~~~

~~~~confluence-layout-section single
--- cell ---
Bottom
~~~~"#;
        let rendered = markdown_to_storage(markdown, false).expect("layout block converts");
        assert!(rendered.storage.contains("<ac:layout>"));
        assert!(
            rendered
                .storage
                .contains(r#"<ac:layout-section ac:type="two_equal" ac:breakout-mode="default">"#)
        );
        assert!(rendered.storage.contains("<ac:layout-cell><h2>Left</h2>"));
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="info">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:layout-section ac:type="single">"#)
        );
        assert!(rendered.storage.contains("<p>Bottom</p>"));
    }

    #[test]
    fn code_macros_export_to_confluence_code_fences() {
        let storage = r#"<ac:structured-macro ac:name="code"><ac:parameter ac:name="language">rust</ac:parameter><ac:parameter ac:name="title">main.rs</ac:parameter><ac:plain-text-body><![CDATA[fn main() {
    println!("hello");
}]]></ac:plain-text-body></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains("~~~confluence-code rust"));
        assert!(markdown.contains("title: main.rs"));
        assert!(markdown.contains("---"));
        assert!(markdown.contains("println!(\"hello\");"));
    }

    #[test]
    fn code_macro_fences_round_trip_back_to_structured_macros() {
        let markdown = r#"~~~confluence-code rust
title: main.rs
linenumbers: true
---
fn main() {
    println!("hello");
}
~~~"#;
        let rendered = markdown_to_storage(markdown, false).expect("code macro converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="code">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="language">rust</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="title">main.rs</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="linenumbers">true</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains("<ac:plain-text-body><![CDATA[fn main() {")
        );
    }

    #[test]
    fn code_macro_fences_escape_cdata_terminators() {
        let markdown = r#"~~~confluence-code sql
SELECT ']]>' AS sentinel;
~~~"#;
        let rendered = markdown_to_storage(markdown, false).expect("code macro converts");
        assert!(rendered.storage.contains("]]]]><![CDATA[>"));
    }

    #[test]
    fn simple_tables_export_to_markdown_tables() {
        let storage = r#"<table><tbody><tr><th>Name</th><th>Status</th></tr><tr><td>CLI</td><td>Ready</td></tr></tbody></table>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains("| Name | Status |"));
        assert!(markdown.contains("| --- | --- |"));
        assert!(markdown.contains("| CLI | Ready |"));
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
