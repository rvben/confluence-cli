//! Local structural checks for Confluence storage fragments, not HTML documents.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;
use roxmltree::{Document, Node, ParsingOptions, TextPos};
use serde_json::json;

use crate::output::{ErrorKind, typed_error_with_details};

// Atlassian's storage namespaces; explicit declarations and alternate prefixes
// must receive the same checks as the implicit ac/ri prefixes in API fragments.
// https://docs.atlassian.com/atlassian-confluence/4.1.3/constant-values.html
const AC_NS: &str = "http://atlassian.com/content";
const RI_NS: &str = "http://atlassian.com/resource/identifier";

pub(crate) fn validate_storage(storage: &str) -> Result<()> {
    // Confluence accepts named XHTML entities (e.g. &nbsp;). Supply inert
    // declarations only for non-XML entities, leaving their actual resolution
    // to Confluence. The body remains byte-for-byte intact: replacing references
    // with a regex could accidentally turn invalid element/attribute names into
    // valid XML, or hide illegal characters in comments and CDATA.
    let wrapped = wrap_fragment(storage, |_| " ".to_string());
    // Only the synthetic DTD can occur before the root. User-supplied DTDs are
    // invalid inside a fragment, and roxmltree never fetches external entities.
    let options = ParsingOptions {
        allow_dtd: true,
        ..ParsingOptions::default()
    };
    let document = Document::parse_with_options(&wrapped, options).map_err(|error| {
        let position = if matches!(
            error,
            roxmltree::Error::UnexpectedEndOfStream | roxmltree::Error::UnclosedRootNode
        ) {
            end_position(storage)
        } else {
            body_position(storage, error.pos())
        };
        let mut reason = error
            .to_string()
            .replace(&format!(" at {}", error.pos()), "");
        if let roxmltree::Error::UnexpectedCloseTag(expected, actual, _) = &error
            && actual == "storage-root"
            && error.pos().row > end_position(storage).row + 1
        {
            reason = format!("element '{expected}' is not closed before the end of the body");
        }
        invalid_storage(position, &reason)
    })?;

    for node in document.descendants().filter(|node| {
        node.has_tag_name((AC_NS, "structured-macro")) || node.has_tag_name((AC_NS, "macro"))
    }) {
        let name = node.attribute((AC_NS, "name")).unwrap_or_default();
        let mut bodies = node.children().filter(|child| {
            child.has_tag_name((AC_NS, "plain-text-body"))
                || child.has_tag_name((AC_NS, "rich-text-body"))
        });
        let first_body = bodies.next();
        if let Some(extra) = bodies.next() {
            return Err(invalid_storage(
                body_position(storage, document.text_pos_at(extra.range().start)),
                "a macro can have only one body; remove the duplicate body element",
            ));
        }
        let expected = match name {
            "code" | "noformat" => "plain-text-body",
            "expand" | "info" | "note" | "warning" | "tip" | "panel" => "rich-text-body",
            _ => continue, // Custom macro contracts belong to Confluence.
        };
        if let Some(body) = first_body
            && body.tag_name().name() != expected
        {
            return Err(invalid_storage(
                body_position(storage, document.text_pos_at(body.range().start)),
                &format!(
                    "macro '{name}' requires ac:{expected}, found ac:{}",
                    body.tag_name().name()
                ),
            ));
        }
    }
    for node in document
        .descendants()
        .filter(|node| node.has_tag_name((AC_NS, "plain-text-body")))
    {
        if let Some(child) = node.children().find(|child| child.is_element()) {
            return Err(invalid_storage(
                body_position(storage, document.text_pos_at(child.range().start)),
                "ac:plain-text-body cannot contain XML elements; wrap literal code in CDATA or escape its text",
            ));
        }
    }
    Ok(())
}

/// Wraps a storage body in a root element that declares the `ac` and `ri`
/// namespaces and the named entities the body uses, each with the value
/// `entity_value` gives it.
fn wrap_fragment(storage: &str, entity_value: impl Fn(&str) -> String) -> String {
    let declarations = entity_declarations(storage, |name| Some(entity_value(name)));
    // Bodies are fragments with multiple top-level nodes and implicit ac/ri
    // namespaces. A newline keeps original columns intact on the first line.
    format!(
        "<!DOCTYPE storage-root [{declarations}]><storage-root xmlns:ac=\"{AC_NS}\" xmlns:ri=\"{RI_NS}\">\n{storage}\n</storage-root>"
    )
}

