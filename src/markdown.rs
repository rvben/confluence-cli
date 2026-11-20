use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd, html};
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
    /// Title as of the last pull/apply, used by plan to detect title changes.
    #[serde(default)]
    pub last_pulled_title: Option<String>,
    /// Labels as of the last pull/apply, used by plan to detect label changes.
    #[serde(default)]
    pub last_pulled_labels: Vec<String>,
    /// Status as of the last pull/apply, used by plan to detect status changes.
    #[serde(default)]
    pub last_pulled_status: Option<String>,
    /// SHA-256 of the JSON-serialised properties map as of the last pull/apply.
    #[serde(default)]
    pub last_pulled_properties_hash: Option<String>,
    /// Informational parent value written to frontmatter at the last pull/apply.
    /// The nested option distinguishes a root page from a legacy sidecar.
    #[serde(default)]
    pub last_pulled_parent: Option<Option<String>>,
    /// Markdown body as of the last pull/apply, used to render a real plan diff.
    #[serde(default)]
    pub last_pulled_body_markdown: Option<String>,
    /// Whether the storage this document converts to says something else than
    /// the storage it was pulled from. The first body change applied to such
    /// a page also publishes that difference.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub storage_differs_from_remote: bool,
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

impl PageLinkPlaceholder {
    /// Link text given to a page link that has no link body in storage.
    /// A link still carrying this text is written back without a body.
    pub(crate) fn default_label(&self) -> String {
        self.content_title
            .clone()
            .or_else(|| self.content_id.clone())
            .or_else(|| self.anchor.clone())
            .unwrap_or_else(|| "Confluence page".to_string())
    }
}

impl UserMentionPlaceholder {
    /// Link text given to a mention that has no link body in storage.
    /// Markdown links need visible text; a mention still carrying this text
    /// is written back without a body.
    pub(crate) fn default_label(&self) -> String {
        self.username
            .as_deref()
            .map(|username| format!("@{username}"))
            .or_else(|| self.account_id.clone())
            .or_else(|| self.user_key.clone())
            .unwrap_or_else(|| "@user".to_string())
    }
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
        if let Some(block) = render_top_level_block(child, &wrapped)
            && !block.trim().is_empty()
        {
            blocks.push(block.trim().to_string());
        }
    }
    Some(blocks.join("\n\n"))
}

fn storage_to_markdown_fallback(storage: &str) -> String {
    let mut xml_fragments = Vec::new();
    let mut normalized = String::with_capacity(storage.len());
    // Letters and digits only, which the HTML converter writes out as they
    // are. The digest keeps the storage itself from spelling the marker.
    let digest = Sha256::digest(storage.as_bytes());
    let digest: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect();
    let marker = format!("CONFLUENCEXMLPLACEHOLDER{digest}N");

    let mut rest = 0;
    for element in confluence_element_ranges(storage) {
        normalized.push_str(&storage[rest..element.start]);
        normalized.push_str(&format!("{marker}{}END", xml_fragments.len()));
        rest = element.end;
        xml_fragments.push(&storage[element]);
    }
    normalized.push_str(&storage[rest..]);

    let markdown = html2md::parse_html(&normalized);
    let placeholder = Regex::new(&format!("{marker}([0-9]+)END")).expect("valid regex");
    let markdown = placeholder.replace_all(&markdown, |captures: &regex::Captures<'_>| {
        match captures[1]
            .parse::<usize>()
            .ok()
            .and_then(|idx| xml_fragments.get(idx))
        {
            Some(fragment) => confluence_raw_block(fragment),
            None => captures[0].to_string(),
        }
    });
    markdown.trim().to_string()
}

/// Finds the outermost Confluence elements in storage that is not well
/// formed, each from its opening tag to the closing tag that matches it. An
/// element that is never closed is passed over, and the ones it holds are
/// found on their own.
fn confluence_element_ranges(storage: &str) -> Vec<Range<usize>> {
    static TAG: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?s)<!\[CDATA\[.*?\]\]>|<!--.*?-->|<(/?)((?:ac|ri):[\w-]+)(?:\s(?:[^>"']|"[^"]*"|'[^']*')*?)?(/?)>"#,
        )
            .expect("valid Confluence tag regex")
    });
    struct Tag<'a> {
        range: Range<usize>,
        name: &'a str,
        closing: bool,
        empty: bool,
    }
    // A character data section or a comment matches without a name, which
    // keeps tags spelled inside one out of the list.
    let tags: Vec<Tag<'_>> = TAG
        .captures_iter(storage)
        .filter_map(|captures| {
            Some(Tag {
                range: captures.get(0)?.range(),
                name: captures.get(2)?.as_str(),
                closing: !captures[1].is_empty(),
                empty: !captures[3].is_empty(),
            })
        })
        .collect();

    let mut ranges = Vec::new();
    let mut index = 0;
    while index < tags.len() {
        let tag = &tags[index];
        index += 1;
        if tag.closing {
            continue;
        }
        if tag.empty {
            ranges.push(tag.range.clone());
            continue;
        }
        let mut depth = 1;
        for (offset, other) in tags[index..].iter().enumerate() {
            if other.name != tag.name || other.empty {
                continue;
            }
            depth += if other.closing { -1 } else { 1 };
            if depth == 0 {
                ranges.push(tag.range.start..other.range.end);
                index += offset + 1;
                break;
            }
        }
    }
    ranges
}

pub fn markdown_to_storage(markdown: &str, allow_lossy: bool) -> Result<ConversionOutput> {
    markdown_to_storage_in_file(markdown, allow_lossy, Some(0))
}

/// Converts Markdown that follows `lines_before` lines of its file, so that
/// an invalid body is reported with the lines of that file. Without a known
/// number of lines, no Markdown lines are reported.
pub(crate) fn markdown_to_storage_in_file(
    markdown: &str,
    allow_lossy: bool,
    lines_before: Option<usize>,
) -> Result<ConversionOutput> {
    let converted = render_markdown_storage(markdown, allow_lossy)?;
    validate_converted_markdown(&converted.storage, markdown, allow_lossy, lines_before)?;
    Ok(converted)
}

/// Validates the storage `markdown` converts to. An invalid body is reported
/// with the lines of the Markdown block at which it goes wrong, counted from
/// one after `lines_before` lines.
pub(crate) fn validate_converted_markdown(
    storage: &str,
    markdown: &str,
    allow_lossy: bool,
    lines_before: Option<usize>,
) -> Result<()> {
    crate::storage::validate_generated_storage(storage).map_err(|error| {
        let mut error = match error.downcast::<crate::output::CliError>() {
            Ok(error) => error,
            Err(error) => return error,
        };
        let lines = lines_before.and_then(|lines_before| {
            let details = error.details.as_ref()?;
            let reason = details.get("reason")?.as_str()?;
            let (end_line, end_column) = crate::storage::end_line_and_column(storage);
            let at_end = details.get("line")?.as_u64()? == u64::from(end_line)
                && details.get("column")?.as_u64()? == u64::from(end_column);
            let (first, last) = lines_of_invalid_block(markdown, allow_lossy, reason, at_end)?;
            Some((lines_before + first, lines_before + last))
        });
        if let Some((first, last)) = lines {
            let lines = if first == last {
                format!("Markdown line {first}")
            } else {
                format!("Markdown lines {first}-{last}")
            };
            if let Some(details) = &error.details {
                error.message = format!(
                    "invalid storage generated from {lines}: {} (line {}, column {} of the generated storage)",
                    details["reason"].as_str().unwrap_or_default(),
                    details["line"],
                    details["column"],
                );
            }
            error.hint = Some("Check raw HTML and confluence-storage blocks in the named Markdown lines. The other line and column refer to the generated storage XML. --allow-lossy does not permit malformed XML.".to_string());
            if let Some(details) = &mut error.details {
                details["markdown_start_line"] = serde_json::json!(first);
                details["markdown_end_line"] = serde_json::json!(last);
            }
        }
        error.into()
    })
}

/// How many conversions of the whole Markdown the search for a block left
/// open may cost before it names no block.
const END_SEARCH_CONVERSIONS: usize = 64;

/// The first and last line, counted from one, of the top-level block of
/// `markdown` with which the converted storage becomes invalid for `reason`
/// and stays so: the storage of the Markdown up to that block and up to every
/// later block is invalid for the same reason, and the storage of the
/// Markdown before it is not.
///
/// `at_end` says that the storage is invalid where it ends, which is how an
/// element that is never closed is reported. Markup left open in one block
/// and closed in a later one leaves the storage open for the blocks between
/// them only, so the block named is the one after the last block up to which
/// nothing is open, whichever element the storage up to a block in between
/// reports. Finding it converts the Markdown up to each block from the last
/// one back.
/// That work is bounded to `END_SEARCH_CONVERSIONS` conversions of the whole
/// Markdown, beyond which no block is named. Any other reason stays once the
/// block that holds its cause is reached, and a search by halves finds that
/// block.
fn lines_of_invalid_block(
    markdown: &str,
    allow_lossy: bool,
    reason: &str,
    at_end: bool,
) -> Option<(usize, usize)> {
    let mut blocks = Vec::new();
    let mut depth = 0usize;
    for (event, range) in Parser::new_ext(markdown, markdown_options()).into_offset_iter() {
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    blocks.push(range);
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            _ if depth == 0 => blocks.push(range),
            _ => {}
        }
    }
    let invalid_through = |block: &Range<usize>| {
        render_markdown_storage(&markdown[..block.end], allow_lossy)
            .ok()
            .and_then(|converted| crate::storage::invalid_reason(&converted.storage))
            .as_deref()
            == Some(reason)
    };
    let valid_blocks = if at_end {
        let mut converted_bytes = 0;
        let mut valid_blocks = 0;
        for (index, block) in blocks.iter().enumerate().rev() {
            converted_bytes += block.end;
            if converted_bytes > END_SEARCH_CONVERSIONS * markdown.len() {
                return None;
            }
            let open_through = render_markdown_storage(&markdown[..block.end], allow_lossy)
                .is_ok_and(|converted| crate::storage::is_invalid_at_end(&converted.storage));
            if !open_through {
                valid_blocks = index + 1;
                break;
            }
        }
        valid_blocks
    } else {
        blocks.partition_point(|block| !invalid_through(block))
    };
    let block = blocks.get(valid_blocks)?.clone();
    let line_of = |offset: usize| markdown[..offset].matches('\n').count() + 1;
    let text = &markdown[block.clone()];
    let last = block.start + text.trim_end().len();
    Some((line_of(block.start), line_of(last)))
}

/// Render a diagnostic artifact even if its embedded storage XML is malformed.
/// Remote write paths must use markdown_to_storage instead.
pub(crate) fn render_markdown_storage(
    markdown: &str,
    allow_lossy: bool,
) -> Result<ConversionOutput> {
    let (normalized_layouts, layout_fragments) = replace_layout_blocks(markdown, allow_lossy)?;
    let (normalized_macros, macro_fragments) =
        replace_confluence_macro_blocks(&normalized_layouts, allow_lossy)?;
    let mut raw_fragments = StorageFragments::for_markdown(&normalized_macros);
    let normalized = replace_inline_storage_fences(&normalized_macros, &mut raw_fragments);

    let escapes_confluence_markup = has_unescaped_confluence_markup(&normalized);
    let events = parse_with_storage_fences(&normalized, &mut raw_fragments);
    let mut html_output = String::new();
    html::push_html(&mut html_output, events.into_iter());
    // Raw storage blocks and macros are restored afterwards, so they stay as
    // written, and link bodies are not yet wrapped as literal text.
    let html_output = convert_checkbox_lists_to_task_lists(&html_output);
    let mut html_output = convert_generated_markup(&html_output);
    html_output = raw_fragments.restore(&html_output);
    for idx in (0..layout_fragments.len()).rev() {
        let fragment = &layout_fragments[idx];
        html_output = html_output.replace(
            &format!("<p>CONFLUENCE_LAYOUT_PLACEHOLDER_{idx}</p>"),
            fragment,
        );
        html_output =
            html_output.replace(&format!("CONFLUENCE_LAYOUT_PLACEHOLDER_{idx}"), fragment);
    }
    for idx in (0..macro_fragments.len()).rev() {
        let fragment = &macro_fragments[idx];
        html_output = html_output.replace(
            &format!("<p>CONFLUENCE_MACRO_PLACEHOLDER_{idx}</p>"),
            fragment,
        );
        html_output = html_output.replace(&format!("CONFLUENCE_MACRO_PLACEHOLDER_{idx}"), fragment);
    }

    let mut lossy = Vec::new();
    if escapes_confluence_markup {
        lossy.push("raw Confluence XML was escaped during Markdown conversion".to_string());
    }
    if !allow_lossy && !lossy.is_empty() {
        bail!(lossy.join("; "));
    }

    let storage = html_output.trim().to_string();
    Ok(ConversionOutput { storage })
}

const STORAGE_FENCE_INFO: &str = "confluence-storage";

/// The storage fragments taken out of a Markdown document while it is parsed
/// and rendered, each with a placeholder standing in for it.
struct StorageFragments {
    /// The text every placeholder starts with. It carries a digest of the
    /// document, so nothing the document says can spell it, however the
    /// text is escaped or encoded.
    marker: String,
    fragments: Vec<String>,
}

impl StorageFragments {
    fn for_markdown(markdown: &str) -> Self {
        let digest = Sha256::digest(markdown.as_bytes());
        let digest: String = digest[..8]
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect();
        Self {
            marker: format!("CONFLUENCE_XML_PLACEHOLDER_{digest}"),
            fragments: Vec::new(),
        }
    }

    /// Keeps a fragment and returns its placeholder. The placeholder ends in
    /// a letter so that digits following it are not read as part of its
    /// number.
    fn hold(&mut self, fragment: String) -> String {
        let placeholder = format!("{}_{}_END", self.marker, self.fragments.len());
        self.fragments.push(fragment);
        placeholder
    }

    /// Puts the fragments back in place of their placeholders, together with
    /// the paragraph the parser wraps around a placeholder standing alone.
    ///
    /// The fragments go in during a single pass over the rendered HTML, so
    /// nothing inside a fragment is replaced.
    fn restore(&self, html: &str) -> String {
        let mut out = String::with_capacity(html.len());
        let mut rest = html;
        while let Some(at) = rest.find(&self.marker) {
            let (before, after) = rest.split_at(at);
            let Some((fragment, after)) = self.fragment_at(&after[self.marker.len()..]) else {
                out.push_str(before);
                out.push_str(&self.marker);
                rest = &after[self.marker.len()..];
                continue;
            };
            let (before, after) = match (before.strip_suffix("<p>"), after.strip_prefix("</p>")) {
                (Some(before), Some(after)) => (before, after),
                _ => (before, after),
            };
            out.push_str(before);
            out.push_str(fragment);
            rest = after;
        }
        out.push_str(rest);
        out
    }

    /// Reads the rest of a placeholder after its marker, returning the
    /// fragment it stands for and the text that follows it.
    fn fragment_at<'t>(&self, text: &'t str) -> Option<(&str, &'t str)> {
        let text = text.strip_prefix('_')?;
        let digits = text.len()
            - text
                .trim_start_matches(|ch: char| ch.is_ascii_digit())
                .len();
        let (index, text) = text.split_at(digits);
        let fragment = self.fragments.get(index.parse::<usize>().ok()?)?;
        Some((fragment, text.strip_prefix("_END")?))
    }
}

/// Parses Markdown and swaps every `confluence-storage` code block for a
/// placeholder, adding the storage it holds to `fragments`.
///
/// The parser decides what a fenced block is, so a fence inside a block quote
/// or a list item comes without its container's markers, and one shown inside
/// another code block stays text.
fn parse_with_storage_fences<'a>(
    markdown: &'a str,
    fragments: &mut StorageFragments,
) -> Vec<Event<'a>> {
    let mut events = Vec::new();
    // The events and source of the storage block being read.
    let mut open: Option<(Vec<Event<'a>>, &'a str)> = None;
    for (event, range) in Parser::new_ext(markdown, markdown_options()).into_offset_iter() {
        match (event, &mut open) {
            (Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))), None)
                if info.trim() == STORAGE_FENCE_INFO =>
            {
                let start = Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info)));
                open = Some((vec![start], &markdown[range]));
            }
            (Event::End(TagEnd::CodeBlock), Some((block, source))) => {
                let content: String = block
                    .iter()
                    .filter_map(|event| match event {
                        Event::Text(text) => Some(text.as_ref()),
                        _ => None,
                    })
                    .collect();
                if has_closing_fence(source, &content) {
                    // The last newline belongs to the closing line.
                    let fragment = content.strip_suffix('\n').unwrap_or(&content);
                    let placeholder = fragments.hold(fragment.to_string());
                    events.push(Event::Html(format!("{placeholder}\n").into()));
                } else {
                    events.append(block);
                    events.push(Event::End(TagEnd::CodeBlock));
                }
                open = None;
            }
            (event, Some((block, _))) => block.push(event),
            (event, None) => events.push(event),
        }
    }
    events
}

/// Whether a fenced code block ends with a closing fence, given its source
/// and the content the parser read from it. A block that runs to the end of
/// its container without one is left as code.
///
/// The source holds the opening line, one line for each line of content, and
/// the closing line if there is one.
fn has_closing_fence(source: &str, content: &str) -> bool {
    let source_lines = source.trim_end_matches(['\n', '\r']).split('\n').count();
    let content_lines = content.split_inclusive('\n').count();
    source_lines == content_lines + 2
}

/// Replaces `confluence-storage` fences that are not code blocks with a
/// placeholder, adding the storage they hold to `fragments`.
///
/// The fallback for malformed storage writes fragments in the middle of a
/// paragraph, so that other text shares a line with the opening fence, the
/// closing fence, or both. The parser reads no storage block there, so such a
/// fence is matched as text: it closes at the first line that starts with as
/// many backticks as opened it. Fences the parser reads as closed code blocks
/// are left to it, along with everything else inside a code block.
fn replace_inline_storage_fences(markdown: &str, fragments: &mut StorageFragments) -> String {
    static OPENING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(`{3,})confluence-storage[\s&&[^\n]]*\n").expect("valid regex")
    });

    if !OPENING.is_match(markdown) {
        return markdown.to_string();
    }
    let code_blocks: Vec<Range<usize>> = Parser::new_ext(markdown, markdown_options())
        .into_offset_iter()
        .filter_map(|(event, range)| {
            matches!(event, Event::Start(Tag::CodeBlock(_))).then_some(range)
        })
        .collect();

    let mut out = String::with_capacity(markdown.len());
    let mut cursor = 0;
    let mut search_from = 0;
    // The shortest fence found to have no closing line. No longer fence that
    // opens later has one either.
    let mut unclosed = usize::MAX;
    // Taking a fence out of the text changes where the code blocks after it
    // are, so from the first one on the fences are matched as text alone.
    let mut by_text_alone = false;
    while let Some(captures) = OPENING.captures_at(markdown, search_from) {
        let opening = captures.get(0).expect("whole match");
        let ticks = &captures[1];
        let closing = format!("\n{ticks}");
        let body_start = opening.end() - 1;
        search_from = opening.end();

        // Code blocks come in source order and do not overlap.
        let after = code_blocks.partition_point(|block| block.end <= opening.start());
        let block = code_blocks
            .get(after)
            .filter(|block| !by_text_alone && block.start <= opening.start());
        let at = match block {
            Some(block) if !is_container_prefix(&markdown[block.start..opening.start()]) => {
                // The fence is text inside another code block.
                search_from = block.end.max(opening.end());
                continue;
            }
            Some(block) => {
                // The fence opens a code block of its own. It is a fragment
                // in a paragraph when other text stands on the line before or
                // after it, or when something other than spaces follows the
                // backticks that close it, which the parser reads as one
                // more line of code.
                let end = block.end.max(body_start);
                let joins_other_text = !stands_apart(markdown, block);
                let fragment_end = markdown[body_start..end].find(&closing).filter(|at| {
                    let after_ticks = &markdown[body_start + at + closing.len()..];
                    let rest_of_line = after_ticks.lines().next().unwrap_or_default();
                    joins_other_text
                        || !rest_of_line
                            .trim_start_matches('`')
                            .trim_matches(' ')
                            .is_empty()
                });
                let Some(at) = fragment_end else {
                    search_from = end.max(opening.end());
                    continue;
                };
                at
            }
            None if ticks.len() >= unclosed => continue,
            None => {
                let Some(at) = markdown[body_start..].find(&closing) else {
                    unclosed = ticks.len();
                    continue;
                };
                at
            }
        };

        // The fragment starts after the opening line and any blank lines.
        let mut body = &markdown[body_start..body_start + at];
        while let Some((line, rest)) = body.split_once('\n')
            && line.trim().is_empty()
        {
            body = rest;
        }
        out.push_str(&markdown[cursor..opening.start()]);
        out.push_str(&fragments.hold(body.to_string()));
        cursor = body_start + at + closing.len();
        search_from = cursor;
        by_text_alone = true;
    }
    out.push_str(&markdown[cursor..]);
    out
}

/// Whether a code block stands apart from the text around it: the line
/// before it and the line after it are blank or missing.
fn stands_apart(markdown: &str, block: &Range<usize>) -> bool {
    let line_start = markdown[..block.start].rfind('\n').map_or(0, |at| at + 1);
    let line_before = markdown[..line_start]
        .strip_suffix('\n')
        .map(|before| before.rsplit('\n').next().unwrap_or_default());
    let block_end = block.start + markdown[block.clone()].trim_end().len();
    let line_after = markdown[block_end..]
        .split_once('\n')
        .map(|(_, after)| after.split('\n').next().unwrap_or_default());
    [line_before, line_after]
        .into_iter()
        .flatten()
        .all(|line| is_container_prefix(line.trim_end_matches('\r')))
}

