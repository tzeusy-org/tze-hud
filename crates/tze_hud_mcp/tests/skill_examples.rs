//! Keep the two agent-facing manuals on the same argument parser as MCP.
use serde_json::Value;
use std::path::{Path, PathBuf};
use tze_hud_mcp::tools::{ClearParams, HoldParams, InputParams, PublishParams, parse_args};

fn arguments(tool: &str, value: Value) -> Result<(), String> {
    match tool {
        // Discovery has no verb-specific argument type; MCP arguments are objects.
        "hud_surfaces" => parse_args::<serde_json::Map<String, Value>>(value).map(|_| ()),
        "hud_publish" => parse_args::<PublishParams>(value).map(|_| ()),
        "hud_hold" => parse_args::<HoldParams>(value).map(|_| ()),
        "hud_clear" => parse_args::<ClearParams>(value).map(|_| ()),
        "hud_input" => parse_args::<InputParams>(value).map(|_| ()),
        _ => return Err(format!("unknown tool annotation: {tool}")),
    }
    .map_err(|error| error.to_string())
}

fn check_document(path: &Path, text: &str) -> Vec<String> {
    let lines: Vec<_> = text.lines().collect();
    let mut errors = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if lines[index].trim() != "```json" {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        while index < lines.len() && lines[index].trim() != "```" {
            index += 1;
        }
        let location = format!("{}:{}", path.display(), start + 1);
        if index == lines.len() {
            errors.push(format!("{location}: unclosed JSON fence"));
            break;
        }
        let annotation = start.checked_sub(1).map(|i| lines[i].trim()).unwrap_or("");
        let result = if let Some(tool) = annotation
            .strip_prefix("<!-- tool: ")
            .and_then(|s| s.strip_suffix(" -->"))
        {
            serde_json::from_str(&lines[start + 1..index].join("\n"))
                .map_err(|error| error.to_string())
                .and_then(|value| arguments(tool, value))
        } else if matches!(
            annotation,
            "<!-- example: response -->" | "<!-- example: error -->" | "<!-- example: partial -->"
        ) {
            Ok(())
        } else {
            Err(
                "JSON fence needs a tool or response/error/partial annotation immediately above"
                    .into(),
            )
        };
        if let Err(error) = result {
            errors.push(format!("{location}: {error}"));
        }
        index += 1;
    }
    errors
}

fn markdown_files(directory: &Path, paths: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).expect("read skill references") {
        let path = entry.expect("read skill entry").path();
        if path.is_dir() {
            markdown_files(&path, paths);
        } else if path.extension().is_some_and(|ext| ext == "md") {
            paths.push(path);
        }
    }
}

#[test]
fn skill_json_examples_use_production_arguments() {
    // Failure cases execute the same scanner and production deserializer as the
    // repository pass. Exempt blocks are examples, not tool argument documents.
    for (annotation, body, valid) in [
        (
            "<!-- tool: hud_publish -->",
            r#"{"surface":"zone:notification-area","content":"hello"}"#,
            true,
        ),
        (
            "<!-- tool: hud_publish -->",
            r#"{"surface":"zone:notification-area","typo":1}"#,
            false,
        ),
        ("<!-- tool: hud_publish -->", r#"{"ttl_ms":"soon"}"#, false),
        (
            "<!-- tool: hud_hold -->",
            r#"{"surface":"portal:session","ttl_ms":1000}"#,
            true,
        ),
        (
            "<!-- tool: hud_clear -->",
            r#"{"surface":"portal:session"}"#,
            true,
        ),
        (
            "<!-- tool: hud_input -->",
            r#"{"ack":["one"],"wait_ms":10}"#,
            true,
        ),
        ("<!-- tool: hud_input -->", r#"{"ack":"one"}"#, false),
        ("<!-- tool: hud_surfaces -->", "{}", true),
        ("<!-- tool: invented -->", "{}", false),
        ("<!-- tool: hud_publish -->", "{broken", false),
        ("", "{}", false),
        ("<!-- tool: hud_input -->\n", "{}", false),
        ("<!-- example: response -->", r#"{"ok":true}"#, true),
        (
            "<!-- example: error -->",
            r#"{"code":"INVALID_ARGUMENT"}"#,
            true,
        ),
        ("<!-- example: partial -->", "...", true),
        ("<!-- example: arbitrary -->", "{}", false),
    ] {
        let document = format!("# Example\n{annotation}\n```json\n{body}\n```\n");
        let errors = check_document(Path::new("fixture.md"), &document);
        assert_eq!(errors.is_empty(), valid, "{annotation}: {errors:?}");
        assert!(errors.iter().all(|error| error.starts_with("fixture.md:")));
    }
    let mut fence_failures = Vec::new();
    for fence in ["````", "~~~~"] {
        for (annotation, body, valid) in [
            ("", r#"{"surface":"zone:x","typo":1}"#, false),
            (
                "<!-- tool: hud_publish -->",
                r#"{"surface":"zone:x","typo":1}"#,
                false,
            ),
            (
                "<!-- tool: hud_publish -->",
                r#"{"surface":"zone:x","content":"hello"}"#,
                true,
            ),
        ] {
            let document = format!("{annotation}\n{fence}json\n{body}\n{fence}\n");
            let errors = check_document(Path::new("fixture.md"), &document);
            if errors.is_empty() != valid {
                fence_failures.push(format!("{fence} / {annotation}: {errors:?}"));
            }
        }
    }
    // A literal JSON demonstration inside an outer code block is not an example.
    for document in [
        "````markdown\n```json\n{literal\n```\n````\n",
        "~~~~text\n```json\n{literal\n```\n~~~~\n",
    ] {
        let errors = check_document(Path::new("fixture.md"), document);
        if !errors.is_empty() {
            fence_failures.push(format!("outer fence: {errors:?}"));
        }
    }
    assert!(fence_failures.is_empty(), "{fence_failures:#?}");
    assert_eq!(
        check_document(Path::new("fixture.md"), "```json\n{}")[0],
        "fixture.md:1: unclosed JSON fence"
    );

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut paths = Vec::new();
    for skill in ["th-hud-publish", "hud-projection"] {
        let directory = root.join(".claude/skills").join(skill);
        paths.push(directory.join("SKILL.md"));
        markdown_files(&directory.join("references"), &mut paths);
    }
    paths.sort();
    let errors: Vec<_> = paths
        .iter()
        .flat_map(|path| {
            let text = std::fs::read_to_string(path).expect("read skill Markdown");
            check_document(path.strip_prefix(&root).unwrap(), &text)
        })
        .collect();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}
