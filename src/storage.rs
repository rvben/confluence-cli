//! Local structural checks for Confluence storage fragments, not HTML documents.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;
use roxmltree::{Document, ParsingOptions, TextPos};
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
        declarations.push_str(&format!("<!ENTITY {name} ' '>"));
    }
    // Bodies are fragments with multiple top-level nodes and implicit ac/ri
    // namespaces. A newline keeps original columns intact on the first line.
    let wrapped = format!(
        "<!DOCTYPE storage-root [{declarations}]><storage-root xmlns:ac=\"{AC_NS}\" xmlns:ri=\"{RI_NS}\">\n{storage}\n</storage-root>"
    );
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

/// Coordinates here refer to converted storage, not to the Markdown source.
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
        json!({"line": position.row, "column": position.col}),
    )
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
            &json!({"line": 2, "column": 4})
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
            assert_eq!(
                error.details.as_ref().unwrap(),
                &json!({"line": line, "column": column}),
                "{body}"
            );
        }
        let error = validate_storage("<p>unclosed").unwrap_err();
        assert!(error.to_string().contains("element 'p' is not closed"));
        assert!(!error.to_string().contains("storage-root"));
    }
}