/// Whether text holds nothing but what may precede a fence on its line:
/// block quote markers and indentation.
fn is_container_prefix(text: &str) -> bool {
    text.chars().all(|ch| matches!(ch, ' ' | '\t' | '>'))
}

/// Whether Markdown text holds Confluence markup, which the renderer writes
/// out as escaped text. Markup that is escaped with a backslash, written
/// with a character reference or placed in code is meant as text.
fn has_unescaped_confluence_markup(markdown: &str) -> bool {
    let is_markup = |text: &str, start: usize| {
        ["<ac:", "<ri:"].iter().any(|markup| {
            text.match_indices(markup)
                .any(|(at, _)| !(at == 0 && markdown[..start].ends_with('\\')))
        })
    };

    // Text reaches the renderer in pieces. Pieces that are adjacent in the
    // source and read the same there are one stretch of text as written.
    let mut stretch = String::new();
    let mut stretch_range = 0..0;
    // Code is published as written, and a metadata block not at all.
    let mut code_blocks = 0usize;
    for (event, range) in Parser::new_ext(markdown, markdown_options()).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_) | Tag::MetadataBlock(_)) => code_blocks += 1,
            Event::End(TagEnd::CodeBlock | TagEnd::MetadataBlock(_)) => code_blocks -= 1,
            Event::Text(text) if code_blocks == 0 && *text == markdown[range.clone()] => {
                if range.start != stretch_range.end {
                    if is_markup(&stretch, stretch_range.start) {
                        return true;
                    }
                    stretch.clear();
                    stretch_range = range.start..range.start;
                }
                stretch.push_str(&text);
                stretch_range.end = range.end;
            }
            _ => {}
        }
    }
    is_markup(&stretch, stretch_range.start)
}

/// Converts the images and attachment links the Markdown renderer wrote to
/// the elements Confluence stores for them.
fn convert_generated_markup(html: &str) -> String {
    static IMAGE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"<img([^>]*?)src="([^"]+)"([^>]*)>"#).expect("valid image regex")
    });

    let html = rewrite_outside_literal_sections(html, rewrite_attachment_links_to_storage);
    rewrite_outside_literal_sections(&html, |markup| {
        IMAGE
            .replace_all(markup, |captures: &regex::Captures<'_>| {
                // An image with a separate end tag keeps its form, because
                // only the start tag is replaced here.
                let attributes = format!("{} {}", &captures[1], &captures[3]);
                captures[3]
                    .trim_end()
                    .ends_with('/')
                    .then(|| image_to_storage_macro(&captures[2], &attributes))
                    .flatten()
                    .unwrap_or_else(|| captures[0].to_string())
            })
            .into_owned()
    })
}

const CDATA_SECTION: (&str, &str) = ("<![CDATA[", "]]>");
const COMMENT_SECTION: (&str, &str) = ("<!--", "-->");

/// Applies `rewrite` to the markup between CDATA sections and copies the
/// sections themselves unchanged. A comment is not set apart here, so an
/// element whose content holds one is still seen whole.
pub(crate) fn rewrite_outside_cdata(storage: &str, rewrite: impl Fn(&str) -> String) -> String {
    rewrite_outside_sections(storage, [CDATA_SECTION, CDATA_SECTION], rewrite)
}

/// Applies `rewrite` to the markup between CDATA sections and comments, and
/// copies those unchanged: what they hold is text, whatever it looks like.
pub(crate) fn rewrite_outside_literal_sections(
    storage: &str,
    rewrite: impl Fn(&str) -> String,
) -> String {
    rewrite_outside_sections(storage, [CDATA_SECTION, COMMENT_SECTION], rewrite)
}

fn rewrite_outside_sections(
    storage: &str,
    sections: [(&str, &str); 2],
    rewrite: impl Fn(&str) -> String,
) -> String {
    let find_from = |opener: &str, from: usize| storage[from..].find(opener).map(|at| from + at);
    let mut rewritten = String::with_capacity(storage.len());
    let mut cursor = 0;
    // Where each kind of section next opens. A position is searched for again
    // only once the cursor has passed it, so the text is scanned once per kind.
    let mut next_open = sections.map(|(opener, _)| find_from(opener, 0));
    loop {
        for (next, (opener, _)) in next_open.iter_mut().zip(sections) {
            if next.is_some_and(|open| open < cursor) {
                *next = find_from(opener, cursor);
            }
        }
        let Some((open, (opener, closer))) = next_open
            .iter()
            .zip(sections)
            .filter_map(|(next, section)| Some(((*next)?, section)))
            .min_by_key(|(open, _)| *open)
        else {
            break;
        };
        rewritten.push_str(&rewrite(&storage[cursor..open]));
        let body = open + opener.len();
        let end = storage[body..]
            .find(closer)
            .map_or(storage.len(), |close| body + close + closer.len());
        rewritten.push_str(&storage[open..end]);
        cursor = end;
    }
    rewritten.push_str(&rewrite(&storage[cursor..]));
    rewritten
}

/// Resolves a link or image target to an attachment of the page it is
/// written on, returning the file name and any fragment. Such a target is
/// the path `attachments/<file name>`, however it is spelled.
fn own_attachment_target(target: &str) -> Option<(String, Option<String>)> {
    let target = decode_character_references(target);
    if !is_local_target(&target) {
        return None;
    }
    let (path_part, fragment) = split_target_fragment(&target);
    // A query asks for the file in some way, which a file name cannot say.
    if path_part.contains('?') {
        return None;
    }
    let path_part = urlencoding::decode(path_part).ok()?;
    let mut segments = Vec::new();
    for component in Path::new(path_part.as_ref()).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(segment) => segments.push(segment.to_str()?),
            Component::ParentDir => {
                segments.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    let ["attachments", file_name] = segments.as_slice() else {
        return None;
    };
    let fragment = match fragment {
        Some(fragment) => Some(urlencoding::decode(fragment).ok()?.into_owned()),
        None => None,
    };
    Some((file_name.to_string(), fragment))
}

/// Resolves the character references the Markdown renderer writes into
/// attribute values and text: the predefined XML entities and numeric
/// references.
pub(crate) fn decode_character_references(value: &str) -> String {
    static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"&(?:#x([0-9a-fA-F]{1,6})|#([0-9]{1,7})|(amp|lt|gt|quot|apos));")
            .expect("valid character reference regex")
    });
    if !value.contains('&') {
        return value.to_string();
    }
    REFERENCE
        .replace_all(value, |captures: &regex::Captures<'_>| {
            let code_point = if let Some(hex) = captures.get(1) {
                u32::from_str_radix(hex.as_str(), 16).ok()
            } else if let Some(decimal) = captures.get(2) {
                decimal.as_str().parse().ok()
            } else {
                return match &captures[3] {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "quot" => "\"",
                    _ => "'",
                }
                .to_string();
            };
            code_point
                .and_then(char::from_u32)
                .map_or_else(|| captures[0].to_string(), String::from)
        })
        .into_owned()
}

/// Converts an image of this page's attachment or of an external URL to the
/// `ac:image` element Confluence stores for it. Other images, and images
/// with attributes that element has no counterpart for here, keep their
/// `img` form.
fn image_to_storage_macro(src: &str, attributes: &str) -> Option<String> {
    static ATTRIBUTE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"([^\s="/]+)\s*=\s*"([^"]*)""#).expect("valid attribute regex")
    });

    let leftover = ATTRIBUTE.replace_all(attributes, "");
    if !leftover.trim().trim_matches('/').trim().is_empty() {
        return None;
    }
    // Attribute values are already escaped by the Markdown renderer.
    let mut image = String::from("<ac:image");
    for captures in ATTRIBUTE.captures_iter(attributes) {
        let storage_name = match &captures[1] {
            "alt" => "ac:alt",
            "title" => "ac:title",
            _ => return None,
        };
        if !captures[2].is_empty() {
            image.push_str(&format!(r#" {storage_name}="{}""#, &captures[2]));
        }
    }
    image.push('>');

    if let Some((file_name, None)) = own_attachment_target(src) {
        image.push_str(&format!(
            r#"<ri:attachment ri:filename="{}" />"#,
            escape_xml(&file_name)
        ));
    } else if src.starts_with("http://") || src.starts_with("https://") {
        image.push_str(&format!(r#"<ri:url ri:value="{src}" />"#));
    } else {
        return None;
    }
    image.push_str("</ac:image>");
    Some(image)
}

/// Converts links to this page's attachments to `ac:link` elements. A link
/// whose text is just the file name is written without a link body. A link
/// with further attributes, such as a title, stays an anchor, because
/// `ac:link` has nowhere to keep them.
fn rewrite_attachment_links_to_storage(html: &str) -> String {
    static LINK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?s)<a([^>]*?)href="([^"]+)"([^>]*)>(.*?)</a>"#)
            .expect("valid attachment link regex")
    });
    LINK.replace_all(html, |captures: &regex::Captures<'_>| {
        if !(captures[1].trim().is_empty() && captures[3].trim().is_empty()) {
            return captures[0].to_string();
        }
        let Some((file_name, fragment)) = own_attachment_target(&captures[2]) else {
            return captures[0].to_string();
        };
        // A link written inside the link text ends the match at its own end
        // tag, so what was matched is not the whole attachment link.
        if captures[4].contains("<a ") {
            return captures[0].to_string();
        }
        let anchor = fragment
            .map(|fragment| format!(r#" ac:anchor="{}""#, escape_xml(&fragment)))
            .unwrap_or_default();
        let escaped_name = escape_xml(&file_name);
        let body = link_body_unless_default(&captures[4], &file_name);
        format!(
            r#"<ac:link{anchor}><ri:attachment ri:filename="{escaped_name}" />{body}</ac:link>"#
        )
    })
    .into_owned()
}

/// Builds a link body, or none when the rendered link text is only the label
/// pull generates for a link that has no body in storage.
pub(crate) fn link_body_unless_default(body_html: &str, default_label: &str) -> String {
    if decode_character_references(body_html.trim()) == default_label {
        String::new()
    } else {
        build_storage_link_body(body_html)
    }
}

pub(crate) fn build_storage_link_body(body_html: &str) -> String {
    let trimmed = body_html.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.contains('<') {
        format!("<ac:link-body>{trimmed}</ac:link-body>")
    } else {
        // CDATA holds literal text, and a literal `]]>` has to be split
        // across two sections to stay inside it.
        let text = decode_character_references(trimmed).replace("]]>", "]]]]><![CDATA[>");
        format!("<ac:plain-text-link-body><![CDATA[{text}]]></ac:plain-text-link-body>")
    }
}

pub(crate) fn is_local_target(target: &str) -> bool {
    !(target.is_empty()
        || target.starts_with('#')
        || target.starts_with('/')
        || target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
        || target.starts_with("tel:")
        || target.starts_with("data:"))
}

pub(crate) fn split_target_fragment(target: &str) -> (&str, Option<&str>) {
    if let Some((path, fragment)) = target.split_once('#') {
        (path, Some(fragment))
    } else {
        (target, None)
    }
}