/// A declaration for each entity the body names beyond those XML defines,
/// giving it the value `entity_value` returns. An entity without a value is
/// left undeclared.
fn entity_declarations(storage: &str, entity_value: impl Fn(&str) -> Option<String>) -> String {
    static ENTITIES: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"&[A-Za-z][A-Za-z0-9]*;").expect("valid entity regex"));
    let entities: BTreeSet<_> = ENTITIES
        .find_iter(storage)
        .map(|reference| &storage[reference.start() + 1..reference.end() - 1])
        .filter(|name| !matches!(*name, "amp" | "lt" | "gt" | "quot" | "apos"))
        .collect();
    let mut declarations = String::new();
    for name in entities {
        // Names come from the restricted regex, so cannot inject declarations.
        if let Some(value) = entity_value(name) {
            declarations.push_str(&format!("<!ENTITY {name} '{value}'>"));
        }
    }
    declarations
}

/// Declarations that make each named entity of HTML in the body stand for
/// its characters. A name that HTML does not have, or that stands for
/// markup, is left undeclared: it is no reference where it is written as
/// text, in a comment or in character data, and anywhere else the body is
/// not well formed.
pub(crate) fn named_character_declarations(storage: &str) -> String {
    entity_declarations(storage, named_characters)
}

/// Whether two storage bodies describe the same document. Spellings that
/// Confluence treats alike compare equal: attribute order, layout white
/// space, a named entity and its characters, `b` and `strong`, `i` and `em`,
/// `ac:macro` and `ac:structured-macro`, a list item's sole paragraph, a
/// list that starts at one, the line break that ends a code block, and what
/// Confluence assigns to an element itself: `local-id`, `ac:macro-id` and
/// `ac:schema-version`. A body that is not well formed is the same document
/// only as itself.
pub(crate) fn same_document(left: &str, right: &str) -> bool {
    left == right
        || matches!(
            (canonical_form(left), canonical_form(right)),
            (Some(left), Some(right)) if left == right
        )
}

fn canonical_form(storage: &str) -> Option<String> {
    // An entity HTML names says the same as the characters it stands for,
    // which is how converted Markdown spells it. Any other entity stands for
    // itself, so that it is the same only as the same entity.
    let wrapped = wrap_fragment(storage, |name| {
        named_characters(name).unwrap_or_else(|| format!("\u{e000}{name}\u{e001}"))
    });
    let options = ParsingOptions {
        allow_dtd: true,
        ..ParsingOptions::default()
    };
    let document = Document::parse_with_options(&wrapped, options).ok()?;
    let mut out = String::new();
    for child in document.root_element().children() {
        write_canonical(child, false, &mut out);
    }
    Some(out)
}

/// The characters of an HTML named entity, as references an entity
/// declaration can hold. Markup characters are left out, because a
/// declaration would read them as markup.
fn named_characters(name: &str) -> Option<String> {
    let (first, second) = *markup5ever::data::NAMED_ENTITIES.get(format!("{name};").as_str())?;
    let characters: Vec<u32> = [first, second]
        .into_iter()
        .filter(|code_point| *code_point != 0)
        .collect();
    let plain = !characters.is_empty()
        && characters
            .iter()
            .all(|code_point| !matches!(char::from_u32(*code_point), None | Some('<' | '&')));
    plain.then(|| {
        characters
            .iter()
            .map(|code_point| format!("&#x{code_point:x};"))
            .collect()
    })
}

