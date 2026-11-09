//! Keep the two agent-facing manuals on the same argument parser as MCP.
use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};
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

struct Example {
    line: usize,
    annotation: String,
    body: String,
}

fn check_example(example: &Example) -> Result<(), String> {
    if let Some(tool) = example
        .annotation
        .strip_prefix("<!-- tool: ")
        .and_then(|s| s.strip_suffix(" -->"))
    {
        serde_json::from_str(&example.body)
            .map_err(|error| error.to_string())
            .and_then(|value| arguments(tool, value))
    } else if matches!(
        example.annotation.as_str(),
        "<!-- example: response -->" | "<!-- example: error -->" | "<!-- example: partial -->"
    ) {
        Ok(())
    } else {
        Err("JSON fence needs a tool or response/error/partial annotation immediately above".into())
    }
}

fn check_document(path: &Path, text: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let mut annotation = None;
    let mut example: Option<Example> = None;
    // CommonMark owns fence lengths, tilde fences, container prefixes, and
    // literal nested fences. Only the extracted arguments use our MCP parser.
    for (event, range) in Parser::new(text).into_offset_iter() {
        match event {
            Event::Html(html) => {
                let line = text[..range.start].bytes().filter(|&b| b == b'\n').count() + 1;
                annotation = Some((line, html.trim().to_owned()));
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info)))
                if info
                    .split_whitespace()
                    .next()
                    .is_some_and(|language| language.eq_ignore_ascii_case("json")) =>
            {
                let line = text[..range.start].bytes().filter(|&b| b == b'\n').count() + 1;
                let annotation = annotation
                    .as_ref()
                    .filter(|(annotation_line, _)| *annotation_line + 1 == line)
                    .map(|(_, annotation)| annotation.clone())
                    .unwrap_or_default();
                example = Some(Example {
                    line,
                    annotation,
                    body: String::new(),
                });
            }
            Event::Text(body) => {
                if let Some(example) = &mut example {
                    example.body.push_str(&body);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(example) = example.take()
                    && let Err(error) = check_example(&example)
                {
                    errors.push(format!("{}:{}: {error}", path.display(), example.line));
                }
            }
            _ => {}
        }
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
    for document in [
        "<!-- tool: hud_input -->\n```json\n{}\n````\n",
        "<!-- tool: hud_input -->\n~~~JSON extra-info\n{}\n~~~~\n",
        "> <!-- tool: hud_input -->\n> ```json\n> {}\n> ```\n",
        "- <!-- tool: hud_input -->\n  ```json\n  {}\n  ```\n",
        // CommonMark closes a fenced block at EOF, too.
        "<!-- tool: hud_input -->\n```json\n{}",
    ] {
        let errors = check_document(Path::new("fixture.md"), document);
        assert!(errors.is_empty(), "{document}: {errors:?}");
    }
    for document in [
        "```json\n{}",
        // The wrong delimiter and a shorter fence are literal code, not closers.
        "<!-- tool: hud_input -->\n````json\n{}\n```\n````\n",
        "<!-- tool: hud_input -->\n~~~json\n{}\n```\n~~~\n",
    ] {
        let errors = check_document(Path::new("fixture.md"), document);
        assert!(!errors.is_empty(), "{document}");
        assert!(errors.iter().all(|error| error.starts_with("fixture.md:")));
    }

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