/// Markdown extensions honoured when converting to storage. Each one needs
/// syntax an author writes on purpose, and text pulled from a page is
/// escaped where it would read as that syntax. Extensions that reinterpret
/// ordinary prose (smart punctuation, subscript, superscript, math,
/// wikilinks) stay off, because they rewrite text a page already contains.
pub(crate) fn markdown_options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_HEADING_ATTRIBUTES
        | Options::ENABLE_DEFINITION_LIST
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_GFM
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS
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
                Some(escape_block_starts(text.trim()))
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

    let name = node.tag_name().name();
    // Markdown writes these elements as syntax that has no place for an
    // attribute. A list checks its own, since it also nests.
    let written_as_syntax = matches!(
        name,
        "p" | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "pre"
            | "blockquote"
            | "table"
            | "hr"
            | "div"
            | "section"
            | "article"
            | "main"
    );
    if written_as_syntax && has_attributes_besides(node, &[]) {
        return None;
    }

    match name {
        "p" => {
            if node.children().any(is_block_like_confluence_node) {
                return render_container_blocks(node, source);
            }
            let inline = render_inline_children(node, source)?;
            if inline.trim().is_empty() {
                None
            } else {
                Some(escape_block_starts(inline.trim()))
            }
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = node.tag_name().name()[1..].parse::<usize>().ok()?;
            let inline = render_inline_children(node, source)?;
            Some(format!(
                "{} {}",
                "#".repeat(level),
                escape_heading_end(inline.trim())
            ))
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
        "structured-macro" | "macro" => render_supported_macro_block(node, source),
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

/// Renders preformatted text as a fenced code block when it has the shape a
/// fenced block converts back to: a `<pre>` holding a single `<code>` that
/// holds text and names at most a language.
fn render_preformatted(node: Node<'_, '_>) -> Option<String> {
    let code =
        sole_child(node).filter(|child| child.is_element() && !is_confluence_node(*child))?;
    if code.tag_name().name() != "code" || code.children().any(|child| !child.is_text()) {
        return None;
    }
    let language = match code.attributes().collect::<Vec<_>>().as_slice() {
        [] => "",
        [class] if class.namespace().is_none() && class.name() == "class" => {
            fence_language(class.value())?
        }
        _ => return None,
    };
    let content: String = code.children().filter_map(|child| child.text()).collect();
    // The line break that ends the last line belongs to the fence.
    let content = content.strip_suffix('\n').unwrap_or(&content);
    let fence = backtick_fence(content);
    Some(format!("{fence}{language}\n{content}\n{fence}"))
}

/// The language a `class` attribute names, when that is all it says and the
/// name can follow a code fence.
fn fence_language(class: &str) -> Option<&str> {
    class.strip_prefix("language-").filter(|language| {
        !language.is_empty()
            && !language.contains(|character: char| {
                character.is_whitespace() || matches!(character, '`' | '\\' | '&')
            })
    })
}

/// The only child of a node, if it has exactly one.
fn sole_child<'a, 'input>(node: Node<'a, 'input>) -> Option<Node<'a, 'input>> {
    let mut children = node.children();
    let first = children.next()?;
    children.next().is_none().then_some(first)
}

/// Whether an element carries an attribute other than the ones named and the
/// identifier Confluence assigns.
fn has_attributes_besides(node: Node<'_, '_>, carried: &[&str]) -> bool {
    node.attributes().any(|attribute| {
        attribute.name() != LOCAL_ID_ATTRIBUTE
            && !(attribute.namespace().is_none() && carried.contains(&attribute.name()))
    })
}

/// The children of an element when all of them are elements of one name
/// without attributes, leaving out what renders as nothing.
fn plain_children<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<Vec<Node<'a, 'input>>> {
    node.children()
        .filter(|child| !renders_nothing(*child))
        .map(|child| {
            (child.is_element()
                && !is_confluence_node(child)
                && child.tag_name().name() == name
                && !has_attributes_besides(child, &[]))
            .then_some(child)
        })
        .collect()
}

fn render_list(node: Node<'_, '_>, source: &str, indent: usize, ordered: bool) -> Option<String> {
    if has_attributes_besides(node, if ordered { &["start"] } else { &[] }) {
        return None;
    }
    let items = plain_children(node, "li")?;
    if items.is_empty() {
        return None;
    }

    // Markdown takes the list's first number from its first marker, which
    // holds nine digits at most. A list that starts at one keeps the repeated
    // `1.` marker, and one that starts beyond what a marker holds has no
    // Markdown form.
    const LARGEST_MARKER: u64 = 999_999_999;
    let start = match node.attribute("start") {
        Some(value) => Some(value.trim().parse::<u64>().ok()?),
        None => None,
    }
    .filter(|start| *start != 1);
    if start.is_some_and(|start| start > LARGEST_MARKER) {
        return None;
    }

    let (bullet, delimiter) = if takes_alternative_marker(node) {
        ("*", ")")
    } else {
        ("-", ".")
    };

    // Whether every item keeps its text in paragraphs, which is what a loose
    // Markdown list reads back.
    let paragraphs_only = !items.iter().copied().any(has_text_outside_paragraphs);

    let mut rendered = Vec::new();
    for (position, item) in items.into_iter().enumerate() {
        let marker = match (ordered, start) {
            (false, _) => bullet.to_string(),
            (true, None) => format!("1{delimiter}"),
            (true, Some(start)) => format!(
                "{}{delimiter}",
                start.saturating_add(position as u64).min(LARGEST_MARKER)
            ),
        };
        rendered.push(render_list_item(
            item,
            source,
            indent,
            &marker,
            paragraphs_only,
        )?);
    }
    Some(rendered.join("\n"))
}

/// Whether a list has to use the alternative marker to stay a list of its
/// own. Markdown reads lists that follow each other as a single list unless
/// their markers differ, so every other one in such a run takes the
/// alternative. Bullet and task lists share the bullet markers.
fn takes_alternative_marker(list: Node<'_, '_>) -> bool {
    let is_numbered = |node: Node<'_, '_>| node.tag_name().name() == "ol";
    let is_list = |node: Node<'_, '_>| {
        node.is_element() && matches!(node.tag_name().name(), "ul" | "ol" | "task-list")
    };
    let preceding = std::iter::successors(list.prev_sibling(), |sibling| sibling.prev_sibling())
        .filter(|sibling| !renders_nothing(*sibling))
        .take_while(|sibling| is_list(*sibling) && is_numbered(*sibling) == is_numbered(list))
        .count();
    preceding % 2 == 1
}

fn render_list_item(
    node: Node<'_, '_>,
    source: &str,
    indent: usize,
    marker: &str,
    list_holds_paragraphs_only: bool,
) -> Option<String> {
    let prefix = format!("{}{} ", " ".repeat(indent), marker);
    // A nested list belongs to this item only when indented to the column
    // where the item's own content starts, which depends on the marker width.
    let nested_indent = prefix.len();
    // Paragraphs of the item in document order. Text outside a `<p>` collects
    // in `inline` and becomes a paragraph of its own where a `<p>` starts.
    let mut paragraphs: Vec<String> = Vec::new();
    let mut inline = String::new();
    let mut nested = Vec::new();

    for child in node.children() {
        // Nested lists are written after the item's text, so text that
        // follows one in the document has no place to go.
        let is_nested_list = child.is_element() && matches!(child.tag_name().name(), "ul" | "ol");
        let is_blank_text = child.is_text() && child.text().unwrap_or_default().trim().is_empty();
        if !nested.is_empty() && !is_nested_list && !is_blank_text && !renders_nothing(child) {
            return None;
        }
        match child.node_type() {
            NodeType::Text => {
                inline.push_str(&normalize_text_node(child.text().unwrap_or_default()))
            }
            NodeType::Element => {
                if child.tag_name().name() == "ul" {
                    nested.push(render_list(child, source, nested_indent, false)?);
                    continue;
                }
                if child.tag_name().name() == "ol" {
                    // Only a list numbered from one can follow an item's
                    // text without a blank line, and a blank line would
                    // turn that text into a paragraph.
                    if child
                        .attribute("start")
                        .is_some_and(|start| start.trim() != "1")
                    {
                        return None;
                    }
                    nested.push(render_list(child, source, nested_indent, true)?);
                    continue;
                }
                if child.tag_name().name() == "p" {
                    if has_attributes_besides(child, &[]) {
                        return None;
                    }
                    if !inline.trim().is_empty() {
                        paragraphs.push(inline.trim().to_string());
                    }
                    inline.clear();
                    paragraphs.push(render_inline_children(child, source)?.trim().to_string());
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

    if !inline.trim().is_empty() || paragraphs.is_empty() {
        paragraphs.push(inline.trim().to_string());
    }
    // An empty paragraph next to others would vanish between the blank lines
    // that separate them.
    if paragraphs.len() > 1 && paragraphs.iter().any(String::is_empty) {
        return None;
    }
    // The blank line between paragraphs makes the whole Markdown list loose,
    // which reads every item's text as a paragraph. Text that sits outside a
    // `<p>` anywhere in the list would come back wrapped in one.
    if paragraphs.len() > 1 && !list_holds_paragraphs_only {
        return None;
    }

    // Further paragraphs follow a blank line, indented to the item's content.
    let continuation = format!("\n\n{}", " ".repeat(nested_indent));
    let mut out = prefix;
    out.push_str(
        &paragraphs
            .iter()
            .map(|paragraph| escape_block_starts(paragraph))
            .collect::<Vec<_>>()
            .join(&continuation),
    );
    for block in nested {
        if !block.trim().is_empty() {
            out.push('\n');
            out.push_str(&block);
        }
    }
    Some(out)
}

/// Renders a table as a Markdown table when it has the shape a Markdown
/// table converts back to: one row of header cells in a `<thead>`, then rows
/// of as many data cells in a `<tbody>`, none of them with attributes.
fn render_table(node: Node<'_, '_>, source: &str) -> Option<String> {
    let sections: Vec<_> = node
        .children()
        .filter(|child| !renders_nothing(*child))
        .collect();
    let [head, body] = sections.as_slice() else {
        return None;
    };
    let is_section = |section: Node<'_, '_>, name: &str| {
        section.is_element()
            && !is_confluence_node(section)
            && section.tag_name().name() == name
            && !has_attributes_besides(section, &[])
    };
    if !is_section(*head, "thead") || !is_section(*body, "tbody") {
        return None;
    }

    let render_row = |row: Node<'_, '_>, cell_name: &str| -> Option<Vec<String>> {
        plain_children(row, cell_name)?
            .into_iter()
            .map(|cell| {
                let value = render_inline_children(cell, source)?;
                let value = value.trim();
                // A row is a single line.
                (!value.contains('\n')).then(|| value.replace('|', r"\|"))
            })
            .collect()
    };

    let header_rows = plain_children(*head, "tr")?;
    let [header_row] = header_rows.as_slice() else {
        return None;
    };
    let headers = render_row(*header_row, "th")?;
    if headers.is_empty() {
        return None;
    }
    let mut lines = vec![
        format!("| {} |", headers.join(" | ")),
        format!("| {} |", vec!["---"; headers.len()].join(" | ")),
    ];
    for row in plain_children(*body, "tr")? {
        let cells = render_row(row, "td")?;
        if cells.len() != headers.len() {
            return None;
        }
        lines.push(format!("| {} |", cells.join(" | ")));
    }
    Some(lines.join("\n"))
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

    let bullet = if takes_alternative_marker(node) {
        "*"
    } else {
        "-"
    };
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
        lines.push(format!("{bullet} {marker} {}", text.trim()));
    }
    Some(lines.join("\n"))
}

fn render_supported_macro_block(node: Node<'_, '_>, source: &str) -> Option<String> {
    let name = node.attribute((AC_NS, "name"))?;
    if name == "anchor" {
        return render_default_parameter_macro_block("anchor", "name", node);
    }
    if name == "code" {
        return render_code_macro_block(node);
    }
    if name == "noformat" {
        return render_noformat_macro_block(node);
    }
    if name == "excerpt" {
        return render_excerpt_macro_block(node, source);
    }
    if matches!(name, "details" | "content-properties") {
        return render_excerpt_macro_block_named("content-properties", node, source);
    }
    if matches!(name, "detailssummary" | "content-properties-report") {
        return render_parameter_only_macro_block("content-properties-report", node);
    }
    if name == "attachments" {
        return render_parameter_only_macro_block("attachments", node);
    }
    if name == "view-file" {
        return render_attachment_preview_macro_block("view-file", node);
    }
    if name == "viewdoc" {
        return render_attachment_preview_macro_block("view-doc", node);
    }
    if name == "viewxls" {
        return render_attachment_preview_macro_block("view-xls", node);
    }
    if name == "viewppt" {
        return render_attachment_preview_macro_block("view-ppt", node);
    }
    if matches!(name, "blog-posts" | "blogposts") {
        return render_parameter_only_macro_block("blog-posts", node);
    }
    if name == "contributors" {
        return render_spaces_macro_block("contributors", node);
    }
    if name == "contributors-summary" {
        return render_spaces_macro_block("contributors-summary", node);
    }
    if matches!(
        name,
        "recently-updated-dashboard" | "recentlyupdated-dashboard"
    ) {
        return render_spaces_macro_block("recently-updated-dashboard", node);
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
    if name == "content-by-user" || name == "contentbyuser" {
        return render_default_parameter_macro_block("content-by-user", "user", node);
    }
    if name == "content-report-table" {
        return render_spaces_macro_block("content-report-table", node);
    }
    if name == "search" {
        return render_generic_alias_macro_block("search", node, source);
    }
    if name == "navmap" {
        return render_generic_alias_macro_block("navmap", node, source);
    }
    if matches!(name, "tasks-report-macro" | "task-report") {
        return render_parameter_only_macro_block("task-report", node);
    }
    if name == "recently-updated" || name == "recentlyupdated" {
        return render_recently_updated_macro_block(node);
    }
    if name == "livesearch" {
        return render_space_key_macro_block("livesearch", node);
    }
    if matches!(name, "page-index" | "pageindex") {
        return Some(":::confluence-page-index\n:::".to_string());
    }
    if matches!(name, "listlabels" | "labels-list") {
        return render_space_key_macro_block("labels-list", node);
    }
    if name == "popular-labels" {
        return render_space_key_macro_block("popular-labels", node);
    }
    if name == "related-labels" {
        return render_parameter_only_macro_block("related-labels", node);
    }
    if name == "recently-used-labels" {
        return render_parameter_only_macro_block("recently-used-labels", node);
    }
    if name == "gallery" {
        return render_parameter_only_macro_block("gallery", node);
    }
    if matches!(name, "favpages" | "favorite-pages" | "favourite-pages") {
        return Some(":::confluence-favorite-pages\n:::".to_string());
    }
    if matches!(name, "change-history" | "changehistory") {
        return Some(":::confluence-change-history\n:::".to_string());
    }
    if name == "spaces" {
        return render_default_parameter_macro_block("spaces-list", "scope", node);
    }
    if name == "space-details" {
        return render_parameter_only_macro_block("space-details", node);
    }
    if name == "space-attachments" {
        return render_parameter_only_macro_block("space-attachments", node);
    }
    if name == "profile" {
        return render_parameter_only_macro_block("profile", node);
    }
    if name == "status-list" {
        return render_parameter_only_macro_block("status-list", node);
    }
    if name == "network" {
        return render_default_parameter_macro_block("network", "mode", node);
    }
    if name == "toc" {
        return render_parameter_only_macro_block("toc", node);
    }
    if name == "toc-zone" {
        return render_excerpt_macro_block_named("toc-zone", node, source);
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
    if matches!(name, "info" | "note" | "tip" | "warning") {
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
    } else if let Some(markdown) = render_generic_macro_block(node, source) {
        Some(markdown)
    } else {
        Some(confluence_raw_block(raw_xml_fragment(node, source)))
    }
}

fn render_excerpt_macro_block(node: Node<'_, '_>, source: &str) -> Option<String> {
    render_excerpt_macro_block_named("excerpt", node, source)
}

fn render_excerpt_macro_block_named(
    block_name: &str,
    node: Node<'_, '_>,
    source: &str,
) -> Option<String> {
    let body = namespaced_child(node, AC_NS, "rich-text-body")?;
    let body_markdown = storage_to_markdown(&inner_xml_fragment(body, source));
    let parameters = collect_macro_parameters(node);
    Some(render_rich_text_macro_block(
        block_name,
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
    if let Some(parameter) = find_macro_parameter(node, "spaceKey")
        && let Some(space) = namespaced_child(parameter, RI_NS, "space")
    {
        let space_key = space
            .attribute((RI_NS, "space-key"))
            .or_else(|| space.attribute("ri:space-key"))
            .or_else(|| space.attribute("space-key"))?;
        parameters.insert("spaceKey".to_string(), space_key.to_string());
    }
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
        parameters.insert(
            "spaces".to_string(),
            collect_macro_parameter_value(parameter),
        );
    }
    Some(render_parameter_only_macro_block_with_parameters(
        "recently-updated",
        &parameters,
    ))
}

fn render_space_key_macro_block(name: &str, node: Node<'_, '_>) -> Option<String> {
    let mut parameters = collect_macro_parameters(node);
    if let Some(parameter) = find_macro_parameter(node, "spaceKey") {
        parameters.insert(
            "spaceKey".to_string(),
            collect_macro_parameter_value(parameter),
        );
    }
    Some(render_parameter_only_macro_block_with_parameters(
        name,
        &parameters,
    ))
}

fn render_spaces_macro_block(name: &str, node: Node<'_, '_>) -> Option<String> {
    let parameters = collect_macro_parameters(node);
    Some(render_parameter_only_macro_block_with_parameters(
        name,
        &parameters,
    ))
}

fn render_attachment_preview_macro_block(name: &str, node: Node<'_, '_>) -> Option<String> {
    let mut parameters = collect_macro_parameters(node);
    if let Some(page) = parameters.get("page").cloned() {
        let trimmed = page.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('@') {
            let placeholder = parse_page_placeholder_url(trimmed)
                .unwrap_or_else(|| parse_default_parameter_page_target(trimmed));
            parameters.insert("page".to_string(), build_page_placeholder_url(&placeholder));
        }
    }
    if let Some(name_parameter) = parameters.remove("name") {
        parameters.insert("attachment".to_string(), name_parameter);
    }
    Some(render_parameter_only_macro_block_with_parameters(
        name,
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

fn render_default_parameter_macro_block(
    name: &str,
    default_parameter_name: &str,
    node: Node<'_, '_>,
) -> Option<String> {
    let mut parameters = collect_macro_parameters(node);
    if let Some(value) = parameters.remove("") {
        parameters.insert(default_parameter_name.to_string(), value);
    }
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

fn render_generic_alias_macro_block(
    name: &str,
    node: Node<'_, '_>,
    source: &str,
) -> Option<String> {
    let rich_text_body = namespaced_child(node, AC_NS, "rich-text-body");
    if node.children().any(|child| {
        child.is_element()
            && !matches!(
                (child.tag_name().namespace(), child.tag_name().name()),
                (Some(AC_NS), "parameter") | (Some(AC_NS), "rich-text-body")
            )
    }) {
        return None;
    }

    let parameters = collect_generic_macro_parameters(node)?;
    let body_markdown =
        rich_text_body.map(|body| storage_to_markdown(&inner_xml_fragment(body, source)));
    Some(render_generic_alias_macro_block_with_parameters(
        name,
        &parameters,
        body_markdown.as_deref(),
    ))
}

fn render_generic_macro_block(node: Node<'_, '_>, source: &str) -> Option<String> {
    let name = node.attribute((AC_NS, "name"))?;
    let rich_text_body = namespaced_child(node, AC_NS, "rich-text-body");
    if node.children().any(|child| {
        child.is_element()
            && !matches!(
                (child.tag_name().namespace(), child.tag_name().name()),
                (Some(AC_NS), "parameter") | (Some(AC_NS), "rich-text-body")
            )
    }) {
        return None;
    }

    let parameters = collect_generic_macro_parameters(node)?;
    let body_markdown =
        rich_text_body.map(|body| storage_to_markdown(&inner_xml_fragment(body, source)));
    Some(render_generic_macro_block_with_parameters(
        name,
        &parameters,
        body_markdown.as_deref(),
    ))
}

fn render_generic_alias_macro_block_with_parameters(
    name: &str,
    parameters: &BTreeMap<String, String>,
    body_markdown: Option<&str>,
) -> String {
    let mut markdown = format!(":::confluence-{name}\n");
    for (parameter_name, parameter_value) in parameters {
        markdown.push_str(&format!("{parameter_name}: {parameter_value}\n"));
    }
    if let Some(body_markdown) = body_markdown {
        markdown.push_str("---\n");
        let trimmed = body_markdown.trim();
        if !trimmed.is_empty() {
            markdown.push_str(trimmed);
            markdown.push('\n');
        }
    }
    markdown.push_str(":::");
    markdown
}

fn collect_generic_macro_parameters(node: Node<'_, '_>) -> Option<BTreeMap<String, String>> {
    let mut parameters = BTreeMap::new();
    for parameter in node.children().filter(|child| {
        child.is_element()
            && child.tag_name().namespace() == Some(AC_NS)
            && child.tag_name().name() == "parameter"
    }) {
        let name = parameter.attribute((AC_NS, "name"))?;
        let rendered_name = if name.is_empty() { "$default" } else { name };
        let value = collect_generic_macro_parameter_value(parameter)?;
        parameters.insert(rendered_name.to_string(), value);
    }
    Some(parameters)
}

fn collect_generic_macro_parameter_value(parameter: Node<'_, '_>) -> Option<String> {
    let space_keys: Vec<_> = parameter
        .children()
        .filter(|child| {
            child.is_element()
                && child.tag_name().namespace() == Some(RI_NS)
                && child.tag_name().name() == "space"
        })
        .filter_map(|space| {
            space
                .attribute((RI_NS, "space-key"))
                .or_else(|| space.attribute("ri:space-key"))
                .or_else(|| space.attribute("space-key"))
                .map(ToOwned::to_owned)
        })
        .collect();
    if !space_keys.is_empty() {
        return Some(format!("!space {}", space_keys.join(",")));
    }

    let user_placeholders = collect_user_parameter_placeholders(parameter);
    if !user_placeholders.is_empty() {
        return Some(format!("!user {}", user_placeholders.join(",")));
    }

    if let Some(page) = namespaced_child(parameter, RI_NS, "page") {
        return Some(format!(
            "!page {}",
            build_page_placeholder_url(&page_resource_placeholder(page))
        ));
    }

    if let Some(page) = namespaced_child(parameter, AC_NS, "link")
        .and_then(|link| namespaced_child(link, RI_NS, "page"))
    {
        return Some(format!(
            "!page-link {}",
            build_page_placeholder_url(&page_resource_placeholder(page))
        ));
    }

    if parameter.children().all(|child| {
        if matches!(child.node_type(), NodeType::Text | NodeType::Comment) {
            true
        } else {
            !child.is_element()
        }
    }) {
        let text = parameter.text().unwrap_or_default().trim().to_string();
        if let Some(user) = parse_user_resource_identifier_text(&text) {
            return Some(format!("!user {}", build_user_placeholder_url(&user)));
        }
        if let Some(page) = parse_default_link_page_resource_identifier_text(&text) {
            return Some(format!("!page-link {}", build_page_placeholder_url(&page)));
        }
        if let Some(page) = parse_page_resource_identifier_text(&text) {
            return Some(format!("!page {}", build_page_placeholder_url(&page)));
        }
        return Some(text);
    }

    None
}

fn render_generic_macro_block_with_parameters(
    name: &str,
    parameters: &BTreeMap<String, String>,
    body_markdown: Option<&str>,
) -> String {
    let mut markdown = format!(":::confluence-macro {name}\n");
    for (parameter_name, parameter_value) in parameters {
        markdown.push_str(&format!("{parameter_name}: {parameter_value}\n"));
    }
    if let Some(body_markdown) = body_markdown {
        markdown.push_str("---\n");
        let trimmed = body_markdown.trim();
        if !trimmed.is_empty() {
            markdown.push_str(trimmed);
            markdown.push('\n');
        }
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

fn collect_user_parameter_placeholders(parameter: Node<'_, '_>) -> Vec<String> {
    parameter
        .children()
        .filter(|child| {
            child.is_element()
                && child.tag_name().namespace() == Some(RI_NS)
                && child.tag_name().name() == "user"
        })
        .map(|user| UserMentionPlaceholder {
            account_id: user.attribute((RI_NS, "account-id")).map(ToOwned::to_owned),
            user_key: user.attribute((RI_NS, "userkey")).map(ToOwned::to_owned),
            username: user.attribute((RI_NS, "username")).map(ToOwned::to_owned),
        })
        .map(|placeholder| build_user_placeholder_url(&placeholder))
        .collect()
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

fn render_noformat_macro_block(node: Node<'_, '_>) -> Option<String> {
    let body = namespaced_child(node, AC_NS, "plain-text-body")?;
    let text = body.text().unwrap_or_default().trim_end_matches('\n');
    let parameters = collect_macro_parameters(node);

    let mut markdown = "~~~confluence-noformat\n".to_string();
    if !parameters.is_empty() {
        for (name, value) in parameters {
            markdown.push_str(&format!("{name}: {value}\n"));
        }
        markdown.push_str("---\n");
    }
    markdown.push_str(text);
    if !text.is_empty() {
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
        "strong" | "b" => render_emphasis(node, source, "**", "strong"),
        "em" | "i" => render_emphasis(node, source, "*", "em"),
        "code" => render_code_span(node, source),
        "a" => render_link(node, source),
        "img" => {
            let src = node.attribute("src")?;
            let alt = node.attribute("alt").unwrap_or_default();
            Some(format!("![{}]({src})", escape_markdown_text(alt)))
        }
        "br" => Some(HARD_BREAK.to_string()),
        _ if is_transparent_inline(node) => render_inline_children(node, source),
        name if INLINE_TAGS.contains(&name) => {
            inline_tag(node, name, &render_inline_children(node, source)?)
        }
        // Markdown has no way to tell where an element it does not know
        // belongs, so the block that holds one is kept as storage.
        _ => None,
    }
}

/// Whether an element adds nothing to the text it holds, so that only its
/// content is written.
fn is_transparent_inline(node: Node<'_, '_>) -> bool {
    node.is_element()
        && !is_confluence_node(node)
        && node.tag_name().name() == "span"
        && node.attributes().len() == 0
}

/// Writes an element as an inline tag around its rendered content. Markdown
/// keeps such a tag as it is and reads the content between its halves as
/// Markdown.
fn inline_tag(node: Node<'_, '_>, name: &str, content: &str) -> Option<String> {
    // A line that holds nothing but a tag starts a block of raw HTML.
    if content.contains('\n') {
        return None;
    }
    let mut tag = format!("<{name}");
    for attribute in node.attributes() {
        // A prefixed attribute needs a namespace declaration that an inline
        // tag does not carry, and Markdown reads a tag only when its
        // attribute names are plain.
        if attribute.namespace().is_some() || !is_markdown_attribute_name(attribute.name()) {
            return None;
        }
        tag.push_str(&format!(
            r#" {}="{}""#,
            attribute.name(),
            escape_inline_tag_attribute(attribute.value())
        ));
    }
    Some(format!("{tag}>{content}</{name}>"))
}

/// Renders emphasis with Markdown delimiters where Markdown reads them as
/// emphasis, and as an inline tag everywhere else.
fn render_emphasis(node: Node<'_, '_>, source: &str, delimiter: &str, tag: &str) -> Option<String> {
    let content = render_inline_children(node, source)?;
    if node.attributes().len() == 0 && delimiters_read_as_emphasis(node) {
        Some(format!("{delimiter}{content}{delimiter}"))
    } else {
        inline_tag(node, tag, &content)
    }
}

/// What borders an emphasis delimiter on one side.
#[derive(Clone, Copy, PartialEq)]
enum Edge {
    /// White space, or the start or end of the enclosing block or element.
    Space,
    /// A letter or digit.
    Word,
    /// Punctuation, which includes the syntax another element is written
    /// with.
    Punctuation,
    /// A character that Markdown implementations classify differently.
    Unclear,
    /// Another emphasis element, whose delimiters would run into this one's.
    Emphasis,
}

/// Whether Markdown pairs the delimiters written around an emphasis element.
///
/// A delimiter opens emphasis only when text follows it directly, and when
/// that text starts with punctuation only after white space or punctuation.
/// Closing mirrors that. Delimiters that touch those of a neighbour, or that
/// sit inside emphasis of the same kind, pair up differently from how the
/// elements nest.
fn delimiters_read_as_emphasis(node: Node<'_, '_>) -> bool {
    let kind = emphasis_kind(node);
    if node
        .ancestors()
        .skip(1)
        .any(|ancestor| emphasis_kind(ancestor) == kind)
    {
        return false;
    }
    let first = node
        .children()
        .find_map(|child| Some((child, edge_of(child, true)?)));
    let last = node
        .children()
        .rev()
        .find_map(|child| Some((child, edge_of(child, false)?)));
    let (Some((first_child, first)), Some((last_child, last))) = (first, last) else {
        return false;
    };
    // Delimiters that meet at both ends form a single run, which Markdown
    // always reads with the strong emphasis innermost.
    if kind == Some("strong") && first_child == last_child && first == Edge::Emphasis {
        return false;
    }
    let before = edge_beside(node, false);
    let after = edge_beside(node, true);
    let flanks = |inner: Edge, outer: Edge| match inner {
        Edge::Space => false,
        Edge::Word => true,
        _ => matches!(outer, Edge::Space | Edge::Punctuation),
    };
    before != Edge::Emphasis
        && flanks(first, before)
        && flanks(
            last,
            if after == Edge::Emphasis {
                // That neighbour is written as a tag.
                Edge::Punctuation
            } else {
                after
            },
        )
}

/// Which kind of emphasis an element is, if any.
fn emphasis_kind(node: Node<'_, '_>) -> Option<&'static str> {
    if !node.is_element() || is_confluence_node(node) {
        return None;
    }
    match node.tag_name().name() {
        "strong" | "b" => Some("strong"),
        "em" | "i" => Some("em"),
        _ => None,
    }
}

/// The edge that a node's rendering starts with (`start`) or ends with, or
/// `None` when the node renders as nothing.
fn edge_of(node: Node<'_, '_>, start: bool) -> Option<Edge> {
    match node.node_type() {
        NodeType::Text => {
            let text = node.text().unwrap_or_default();
            let character = if start {
                text.chars().next()
            } else {
                text.chars().next_back()
            }?;
            Some(if character.is_whitespace() {
                Edge::Space
            } else if character.is_alphanumeric() {
                Edge::Word
            } else if character.is_ascii_punctuation() {
                Edge::Punctuation
            } else {
                Edge::Unclear
            })
        }
        NodeType::Element if is_transparent_inline(node) => {
            if start {
                node.children().find_map(|child| edge_of(child, start))
            } else {
                node.children()
                    .rev()
                    .find_map(|child| edge_of(child, start))
            }
        }
        NodeType::Element if emphasis_kind(node).is_some() => Some(Edge::Emphasis),
        NodeType::Element if node.tag_name().name() == "br" && !is_confluence_node(node) => {
            Some(Edge::Space)
        }
        NodeType::Element => Some(Edge::Punctuation),
        _ => None,
    }
}

/// The edge next to an element, after it (`after`) or before it.
fn edge_beside(node: Node<'_, '_>, after: bool) -> Edge {
    fn step<'a, 'input>(node: Node<'a, 'input>, after: bool) -> Option<Node<'a, 'input>> {
        if after {
            node.next_sibling()
        } else {
            node.prev_sibling()
        }
    }
    let mut current = node;
    loop {
        // A neighbour shows the end that faces this element.
        if let Some(edge) =
            std::iter::successors(step(current, after), |sibling| step(*sibling, after))
                .find_map(|sibling| edge_of(sibling, after))
        {
            return edge;
        }
        match current.parent() {
            Some(parent) if is_transparent_inline(parent) => current = parent,
            // Whatever else encloses the element is written with syntax or
            // starts a block.
            _ => return Edge::Space,
        }
    }
}

/// Renders inline code as a code span. Code that holds more than text is
/// written as an inline tag.
fn render_code_span(node: Node<'_, '_>, source: &str) -> Option<String> {
    let text: String = node
        .children()
        .filter_map(|child| child.text())
        .collect::<String>()
        .replace(['\n', '\r'], " ");
    if node.attributes().len() > 0
        || node.children().any(|child| child.is_element())
        || text.is_empty()
    {
        return inline_tag(node, "code", &render_inline_children(node, source)?);
    }
    let longest_run = text
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    let delimiter = "`".repeat(longest_run + 1);
    // Markdown takes one space off both ends of a code span when both are
    // there, which is also what keeps a backtick at an end apart from the
    // delimiter.
    let spaced = text.starts_with(' ') && text.ends_with(' ') && text.trim() != "";
    let padding = if spaced || text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    Some(format!("{delimiter}{padding}{text}{padding}{delimiter}"))
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

/// Renders an image as Markdown only when Markdown can express all of it:
/// an attachment of this page or an external URL, with optional alt text and
/// title. Anything else (size, alignment, an attachment owned by another
/// page) is left for the caller to preserve as raw storage.
fn render_confluence_image(node: Node<'_, '_>) -> Option<String> {
    let mut alt = "";
    let mut title = None;
    for attribute in node.attributes() {
        match (attribute.namespace(), attribute.name()) {
            (Some(AC_NS), "alt") => alt = attribute.value(),
            (Some(AC_NS), "title") => title = Some(attribute.value()),
            _ => return None,
        }
    }

    let resource = sole_element_child(node)?;
    let target = if is_ri_element(resource, "attachment") {
        let file_name = attachment_file_name(resource)?;
        format!("attachments/{}", encode_target_segment(file_name))
    } else if is_ri_element(resource, "url") {
        sole_attribute(resource, "value")?.to_string()
    } else {
        return None;
    };

    let destination = markdown_link_destination(&target)?;
    let title = title
        .map(|title| {
            format!(
                r#" "{}""#,
                escape_markdown_text(title).replace('"', r#"\""#)
            )
        })
        .unwrap_or_default();
    Some(format!(
        "![{}]({destination}{title})",
        escape_markdown_text(alt)
    ))
}

/// Whether a node renders as nothing: whitespace between elements, a
/// comment or a processing instruction.
/// Whether a list item holds content other than paragraphs and nested lists.
fn has_text_outside_paragraphs(item: Node<'_, '_>) -> bool {
    item.children().any(|child| {
        !renders_nothing(child)
            && !(child.is_element() && matches!(child.tag_name().name(), "p" | "ul" | "ol"))
    })
}

fn renders_nothing(node: Node<'_, '_>) -> bool {
    node.is_comment()
        || node.is_pi()
        || (node.is_text() && node.text().is_some_and(|text| text.trim().is_empty()))
}

fn is_ri_element(node: Node<'_, '_>, name: &str) -> bool {
    node.tag_name().namespace() == Some(RI_NS) && node.tag_name().name() == name
}

/// The only element child of a node, if it has exactly one.
fn sole_element_child<'a, 'input>(node: Node<'a, 'input>) -> Option<Node<'a, 'input>> {
    let mut elements = node.children().filter(Node::is_element);
    let first = elements.next()?;
    elements.next().is_none().then_some(first)
}

/// The value of a resource identifier's single `ri:` attribute, provided the
/// element carries nothing else.
fn sole_attribute<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    if node.attributes().len() != 1 || node.children().any(|child| child.is_element()) {
        return None;
    }
    node.attribute((RI_NS, name))
}

/// The file name of an attachment of this page, provided the element says
/// nothing else about it. Confluence Cloud stamps each reference with the
/// attachment's version when the page is saved and does so again on the next
/// save, so that stamp is not content to preserve.
fn attachment_file_name<'a>(node: Node<'a, '_>) -> Option<&'a str> {
    let only_names_the_file = node.attributes().all(|attribute| {
        attribute.namespace() == Some(RI_NS)
            && matches!(attribute.name(), "filename" | "version-at-save")
    });
    if !only_names_the_file || node.children().any(|child| child.is_element()) {
        return None;
    }
    node.attribute((RI_NS, "filename"))
}

/// Formats a link or image destination, using the angle-bracket form when
/// the target contains characters that would otherwise end it.
fn markdown_link_destination(target: &str) -> Option<String> {
    if target.contains(['<', '>', '\n']) {
        return None;
    }
    if target.contains([' ', '(', ')']) || target.is_empty() {
        Some(format!("<{target}>"))
    } else {
        Some(target.to_string())
    }
}

/// Percent-encodes the characters of a file name or anchor that would be
/// read as URL or Markdown syntax inside a link destination, leaving the
/// rest readable.
pub(crate) fn encode_target_segment(segment: &str) -> String {
    let mut encoded = String::with_capacity(segment.len());
    for character in segment.chars() {
        if character.is_control()
            || matches!(
                character,
                ' ' | '%' | '#' | '?' | '&' | '<' | '>' | '(' | ')' | '\\' | '"'
            )
        {
            let mut buffer = [0; 4];
            for byte in character.encode_utf8(&mut buffer).bytes() {
                encoded.push_str(&format!("%{byte:02X}"));
            }
        } else {
            encoded.push(character);
        }
    }
    encoded
}

/// Attribute that Confluence assigns to an element to identify it. It says
/// nothing about the content.
const LOCAL_ID_ATTRIBUTE: &str = "local-id";

/// Renders a link as a Markdown link. A link that carries more than its
/// address, such as how Confluence displays it, is written as an inline tag
/// with those attributes, which Markdown has no syntax for.
fn render_link(node: Node<'_, '_>, source: &str) -> Option<String> {
    let href = node.attribute("href")?;
    let label = render_inline_children(node, source)?.trim().to_string();
    let attributes: Vec<_> = node
        .attributes()
        .filter(|attribute| {
            attribute.namespace().is_some() || attribute.name() != LOCAL_ID_ATTRIBUTE
        })
        .collect();
    if attributes.len() == 1 {
        return Some(format!(
            "[{}]({href})",
            if label.is_empty() { href } else { &label }
        ));
    }

    let mut tag = String::from("<a");
    for attribute in attributes {
        // A prefixed attribute needs a namespace declaration that an inline
        // tag does not carry, and Markdown reads a tag only when its
        // attribute names are plain.
        if attribute.namespace().is_some() || !is_markdown_attribute_name(attribute.name()) {
            return None;
        }
        tag.push_str(&format!(
            r#" {}="{}""#,
            attribute.name(),
            escape_inline_tag_attribute(attribute.value())
        ));
    }
    tag.push('>');
    tag.push_str(&label);
    tag.push_str("</a>");
    // Markdown ends an inline tag at a blank line, and a line break inside
    // one has no single reading.
    (!tag.contains('\n')).then_some(tag)
}

/// Escapes an attribute value of an inline tag. A bar is written as a
/// character reference, since a table cell would otherwise put a backslash
/// before it that an inline tag keeps. So is white space other than a space,
/// which an attribute value holds only when written that way.
fn escape_inline_tag_attribute(value: &str) -> String {
    escape_xml(value)
        .replace('|', "&#124;")
        .replace('\t', "&#9;")
        .replace('\n', "&#10;")
        .replace('\r', "&#13;")
}

/// Whether Markdown accepts the name as an attribute name of an inline tag.
fn is_markdown_attribute_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
        })
}

/// Attributes of a Confluence link that say how the link is displayed. A
/// placeholder link target carries them as query parameters of the same
/// name.
const LINK_DISPLAY_ATTRIBUTES: [&str; 2] = ["card-appearance", "tooltip"];

/// Reads the display attributes of a Confluence link. There is no result
/// when the link has an attribute that a placeholder link target cannot
/// carry.
fn confluence_link_display_attributes<'a>(node: Node<'a, '_>) -> Option<Vec<(&'a str, &'a str)>> {
    let mut display = Vec::new();
    for attribute in node.attributes() {
        if attribute.namespace() != Some(AC_NS) {
            return None;
        }
        if LINK_DISPLAY_ATTRIBUTES.contains(&attribute.name()) {
            display.push((attribute.name(), attribute.value()));
        } else if attribute.name() != "anchor" {
            return None;
        }
    }
    Some(display)
}

fn with_display_attributes(target: &str, display: &[(&str, &str)]) -> String {
    if display.is_empty() {
        return target.to_string();
    }
    let Ok(mut url) = Url::parse(target) else {
        return target.to_string();
    };
    url.query_pairs_mut().extend_pairs(display);
    url.to_string()
}

/// The display attributes that a placeholder link target carries, by the
/// name they have on a Confluence link.
pub(crate) fn link_display_attributes(target: &str) -> Vec<(String, String)> {
    let Ok(url) = Url::parse(&target.replace("&amp;", "&")) else {
        return Vec::new();
    };
    let mut display: Vec<(String, String)> = Vec::new();
    for (key, value) in url.query_pairs() {
        // An element holds an attribute once, so the first value counts.
        if LINK_DISPLAY_ATTRIBUTES.contains(&key.as_ref())
            && !display.iter().any(|(name, _)| *name == key)
        {
            display.push((key.into_owned(), value.into_owned()));
        }
    }
    display
}

/// Query parameter that marks a placeholder link whose text was generated
/// on pull because the Confluence link has no body of its own.
const GENERATED_LABEL_PARAMETER: (&str, &str) = ("label", "generated");

/// The label of a pulled link and its target. A link without a body gets
/// the generated label, and its target records that the label is generated.
fn label_or_generated(label: Option<String>, generated: &str, target: String) -> (String, String) {
    match label {
        Some(label) => (label, target),
        None => (
            escape_markdown_text(generated),
            mark_generated_label(&target),
        ),
    }
}

fn mark_generated_label(target: &str) -> String {
    let Ok(mut url) = Url::parse(target) else {
        return target.to_string();
    };
    url.query_pairs_mut()
        .append_pair(GENERATED_LABEL_PARAMETER.0, GENERATED_LABEL_PARAMETER.1);
    url.to_string()
}

/// Whether a placeholder link target says its link text was generated on
/// pull rather than taken from a link body.
pub(crate) fn has_generated_label(target: &str) -> bool {
    Url::parse(&target.replace("&amp;", "&")).is_ok_and(|url| {
        url.query_pairs().any(|(key, value)| {
            key == GENERATED_LABEL_PARAMETER.0 && value == GENERATED_LABEL_PARAMETER.1
        })
    })
}

fn render_confluence_link(node: Node<'_, '_>, source: &str) -> Option<String> {
    let anchor = node.attribute((AC_NS, "anchor"));
    let display = confluence_link_display_attributes(node)?;
    let label = link_label(node, source)?;

    if let Some(page) = namespaced_child(node, RI_NS, "page") {
        // A placeholder target says which page it stands for by its id or
        // its title.
        if !page.has_attribute((RI_NS, "content-id"))
            && !page.has_attribute((RI_NS, "content-title"))
        {
            return None;
        }
        let placeholder = PageLinkPlaceholder {
            content_id: page.attribute((RI_NS, "content-id")).map(ToOwned::to_owned),
            space_key: page.attribute((RI_NS, "space-key")).map(ToOwned::to_owned),
            content_title: page
                .attribute((RI_NS, "content-title"))
                .map(ToOwned::to_owned),
            anchor: anchor.map(ToOwned::to_owned),
        };
        let target = with_display_attributes(&build_page_placeholder_url(&placeholder), &display);
        let (label, target) = label_or_generated(label, &placeholder.default_label(), target);
        return Some(format!("[{}]({target})", label.trim()));
    }

    if let Some(user) = namespaced_child(node, RI_NS, "user") {
        let placeholder = UserMentionPlaceholder {
            account_id: user.attribute((RI_NS, "account-id")).map(ToOwned::to_owned),
            user_key: user.attribute((RI_NS, "userkey")).map(ToOwned::to_owned),
            username: user.attribute((RI_NS, "username")).map(ToOwned::to_owned),
        };
        let target = with_display_attributes(&build_user_placeholder_url(&placeholder), &display);
        let (label, target) = label_or_generated(label, &placeholder.default_label(), target);
        return Some(format!("[{}]({target})", label.trim()));
    }

    // Only a link to a page or a user has a target that carries them.
    if !display.is_empty() {
        return None;
    }

    if let Some(attachment) = namespaced_child(node, RI_NS, "attachment") {
        let file_name = attachment_file_name(attachment)?;
        let mut target = format!("attachments/{}", encode_target_segment(file_name));
        if let Some(anchor) = anchor {
            target.push('#');
            target.push_str(&encode_target_segment(anchor));
        }
        let destination = markdown_link_destination(&target)?;
        // A link without a body shows the file name. An explicit body that
        // reads the same would be taken for that, so such a link has no
        // Markdown form.
        let generated = escape_markdown_text(file_name);
        if label.as_deref().map(str::trim) == Some(generated.as_str()) {
            return None;
        }
        let label = label.unwrap_or(generated);
        return Some(format!("[{}]({destination})", label.trim()));
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

/// Renders a link's body as link text. The outer option is empty when the
/// body has no Markdown form, the inner one when the link has no body.
fn link_label(node: Node<'_, '_>, source: &str) -> Option<Option<String>> {
    if let Some(body) = namespaced_child(node, AC_NS, "plain-text-link-body") {
        return Some(body.text().map(|text| escape_markdown_text(text.trim())));
    }
    if let Some(body) = namespaced_child(node, AC_NS, "link-body") {
        return render_inline_children(body, source).map(|text| Some(text.trim().to_string()));
    }
    Some(None)
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

fn is_block_like_confluence_node(node: Node<'_, '_>) -> bool {
    if !node.is_element() || node.tag_name().namespace() != Some(AC_NS) {
        return false;
    }
    match node.tag_name().name() {
        "layout" | "task-list" => true,
        "structured-macro" | "macro" => node.attribute((AC_NS, "name")) != Some("status"),
        _ => false,
    }
}

fn contains_confluence_markup(node: Node<'_, '_>) -> bool {
    node.descendants()
        .any(|child| child.is_element() && is_confluence_node(child))
}

/// Elements that sit inside a line of text.
const INLINE_TAGS: [&str; 28] = [
    "span", "strong", "b", "em", "i", "code", "a", "img", "br", "small", "big", "u", "sub", "sup",
    "del", "s", "strike", "ins", "kbd", "mark", "cite", "q", "abbr", "var", "samp", "time", "tt",
    "dfn",
];

fn is_inlineish_tag(node: Node<'_, '_>) -> bool {
    INLINE_TAGS.contains(&node.tag_name().name())
}

fn has_block_children(node: Node<'_, '_>) -> bool {
    node.children()
        .any(|child| child.is_element() && !is_inlineish_tag(child))
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

/// Escapes characters that Markdown would read as syntax, so literal page
/// text converts back to the same text.
pub(crate) fn escape_markdown_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for (index, ch) in text.char_indices() {
        let rest = &text[index + ch.len_utf8()..];
        let needs_escape = match ch {
            '\\' | '*' | '_' | '[' | ']' | '`' | '~' => true,
            '<' => starts_markup(rest),
            '&' => starts_entity(rest),
            _ => false,
        };
        if needs_escape {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Escapes the last character of heading content where Markdown would take
/// the end of the line as syntax: a `}` closes an attribute block and a `#`
/// belongs to a closing sequence.
fn escape_heading_end(content: &str) -> String {
    match content.char_indices().next_back() {
        Some((last, '}' | '#')) => format!("{}\\{}", &content[..last], &content[last..]),
        _ => content.to_string(),
    }
}

/// Whether the text after a `<` could open an HTML tag, comment, declaration
/// or autolink.
fn starts_markup(rest: &str) -> bool {
    rest.chars()
        .next()
        .is_some_and(|next| next.is_ascii_alphabetic() || matches!(next, '/' | '!' | '?'))
}

/// Whether the text after an `&` completes a character reference.
fn starts_entity(rest: &str) -> bool {
    let end = rest
        .bytes()
        .position(|byte| !(byte.is_ascii_alphanumeric() || byte == b'#'))
        .unwrap_or(rest.len());
    if rest.as_bytes().get(end) != Some(&b';') {
        return false;
    }
    let name = &rest[..end];
    if let Some(digits) = name.strip_prefix('#') {
        let digits = digits
            .strip_prefix(['x', 'X'])
            .filter(|hex| hex.chars().all(|ch| ch.is_ascii_hexdigit()))
            .or_else(|| {
                Some(digits).filter(|decimal| decimal.chars().all(|ch| ch.is_ascii_digit()))
            });
        return digits.is_some_and(|digits| !digits.is_empty());
    }
    !name.is_empty() && name.chars().all(|ch| ch.is_ascii_alphanumeric())
}

/// Escapes rendered inline content whose lines would otherwise open a
/// heading, list, block quote, definition, thematic break, metadata block
/// or setext underline. Lines
/// begin at the start of the content and after each hard line break.
fn escape_block_starts(inline: &str) -> String {
    inline
        .split(HARD_BREAK)
        .map(escape_block_start)
        .collect::<Vec<_>>()
        .join(HARD_BREAK)
}

const HARD_BREAK: &str = "  \n";

fn escape_block_start(line: &str) -> String {
    let content = line.trim_start();
    let indent = &line[..line.len() - content.len()];
    let followed_by_space_or_end = |rest: &str| rest.is_empty() || rest.starts_with([' ', '\t']);
    let only = |allowed: &[char]| {
        !content.trim_end().is_empty() && content.trim_end().chars().all(|ch| allowed.contains(&ch))
    };

    let escape_at = match content.chars().next() {
        Some('#') => {
            let rest = content.trim_start_matches('#');
            (content.len() - rest.len() <= 6 && followed_by_space_or_end(rest)).then_some(0)
        }
        Some('>') => Some(0),
        Some('-') => (followed_by_space_or_end(&content[1..]) || only(&['-', ' '])).then_some(0),
        Some('+') => (followed_by_space_or_end(&content[1..]) || only(&['+'])).then_some(0),
        Some('=') => only(&['=']).then_some(0),
        Some(':') => Some(0),
        Some(first) if first.is_ascii_digit() => {
            let digits = content.len()
                - content
                    .trim_start_matches(|ch: char| ch.is_ascii_digit())
                    .len();
            let rest = &content[digits..];
            (digits <= 9 && rest.starts_with(['.', ')']) && followed_by_space_or_end(&rest[1..]))
                .then_some(digits)
        }
        _ => None,
    };

    match escape_at {
        Some(position) => format!("{indent}{}\\{}", &content[..position], &content[position..]),
        None => line.to_string(),
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
    let raw = raw_xml_fragment(node, source);
    // A blank line ends a Markdown HTML block, and what follows it would be
    // read as Markdown, so such a fragment needs the fence as well.
    let has_blank_line = raw.trim().lines().any(|line| line.trim().is_empty());
    if contains_confluence_markup(node) || is_confluence_node(node) || has_blank_line {
        confluence_raw_block(raw)
    } else {
        raw.trim().to_string()
    }
}

fn confluence_raw_block(raw: &str) -> String {
    let raw = raw.trim();
    let fence = backtick_fence(raw);
    format!("{fence}confluence-storage\n{raw}\n{fence}")
}

/// A code fence longer than every run of backticks in the content, so that
/// nothing inside can close it.
fn backtick_fence(content: &str) -> String {
    let longest_run = content
        .split(|ch| ch != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    "`".repeat(longest_run.max(2) + 1)
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
            Some((name.to_string(), collect_macro_parameter_value(parameter)))
        })
        .collect()
}

fn collect_macro_parameter_value(parameter: Node<'_, '_>) -> String {
    let space_keys: Vec<_> = parameter
        .children()
        .filter(|child| {
            child.is_element()
                && child.tag_name().namespace() == Some(RI_NS)
                && child.tag_name().name() == "space"
        })
        .filter_map(|space| {
            space
                .attribute((RI_NS, "space-key"))
                .or_else(|| space.attribute("ri:space-key"))
                .or_else(|| space.attribute("space-key"))
                .map(ToOwned::to_owned)
        })
        .collect();
    if !space_keys.is_empty() {
        return space_keys.join(",");
    }

    let user_placeholders = collect_user_parameter_placeholders(parameter);
    if !user_placeholders.is_empty() {
        return user_placeholders.join(",");
    }

    if let Some(page) = namespaced_child(parameter, RI_NS, "page") {
        return build_page_placeholder_url(&page_resource_placeholder(page));
    }

    if let Some(page) = namespaced_child(parameter, AC_NS, "link")
        .and_then(|link| namespaced_child(link, RI_NS, "page"))
    {
        return build_page_placeholder_url(&page_resource_placeholder(page));
    }

    if let Some(attachment) = namespaced_child(parameter, RI_NS, "attachment")
        && let Some(file_name) = attachment.attribute((RI_NS, "filename"))
    {
        return file_name.to_string();
    }

    let text = parameter.text().unwrap_or_default().trim().to_string();
    if let Some(user) = parse_user_resource_identifier_text(&text) {
        return build_user_placeholder_url(&user);
    }
    text
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

const USER_DISPLAY_NAME_PARAMETER: &str = "name";

/// A mention target that also records the name displayed for the user. The
/// label generated for such a mention is that name.
pub(crate) fn with_user_display_name(target: &str, name: &str) -> String {
    let Ok(mut url) = Url::parse(target) else {
        return target.to_string();
    };
    url.query_pairs_mut()
        .append_pair(USER_DISPLAY_NAME_PARAMETER, name);
    url.to_string()
}

/// The link text pull generates for a mention without a link body: the name
/// its target records for the user, or else the user's identifier.
pub(crate) fn generated_user_label(target: &str, user: &UserMentionPlaceholder) -> String {
    Url::parse(&target.replace("&amp;", "&"))
        .ok()
        .and_then(|url| {
            url.query_pairs()
                .find_map(|(key, value)| (key == USER_DISPLAY_NAME_PARAMETER).then_some(value))
                .map(|name| format!("@{name}"))
        })
        .unwrap_or_else(|| user.default_label())
}

impl UserMentionPlaceholder {
    /// The identifier a lookup of this user goes by.
    pub(crate) fn reference(&self) -> Option<crate::model::UserReference> {
        use crate::model::UserReference;
        self.account_id
            .clone()
            .map(UserReference::AccountId)
            .or_else(|| self.user_key.clone().map(UserReference::UserKey))
            .or_else(|| self.username.clone().map(UserReference::Username))
    }
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

fn parse_user_resource_identifier_text(target: &str) -> Option<UserMentionPlaceholder> {
    let trimmed = target.trim();
    let bracketed = trimmed
        .strip_suffix(']')?
        .rsplit_once('[')
        .map(|(_, suffix)| suffix)?;
    if !bracketed.contains("accountId=")
        && !bracketed.contains("userKey=")
        && !bracketed.contains("userName=")
    {
        return None;
    }

    let mut placeholder = UserMentionPlaceholder::default();
    for part in bracketed.split(',') {
        let (key, value) = part.split_once('=')?;
        let value = value.trim();
        if value == "<null>" || value.is_empty() {
            continue;
        }
        match key.trim() {
            "accountId" => placeholder.account_id = Some(value.to_string()),
            "userKey" => placeholder.user_key = Some(value.to_string()),
            "userName" => placeholder.username = Some(value.to_string()),
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

fn parse_page_resource_identifier_text(target: &str) -> Option<PageLinkPlaceholder> {
    let trimmed = target.trim();
    if !trimmed.contains("PageResourceIdentifier[") {
        return None;
    }
    let bracketed = extract_bracketed_expression(trimmed, "PageResourceIdentifier[")?
        .strip_prefix("PageResourceIdentifier[")?
        .strip_suffix(']')?;

    let mut placeholder = PageLinkPlaceholder::default();
    for part in bracketed.split(',') {
        let (key, value) = part.split_once('=')?;
        let value = value.trim();
        if value == "<null>" || value.is_empty() {
            continue;
        }
        match key.trim() {
            "spaceKey" => placeholder.space_key = Some(value.to_string()),
            "title" => placeholder.content_title = Some(value.to_string()),
            "contentId" => placeholder.content_id = Some(value.to_string()),
            _ => {}
        }
    }

    if placeholder.content_id.is_none() && placeholder.content_title.is_none() {
        None
    } else {
        Some(placeholder)
    }
}

fn parse_default_link_page_resource_identifier_text(target: &str) -> Option<PageLinkPlaceholder> {
    let trimmed = target.trim();
    if !trimmed.contains("DefaultLink[") {
        return None;
    }
    let page = extract_bracketed_expression(trimmed, "PageResourceIdentifier[")?;
    let mut placeholder = parse_page_resource_identifier_text(page)?;
    if let Some(anchor) = extract_optional_value(trimmed, "anchor=Optional[")
        && !anchor.is_empty()
        && anchor != "empty"
    {
        placeholder.anchor = Some(anchor.to_string());
    }
    Some(placeholder)
}

fn extract_bracketed_expression<'a>(target: &'a str, marker: &str) -> Option<&'a str> {
    let start = target.find(marker)?;
    let mut depth = 0;
    let mut end = None;
    for (offset, ch) in target[start..].char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(start + offset + ch.len_utf8());
                    break;
                }
            }
            _ => {}
        }
    }
    end.map(|end| &target[start..end])
}

fn extract_optional_value<'a>(target: &'a str, marker: &str) -> Option<&'a str> {
    let start = target.find(marker)? + marker.len();
    let end = target[start..].find(']')?;
    Some(&target[start..start + end])
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
        let body_storage = render_markdown_storage(body_markdown, allow_lossy)?.storage;
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
        let body_storage = render_markdown_storage(body_markdown, allow_lossy)?.storage;
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

    let noformat_macro_re =
        Regex::new(r"(?ms)^~~~confluence-noformat[ \t]*\n(.*?)\n~~~[ \t]*(?:\n|$)")?;
    let mut noformat_normalized = String::with_capacity(code_normalized.len());
    last = 0;
    for captures in noformat_macro_re.captures_iter(&code_normalized) {
        let Some(full_match) = captures.get(0) else {
            continue;
        };
        noformat_normalized.push_str(&code_normalized[last..full_match.start()]);
        let block_body = captures.get(1).map(|m| m.as_str()).unwrap_or_default();
        let (parameters, text) = parse_noformat_macro_block(block_body)?;
        let idx = fragments.len();
        fragments.push(build_noformat_macro_storage(&parameters, &text));
        noformat_normalized.push_str(&format!("CONFLUENCE_MACRO_PLACEHOLDER_{idx}"));
        last = full_match.end();
    }
    noformat_normalized.push_str(&code_normalized[last..]);

    Ok((noformat_normalized, fragments))
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
        let generic_macro_name = trimmed
            .strip_prefix(":::confluence-macro ")
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let macro_name = match trimmed {
            ":::confluence-anchor" => Some("anchor"),
            ":::confluence-excerpt" => Some("excerpt"),
            ":::confluence-content-properties" => Some("content-properties"),
            ":::confluence-content-properties-report" => Some("content-properties-report"),
            ":::confluence-attachments" => Some("attachments"),
            ":::confluence-view-file" => Some("view-file"),
            ":::confluence-view-doc" => Some("view-doc"),
            ":::confluence-view-xls" => Some("view-xls"),
            ":::confluence-view-ppt" => Some("view-ppt"),
            ":::confluence-blog-posts" => Some("blog-posts"),
            ":::confluence-contributors" => Some("contributors"),
            ":::confluence-contributors-summary" => Some("contributors-summary"),
            ":::confluence-recently-updated-dashboard" => Some("recently-updated-dashboard"),
            ":::confluence-toc-zone" => Some("toc-zone"),
            ":::confluence-toc" => Some("toc"),
            ":::confluence-children" => Some("children"),
            ":::confluence-excerpt-include" => Some("excerpt-include"),
            ":::confluence-include-page" => Some("include-page"),
            ":::confluence-page-tree" => Some("page-tree"),
            ":::confluence-page-tree-search" => Some("page-tree-search"),
            ":::confluence-content-by-label" => Some("content-by-label"),
            ":::confluence-content-by-user" => Some("content-by-user"),
            ":::confluence-content-report-table" => Some("content-report-table"),
            ":::confluence-search" => Some("search"),
            ":::confluence-navmap" => Some("navmap"),
            ":::confluence-task-report" => Some("task-report"),
            ":::confluence-recently-updated" => Some("recently-updated"),
            ":::confluence-livesearch" => Some("livesearch"),
            ":::confluence-page-index" => Some("page-index"),
            ":::confluence-labels-list" => Some("labels-list"),
            ":::confluence-popular-labels" => Some("popular-labels"),
            ":::confluence-related-labels" => Some("related-labels"),
            ":::confluence-recently-used-labels" => Some("recently-used-labels"),
            ":::confluence-gallery" => Some("gallery"),
            ":::confluence-favorite-pages" => Some("favorite-pages"),
            ":::confluence-change-history" => Some("change-history"),
            ":::confluence-spaces-list" => Some("spaces-list"),
            ":::confluence-space-details" => Some("space-details"),
            ":::confluence-space-attachments" => Some("space-attachments"),
            ":::confluence-profile" => Some("profile"),
            ":::confluence-status-list" => Some("status-list"),
            ":::confluence-network" => Some("network"),
            _ => None,
        };

        if macro_name.is_none() && generic_macro_name.is_none() {
            output.push_str(lines[index]);
            output.push('\n');
            index += 1;
            continue;
        }

        let mut body_lines = Vec::new();
        index += 1;
        while index < lines.len() && lines[index].trim() != ":::" {
            body_lines.push(lines[index]);
            index += 1;
        }
        let macro_label = macro_name.or(generic_macro_name).unwrap_or("macro");
        if index >= lines.len() {
            bail!("unterminated confluence {macro_label} block");
        }

        let body = body_lines.join("\n");
        let fragment = match macro_name.unwrap_or("__generic__") {
            "anchor" => {
                let parameters = parse_macro_parameter_lines(&body, "confluence anchor macro")?;
                build_default_parameter_macro_storage("anchor", "name", &parameters)
            }
            "excerpt" => {
                let (parameters, body_storage) =
                    parse_rich_text_macro_block("confluence excerpt macro", &body, allow_lossy)?;
                build_rich_text_macro_storage("excerpt", &parameters, &body_storage)
            }
            "content-properties" => {
                let (parameters, body_storage) = parse_rich_text_macro_block(
                    "confluence content-properties macro",
                    &body,
                    allow_lossy,
                )?;
                build_legacy_rich_text_macro_storage("details", &parameters, &body_storage)
            }
            "content-properties-report" => {
                let parameters = parse_macro_parameter_lines(
                    &body,
                    "confluence content-properties-report macro",
                )?;
                build_legacy_parameter_only_macro_storage("detailssummary", &parameters)
            }
            "attachments" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence attachments macro")?;
                build_parameter_only_macro_storage("attachments", &parameters)
            }
            "view-file" => {
                let parameters = parse_macro_parameter_lines(&body, "confluence view-file macro")?;
                build_attachment_preview_macro_storage("view-file", &parameters)?
            }
            "view-doc" => {
                let parameters = parse_macro_parameter_lines(&body, "confluence view-doc macro")?;
                build_attachment_preview_macro_storage("viewdoc", &parameters)?
            }
            "view-xls" => {
                let parameters = parse_macro_parameter_lines(&body, "confluence view-xls macro")?;
                build_attachment_preview_macro_storage("viewxls", &parameters)?
            }
            "view-ppt" => {
                let parameters = parse_macro_parameter_lines(&body, "confluence view-ppt macro")?;
                build_attachment_preview_macro_storage("viewppt", &parameters)?
            }
            "blog-posts" => {
                let parameters = parse_macro_parameter_lines(&body, "confluence blog-posts macro")?;
                build_blog_posts_macro_storage(&parameters)
            }
            "contributors" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence contributors macro")?;
                build_spaces_macro_storage("contributors", &parameters)
            }
            "contributors-summary" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence contributors-summary macro")?;
                build_spaces_macro_storage("contributors-summary", &parameters)
            }
            "recently-updated-dashboard" => {
                let parameters = parse_macro_parameter_lines(
                    &body,
                    "confluence recently-updated-dashboard macro",
                )?;
                build_spaces_macro_storage("recently-updated-dashboard", &parameters)
            }
            "toc-zone" => {
                let (parameters, body_storage) =
                    parse_rich_text_macro_block("confluence toc-zone macro", &body, allow_lossy)?;
                build_rich_text_macro_storage("toc-zone", &parameters, &body_storage)
            }
            "toc" => {
                let parameters = parse_macro_parameter_lines(&body, "confluence toc macro")?;
                build_parameter_only_macro_storage("toc", &parameters)
            }
            "children" => {
                let parameters = parse_macro_parameter_lines(&body, "confluence children macro")?;
                build_children_macro_storage(&parameters)?
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
            "content-by-user" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence content-by-user macro")?;
                build_default_user_parameter_macro_storage("content-by-user", "user", &parameters)
            }
            "content-report-table" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence content-report-table macro")?;
                build_spaces_macro_storage("content-report-table", &parameters)
            }
            "search" => {
                let (parameters, body_storage) =
                    parse_generic_macro_block("confluence search macro", &body, allow_lossy)?;
                build_generic_macro_storage("search", &parameters, body_storage.as_deref())?
            }
            "navmap" => {
                let (parameters, body_storage) =
                    parse_generic_macro_block("confluence navmap macro", &body, allow_lossy)?;
                build_generic_macro_storage("navmap", &parameters, body_storage.as_deref())?
            }
            "task-report" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence task-report macro")?;
                build_parameter_only_macro_storage("tasks-report-macro", &parameters)
            }
            "recently-updated" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence recently-updated macro")?;
                build_recently_updated_macro_storage(&parameters)
            }
            "livesearch" => {
                let parameters = parse_macro_parameter_lines(&body, "confluence livesearch macro")?;
                build_space_key_macro_storage("livesearch", &parameters)
            }
            "page-index" => {
                let parameters = parse_macro_parameter_lines(&body, "confluence page-index macro")?;
                if parameters.is_empty() {
                    build_simple_macro_storage("page-index")
                } else {
                    build_parameter_only_macro_storage("page-index", &parameters)
                }
            }
            "labels-list" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence labels-list macro")?;
                build_space_key_macro_storage("listlabels", &parameters)
            }
            "popular-labels" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence popular-labels macro")?;
                build_space_key_macro_storage("popular-labels", &parameters)
            }
            "related-labels" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence related-labels macro")?;
                build_parameter_only_macro_storage("related-labels", &parameters)
            }
            "recently-used-labels" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence recently-used-labels macro")?;
                build_parameter_only_macro_storage("recently-used-labels", &parameters)
            }
            "gallery" => {
                let parameters = parse_macro_parameter_lines(&body, "confluence gallery macro")?;
                build_parameter_only_macro_storage("gallery", &parameters)
            }
            "favorite-pages" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence favorite-pages macro")?;
                if parameters.is_empty() {
                    build_simple_macro_storage("favpages")
                } else {
                    build_parameter_only_macro_storage("favpages", &parameters)
                }
            }
            "change-history" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence change-history macro")?;
                if parameters.is_empty() {
                    build_simple_macro_storage("change-history")
                } else {
                    build_parameter_only_macro_storage("change-history", &parameters)
                }
            }
            "spaces-list" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence spaces-list macro")?;
                build_default_parameter_macro_storage("spaces", "scope", &parameters)
            }
            "space-details" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence space-details macro")?;
                build_parameter_only_macro_storage("space-details", &parameters)
            }
            "space-attachments" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence space-attachments macro")?;
                build_single_space_parameter_macro_storage(
                    "space-attachments",
                    "space",
                    &parameters,
                )
            }
            "profile" => {
                let parameters = parse_macro_parameter_lines(&body, "confluence profile macro")?;
                build_user_parameter_macro_storage("profile", "user", &parameters)
            }
            "status-list" => {
                let parameters =
                    parse_macro_parameter_lines(&body, "confluence status-list macro")?;
                build_user_parameter_macro_storage("status-list", "username", &parameters)
            }
            "network" => {
                let parameters = parse_macro_parameter_lines(&body, "confluence network macro")?;
                build_network_macro_storage(&parameters)
            }
            _ => {
                let generic_name = generic_macro_name.expect("generic macro name");
                let (parameters, body_storage) = parse_generic_macro_block(
                    &format!("confluence macro `{generic_name}`"),
                    &body,
                    allow_lossy,
                )?;
                build_generic_macro_storage(generic_name, &parameters, body_storage.as_deref())?
            }
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
        let body_storage = render_markdown_storage(body_markdown, allow_lossy)?.storage;
        Ok((parameters, body_storage))
    } else {
        Ok((
            BTreeMap::new(),
            render_markdown_storage(trimmed_body, allow_lossy)?.storage,
        ))
    }
}