fn write_canonical(node: Node<'_, '_>, preformatted: bool, out: &mut String) {
    if node.is_text() {
        write_canonical_text(node, preformatted, out);
        return;
    }
    if node.is_comment() {
        // A parser reads every line ending as a line feed, also in a comment.
        let text = node.text().unwrap_or_default();
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        out.push_str(&format!("<!--{text}-->"));
        return;
    }
    if !node.is_element() {
        return;
    }
    let plain = node.tag_name().namespace().is_none();
    let name = match node.tag_name().name() {
        "b" if plain => "strong",
        "i" if plain => "em",
        "macro" if node.tag_name().namespace() == Some(AC_NS) => "structured-macro",
        name => name,
    };
    out.push_str(&format!(
        "<{}{name}",
        namespace_prefix(node.tag_name().namespace())
    ));
    let mut attributes: Vec<String> = node
        .attributes()
        .filter(|attribute| {
            let assigned = match attribute.namespace() {
                None => attribute.name() == "local-id",
                Some(AC_NS) => {
                    matches!(attribute.name(), "local-id" | "macro-id" | "schema-version")
                }
                Some(_) => false,
            };
            let default_start = plain
                && name == "ol"
                && attribute.name() == "start"
                && attribute.value().trim() == "1";
            !assigned && !default_start
        })
        .map(|attribute| {
            format!(
                " {}{}={:?}",
                namespace_prefix(attribute.namespace()),
                attribute.name(),
                attribute.value()
            )
        })
        .collect();
    attributes.sort();
    out.push_str(&attributes.concat());
    out.push('>');
    let content = sole_paragraph_of_list_item(node).unwrap_or(node);
    for child in content.children() {
        write_canonical(child, preformatted || (plain && name == "pre"), out);
    }
    out.push_str("</>");
}

fn namespace_prefix(namespace: Option<&str>) -> String {
    match namespace {
        None => String::new(),
        Some(AC_NS) => "ac:".to_string(),
        Some(RI_NS) => "ri:".to_string(),
        Some(other) => format!("{{{other}}}"),
    }
}

/// The paragraph that is all a list item holds, which says the same as the
/// item holding the paragraph's content directly.
fn sole_paragraph_of_list_item<'a, 'input>(node: Node<'a, 'input>) -> Option<Node<'a, 'input>> {
    if !node.has_tag_name("li") {
        return None;
    }
    let mut elements = node.children().filter(Node::is_element);
    let paragraph = elements.next().filter(|child| child.has_tag_name("p"))?;
    let alone = elements.next().is_none()
        && node
            .children()
            .filter(Node::is_text)
            .all(|text| text.text().unwrap_or_default().trim().is_empty());
    let bare = paragraph
        .attributes()
        .all(|attribute| attribute.namespace().is_none() && attribute.name() == "local-id");
    (alone && bare).then_some(paragraph)
}

/// The HTML elements that stand in a line of text.
const PHRASING: [&str; 37] = [
    "a", "abbr", "acronym", "b", "bdi", "bdo", "big", "cite", "code", "data", "del", "dfn", "em",
    "font", "i", "img", "ins", "kbd", "label", "mark", "q", "s", "samp", "small", "span", "strike",
    "strong", "sub", "sup", "time", "tt", "u", "var", "wbr", "ruby", "rt", "rp",
];

fn is_phrasing(node: Node<'_, '_>) -> bool {
    node.tag_name().namespace().is_none() && PHRASING.contains(&node.tag_name().name())
}

/// Whether a node sits on lines of its own, so that white space in the text
/// beside it shows nothing. A macro does unless it stands in running text:
/// inside a paragraph, a heading or an inline element, or beside text or an
/// inline element.
fn breaks_line(node: Node<'_, '_>) -> bool {
    const RUNNING_TEXT: [&str; 7] = ["p", "h1", "h2", "h3", "h4", "h5", "h6"];
    if !node.is_element() {
        return !node.is_text();
    }
    let name = node.tag_name().name();
    match node.tag_name().namespace() {
        None => !PHRASING.contains(&name),
        Some(AC_NS) => match name {
            "link" | "image" | "emoticon" | "placeholder" | "inline-comment-marker" => false,
            "structured-macro" | "macro" => {
                let within_text = node.ancestors().skip(1).any(|ancestor| {
                    is_phrasing(ancestor)
                        || (ancestor.tag_name().namespace().is_none()
                            && RUNNING_TEXT.contains(&ancestor.tag_name().name()))
                });
                let beside_text = node.parent().is_some_and(|parent| {
                    parent.children().any(|sibling| {
                        is_phrasing(sibling)
                            || (sibling.is_text()
                                && !sibling.text().unwrap_or_default().trim().is_empty())
                    })
                });
                !within_text && !beside_text
            }
            _ => true,
        },
        Some(_) => true,
    }
}

