//! Reads every document of `tests/corpus/reading` with `confluence convert`
//! and compares the storage with what release 0.1.41 wrote for it.
//!
//! Beside `NAME.md` stands `NAME.0.1.41.xml`, the storage 0.1.41 wrote, or
//! `NAME.0.1.41.refused`, the message it refused the document with. A document
//! that reads differently on purpose is listed in `DIFFERENCES` with the
//! reason, and holds the storage it reads as in `NAME.xml`, or a part of the
//! message it is refused with in `NAME.refused`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The documents that read differently from 0.1.41, and why.
const DIFFERENCES: &[(&str, &str)] = &[
    (
        "checkbox-above-a-nested-list",
        "blocks below a checkbox that is alone on its line are the body of its task",
    ),
    (
        "checkbox-above-a-paragraph",
        "blocks below a checkbox that is alone on its line are the body of its task",
    ),
    (
        "checkbox-ending-the-text",
        "a checkbox is a task also where no line break follows it",
    ),
    (
        "checkboxes-in-a-numbered-list",
        "Confluence has no numbered task list, so the checkbox stays the text of its item",
    ),
    (
        "code-fence-line-after-a-panel-in-html",
        "the line break after the block is kept",
    ),
    (
        "code-in-html",
        "the line break before the closing tag is kept",
    ),
    (
        "code-opening-with-three-tildes",
        "a line of three tildes closes the block, which is how an empty code macro is written",
    ),
    (
        "code-with-indented-closing-line",
        "a block that no line closes is refused instead of published as code",
    ),
    (
        "empty-tasks-around-a-plain-item",
        "a plain item between tasks is kept",
    ),
    (
        "excerpt-indented-by-three-spaces",
        "the line of dashes that ends the parameter lines may be indented as the block is",
    ),
    (
        "excerpt-with-a-rule-in-a-comment",
        "a comment is text as written, and storage holds no comment with two dashes in it",
    ),
    (
        "expand-indented-by-three-spaces",
        "a macro line may be indented by up to three spaces, whatever the macro",
    ),
    (
        "front-matter-value-opening-a-storage-fence",
        "front matter holds its lines, whatever a value reads as",
    ),
    (
        "layout-holding-a-storage-fence-with-a-cell-line",
        "an inline storage fence holds its lines as written",
    ),
    ("macro-holding-only-a-rule", "parameter lines may be empty"),
    (
        "macro-indented-by-four-spaces",
        "four spaces of indentation make a code block",
    ),
    (
        "macro-lines-in-a-list-item",
        "lines of a list item are text of that item",
    ),
    (
        "macro-lines-indented-in-html",
        "a macro line may be indented by up to three spaces, also in raw markup",
    ),
    (
        "macro-with-a-rule-indented-by-three-spaces",
        "the line of dashes that ends the parameter lines may be indented as the block is",
    ),
    (
        "macros-with-parameters",
        "code macros that follow each other are blocks, not the text of a paragraph",
    ),
    (
        "nested-tasks-and-plain-items",
        "tasks are read from the list structure, at every depth",
    ),
    (
        "panel-holding-an-open-code-fence",
        "a code fence that no line closes holds the line that would close the block",
    ),
    (
        "panel-holding-a-storage-fence-with-a-closing-line",
        "an inline storage fence holds its lines as written",
    ),
    ("panel-in-a-comment", "a comment is text as written"),
    (
        "panel-in-a-comment-in-html",
        "the line break after the block is kept",
    ),
    (
        "panel-in-a-panel",
        "a block inside a block closes with its own line",
    ),
    (
        "panel-in-html",
        "the line break before the closing tag is kept",
    ),
    (
        "panel-indented-by-two-spaces",
        "a macro line may be indented by up to three spaces, whatever the macro",
    ),
    (
        "panel-without-closing-line",
        "a block that no line closes is refused instead of published as text",
    ),
    (
        "panels-and-expand",
        "macros that follow each other are blocks, not the text of a paragraph",
    ),
    (
        "storage-fence-shown-in-code",
        "a storage fence shown in a code block is code",
    ),
    (
        "text-after-a-panel-in-html",
        "the line break after the block is kept",
    ),
];

#[derive(Debug, PartialEq)]
enum Reading {
    Storage(String),
    Refused(String),
}

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/reading")
}

fn convert(document: &Path) -> Reading {
    let home = tempfile::tempdir().expect("temporary directory");
    let storage = home.path().join("storage.xml");
    let output = Command::new(env!("CARGO_BIN_EXE_confluence"))
        .args(["convert", "--output", "json", "--body-file"])
        .arg(document)
        .arg("--output-file")
        .arg(&storage)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env_remove("CONFLUENCE_PROFILE")
        .env_remove("CONFLUENCE_DOMAIN")
        .env_remove("CONFLUENCE_API_TOKEN")
        .env_remove("CONFLUENCE_TOKEN")
        .output()
        .expect("confluence command to run");
    if output.status.success() {
        Reading::Storage(fs::read_to_string(&storage).expect("converted storage"))
    } else {
        Reading::Refused(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

/// What the files beside a document record under `suffix`: its storage, or
/// the message of a refusal. Exactly one of the two is recorded, or neither.
fn recorded(name: &str, suffix: &str) -> Option<Reading> {
    let read = |extension: &str| {
        fs::read_to_string(corpus().join(format!("{name}{suffix}.{extension}"))).ok()
    };
    match (read("xml"), read("refused")) {
        (Some(storage), None) => Some(Reading::Storage(storage)),
        (None, Some(message)) => Some(Reading::Refused(message.trim().to_string())),
        (None, None) => None,
        (Some(_), Some(_)) => panic!("{name}{suffix} is recorded as both storage and a refusal"),
    }
}

#[test]
fn documents_read_as_in_0_1_41_except_where_listed() {
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(corpus()).expect("corpus directory") {
        let path = entry.expect("corpus entry").path();
        if path.extension().is_some_and(|extension| extension == "md") {
            let name = path.file_stem().and_then(|stem| stem.to_str());
            names.insert(name.expect("document name").to_string());
        }
    }
    assert!(
        names.len() >= 65,
        "the corpus holds {} documents",
        names.len()
    );
    for (name, _) in DIFFERENCES {
        assert!(
            names.contains(*name),
            "{name} is listed and not in the corpus"
        );
    }

    for name in &names {
        let released = recorded(name, ".0.1.41")
            .unwrap_or_else(|| panic!("{name} has no record of what 0.1.41 read"));
        let read = convert(&corpus().join(format!("{name}.md")));
        let expected = recorded(name, "");
        let reason = DIFFERENCES
            .iter()
            .find(|(listed, _)| listed == name)
            .map(|(_, reason)| *reason);
        match (reason, expected) {
            (None, None) => match (&read, &released) {
                (Reading::Refused(_), Reading::Refused(_)) => {}
                _ => assert_eq!(read, released, "{name} reads differently from 0.1.41"),
            },
            (Some(reason), Some(expected)) => {
                match (&read, &expected) {
                    (Reading::Refused(message), Reading::Refused(part)) => assert!(
                        message.contains(part.as_str()),
                        "{name} ({reason}) is refused with: {message}"
                    ),
                    _ => assert_eq!(read, expected, "{name} ({reason})"),
                }
                let differs = match (&expected, &released) {
                    (Reading::Refused(_), Reading::Refused(_)) => false,
                    _ => expected != released,
                };
                assert!(differs, "{name} is listed and reads as in 0.1.41");
            }
            (None, Some(_)) => panic!("{name} records a difference that is not listed"),
            (Some(_), None) => panic!("{name} is listed without a record of what it reads as"),
        }
    }
}