fn parse_generic_macro_block(
    context: &str,
    block_body: &str,
    allow_lossy: bool,
) -> Result<(BTreeMap<String, String>, Option<String>)> {
    let trimmed_body = block_body.trim_end_matches('\n');
    if let Some(body_markdown) = trimmed_body.strip_prefix("---\n") {
        let body_storage = render_markdown_storage(body_markdown, allow_lossy)?.storage;
        return Ok((BTreeMap::new(), Some(body_storage)));
    }
    if let Some((header, body_markdown)) = trimmed_body.split_once("\n---\n") {
        let parameters = parse_macro_parameter_lines(header, context)?;
        let body_storage = render_markdown_storage(body_markdown, allow_lossy)?.storage;
        return Ok((parameters, Some(body_storage)));
    }

    let parameters = parse_macro_parameter_lines(trimmed_body, context)?;
    Ok((parameters, None))
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
            let body_storage = render_markdown_storage(&cell, allow_lossy)?.storage;
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

fn parse_noformat_macro_block(block_body: &str) -> Result<(BTreeMap<String, String>, String)> {
    let trimmed_body = block_body.trim_end_matches('\n');
    if let Some((header, text)) = trimmed_body.split_once("\n---\n") {
        let parameters = parse_macro_parameter_lines(header, "confluence noformat macro")?;
        Ok((parameters, text.to_string()))
    } else {
        Ok((BTreeMap::new(), trimmed_body.to_string()))
    }
}

fn build_code_macro_storage(parameters: &BTreeMap<String, String>, code: &str) -> String {
    let parameters_xml = build_macro_parameters_xml(parameters);
    let body = wrap_cdata(code);
    format!(
        r#"<ac:structured-macro ac:name="code">{parameters_xml}<ac:plain-text-body><![CDATA[{body}]]></ac:plain-text-body></ac:structured-macro>"#
    )
}

fn build_noformat_macro_storage(parameters: &BTreeMap<String, String>, text: &str) -> String {
    let parameters_xml = build_macro_parameters_xml(parameters);
    let body = wrap_cdata(text);
    format!(
        r#"<ac:structured-macro ac:name="noformat">{parameters_xml}<ac:plain-text-body><![CDATA[{body}]]></ac:plain-text-body></ac:structured-macro>"#
    )
}

fn build_attachment_preview_macro_storage(
    name: &str,
    parameters: &BTreeMap<String, String>,
) -> Result<String> {
    let mut parameters = parameters.clone();
    let attachment = parameters
        .remove("attachment")
        .or_else(|| parameters.remove("name"))
        .ok_or_else(|| {
            anyhow::anyhow!("confluence {name} macro requires an `attachment` parameter")
        })?;
    let file_name = attachment_preview_file_name(&attachment)?;

    let page_xml = parameters.remove("page").map(|page| {
        let placeholder = parse_page_placeholder_url(&page)
            .unwrap_or_else(|| parse_default_parameter_page_target(&page));
        let page_resource_xml = build_page_title_resource_xml(&placeholder)?;
        Ok::<_, anyhow::Error>(format!(
            r#"<ac:parameter ac:name="page"><ac:link>{page_resource_xml}</ac:link></ac:parameter>"#
        ))
    }).transpose()?;
    let parameters_xml = build_macro_parameters_xml(&parameters);
    Ok(format!(
        r#"<ac:structured-macro ac:name="{name}">{parameters_xml}{}<ac:parameter ac:name="name"><ri:attachment ri:filename="{}" /></ac:parameter></ac:structured-macro>"#,
        page_xml.unwrap_or_default(),
        escape_xml(&file_name),
    ))
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

fn build_legacy_rich_text_macro_storage(
    name: &str,
    parameters: &BTreeMap<String, String>,
    body_storage: &str,
) -> String {
    let parameters_xml = build_macro_parameters_xml(parameters);
    format!(
        r#"<ac:macro ac:name="{name}">{parameters_xml}<ac:rich-text-body>{body_storage}</ac:rich-text-body></ac:macro>"#
    )
}

fn build_parameter_only_macro_storage(name: &str, parameters: &BTreeMap<String, String>) -> String {
    let parameters_xml = build_macro_parameters_xml(parameters);
    format!(r#"<ac:structured-macro ac:name="{name}">{parameters_xml}</ac:structured-macro>"#)
}

fn build_generic_macro_storage(
    name: &str,
    parameters: &BTreeMap<String, String>,
    body_storage: Option<&str>,
) -> Result<String> {
    let parameters_xml = build_generic_macro_parameters_xml(parameters)?;
    Ok(match body_storage {
        Some(body_storage) => format!(
            r#"<ac:structured-macro ac:name="{name}">{parameters_xml}<ac:rich-text-body>{body_storage}</ac:rich-text-body></ac:structured-macro>"#
        ),
        None => format!(
            r#"<ac:structured-macro ac:name="{name}">{parameters_xml}</ac:structured-macro>"#
        ),
    })
}

fn build_generic_macro_parameters_xml(parameters: &BTreeMap<String, String>) -> Result<String> {
    parameters
        .iter()
        .try_fold(String::new(), |mut xml, (name, value)| {
            xml.push_str(&build_generic_macro_parameter_xml(name, value)?);
            Ok(xml)
        })
}

fn build_generic_macro_parameter_xml(name: &str, value: &str) -> Result<String> {
    let parameter_name = if name == "$default" { "" } else { name };
    let escaped_name = escape_xml(parameter_name);

    if let Some(space_values) = value.strip_prefix("!space ") {
        return build_space_parameter_xml(parameter_name, space_values.trim()).ok_or_else(|| {
            anyhow::anyhow!(
                "generic confluence macro parameter `{name}` declared `!space` without a space key"
            )
        });
    }

    if let Some(user_value) = value.strip_prefix("!user ") {
        let trimmed = user_value.trim();
        return build_optional_user_parameter_xml(parameter_name, trimmed).ok_or_else(|| {
            anyhow::anyhow!(
                "generic confluence macro parameter `{name}` declared `!user` without a valid confluence-user placeholder"
            )
        });
    }

    if let Some(page_value) = value.strip_prefix("!page-link ") {
        let placeholder = parse_page_placeholder_url(page_value.trim()).ok_or_else(|| {
            anyhow::anyhow!(
                "generic confluence macro parameter `{name}` declared `!page-link` without a valid confluence-page placeholder"
            )
        })?;
        let page_xml = build_page_resource_xml(&placeholder)?;
        return Ok(format!(
            r#"<ac:parameter ac:name="{escaped_name}"><ac:link>{page_xml}</ac:link></ac:parameter>"#
        ));
    }

    if let Some(page_value) = value.strip_prefix("!page ") {
        let placeholder = parse_page_placeholder_url(page_value.trim()).ok_or_else(|| {
            anyhow::anyhow!(
                "generic confluence macro parameter `{name}` declared `!page` without a valid confluence-page placeholder"
            )
        })?;
        let page_xml = build_page_resource_xml(&placeholder)?;
        return Ok(format!(
            r#"<ac:parameter ac:name="{escaped_name}">{page_xml}</ac:parameter>"#
        ));
    }

    if parse_user_placeholder_url(value).is_some() {
        return Ok(build_user_parameter_xml(parameter_name, value));
    }

    if let Some(placeholder) = parse_page_placeholder_url(value) {
        let page_xml = build_page_resource_xml(&placeholder)?;
        return Ok(format!(
            r#"<ac:parameter ac:name="{escaped_name}">{page_xml}</ac:parameter>"#
        ));
    }

    Ok(format!(
        r#"<ac:parameter ac:name="{escaped_name}">{}</ac:parameter>"#,
        escape_xml(value)
    ))
}

fn build_legacy_parameter_only_macro_storage(
    name: &str,
    parameters: &BTreeMap<String, String>,
) -> String {
    let parameters_xml = build_macro_parameters_xml(parameters);
    format!(r#"<ac:macro ac:name="{name}">{parameters_xml}</ac:macro>"#)
}

fn build_simple_macro_storage(name: &str) -> String {
    format!(r#"<ac:macro ac:name="{name}" />"#)
}

fn attachment_preview_file_name(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("attachment parameter cannot be empty");
    }
    let path_part = trimmed.split('#').next().unwrap_or(trimmed);
    let candidate = Path::new(path_part);
    if let Some(file_name) = candidate.file_name().and_then(|name| name.to_str())
        && !file_name.is_empty()
    {
        return Ok(file_name.to_string());
    }
    Ok(trimmed.to_string())
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
    if let Some(root) = parameters.get("root").cloned()
        && let Some(placeholder) = parse_page_placeholder_url(&root)
        && let Ok(root_target) = build_title_page_target(&placeholder)
    {
        parameters.insert("root".to_string(), root_target);
    }
    build_parameter_only_macro_storage("pagetreesearch", &parameters)
}

fn build_recently_updated_macro_storage(parameters: &BTreeMap<String, String>) -> String {
    let mut parameters = parameters.clone();
    let spaces_xml = if let Some(spaces) = parameters.remove("spaces") {
        build_space_parameter_xml("spaces", &spaces)
    } else {
        None
    };
    let author_xml = parameters
        .remove("author")
        .and_then(|author| build_optional_user_parameter_xml("author", &author));
    let parameters_xml = build_macro_parameters_xml(&parameters);
    format!(
        r#"<ac:structured-macro ac:name="recently-updated">{parameters_xml}{}{}</ac:structured-macro>"#,
        spaces_xml.unwrap_or_default(),
        author_xml.unwrap_or_default()
    )
}

fn build_spaces_macro_storage(name: &str, parameters: &BTreeMap<String, String>) -> String {
    let mut parameters = parameters.clone();
    let spaces_xml = parameters
        .remove("spaces")
        .and_then(|spaces| build_space_parameter_xml("spaces", &spaces));
    let parameters_xml = build_macro_parameters_xml(&parameters);
    format!(
        r#"<ac:structured-macro ac:name="{name}">{parameters_xml}{}</ac:structured-macro>"#,
        spaces_xml.unwrap_or_default()
    )
}

fn build_blog_posts_macro_storage(parameters: &BTreeMap<String, String>) -> String {
    let mut parameters = parameters.clone();
    let spaces_xml = parameters
        .remove("spaces")
        .and_then(|spaces| build_space_parameter_xml("spaces", &spaces));
    let author_xml = parameters
        .remove("author")
        .and_then(|author| build_optional_user_parameter_xml("author", &author));
    let parameters_xml = build_macro_parameters_xml(&parameters);
    format!(
        r#"<ac:structured-macro ac:name="blog-posts">{parameters_xml}{}{}</ac:structured-macro>"#,
        spaces_xml.unwrap_or_default(),
        author_xml.unwrap_or_default()
    )
}

fn build_space_key_macro_storage(name: &str, parameters: &BTreeMap<String, String>) -> String {
    let mut parameters = parameters.clone();
    let space_key_xml = if let Some(space_key) = parameters.remove("spaceKey") {
        let trimmed = space_key.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(format!(
                r#"<ac:parameter ac:name="spaceKey"><ri:space ri:space-key="{}" /></ac:parameter>"#,
                escape_xml(trimmed)
            ))
        }
    } else {
        None
    };
    let parameters_xml = build_macro_parameters_xml(&parameters);
    format!(
        r#"<ac:structured-macro ac:name="{name}">{parameters_xml}{}</ac:structured-macro>"#,
        space_key_xml.unwrap_or_default()
    )
}

fn build_user_parameter_macro_storage(
    name: &str,
    user_parameter_name: &str,
    parameters: &BTreeMap<String, String>,
) -> String {
    let mut parameters = parameters.clone();
    let user_xml = parameters
        .remove(user_parameter_name)
        .map(|user| build_user_parameter_xml(user_parameter_name, &user))
        .unwrap_or_default();
    let parameters_xml = build_macro_parameters_xml(&parameters);
    format!(
        r#"<ac:structured-macro ac:name="{name}">{parameters_xml}{user_xml}</ac:structured-macro>"#
    )
}

fn build_default_user_parameter_macro_storage(
    name: &str,
    default_parameter_name: &str,
    parameters: &BTreeMap<String, String>,
) -> String {
    let mut parameters = parameters.clone();
    let user_xml = parameters
        .remove(default_parameter_name)
        .map(|user| build_user_parameter_xml("", &user))
        .unwrap_or_default();
    let parameters_xml = build_macro_parameters_xml(&parameters);
    format!(
        r#"<ac:structured-macro ac:name="{name}">{parameters_xml}{user_xml}</ac:structured-macro>"#
    )
}

fn build_network_macro_storage(parameters: &BTreeMap<String, String>) -> String {
    let mut parameters = parameters.clone();
    if let Some(mode) = parameters.remove("mode") {
        parameters.insert(String::new(), mode);
    }
    let username_xml = parameters
        .remove("username")
        .map(|user| build_user_parameter_xml("username", &user))
        .unwrap_or_default();
    let parameters_xml = build_macro_parameters_xml(&parameters);
    format!(
        r#"<ac:structured-macro ac:name="network">{parameters_xml}{username_xml}</ac:structured-macro>"#
    )
}

fn build_default_parameter_macro_storage(
    name: &str,
    default_parameter_name: &str,
    parameters: &BTreeMap<String, String>,
) -> String {
    let mut parameters = parameters.clone();
    if let Some(value) = parameters.remove(default_parameter_name) {
        parameters.insert(String::new(), value);
    }
    build_parameter_only_macro_storage(name, &parameters)
}

fn build_single_space_parameter_macro_storage(
    name: &str,
    parameter_name: &str,
    parameters: &BTreeMap<String, String>,
) -> String {
    let mut parameters = parameters.clone();
    let space_xml = parameters
        .remove(parameter_name)
        .and_then(|space| build_space_parameter_xml(parameter_name, &space));
    let parameters_xml = build_macro_parameters_xml(&parameters);
    format!(
        r#"<ac:structured-macro ac:name="{name}">{parameters_xml}{}</ac:structured-macro>"#,
        space_xml.unwrap_or_default()
    )
}

fn build_space_parameter_xml(name: &str, spaces: &str) -> Option<String> {
    let trimmed = spaces.trim();
    if trimmed.is_empty() {
        return None;
    }
    let values: Vec<_> = trimmed
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect();
    if values.is_empty() {
        return None;
    }
    if values.len() == 1 && !values[0].starts_with('@') && values[0] != "*" {
        return Some(format!(
            r#"<ac:parameter ac:name="{name}"><ri:space ri:space-key="{}" /></ac:parameter>"#,
            escape_xml(values[0])
        ));
    }
    let resources = values
        .iter()
        .map(|value| format!(r#"<ri:space ri:space-key="{}" />"#, escape_xml(value)))
        .collect::<String>();
    Some(format!(
        r#"<ac:parameter ac:name="{name}">{resources}</ac:parameter>"#
    ))
}

fn build_user_parameter_xml(name: &str, user: &str) -> String {
    if let Some(parameter_xml) = build_optional_user_parameter_xml(name, user) {
        return parameter_xml;
    }
    format!(
        r#"<ac:parameter ac:name="{name}">{}</ac:parameter>"#,
        escape_xml(user)
    )
}

fn build_optional_user_parameter_xml(name: &str, user: &str) -> Option<String> {
    let trimmed = user.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(placeholder) = parse_user_resource_identifier_text(trimmed)
        && let Some(resource_xml) = build_user_resource_xml(&placeholder)
    {
        return Some(format!(
            r#"<ac:parameter ac:name="{name}">{resource_xml}</ac:parameter>"#
        ));
    }
    if !trimmed.contains(',')
        && let Some(placeholder) = parse_user_placeholder_url(trimmed)
        && let Some(resource_xml) = build_user_resource_xml(&placeholder)
    {
        return Some(format!(
            r#"<ac:parameter ac:name="{name}">{resource_xml}</ac:parameter>"#
        ));
    }

    let values: Vec<_> = trimmed
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect();
    if values.is_empty() {
        return None;
    }

    let resources = values
        .iter()
        .map(|value| {
            parse_user_placeholder_url(value)
                .or_else(|| parse_user_resource_identifier_text(value))
                .and_then(|placeholder| build_user_resource_xml(&placeholder))
        })
        .collect::<Option<Vec<_>>>();
    if let Some(resources) = resources {
        return Some(format!(
            r#"<ac:parameter ac:name="{name}">{}</ac:parameter>"#,
            resources.join("")
        ));
    }

    None
}

fn build_optional_linked_page_parameter_xml(name: &str, page: &str) -> Result<Option<String>> {
    let trimmed = page.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let placeholder = parse_page_placeholder_url(trimmed)
        .unwrap_or_else(|| parse_default_parameter_page_target(trimmed));
    let page_xml = build_page_resource_xml(&placeholder)?;
    Ok(Some(format!(
        r#"<ac:parameter ac:name="{name}"><ac:link>{page_xml}</ac:link></ac:parameter>"#
    )))
}

fn build_children_macro_storage(parameters: &BTreeMap<String, String>) -> Result<String> {
    let mut parameters = parameters.clone();
    let page_xml = parameters
        .remove("page")
        .map(|page| build_optional_linked_page_parameter_xml("page", &page))
        .transpose()?
        .flatten();
    let parameters_xml = build_macro_parameters_xml(&parameters);
    Ok(format!(
        r#"<ac:structured-macro ac:name="children">{parameters_xml}{}</ac:structured-macro>"#,
        page_xml.unwrap_or_default()
    ))
}

fn build_user_resource_xml(user: &UserMentionPlaceholder) -> Option<String> {
    if let Some(account_id) = user.account_id.as_deref() {
        return Some(format!(
            r#"<ri:user ri:account-id="{}" />"#,
            escape_xml(account_id)
        ));
    }
    if let Some(user_key) = user.user_key.as_deref() {
        return Some(format!(
            r#"<ri:user ri:userkey="{}" />"#,
            escape_xml(user_key)
        ));
    }
    user.username
        .as_deref()
        .map(|username| format!(r#"<ri:user ri:username="{}" />"#, escape_xml(username)))
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
    parameters
        .iter()
        .map(|(name, value)| {
            format!(
                r#"<ac:parameter ac:name="{}">{}</ac:parameter>"#,
                escape_xml(name),
                escape_xml(value)
            )
        })
        .collect::<String>()
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
        Some(content_id) => format!("{}--{}", safe_slug(title), safe_slug(content_id)),
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
    atomic_write(
        &doc.markdown_path,
        render_document(&doc.frontmatter, &doc.body_markdown)?.as_bytes(),
    )?;
    atomic_write(
        &doc.sidecar_path,
        serde_json::to_string_pretty(&doc.sidecar)?.as_bytes(),
    )?;
    Ok(())
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path {} has no parent directory", path.display()))?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".confluence-write-")
        .tempfile_in(parent)
        .with_context(|| format!("failed to stage write for {}", path.display()))?;
    temporary
        .write_all(contents)
        .with_context(|| format!("failed to write staged file for {}", path.display()))?;
    temporary
        .flush()
        .with_context(|| format!("failed to flush staged file for {}", path.display()))?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to install {}", path.display()))?;
    Ok(())
}

/// The directory, below the destination of a pull, of what the pull keeps
/// of the pages it leaves out. Nothing in it is a local document.
pub(crate) const PULL_FAILURES_DIR: &str = ".confluence-pull-failures";

/// The record a pull keeps in its failures directory of the files it wrote
/// there: the path of each below that directory and the SHA-256 of its
/// contents. A directory of that name without the record is not a pull's.
pub(crate) const PULL_FAILURES_MANIFEST: &str = "manifest.json";

pub fn scan_local_documents(root: &Path) -> Result<Vec<LocalContentIndex>> {
    if !root.exists() {
        return Err(crate::output::typed_error(
            crate::output::ErrorKind::NotFound,
            format!("sync path `{}` does not exist", root.display()),
        ));
    }
    if !root.is_dir() {
        return Err(crate::output::typed_error(
            crate::output::ErrorKind::InvalidInput,
            format!("sync path `{}` is not a directory", root.display()),
        ));
    }
    let mut docs = Vec::new();
    let documents = WalkDir::new(root).into_iter().filter_entry(|entry| {
        entry.depth() == 0
            || entry.file_name() != PULL_FAILURES_DIR
            || !entry.path().join(PULL_FAILURES_MANIFEST).is_file()
    });
    for entry in documents {
        let entry = entry.with_context(|| format!("failed to scan `{}`", root.display()))?;
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

    fn conversion_error(markdown: &str) -> crate::output::CliError {
        markdown_to_storage(markdown, false)
            .expect_err("Markdown that does not convert")
            .downcast::<crate::output::CliError>()
            .expect("conversion error")
    }

    #[test]
    fn invalid_conversion_names_the_markdown_line_of_the_tag_left_open() {
        let error = conversion_error(
            "# Title\n\nA first paragraph\nof two lines.\n\nUse the <medallion> architecture.\n\nA last paragraph.\n",
        );
        let details = error.details.as_ref().expect("details");
        assert_eq!(details["markdown_start_line"], 6, "{details}");
        assert_eq!(details["markdown_end_line"], 6, "{details}");
        assert_eq!(details["reason"], "expected 'medallion' tag, not 'p'");
        assert_eq!(
            error.message,
            "invalid storage generated from Markdown line 6: expected 'medallion' tag, not 'p' (line 4, column 37 of the generated storage)"
        );
    }

    #[test]
    fn invalid_conversion_names_the_block_left_open_not_an_earlier_one_that_is_closed() {
        let markdown = "<div>\n\nfirst\n\n</div>\n\ntext\n\n<div>\n\nsecond\n";
        let error = markdown_to_storage(markdown, false).expect_err("a div is left open");
        let error = error
            .downcast::<crate::output::CliError>()
            .expect("typed error");
        let details = error.details.expect("details");
        assert_eq!(details["markdown_start_line"], 9, "{}", error.message);
        assert_eq!(details["markdown_end_line"], 9, "{}", error.message);
    }

    #[test]
    fn invalid_conversion_names_no_block_when_finding_it_costs_too_much() {
        let details_of = |paragraphs: usize| {
            let markdown = format!("<div>\n\n{}", "text\n\n".repeat(paragraphs));
            let error = markdown_to_storage(&markdown, false).expect_err("a div is left open");
            let error = error
                .downcast::<crate::output::CliError>()
                .expect("typed error");
            error.details.expect("details")
        };

        let near = details_of(50);
        assert_eq!(near["markdown_start_line"], 1, "{near}");

        let far = details_of(400);
        assert!(far.get("markdown_start_line").is_none(), "{far}");
        assert!(far.get("line").is_some(), "{far}");
    }

    #[test]
    fn invalid_conversion_names_every_line_of_the_block_that_breaks_it() {
        let error = conversion_error("Intro.\n\n- one\n- two <b>bold\n- three\n\nOutro.\n");
        let details = error.details.as_ref().expect("details");
        assert_eq!(details["markdown_start_line"], 3, "{details}");
        assert_eq!(details["markdown_end_line"], 5, "{details}");
        assert!(
            error.message.contains("Markdown lines 3-5"),
            "{}",
            error.message
        );
    }

    #[test]
    fn invalid_conversion_names_the_block_left_open_around_one_that_is_closed() {
        let markdown = "<div>\n\n<section>\n\nx\n\n</section>\n";
        let error = markdown_to_storage(markdown, false).expect_err("a div is left open");
        let error = error
            .downcast::<crate::output::CliError>()
            .expect("typed error");
        let details = error.details.expect("details");
        assert_eq!(details["markdown_start_line"], 1, "{}", error.message);
        assert_eq!(details["markdown_end_line"], 1, "{}", error.message);
    }

    #[test]
    fn invalid_conversion_names_the_storage_block_that_is_not_well_formed() {
        let error =
            conversion_error("Intro.\n\n```confluence-storage\n<p></wrong>\n```\n\nOutro.\n");
        let details = error.details.as_ref().expect("details");
        assert_eq!(details["markdown_start_line"], 3, "{details}");
        assert_eq!(details["markdown_end_line"], 5, "{details}");
    }

    #[test]
    fn markdown_lines_count_from_the_start_of_the_file_that_holds_the_body() {
        let error = markdown_to_storage_in_file("Fine.\n\nAn <open> tag.\n", false, Some(7))
            .expect_err("Markdown that does not convert")
            .downcast::<crate::output::CliError>()
            .expect("conversion error");
        assert_eq!(error.details.expect("details")["markdown_start_line"], 10);
    }

    #[test]
    fn invalid_conversion_without_a_known_file_position_names_no_markdown_line() {
        let error = markdown_to_storage_in_file("An <open> tag.\n", false, None)
            .expect_err("Markdown that does not convert")
            .downcast::<crate::output::CliError>()
            .expect("conversion error");
        let details = error.details.expect("details");
        assert!(details.get("markdown_start_line").is_none(), "{details}");
        assert!(
            !error.message.contains("Markdown line"),
            "{}",
            error.message
        );
    }

    #[test]
    fn remote_content_ids_cannot_escape_document_directory_names() {
        let directory = document_dir_name("Title", Some("../../../outside"));
        assert_eq!(Path::new(&directory).components().count(), 1);
        assert!(!directory.contains('/'));
        assert!(!directory.contains(".."));
    }

    #[test]
    fn storage_macro_round_trips_through_sentinel_block() {
        let storage = r#"<p>Hello</p><ac:structured-macro ac:name="chart"><ac:parameter ac:name="attachment"><ri:attachment ri:filename="data.csv" /></ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains("```confluence-storage"));
        let rendered = markdown_to_storage(&markdown, false).expect("conversion succeeds");
        assert!(rendered.storage.contains("<ac:structured-macro"));
    }

    #[test]
    fn mixed_content_preserves_only_unsupported_confluence_fragments() {
        let storage = r#"<h1>Title</h1><ac:structured-macro ac:name="chart"><ac:parameter ac:name="attachment"><ri:attachment ri:filename="data.csv" /></ac:parameter></ac:structured-macro><p>After</p>"#;
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
    fn paragraph_wrappers_with_block_macros_are_unwrapped() {
        let storage = r#"<p><ac:structured-macro ac:name="noformat"><ac:parameter ac:name="nopanel">true</ac:parameter><ac:plain-text-body><![CDATA[<xml>literal</xml>]]></ac:plain-text-body></ac:structured-macro><ac:structured-macro ac:name="profile"><ac:parameter ac:name="user"><ri:user ri:account-id="abc123" /></ac:parameter></ac:structured-macro></p>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains("~~~confluence-noformat"));
        assert!(markdown.contains(":::confluence-profile"));
        assert!(!markdown.contains("```confluence-storage"));
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
    fn anchor_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="anchor"><ac:parameter ac:name="">intro</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-anchor"));
        assert!(markdown.contains("name: intro"));
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
    fn content_properties_macros_export_to_blocks() {
        let storage = r#"<ac:macro ac:name="details"><ac:parameter ac:name="id">decision</ac:parameter><ac:rich-text-body><table><tbody><tr><th>Owner</th><td>Ada</td></tr><tr><th>Status</th><td>Approved</td></tr></tbody></table></ac:rich-text-body></ac:macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-content-properties"));
        assert!(markdown.contains("id: decision"));
        assert!(markdown.contains("<tr><th>Owner</th><td>Ada</td></tr>"));
        assert!(markdown.contains("<tr><th>Status</th><td>Approved</td></tr>"));
    }

    #[test]
    fn content_properties_report_macros_export_to_blocks() {
        let storage = r#"<ac:macro ac:name="detailssummary"><ac:parameter ac:name="label">decision-record</ac:parameter><ac:parameter ac:name="id">decision</ac:parameter></ac:macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-content-properties-report"));
        assert!(markdown.contains("label: decision-record"));
        assert!(markdown.contains("id: decision"));
    }

    #[test]
    fn attachments_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="attachments"><ac:parameter ac:name="patterns">*.pdf</ac:parameter><ac:parameter ac:name="sortBy">name</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-attachments"));
        assert!(markdown.contains("patterns: *.pdf"));
        assert!(markdown.contains("sortBy: name"));
    }

    #[test]
    fn view_file_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="view-file"><ac:parameter ac:name="name"><ri:attachment ri:filename="preview.pdf" /></ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-view-file"));
        assert!(markdown.contains("attachment: preview.pdf"));
    }

    #[test]
    fn view_doc_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="viewdoc"><ac:parameter ac:name="page"><ac:link><ri:page ri:space-key="TEST" ri:content-title="Docs Home" /></ac:link></ac:parameter><ac:parameter ac:name="name"><ri:attachment ri:filename="manual.docx" /></ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-view-doc"));
        assert!(markdown.contains("attachment: manual.docx"));
        assert!(markdown.contains("page: confluence-page://page?"));
        assert!(!markdown.contains("content-title=confluence-page%3A%2F%2Fpage%3F"));
    }

    #[test]
    fn view_xls_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="viewxls"><ac:parameter ac:name="name"><ri:attachment ri:filename="sheet.xlsx" /></ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-view-xls"));
        assert!(markdown.contains("attachment: sheet.xlsx"));
    }

    #[test]
    fn view_ppt_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="viewppt"><ac:parameter ac:name="name"><ri:attachment ri:filename="slides.pptx" /></ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-view-ppt"));
        assert!(markdown.contains("attachment: slides.pptx"));
    }

    #[test]
    fn blog_posts_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="blog-posts"><ac:parameter ac:name="author"><ri:user ri:userkey="user-123" /></ac:parameter><ac:parameter ac:name="spaces"><ri:space ri:space-key="TEST" /></ac:parameter><ac:parameter ac:name="max">5</ac:parameter><ac:parameter ac:name="time">7</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-blog-posts"));
        assert!(markdown.contains("author: confluence-user://user?userkey=user-123"));
        assert!(markdown.contains("spaces: TEST"));
        assert!(markdown.contains("max: 5"));
        assert!(markdown.contains("time: 7"));
    }

    #[test]
    fn multi_author_blog_posts_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="blog-posts"><ac:parameter ac:name="author"><ri:user ri:userkey="user-123" /><ri:user ri:account-id="abc123" /></ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains("author: confluence-user://user?userkey=user-123,confluence-user://user?account-id=abc123"));
    }

    #[test]
    fn content_by_user_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="content-by-user"><ac:parameter ac:name=""><ri:user ri:userkey="user-123" /></ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-content-by-user"));
        assert!(markdown.contains("user: confluence-user://user?userkey=user-123"));
    }

    #[test]
    fn user_resource_identifier_text_exports_to_user_placeholder() {
        let storage = r#"<ac:structured-macro ac:name="content-by-user"><ac:parameter ac:name="">com.atlassian.confluence.content.render.xhtml.model.resource.identifiers.UserResourceIdentifier@59a93065[accountId=557058:abc,userKey=&lt;null&gt;,userName=557058:abc]</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-content-by-user"));
        assert!(markdown.contains("user: confluence-user://user?account-id=557058%3Aabc"));
    }

    #[test]
    fn contributors_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="contributors"><ac:parameter ac:name="spaces"><ri:space ri:space-key="TEST" /><ri:space ri:space-key="@personal" /></ac:parameter><ac:parameter ac:name="labels">docs,howto</ac:parameter><ac:parameter ac:name="mode">list</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-contributors"));
        assert!(markdown.contains("spaces: TEST,@personal"));
        assert!(markdown.contains("labels: docs,howto"));
        assert!(markdown.contains("mode: list"));
    }

    #[test]
    fn contributors_summary_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="contributors-summary"><ac:parameter ac:name="spaces"><ri:space ri:space-key="TEST" /></ac:parameter><ac:parameter ac:name="columns">edits,comments,labels</ac:parameter><ac:parameter ac:name="limit">10</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-contributors-summary"));
        assert!(markdown.contains("spaces: TEST"));
        assert!(markdown.contains("columns: edits,comments,labels"));
        assert!(markdown.contains("limit: 10"));
    }

    #[test]
    fn recently_updated_dashboard_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="recently-updated-dashboard"><ac:parameter ac:name="limit">10</ac:parameter><ac:parameter ac:name="theme">concise</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-recently-updated-dashboard"));
        assert!(markdown.contains("limit: 10"));
        assert!(markdown.contains("theme: concise"));
    }

    #[test]
    fn recently_used_labels_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="recently-used-labels"><ac:parameter ac:name="scope">space</ac:parameter><ac:parameter ac:name="style">cloud</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-recently-used-labels"));
        assert!(markdown.contains("scope: space"));
        assert!(markdown.contains("style: cloud"));
    }

    #[test]
    fn gallery_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="gallery"><ac:parameter ac:name="sortBy">name</ac:parameter><ac:parameter ac:name="columns">2</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-gallery"));
        assert!(markdown.contains("sortBy: name"));
        assert!(markdown.contains("columns: 2"));
    }

    #[test]
    fn favorite_pages_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="favpages"/>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-favorite-pages"));
    }

    #[test]
    fn change_history_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="change-history"/>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-change-history"));
    }

    #[test]
    fn profile_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="profile"><ac:parameter ac:name="user"><ri:user ri:account-id="abc123" /></ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-profile"));
        assert!(markdown.contains("user: confluence-user://user?account-id=abc123"));
    }

    #[test]
    fn status_list_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="status-list"><ac:parameter ac:name="username"><ri:user ri:userkey="user-123" /></ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-status-list"));
        assert!(markdown.contains("username: confluence-user://user?userkey=user-123"));
    }

    #[test]
    fn network_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="network"><ac:parameter ac:name="">followers</ac:parameter><ac:parameter ac:name="username"><ri:user ri:userkey="user-123" /></ac:parameter><ac:parameter ac:name="max">10</ac:parameter><ac:parameter ac:name="theme">full</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-network"));
        assert!(markdown.contains("mode: followers"));
        assert!(markdown.contains("username: confluence-user://user?userkey=user-123"));
        assert!(markdown.contains("max: 10"));
        assert!(markdown.contains("theme: full"));
    }

    #[test]
    fn spaces_list_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="spaces"><ac:parameter ac:name="">all</ac:parameter><ac:parameter ac:name="width">80%</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-spaces-list"));
        assert!(markdown.contains("scope: all"));
        assert!(markdown.contains("width: 80%"));
    }

    #[test]
    fn space_details_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="space-details"><ac:parameter ac:name="width">50%</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-space-details"));
        assert!(markdown.contains("width: 50%"));
    }

    #[test]
    fn space_attachments_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="space-attachments"><ac:parameter ac:name="space"><ri:space ri:space-key="TEST" /></ac:parameter><ac:parameter ac:name="showFilter">false</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-space-attachments"));
        assert!(markdown.contains("space: TEST"));
        assert!(markdown.contains("showFilter: false"));
    }

    #[test]
    fn noformat_macros_export_to_fenced_blocks() {
        let storage = r#"<ac:structured-macro ac:name="noformat"><ac:parameter ac:name="nopanel">true</ac:parameter><ac:plain-text-body><![CDATA[<xml>literal</xml>
line 2]]></ac:plain-text-body></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains("~~~confluence-noformat"));
        assert!(markdown.contains("nopanel: true"));
        assert!(markdown.contains("---"));
        assert!(markdown.contains("<xml>literal</xml>"));
        assert!(markdown.contains("line 2"));
    }

    #[test]
    fn toc_zone_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="toc-zone"><ac:parameter ac:name="location">top</ac:parameter><ac:parameter ac:name="maxLevel">3</ac:parameter><ac:rich-text-body><h2>Scoped Heading</h2><p>Only this section counts.</p></ac:rich-text-body></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-toc-zone"));
        assert!(markdown.contains("location: top"));
        assert!(markdown.contains("maxLevel: 3"));
        assert!(markdown.contains("---"));
        assert!(markdown.contains("## Scoped Heading"));
        assert!(markdown.contains("Only this section counts."));
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
    fn livesearch_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="livesearch"><ac:parameter ac:name="spaceKey"><ri:space ri:space-key="TEST" /></ac:parameter><ac:parameter ac:name="labels">docs,howto</ac:parameter><ac:parameter ac:name="size">large</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-livesearch"));
        assert!(markdown.contains("spaceKey: TEST"));
        assert!(markdown.contains("labels: docs,howto"));
        assert!(markdown.contains("size: large"));
    }

    #[test]
    fn page_index_macros_export_to_blocks() {
        let storage = r#"<ac:macro ac:name="page-index" />"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-page-index"));
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
        let storage = r#"<ac:structured-macro ac:name="pagetreesearch"><ac:parameter ac:name="root">TEST:Docs Home</ac:parameter><ac:parameter ac:name="spaceKey"><ri:space ri:space-key="TEST" /></ac:parameter></ac:structured-macro>"#;
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
    fn content_report_table_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="content-report-table"><ac:parameter ac:name="labels">e2e-macro-target</ac:parameter><ac:parameter ac:name="spaces"><ri:space ri:space-key="TEST" /></ac:parameter><ac:parameter ac:name="maxResults">5</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-content-report-table"));
        assert!(markdown.contains("labels: e2e-macro-target"));
        assert!(markdown.contains("spaces: TEST"));
        assert!(markdown.contains("maxResults: 5"));
    }

    #[test]
    fn search_macros_export_to_first_class_blocks() {
        let storage = r#"<ac:structured-macro ac:name="search"><ac:parameter ac:name="spacekey"><ri:space ri:space-key="TEST" /></ac:parameter><ac:parameter ac:name="contributor"><ri:user ri:userkey="user-123" /></ac:parameter><ac:parameter ac:name="query">docs</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-search"));
        assert!(markdown.contains("spacekey: !space TEST"));
        assert!(markdown.contains("contributor: !user confluence-user://user?userkey=user-123"));
        assert!(markdown.contains("query: docs"));
    }

    #[test]
    fn navmap_macros_export_to_first_class_blocks() {
        let storage = r#"<ac:structured-macro ac:name="navmap"><ac:parameter ac:name="">Docs Home,Shared Excerpt</ac:parameter><ac:parameter ac:name="title">Macro navigation</ac:parameter><ac:parameter ac:name="wrapAfter">4</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-navmap"));
        assert!(markdown.contains("$default: Docs Home,Shared Excerpt"));
        assert!(markdown.contains("title: Macro navigation"));
        assert!(markdown.contains("wrapAfter: 4"));
    }

    #[test]
    fn task_report_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="tasks-report-macro"><ac:parameter ac:name="spaceAndPage">TEST</ac:parameter><ac:parameter ac:name="labels">e2e-macro-target</ac:parameter><ac:parameter ac:name="status">incomplete</ac:parameter><ac:parameter ac:name="pageSize">20</ac:parameter><ac:parameter ac:name="columns">description,assignee,location</ac:parameter><ac:parameter ac:name="sortBy">page title</ac:parameter><ac:parameter ac:name="reverseSort">false</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-task-report"));
        assert!(markdown.contains("spaceAndPage: TEST"));
        assert!(markdown.contains("labels: e2e-macro-target"));
        assert!(markdown.contains("status: incomplete"));
        assert!(markdown.contains("pageSize: 20"));
        assert!(markdown.contains("columns: description,assignee,location"));
        assert!(markdown.contains("sortBy: page title"));
        assert!(markdown.contains("reverseSort: false"));
    }

    #[test]
    fn recently_updated_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="recently-updated"><ac:parameter ac:name="author"><ri:user ri:userkey="user-123" /></ac:parameter><ac:parameter ac:name="spaces"><ri:space ri:space-key="TEST" /></ac:parameter><ac:parameter ac:name="max">10</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-recently-updated"));
        assert!(markdown.contains("author: confluence-user://user?userkey=user-123"));
        assert!(markdown.contains("spaces: TEST"));
        assert!(markdown.contains("max: 10"));
    }

    #[test]
    fn labels_list_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="listlabels"><ac:parameter ac:name="spaceKey"><ri:space ri:space-key="TEST" /></ac:parameter><ac:parameter ac:name="excludedLabels">drafts,test</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-labels-list"));
        assert!(markdown.contains("spaceKey: TEST"));
        assert!(markdown.contains("excludedLabels: drafts,test"));
    }

    #[test]
    fn popular_labels_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="popular-labels"><ac:parameter ac:name="spaceKey"><ri:space ri:space-key="TEST" /></ac:parameter><ac:parameter ac:name="count">25</ac:parameter><ac:parameter ac:name="style">heatmap</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-popular-labels"));
        assert!(markdown.contains("spaceKey: TEST"));
        assert!(markdown.contains("count: 25"));
        assert!(markdown.contains("style: heatmap"));
    }

    #[test]
    fn related_labels_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="related-labels"><ac:parameter ac:name="labels">docs,howto</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-related-labels"));
        assert!(markdown.contains("labels: docs,howto"));
    }

    #[test]
    fn unsupported_plain_parameter_macros_export_to_generic_blocks() {
        let storage = r#"<ac:structured-macro ac:name="userlister"><ac:parameter ac:name="group">confluence-users</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-macro userlister"));
        assert!(markdown.contains("group: confluence-users"));
    }

    #[test]
    fn unsupported_resource_parameter_macros_export_to_generic_blocks() {
        let storage = r#"<ac:structured-macro ac:name="custom-resource"><ac:parameter ac:name="spacekey"><ri:space ri:space-key="TEST" /></ac:parameter><ac:parameter ac:name="contributor"><ri:user ri:userkey="user-123" /></ac:parameter><ac:parameter ac:name="query">docs</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-macro custom-resource"));
        assert!(markdown.contains("spacekey: !space TEST"));
        assert!(markdown.contains("contributor: !user confluence-user://user?userkey=user-123"));
        assert!(markdown.contains("query: docs"));
    }

    #[test]
    fn unsupported_multi_user_resource_macros_export_to_generic_blocks() {
        let storage = r#"<ac:structured-macro ac:name="custom-resource"><ac:parameter ac:name="contributor"><ri:user ri:userkey="user-123" /><ri:user ri:account-id="abc123" /></ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-macro custom-resource"));
        assert!(markdown.contains(
            "contributor: !user confluence-user://user?userkey=user-123,confluence-user://user?account-id=abc123"
        ));
    }

    #[test]
    fn unsupported_page_resource_macros_export_to_generic_blocks() {
        let storage = r#"<ac:structured-macro ac:name="custom-page"><ac:parameter ac:name="source"><ri:page ri:content-title="Docs Home" ri:space-key="TEST" /></ac:parameter><ac:parameter ac:name="related"><ac:link><ri:page ri:content-id="12345" /></ac:link></ac:parameter><ac:parameter ac:name=""><ri:user ri:account-id="abc123" /></ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-macro custom-page"));
        assert!(markdown.contains(
            "source: !page confluence-page://page?space-key=TEST&content-title=Docs+Home"
        ));
        assert!(markdown.contains("related: !page-link confluence-page://page?content-id=12345"));
        assert!(markdown.contains("$default: !user confluence-user://user?account-id=abc123"));
    }

    #[test]
    fn unsupported_textual_page_resource_macros_export_to_generic_blocks() {
        let storage = r#"<ac:structured-macro ac:name="custom-page"><ac:parameter ac:name="source">PageResourceIdentifier[spaceKey=TEST,title=Docs Home]</ac:parameter><ac:parameter ac:name="related">DefaultLink[destination=Optional[PageResourceIdentifier[spaceKey=TEST,title=Docs Home]],body=Optional.empty,tooltip=Optional.empty,anchor=Optional.empty,target=Optional.empty]</ac:parameter><ac:parameter ac:name="contributor">com.atlassian.confluence.content.render.xhtml.model.resource.identifiers.UserResourceIdentifier@59a93065[accountId=557058:abc,userKey=&lt;null&gt;,userName=557058:abc]</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-macro custom-page"));
        assert!(markdown.contains(
            "source: !page confluence-page://page?space-key=TEST&content-title=Docs+Home"
        ));
        assert!(markdown.contains(
            "related: !page-link confluence-page://page?space-key=TEST&content-title=Docs+Home"
        ));
        assert!(
            markdown.contains("contributor: !user confluence-user://user?account-id=557058%3Aabc")
        );
    }

    #[test]
    fn unsupported_rich_text_macros_export_to_generic_blocks() {
        let storage = r#"<ac:structured-macro ac:name="custom-rich"><ac:parameter ac:name="mode">summary</ac:parameter><ac:rich-text-body><h2>Heads up</h2><p>Body text.</p></ac:rich-text-body></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-macro custom-rich"));
        assert!(markdown.contains("mode: summary"));
        assert!(markdown.contains("---"));
        assert!(markdown.contains("## Heads up"));
        assert!(markdown.contains("Body text."));
    }

    #[test]
    fn whitespace_between_supported_blocks_does_not_force_fallback_export() {
        let storage = concat!(
            "<h1>Macro Source</h1>\n",
            "<ac:structured-macro ac:name=\"anchor\">",
            "<ac:parameter ac:name=\"\">intro</ac:parameter>",
            "</ac:structured-macro>\n",
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
            "<ac:structured-macro ac:name=\"content-by-user\">",
            "<ac:parameter ac:name=\"\"><ri:user ri:userkey=\"user-123\" /></ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"content-report-table\">",
            "<ac:parameter ac:name=\"labels\">e2e-macro-target</ac:parameter>",
            "<ac:parameter ac:name=\"spaces\"><ri:space ri:space-key=\"TEST\" /></ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"search\">",
            "<ac:parameter ac:name=\"spacekey\"><ri:space ri:space-key=\"TEST\" /></ac:parameter>",
            "<ac:parameter ac:name=\"contributor\"><ri:user ri:userkey=\"user-123\" /></ac:parameter>",
            "<ac:parameter ac:name=\"query\">docs</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"navmap\">",
            "<ac:parameter ac:name=\"\">Docs Home,Shared Excerpt</ac:parameter>",
            "<ac:parameter ac:name=\"title\">Macro navigation</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"tasks-report-macro\">",
            "<ac:parameter ac:name=\"spaceAndPage\">TEST</ac:parameter>",
            "<ac:parameter ac:name=\"status\">incomplete</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"recently-updated\">",
            "<ac:parameter ac:name=\"spaces\">TEST</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"livesearch\">",
            "<ac:parameter ac:name=\"spaceKey\"><ri:space ri:space-key=\"TEST\" /></ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:macro ac:name=\"page-index\" />\n",
            "<ac:structured-macro ac:name=\"attachments\">",
            "<ac:parameter ac:name=\"patterns\">*.pdf</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"view-file\">",
            "<ac:parameter ac:name=\"name\"><ri:attachment ri:filename=\"preview.pdf\" /></ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"viewdoc\">",
            "<ac:parameter ac:name=\"page\">TEST:Docs Home</ac:parameter>",
            "<ac:parameter ac:name=\"name\"><ri:attachment ri:filename=\"manual.docx\" /></ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"viewxls\">",
            "<ac:parameter ac:name=\"name\"><ri:attachment ri:filename=\"sheet.xlsx\" /></ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"viewppt\">",
            "<ac:parameter ac:name=\"name\"><ri:attachment ri:filename=\"slides.pptx\" /></ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"blog-posts\">",
            "<ac:parameter ac:name=\"max\">5</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"contributors\">",
            "<ac:parameter ac:name=\"spaces\"><ri:space ri:space-key=\"TEST\" /></ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"contributors-summary\">",
            "<ac:parameter ac:name=\"spaces\"><ri:space ri:space-key=\"TEST\" /></ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"recently-updated-dashboard\">",
            "<ac:parameter ac:name=\"limit\">10</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"recently-used-labels\">",
            "<ac:parameter ac:name=\"scope\">space</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"gallery\">",
            "<ac:parameter ac:name=\"sortBy\">name</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"favpages\"/>\n",
            "<ac:structured-macro ac:name=\"change-history\"/>\n",
            "<ac:structured-macro ac:name=\"profile\">",
            "<ac:parameter ac:name=\"user\"><ri:user ri:account-id=\"abc123\" /></ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"status-list\">",
            "<ac:parameter ac:name=\"username\"><ri:user ri:userkey=\"user-123\" /></ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"network\">",
            "<ac:parameter ac:name=\"\">followers</ac:parameter>",
            "<ac:parameter ac:name=\"username\"><ri:user ri:userkey=\"user-123\" /></ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"spaces\">",
            "<ac:parameter ac:name=\"\">all</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"space-details\">",
            "<ac:parameter ac:name=\"width\">50%</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"space-attachments\">",
            "<ac:parameter ac:name=\"space\">TEST</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"noformat\">",
            "<ac:plain-text-body><![CDATA[<xml>literal</xml>]]></ac:plain-text-body>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"listlabels\">",
            "<ac:parameter ac:name=\"spaceKey\"><ri:space ri:space-key=\"TEST\" /></ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"popular-labels\">",
            "<ac:parameter ac:name=\"spaceKey\"><ri:space ri:space-key=\"TEST\" /></ac:parameter>",
            "<ac:parameter ac:name=\"count\">10</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"related-labels\">",
            "<ac:parameter ac:name=\"labels\">docs</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"userlister\">",
            "<ac:parameter ac:name=\"group\">confluence-users</ac:parameter>",
            "</ac:structured-macro>\n",
            "<ac:structured-macro ac:name=\"children\">",
            "<ac:parameter ac:name=\"all\">true</ac:parameter>",
            "</ac:structured-macro>"
        );
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains("# Macro Source"));
        assert!(markdown.contains(":::confluence-anchor"));
        assert!(markdown.contains(":::confluence-excerpt-include"));
        assert!(markdown.contains(":::confluence-include-page"));
        assert!(markdown.contains(":::confluence-page-tree"));
        assert!(markdown.contains(":::confluence-page-tree-search"));
        assert!(markdown.contains(":::confluence-content-by-label"));
        assert!(markdown.contains(":::confluence-content-by-user"));
        assert!(markdown.contains(":::confluence-content-report-table"));
        assert!(markdown.contains(":::confluence-search"));
        assert!(markdown.contains(":::confluence-navmap"));
        assert!(markdown.contains(":::confluence-task-report"));
        assert!(markdown.contains(":::confluence-recently-updated"));
        assert!(markdown.contains(":::confluence-livesearch"));
        assert!(markdown.contains(":::confluence-page-index"));
        assert!(markdown.contains(":::confluence-attachments"));
        assert!(markdown.contains(":::confluence-view-file"));
        assert!(markdown.contains(":::confluence-view-doc"));
        assert!(markdown.contains(":::confluence-view-xls"));
        assert!(markdown.contains(":::confluence-view-ppt"));
        assert!(markdown.contains(":::confluence-blog-posts"));
        assert!(markdown.contains(":::confluence-contributors"));
        assert!(markdown.contains(":::confluence-contributors-summary"));
        assert!(markdown.contains(":::confluence-recently-updated-dashboard"));
        assert!(markdown.contains(":::confluence-recently-used-labels"));
        assert!(markdown.contains(":::confluence-gallery"));
        assert!(markdown.contains(":::confluence-favorite-pages"));
        assert!(markdown.contains(":::confluence-change-history"));
        assert!(markdown.contains(":::confluence-profile"));
        assert!(markdown.contains(":::confluence-status-list"));
        assert!(markdown.contains(":::confluence-network"));
        assert!(markdown.contains(":::confluence-spaces-list"));
        assert!(markdown.contains(":::confluence-space-details"));
        assert!(markdown.contains(":::confluence-space-attachments"));
        assert!(markdown.contains("~~~confluence-noformat"));
        assert!(markdown.contains(":::confluence-labels-list"));
        assert!(markdown.contains(":::confluence-popular-labels"));
        assert!(markdown.contains(":::confluence-related-labels"));
        assert!(markdown.contains(":::confluence-macro userlister"));
        assert!(markdown.contains(":::confluence-children"));
        assert!(!markdown.contains("CONFLUENCE_XML_PLACEHOLDER"));
    }

    #[test]
    fn children_macros_export_to_blocks() {
        let storage = r#"<ac:structured-macro ac:name="children"><ac:parameter ac:name="page"><ri:page ri:space-key="TEST" ri:content-title="Docs Home" /></ac:parameter><ac:parameter ac:name="all">true</ac:parameter><ac:parameter ac:name="sort">creation</ac:parameter></ac:structured-macro>"#;
        let markdown = storage_to_markdown(storage);
        assert!(markdown.contains(":::confluence-children"));
        assert!(markdown.contains("page: confluence-page://page?"));
        assert!(markdown.contains("content-title=Docs+Home"));
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
    fn content_properties_blocks_round_trip_back_to_legacy_macros() {
        let markdown = ":::confluence-content-properties\nid: decision\n---\n| Field | Value |\n| --- | --- |\n| Owner | Ada |\n| Status | Approved |\n:::";
        let rendered =
            markdown_to_storage(markdown, false).expect("content-properties block converts");
        assert!(rendered.storage.contains(r#"<ac:macro ac:name="details">"#));
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="id">decision</ac:parameter>"#)
        );
        assert!(rendered.storage.contains("<ac:rich-text-body>"));
        assert!(rendered.storage.contains("<table>"));
        assert!(rendered.storage.contains("<th>Field</th>"));
        assert!(rendered.storage.contains("<td>Ada</td>"));
    }

    #[test]
    fn content_properties_report_blocks_round_trip_back_to_legacy_macros() {
        let markdown =
            ":::confluence-content-properties-report\nlabel: decision-record\nid: decision\n:::";
        let rendered =
            markdown_to_storage(markdown, false).expect("content-properties-report block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:macro ac:name="detailssummary">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="label">decision-record</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="id">decision</ac:parameter>"#)
        );
    }

    #[test]
    fn attachments_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-attachments\npatterns: *.pdf\nsortBy: name\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("attachments block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="attachments">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="patterns">*.pdf</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="sortBy">name</ac:parameter>"#)
        );
    }

    #[test]
    fn anchor_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-anchor\nname: intro\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("anchor block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="anchor">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="">intro</ac:parameter>"#)
        );
    }

    #[test]
    fn view_file_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-view-file\nattachment: preview.pdf\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("view-file block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="view-file">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="name"><ri:attachment ri:filename="preview.pdf" /></ac:parameter>"#
        ));
    }

    #[test]
    fn view_doc_blocks_round_trip_back_to_structured_macros() {
        let page = build_page_placeholder_url(&PageLinkPlaceholder {
            space_key: Some("TEST".to_string()),
            content_title: Some("Docs Home".to_string()),
            ..PageLinkPlaceholder::default()
        });
        let markdown =
            format!(":::confluence-view-doc\npage: {page}\nattachment: manual.docx\n:::");
        let rendered = markdown_to_storage(&markdown, false).expect("view-doc block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="viewdoc">"#)
        );
        assert!(
            rendered
                .storage
                .contains(
                    r#"<ac:parameter ac:name="page"><ac:link><ri:page ri:content-title="Docs Home" ri:space-key="TEST" /></ac:link></ac:parameter>"#
                )
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="name"><ri:attachment ri:filename="manual.docx" /></ac:parameter>"#
        ));
    }

    #[test]
    fn view_xls_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-view-xls\nattachment: sheet.xlsx\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("view-xls block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="viewxls">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="name"><ri:attachment ri:filename="sheet.xlsx" /></ac:parameter>"#
        ));
    }

    #[test]
    fn view_ppt_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-view-ppt\nattachment: slides.pptx\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("view-ppt block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="viewppt">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="name"><ri:attachment ri:filename="slides.pptx" /></ac:parameter>"#
        ));
    }

    #[test]
    fn blog_posts_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-blog-posts\nauthor: confluence-user://user?userkey=user-123\nspaces: TEST\nmax: 5\ntime: 7\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("blog-posts block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="blog-posts">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="author"><ri:user ri:userkey="user-123" /></ac:parameter>"#
        ));
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="spaces"><ri:space ri:space-key="TEST" /></ac:parameter>"#
        ));
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="max">5</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="time">7</ac:parameter>"#)
        );
    }

    #[test]
    fn multi_author_blog_posts_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-blog-posts\nauthor: confluence-user://user?userkey=user-123,confluence-user://user?account-id=abc123\n:::";
        let rendered =
            markdown_to_storage(markdown, false).expect("multi-author blog-posts block converts");
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="author"><ri:user ri:userkey="user-123" /><ri:user ri:account-id="abc123" /></ac:parameter>"#
        ));
    }

    #[test]
    fn contributors_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-contributors\nspaces: TEST,@personal\nlabels: docs,howto\nmode: list\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("contributors block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="contributors">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="spaces"><ri:space ri:space-key="TEST" /><ri:space ri:space-key="@personal" /></ac:parameter>"#
        ));
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="labels">docs,howto</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="mode">list</ac:parameter>"#)
        );
    }

    #[test]
    fn contributors_summary_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-contributors-summary\nspaces: TEST\ncolumns: edits,comments,labels\nlimit: 10\n:::";
        let rendered =
            markdown_to_storage(markdown, false).expect("contributors-summary block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="contributors-summary">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="spaces"><ri:space ri:space-key="TEST" /></ac:parameter>"#
        ));
        assert!(
            rendered.storage.contains(
                r#"<ac:parameter ac:name="columns">edits,comments,labels</ac:parameter>"#
            )
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="limit">10</ac:parameter>"#)
        );
    }

    #[test]
    fn recently_updated_dashboard_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-recently-updated-dashboard\nlimit: 10\ntheme: concise\n:::";
        let rendered = markdown_to_storage(markdown, false)
            .expect("recently-updated-dashboard block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="recently-updated-dashboard">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="limit">10</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="theme">concise</ac:parameter>"#)
        );
    }

    #[test]
    fn recently_used_labels_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-recently-used-labels\nscope: space\nstyle: cloud\n:::";
        let rendered =
            markdown_to_storage(markdown, false).expect("recently-used-labels block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="recently-used-labels">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="scope">space</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="style">cloud</ac:parameter>"#)
        );
    }

    #[test]
    fn gallery_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-gallery\nsortBy: name\ncolumns: 2\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("gallery block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="gallery">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="sortBy">name</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="columns">2</ac:parameter>"#)
        );
    }

    #[test]
    fn favorite_pages_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-favorite-pages\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("favorite-pages block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:macro ac:name="favpages" />"#)
                || rendered
                    .storage
                    .contains(r#"<ac:structured-macro ac:name="favpages">"#)
        );
    }

    #[test]
    fn change_history_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-change-history\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("change-history block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:macro ac:name="change-history" />"#)
                || rendered
                    .storage
                    .contains(r#"<ac:structured-macro ac:name="change-history">"#)
        );
    }

    #[test]
    fn profile_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-profile\nuser: confluence-user://user?account-id=abc123\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("profile block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="profile">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="user"><ri:user ri:account-id="abc123" /></ac:parameter>"#
        ));
    }

    #[test]
    fn status_list_blocks_round_trip_back_to_structured_macros() {
        let markdown =
            ":::confluence-status-list\nusername: confluence-user://user?userkey=user-123\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("status-list block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="status-list">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="username"><ri:user ri:userkey="user-123" /></ac:parameter>"#
        ));
    }

    #[test]
    fn network_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-network\nmode: followers\nusername: confluence-user://user?userkey=user-123\nmax: 10\ntheme: full\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("network block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="network">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="">followers</ac:parameter>"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="username"><ri:user ri:userkey="user-123" /></ac:parameter>"#
        ));
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="max">10</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="theme">full</ac:parameter>"#)
        );
    }

    #[test]
    fn spaces_list_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-spaces-list\nscope: all\nwidth: 80%\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("spaces-list block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="spaces">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="">all</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="width">80%</ac:parameter>"#)
        );
    }

    #[test]
    fn space_details_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-space-details\nwidth: 50%\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("space-details block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="space-details">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="width">50%</ac:parameter>"#)
        );
    }

    #[test]
    fn space_attachments_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-space-attachments\nspace: TEST\nshowFilter: false\n:::";
        let rendered =
            markdown_to_storage(markdown, false).expect("space-attachments block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="space-attachments">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="space"><ri:space ri:space-key="TEST" /></ac:parameter>"#
        ));
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="showFilter">false</ac:parameter>"#)
        );
    }

    #[test]
    fn noformat_blocks_round_trip_back_to_structured_macros() {
        let markdown =
            "~~~confluence-noformat\nnopanel: true\n---\n<xml>literal</xml>\nline 2\n~~~";
        let rendered = markdown_to_storage(markdown, false).expect("noformat block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="noformat">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="nopanel">true</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains("<ac:plain-text-body><![CDATA[<xml>literal</xml>\nline 2]]>")
        );
    }

    #[test]
    fn toc_zone_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-toc-zone\nlocation: top\nmaxLevel: 3\n---\n## Scoped Heading\n\nBody text.\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("toc-zone block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="toc-zone">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="location">top</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="maxLevel">3</ac:parameter>"#)
        );
        assert!(rendered.storage.contains("<ac:rich-text-body>"));
        assert!(rendered.storage.contains("<h2>Scoped Heading</h2>"));
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
    fn livesearch_blocks_round_trip_back_to_structured_macros() {
        let markdown =
            ":::confluence-livesearch\nspaceKey: TEST\nlabels: docs,howto\nsize: large\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("livesearch block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="livesearch">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="spaceKey"><ri:space ri:space-key="TEST" /></ac:parameter>"#
        ));
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="labels">docs,howto</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="size">large</ac:parameter>"#)
        );
    }

    #[test]
    fn page_index_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-page-index\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("page-index block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:macro ac:name="page-index" />"#)
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
    fn content_by_user_blocks_round_trip_back_to_structured_macros() {
        let markdown =
            ":::confluence-content-by-user\nuser: confluence-user://user?userkey=user-123\n:::";
        let rendered =
            markdown_to_storage(markdown, false).expect("content-by-user block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="content-by-user">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name=""><ri:user ri:userkey="user-123" /></ac:parameter>"#
        ));
    }

    #[test]
    fn user_resource_identifier_text_round_trips_back_to_structured_user_resource() {
        let markdown = ":::confluence-content-by-user\nuser: com.atlassian.confluence.content.render.xhtml.model.resource.identifiers.UserResourceIdentifier@59a93065[accountId=557058:abc,userKey=<null>,userName=557058:abc]\n:::";
        let rendered =
            markdown_to_storage(markdown, false).expect("user identifier string converts");
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name=""><ri:user ri:account-id="557058:abc" /></ac:parameter>"#
        ));
    }

    #[test]
    fn content_report_table_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-content-report-table\nlabels: e2e-macro-target\nspaces: TEST\nmaxResults: 5\n:::";
        let rendered =
            markdown_to_storage(markdown, false).expect("content-report-table block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="content-report-table">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="labels">e2e-macro-target</ac:parameter>"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="spaces"><ri:space ri:space-key="TEST" /></ac:parameter>"#
        ));
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="maxResults">5</ac:parameter>"#)
        );
    }

    #[test]
    fn search_blocks_round_trip_back_to_structured_macros() {
        let user = build_user_placeholder_url(&UserMentionPlaceholder {
            user_key: Some("user-123".to_string()),
            ..UserMentionPlaceholder::default()
        });
        let markdown = format!(
            ":::confluence-search\nspacekey: !space TEST,@personal\ncontributor: !user {user}\nquery: docs\n:::"
        );
        let rendered = markdown_to_storage(&markdown, false).expect("search block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="search">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="spacekey"><ri:space ri:space-key="TEST" /><ri:space ri:space-key="@personal" /></ac:parameter>"#
        ));
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="contributor"><ri:user ri:userkey="user-123" /></ac:parameter>"#
        ));
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="query">docs</ac:parameter>"#)
        );
    }

    #[test]
    fn navmap_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-navmap\n$default: Docs Home,Shared Excerpt\ntitle: Macro navigation\nwrapAfter: 4\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("navmap block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="navmap">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="">Docs Home,Shared Excerpt</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="title">Macro navigation</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="wrapAfter">4</ac:parameter>"#)
        );
    }

    #[test]
    fn task_report_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-task-report\nspaceAndPage: TEST\nlabels: e2e-macro-target\nstatus: incomplete\npageSize: 20\ncolumns: description,assignee,location\nsortBy: page title\nreverseSort: false\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("task-report block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="tasks-report-macro">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="spaceAndPage">TEST</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="labels">e2e-macro-target</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="status">incomplete</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="pageSize">20</ac:parameter>"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="columns">description,assignee,location</ac:parameter>"#
        ));
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="sortBy">page title</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="reverseSort">false</ac:parameter>"#)
        );
    }

    #[test]
    fn generic_parameter_only_macro_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-macro userlister\ngroup: confluence-users\n:::";
        let rendered =
            markdown_to_storage(markdown, false).expect("generic parameter macro converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="userlister">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="group">confluence-users</ac:parameter>"#)
        );
    }

    #[test]
    fn generic_resource_parameter_macro_blocks_round_trip_back_to_structured_macros() {
        let page = build_page_placeholder_url(&PageLinkPlaceholder {
            space_key: Some("TEST".to_string()),
            content_title: Some("Docs Home".to_string()),
            ..PageLinkPlaceholder::default()
        });
        let linked_page = build_page_placeholder_url(&PageLinkPlaceholder {
            content_id: Some("12345".to_string()),
            ..PageLinkPlaceholder::default()
        });
        let user = build_user_placeholder_url(&UserMentionPlaceholder {
            user_key: Some("user-123".to_string()),
            ..UserMentionPlaceholder::default()
        });
        let markdown = format!(
            ":::confluence-macro search\nspacekey: !space TEST,@personal\ncontributor: !user {user}\nsource: !page {page}\nrelated: !page-link {linked_page}\n$default: plain default\nquery: docs\n:::"
        );
        let rendered =
            markdown_to_storage(&markdown, false).expect("generic resource macro converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="search">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="spacekey"><ri:space ri:space-key="TEST" /><ri:space ri:space-key="@personal" /></ac:parameter>"#
        ));
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="contributor"><ri:user ri:userkey="user-123" /></ac:parameter>"#
        ));
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="source"><ri:page ri:content-title="Docs Home" ri:space-key="TEST" /></ac:parameter>"#
        ));
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="related"><ac:link><ri:page ri:content-id="12345" /></ac:link></ac:parameter>"#
        ));
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="">plain default</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="query">docs</ac:parameter>"#)
        );
    }

    #[test]
    fn generic_multi_user_parameter_macro_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-macro search\ncontributor: !user confluence-user://user?userkey=user-123,confluence-user://user?account-id=abc123\n:::";
        let rendered =
            markdown_to_storage(markdown, false).expect("generic multi-user macro converts");
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="contributor"><ri:user ri:userkey="user-123" /><ri:user ri:account-id="abc123" /></ac:parameter>"#
        ));
    }

    #[test]
    fn generic_rich_text_macro_blocks_round_trip_back_to_structured_macros() {
        let markdown =
            ":::confluence-macro custom-rich\nmode: summary\n---\n## Heads up\n\nBody text.\n:::";
        let rendered =
            markdown_to_storage(markdown, false).expect("generic rich-text macro converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="custom-rich">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="mode">summary</ac:parameter>"#)
        );
        assert!(rendered.storage.contains("<ac:rich-text-body>"));
        assert!(rendered.storage.contains("<h2>Heads up</h2>"));
        assert!(rendered.storage.contains("<p>Body text.</p>"));
    }

    #[test]
    fn recently_updated_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-recently-updated\nauthor: confluence-user://user?userkey=user-123\nspaces: TEST\nmax: 10\n:::";
        let rendered =
            markdown_to_storage(markdown, false).expect("recently-updated block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="recently-updated">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="author"><ri:user ri:userkey="user-123" /></ac:parameter>"#
        ));
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
    fn labels_list_blocks_round_trip_back_to_structured_macros() {
        let markdown =
            ":::confluence-labels-list\nspaceKey: TEST\nexcludedLabels: drafts,test\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("labels-list block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="listlabels">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="spaceKey"><ri:space ri:space-key="TEST" /></ac:parameter>"#
        ));
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="excludedLabels">drafts,test</ac:parameter>"#)
        );
    }

    #[test]
    fn popular_labels_blocks_round_trip_back_to_structured_macros() {
        let markdown =
            ":::confluence-popular-labels\nspaceKey: TEST\ncount: 25\nstyle: heatmap\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("popular-labels block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="popular-labels">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="spaceKey"><ri:space ri:space-key="TEST" /></ac:parameter>"#
        ));
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="count">25</ac:parameter>"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="style">heatmap</ac:parameter>"#)
        );
    }

    #[test]
    fn related_labels_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-related-labels\nlabels: docs,howto\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("related-labels block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="related-labels">"#)
        );
        assert!(
            rendered
                .storage
                .contains(r#"<ac:parameter ac:name="labels">docs,howto</ac:parameter>"#)
        );
    }

    #[test]
    fn macro_placeholder_replacement_is_stable_beyond_single_digits() {
        let mut markdown = String::new();
        for _ in 0..12 {
            markdown.push_str(":::confluence-include-page\n");
            markdown
                .push_str("page: confluence-page://page?space-key=TEST&content-title=Docs+Home\n");
            markdown.push_str(":::\n\n");
        }
        let rendered =
            markdown_to_storage(&markdown, false).expect("many macro placeholders convert");
        assert_eq!(rendered.storage.matches(r#"ac:name="include""#).count(), 12);
        assert!(!rendered.storage.contains("CONFLUENCE_MACRO_PLACEHOLDER_"));
        assert!(!rendered.storage.contains("</ac:structured-macro>0"));
        assert!(!rendered.storage.contains("</ac:structured-macro>1"));
    }

    #[test]
    fn children_blocks_round_trip_back_to_structured_macros() {
        let markdown = ":::confluence-children\npage: confluence-page://page?space-key=TEST&content-title=Docs+Home\nall: true\nsort: creation\n:::";
        let rendered = markdown_to_storage(markdown, false).expect("children block converts");
        assert!(
            rendered
                .storage
                .contains(r#"<ac:structured-macro ac:name="children">"#)
        );
        assert!(rendered.storage.contains(
            r#"<ac:parameter ac:name="page"><ac:link><ri:page ri:content-title="Docs Home" ri:space-key="TEST" /></ac:link></ac:parameter>"#
        ));
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
        let storage = r#"<table><thead><tr><th>Name</th><th>Status</th></tr></thead><tbody><tr><td>CLI</td><td>Ready</td></tr></tbody></table>"#;
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

    #[test]
    fn text_full_of_ampersands_converts_in_linear_time() {
        let storage = format!("<p>{}</p>", "a=1&amp;b=2 ".repeat(300_000));
        let started = std::time::Instant::now();

        let markdown = storage_to_markdown(&storage);

        assert!(
            markdown.starts_with("a=1&b=2 a=1&b=2"),
            "{}",
            &markdown[..40]
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );
    }
}