fn write_canonical_text(node: Node<'_, '_>, preformatted: bool, out: &mut String) {
    let text = node.text().unwrap_or_default();
    let parent = node.parent();
    let literal_body = parent.is_some_and(|parent| {
        parent.has_tag_name((AC_NS, "plain-text-body"))
            || parent.has_tag_name((AC_NS, "plain-text-link-body"))
            || parent.has_tag_name((AC_NS, "parameter"))
    });
    if preformatted || literal_body {
        let ends_code_block = node.next_sibling().is_none()
            && parent.is_some_and(|code| {
                code.has_tag_name("code")
                    && code.parent().is_some_and(|pre| pre.has_tag_name("pre"))
            });
        let text = match text.strip_suffix('\n') {
            Some(stripped) if ends_code_block => stripped,
            _ => text,
        };
        if !text.is_empty() {
            out.push_str(&format!("{text:?}"));
        }
        return;
    }
    // A line break at an end of a text node is layout where a line starts or
    // ends there anyway: at the edge of a parent that sits on lines of its
    // own, or beside an element that does. So is text of nothing but white
    // space between two such places. Other white space separates words, also
    // from a neighbouring element.
    let layout = |edge: &str| edge.contains('\n');
    let line_starts = |sibling: Option<Node<'_, '_>>| match sibling {
        Some(sibling) => breaks_line(sibling),
        None => parent.is_none_or(breaks_line),
    };
    let starts_line = line_starts(node.prev_sibling());
    let ends_line = line_starts(node.next_sibling());
    let is_space = |character: char| character.is_ascii_whitespace();
    let leading = &text[..text.len() - text.trim_start_matches(is_space).len()];
    let trailing = &text[text.trim_end_matches(is_space).len()..];
    let words = text
        .split(is_space)
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if words.is_empty() {
        let shows_nothing =
            (starts_line && ends_line) || (layout(text) && (starts_line || ends_line));
        if !text.is_empty() && !shows_nothing {
            out.push_str("\" \"");
        }
        return;
    }
    let before = if leading.is_empty() || (layout(leading) && starts_line) {
        ""
    } else {
        " "
    };
    let after = if trailing.is_empty() || (layout(trailing) && ends_line) {
        ""
    } else {
        " "
    };
    out.push_str(&format!("{:?}", format!("{before}{words}{after}")));
}

/// Coordinates here refer to converted storage, not to the Markdown source.
/// `validate_converted_markdown` also names the Markdown lines.
pub(crate) fn validate_generated_storage(storage: &str) -> Result<()> {
    validate_storage(storage).map_err(|error| match error.downcast::<crate::output::CliError>() {
        Ok(mut error) => {
            error.message = error.message.replacen("invalid Confluence storage body", "invalid storage generated from Markdown", 1);
            error.hint = Some("Check raw HTML and confluence-storage blocks in the Markdown input. Line and column refer to the generated storage XML, not the Markdown file. --allow-lossy does not permit malformed XML.".to_string());
            if let Some(details) = &mut error.details {
                details["input_format"] = json!("markdown");
            }
            error.into()
        }
        Err(error) => error,
    })
}

/// The line and column, counted from one, just past the end of `storage`,
/// which is where an element that is never closed is reported.
pub(crate) fn end_line_and_column(storage: &str) -> (u32, u32) {
    let end = end_position(storage);
    (end.row, end.col)
}

fn end_position(storage: &str) -> TextPos {
    let row = storage.bytes().filter(|byte| *byte == b'\n').count() as u32 + 1;
    let col = storage
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .chars()
        .count() as u32
        + 1;
    TextPos::new(row, col)
}

fn body_position(storage: &str, wrapped: TextPos) -> TextPos {
    let end = end_position(storage);
    let row = wrapped.row.saturating_sub(1).max(1);
    if row > end.row {
        end
    } else {
        TextPos::new(row, wrapped.col)
    }
}

fn invalid_storage(position: TextPos, reason: &str) -> anyhow::Error {
    typed_error_with_details(
        ErrorKind::InvalidInput,
        format!("invalid Confluence storage body at line {}, column {}: {reason}", position.row, position.col),
        Some("Fix the storage XML locally before retrying. Check closing tags, macro body types, CDATA terminators, and escaped text. Use --format markdown for Markdown input.".to_string()),
        json!({"line": position.row, "column": position.col, "reason": reason}),
    )
}

/// Whether a storage body is invalid where it ends, which is how an element
/// that is never closed is reported.
pub(crate) fn is_invalid_at_end(storage: &str) -> bool {
    let Err(error) = validate_storage(storage) else {
        return false;
    };
    let (line, column) = end_line_and_column(storage);
    error
        .downcast_ref::<crate::output::CliError>()
        .and_then(|error| error.details.as_ref())
        .is_some_and(|details| {
            details["line"].as_u64() == Some(u64::from(line))
                && details["column"].as_u64() == Some(u64::from(column))
        })
}

/// Why a storage body is invalid, without where.
pub(crate) fn invalid_reason(storage: &str) -> Option<String> {
    let error = validate_storage(storage).err()?;
    let error = error.downcast_ref::<crate::output::CliError>()?;
    Some(error.details.as_ref()?.get("reason")?.as_str()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_storage_fragments_and_preserves_body() {
        for body in [
            "",
            "<p>one &amp; two &nbsp; &#160;</p><p>three</p>",
            r#"<ac:link><ri:page ri:content-title="Docs"/><ac:plain-text-link-body><![CDATA[Docs]]></ac:plain-text-link-body></ac:link>"#,
            r#"<ac:structured-macro ac:name="expand"><ac:rich-text-body><p>Text</p></ac:rich-text-body></ac:structured-macro>"#,
            r#"<ac:structured-macro ac:name="code"><ac:plain-text-body><![CDATA[<tag>&nbsp;]]]]><![CDATA[>]]></ac:plain-text-body></ac:structured-macro>"#,
            r#"<ac:structured-macro ac:name="custom"><ac:rich-text-body><p>Text</p></ac:rich-text-body></ac:structured-macro>"#,
        ] {
            validate_storage(body).unwrap_or_else(|error| panic!("{body}: {error}"));
        }
    }

    #[test]
    fn rejects_reported_parse_failures_and_unsafe_text() {
        for body in [
            "<ac:plain-text-body><![CDATA[unterminated",
            "<ac:plain-text-body></ac:rich-text-body>",
            "<ac:rich-text-body><pre></ac:rich-text-body>",
            "<ac:structured-macro><ac:rich-text-body><p>Text</p></ac:rich-text-body>",
            "<p>unescaped & text</p>",
            "<p>bad ]]> text</p>",
            "<p>bad &#0; reference</p>",
        ] {
            let error = validate_storage(body).expect_err(body);
            let error = error.downcast_ref::<crate::output::CliError>().unwrap();
            assert_eq!(error.kind, ErrorKind::InvalidInput);
            assert!(error.details.as_ref().unwrap()["line"].as_u64().unwrap() >= 1);
        }
    }

    #[test]
    fn rejects_well_formed_macros_with_wrong_body_type() {
        for (name, body) in [
            ("code", "rich-text-body"),
            ("noformat", "rich-text-body"),
            ("expand", "plain-text-body"),
        ] {
            let storage = format!(
                "<ac:structured-macro ac:name=\"{name}\"><ac:{body}>text</ac:{body}></ac:structured-macro>"
            );
            assert!(
                validate_storage(&storage)
                    .unwrap_err()
                    .to_string()
                    .contains("requires ac:")
            );
        }
    }

    #[test]
    fn reports_original_body_coordinates() {
        let error = validate_storage("<p>&nbsp; é</p>\n<p></wrong>").unwrap_err();
        let error = error.downcast_ref::<crate::output::CliError>().unwrap();
        assert_eq!(
            error.details.as_ref().unwrap(),
            &json!({"line": 2, "column": 4, "reason": "expected 'p' tag, not 'wrong'"})
        );
        let error = validate_storage("<p><![CDATA[broken").unwrap_err();
        let error = error.downcast_ref::<crate::output::CliError>().unwrap();
        assert_eq!(error.details.as_ref().unwrap()["line"], 1);
    }

    #[test]
    fn entities_cannot_hide_invalid_xml_or_macro_types() {
        for body in [
            "<p&fake;>text</p>",
            "<p a&fake;='value'/>",
            "<!-- \0 &nbsp; -->",
            "<![CDATA[\0 &nbsp;]]>",
            r#"<ac:structured-macro ac:name="cod&#101;"><ac:rich-text-body>text</ac:rich-text-body></ac:structured-macro>"#,
        ] {
            assert!(validate_storage(body).is_err(), "accepted {body:?}");
        }
        validate_storage(
            "<p>&lt;literal&gt; &amp; &quot;quoted&quot; &apos;text&apos; &nbsp; &custom;</p>",
        )
        .unwrap();
        validate_storage("<!-- &custom; --><![CDATA[&custom;]]>").unwrap();
    }

    #[test]
    fn spellings_confluence_treats_alike_are_the_same_document() {
        for (left, right) in [
            (
                r#"<a href="x" title="t">a</a>"#,
                r#"<a title="t" href="x">a</a>"#,
            ),
            (
                "<ul>\n  <li>a</li>\n</ul>\n<p>b</p>",
                "<ul><li>a</li></ul><p>b</p>",
            ),
            ("<p>a\n   b</p>", "<p>a b</p>"),
            (
                "<p><b>a</b> <i>b</i></p>",
                "<p><strong>a</strong> <em>b</em></p>",
            ),
            ("<ul><li><p>a</p></li></ul>", "<ul><li>a</li></ul>"),
            (
                r#"<ul><li><p local-id="1">a</p></li></ul>"#,
                "<ul><li>a</li></ul>",
            ),
            (r#"<ol start="1"><li>a</li></ol>"#, "<ol><li>a</li></ol>"),
            ("<pre><code>a\n</code></pre>", "<pre><code>a</code></pre>"),
            (r#"<p local-id="abc">a</p>"#, "<p>a</p>"),
            (
                "<p>a&nbsp;b &eacute; &rarr;</p>",
                "<p>a\u{a0}b \u{e9} \u{2192}</p>",
            ),
            (
                r#"<ac:structured-macro ac:name="toc" ac:schema-version="1" ac:macro-id="7f" ac:local-id="9" />"#,
                r#"<ac:macro ac:name="toc" />"#,
            ),
            ("<p>a<br />b</p>", "<p>a<br></br>b</p>"),
            ("<p>\na\n</p>", "<p>a</p>"),
            ("<p>x<br />\ny</p>", "<p>x<br />y</p>"),
            ("<p>one\n<em>two</em></p>", "<p>one <em>two</em></p>"),
            (
                "<p><em>one</em>\n<em>two</em></p>",
                "<p><em>one</em> <em>two</em></p>",
            ),
            ("<p>a</p>\n<!-- note -->", "<p>a</p><!-- note -->"),
            ("<!-- a\r\nb\rc -->", "<!-- a\nb\nc -->"),
            ("<pre>a\r\nb\rc</pre>", "<pre>a\nb\nc</pre>"),
            (
                "<ac:structured-macro ac:name=\"code\"><ac:plain-text-body><![CDATA[a\r\nb]]></ac:plain-text-body></ac:structured-macro>",
                "<ac:structured-macro ac:name=\"code\"><ac:plain-text-body><![CDATA[a\nb]]></ac:plain-text-body></ac:structured-macro>",
            ),
            (
                "<p>a</p>\n<ac:structured-macro ac:name=\"toc\" />\n<p>b</p>",
                r#"<p>a</p><ac:structured-macro ac:name="toc" /><p>b</p>"#,
            ),
            (
                r#"<ac:structured-macro ac:name="info"><ac:rich-text-body>
<p>a</p>
</ac:rich-text-body></ac:structured-macro>"#,
                r#"<ac:structured-macro ac:name="info"><ac:rich-text-body><p>a</p></ac:rich-text-body></ac:structured-macro>"#,
            ),
            ("<p>one</p> <p>two</p>", "<p>one</p><p>two</p>"),
            ("<p>one</p> <p>two</p> ", "<p>one</p>\n<p>two</p>"),
            (
                "<table><tbody><tr><td> </td></tr></tbody></table>",
                "<table><tbody><tr><td></td></tr></tbody></table>",
            ),
        ] {
            assert!(same_document(left, right), "{left} and {right}");
            assert!(same_document(right, left), "{right} and {left}");
        }
    }

    #[test]
    fn storage_that_reads_differently_is_another_document() {
        for (left, right) in [
            ("<p>a</p>", "<p>b</p>"),
            ("<p>a</p><!-- note -->", "<p>a</p>"),
            ("<p>a<!-- note --></p>", "<p>a<!-- other --></p>"),
            ("<p>a</p>", "<div><p>a</p></div>"),
            ("<p>a b</p>", "<p>ab</p>"),
            ("<p>a\n<em>b</em></p>", "<p>a<em>b</em></p>"),
            (
                "<p><em>a</em>\n<em>b</em></p>",
                "<p><em>a</em><em>b</em></p>",
            ),
            ("<p><em>a</em>\nb</p>", "<p><em>a</em>b</p>"),
            (
                "<p>a\n<ac:structured-macro ac:name=\"status\" /></p>",
                r#"<p>a<ac:structured-macro ac:name="status" /></p>"#,
            ),
            (
                r#"<ac:structured-macro ac:name="info"><ac:parameter ac:name="title">a b</ac:parameter></ac:structured-macro>"#,
                r#"<ac:structured-macro ac:name="info"><ac:parameter ac:name="title">a  b</ac:parameter></ac:structured-macro>"#,
            ),
            ("<p>a <em>b</em></p>", "<p>a<em>b</em></p>"),
            ("<p>a<em> </em>b</p>", "<p>a<em></em>b</p>"),
            ("<p>a<em>\nb</em></p>", "<p>a<em>b</em></p>"),
            (
                "<p><em>a</em> <em>b</em></p>",
                "<p><em>a</em><em>b</em></p>",
            ),
            ("<p>a\n<font>b</font></p>", "<p>a<font>b</font></p>"),
            ("<p>a\n<strike>b</strike></p>", "<p>a<strike>b</strike></p>"),
            ("<p>a\n<mark>b</mark></p>", "<p>a<mark>b</mark></p>"),
            (
                r#"<p>a
<img src="x" /></p>"#,
                r#"<p>a<img src="x" /></p>"#,
            ),
            (
                "<table><tbody><tr><td>a\n<ac:structured-macro ac:name=\"status\" /></td></tr></tbody></table>",
                r#"<table><tbody><tr><td>a<ac:structured-macro ac:name="status" /></td></tr></tbody></table>"#,
            ),
            (
                "<ul><li><em>a</em>\n<ac:structured-macro ac:name=\"status\" /></li></ul>",
                r#"<ul><li><em>a</em><ac:structured-macro ac:name="status" /></li></ul>"#,
            ),
            ("<p><em>a </em>b</p>", "<p><em>a</em>b</p>"),
            ("<p><span>a</span></p>", "<p>a</p>"),
            (r#"<p class="x">a</p>"#, "<p>a</p>"),
            (r#"<p class="x">a</p>"#, r#"<p class="y">a</p>"#),
            (r#"<ol start="2"><li>a</li></ol>"#, "<ol><li>a</li></ol>"),
            ("<ul><li><p>a</p><p>b</p></li></ul>", "<ul><li>ab</li></ul>"),
            (
                r#"<ul><li><p class="x">a</p></li></ul>"#,
                "<ul><li>a</li></ul>",
            ),
            (
                "<pre><code>a b</code></pre>",
                "<pre><code>a  b</code></pre>",
            ),
            ("<pre><code>a\n\n</code></pre>", "<pre><code>a</code></pre>"),
            ("<pre>a\n</pre>", "<pre>a</pre>"),
            (
                "<table><thead><tr><th>a</th></tr></thead></table>",
                "<table><tbody><tr><th>a</th></tr></tbody></table>",
            ),
            (
                r#"<ac:structured-macro ac:name="toc" />"#,
                r#"<ac:structured-macro ac:name="children" />"#,
            ),
            (
                "<ac:plain-text-body><![CDATA[a  b]]></ac:plain-text-body>",
                "<ac:plain-text-body><![CDATA[a b]]></ac:plain-text-body>",
            ),
            ("<p>&unknownentity;</p>", "<p>&otherentity;</p>"),
            ("<p>&unknownentity;</p>", "<p>unknownentity</p>"),
            ("<p>&LT;</p>", "<p>x</p>"),
            ("<p>a</p>", "<p>a"),
        ] {
            assert!(!same_document(left, right), "{left} and {right}");
        }
    }

    #[test]
    fn storage_that_is_not_well_formed_is_the_same_only_as_itself() {
        assert!(same_document("<p>a<br>b</p>", "<p>a<br>b</p>"));
        assert!(!same_document("<p>a<br>b</p>", "<p>a<br> b</p>"));
        assert!(same_document(
            "<p>&unknownentity;</p>",
            "<p>&unknownentity;</p>"
        ));
    }

    #[test]
    fn rejects_duplicate_bodies_and_markup_in_plain_text() {
        for body in [
            r#"<ac:structured-macro ac:name="custom"><ac:rich-text-body/><ac:plain-text-body/></ac:structured-macro>"#,
            r#"<ac:structured-macro ac:name="code"><ac:plain-text-body><pre>code</pre></ac:plain-text-body></ac:structured-macro>"#,
        ] {
            assert!(validate_storage(body).is_err(), "accepted {body}");
        }
        validate_storage(r#"<ac:structured-macro ac:name="code"><ac:plain-text-body>&lt;pre&gt;code&lt;/pre&gt;</ac:plain-text-body></ac:structured-macro>"#).unwrap();
        validate_storage(r#"<ac:structured-macro ac:name="code"/>"#).unwrap();
    }

    #[test]
    fn rejects_user_dtds_and_external_entities_without_resolving_them() {
        for body in [
            "<!DOCTYPE p SYSTEM 'file:///unreadable'><p/>",
            "<!DOCTYPE p [<!ENTITY secret SYSTEM 'https://example.invalid/secret'>]><p>&secret;</p>",
        ] {
            assert!(validate_storage(body).is_err(), "accepted {body}");
        }
    }

    #[test]
    fn explicit_namespaces_and_alternate_prefixes_receive_macro_checks() {
        let invalid = r#"<c:structured-macro xmlns:c="http://atlassian.com/content" c:name="code"><c:rich-text-body/></c:structured-macro>"#;
        assert!(
            validate_storage(invalid)
                .unwrap_err()
                .to_string()
                .contains("requires ac:plain-text-body")
        );
        let valid = r#"<c:structured-macro xmlns:c="http://atlassian.com/content" c:name="code"><c:plain-text-body><![CDATA[<literal/>]]></c:plain-text-body></c:structured-macro>"#;
        validate_storage(valid).unwrap();
        // An unrelated extension vocabulary must not inherit Confluence rules.
        validate_storage(r#"<c:structured-macro xmlns:c="urn:custom" c:name="code"><c:rich-text-body/></c:structured-macro>"#).unwrap();
    }

    #[test]
    fn locations_survive_entities_unicode_and_crlf() {
        for (body, line, column) in [
            ("<p>&nbsp; é</wrong>", 1, 12),
            ("<p>first</p>\r\n<p>&nbsp; é</wrong>", 2, 12),
            (
                "<p>first</p>\n<ac:structured-macro ac:name='code'><ac:rich-text-body/></ac:structured-macro>",
                2,
                37,
            ),
        ] {
            let error = validate_storage(body).unwrap_err();
            let error = error.downcast_ref::<crate::output::CliError>().unwrap();
            let details = error.details.as_ref().unwrap();
            assert_eq!(
                (&details["line"], &details["column"]),
                (&json!(line), &json!(column)),
                "{body}"
            );
        }
        let error = validate_storage("<p>unclosed").unwrap_err();
        assert!(error.to_string().contains("element 'p' is not closed"));
        assert!(!error.to_string().contains("storage-root"));
    }
}
