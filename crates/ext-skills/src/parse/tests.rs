use super::*;

#[test]
fn real_headers_preserve_the_reference_index() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        let yaml = source
            .strip_prefix("---\n")
            .unwrap()
            .rsplit_once("---")
            .unwrap()
            .0;
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(path.parent().unwrap().join("expected.json")).unwrap(),
        )
        .unwrap();
        let expected = &expected[path.file_name().unwrap().to_str().unwrap()];
        let actual = header(yaml).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(actual.name, expected["name"], "{}", path.display());
        assert_eq!(
            actual
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            expected["description"],
            "{}",
            path.display()
        );
        match path.file_name().unwrap().to_str().unwrap() {
            "anthropics-claude-api.md" => assert_eq!(
                parse(&source, Path::new("skills")).err().unwrap().kind,
                Kind::Description
            ),
            "claude-code-agent-development.md" => assert_eq!(
                parse(&source, Path::new("skills")).err().unwrap().kind,
                Kind::Name
            ),
            _ => {
                parse(&source, Path::new("skills")).unwrap();
                assert_eq!(identity(source.as_bytes()).unwrap(), actual.name);
            }
        }
    }
}

#[test]
fn metadata_is_validated_even_when_not_interpreted() {
    for metadata in [
        "allowed-tools: [Read, 'Bash(git:*)', \"Write\"] # tools",
        "allowed-tools:\n  - Read\n  - Bash(git:*)\nmetadata:\n  owner: team\n  nested:\n    version: 1",
        "metadata: {owner: team, nested: {version: 1}}",
        "metadata: {x}",
        "allowed-tools: [Anthropic's, Read] # comment",
    ] {
        header(&format!(
            "name: review\ndescription: 'Review ''quoted'' code' # comment\n{metadata}\n"
        ))
        .unwrap();
    }
    for metadata in [
        "metadata: [Read",
        "metadata:\n x: [a,,b]",
        "metadata: *alias",
        "metadata:\n x: ok\n x: duplicate",
        "metadata:\n  x: ok\n y: invalid",
    ] {
        assert!(
            header(&format!("name: review\ndescription: code\n{metadata}\n")).is_err(),
            "{metadata}"
        );
    }
}

#[test]
fn errors_are_bounded_and_locate_the_offending_line() {
    let error = header("name: review\ndescription: ok\nname: duplicate")
        .err()
        .unwrap();
    assert_eq!(error.line, 4);
    assert_eq!(error.kind, Kind::Duplicate);
    assert_eq!(
        header("name: review\ndescription: |0").err().unwrap().line,
        3
    );
    assert_eq!(
        header("name: review\ndescription: [x] extra")
            .err()
            .unwrap()
            .line,
        3
    );
    assert_eq!(header(&" ".repeat(8193)).err().unwrap().kind, Kind::Size);
    let deep = format!(
        "name: review\ndescription: ok\nx: {}0{}",
        "[".repeat(18),
        "]".repeat(18)
    );
    assert_eq!(header(&deep).err().unwrap().kind, Kind::Depth);
}

#[test]
fn skill_frontmatter() {
    bolero::check!().for_each(|bytes: &[u8]| {
        if let Ok(text) = std::str::from_utf8(bytes) {
            let _ = parse(text, Path::new("skills"));
            let _ = header(text);
        }
    });
}

#[test]
fn scalar_boundaries_do_not_change_comments_or_delimiters() {
    for description in [
        "Review Anthropic's code # ignored",
        "\"Review Anthropic's code\" # ignored",
        "'Review Anthropic''s code' # ignored",
        "|-\n  Review Anthropic's code",
        ">2-\n  Review Anthropic's code",
    ] {
        let source = format!("---\nname: review\ndescription: {description}\n---\n---\nbody");
        let (_, skill) = parse(&source, Path::new("skills")).unwrap();
        assert_eq!(skill.description, "Review Anthropic's code");
        assert_eq!(skill.body, "---\nbody");
    }
    for invalid in [
        "'unclosed",
        "\"bad\\q\"",
        "null",
        "~",
        "[text]",
        "*alias",
        "!tag text",
        "plain: ambiguous",
        "|0\n  text",
        "ok\n\tbad: indent",
        "|\n  bad\0text",
    ] {
        assert!(
            header(&format!("name: review\ndescription: {invalid}\n")).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn maintained_yaml_accepts_portable_text_and_rejects_expansion() {
    let parsed =
        header("'name': review\ndescription: \"YAML\\Ntext\"\nmetadata: {x: [a, {y: z}]}\n")
            .unwrap();
    assert_eq!(parsed.name, "review");
    assert_eq!(parsed.description, "YAML\u{85}text");
    for extra in [
        "metadata: {x: ok, x: duplicate}",
        "metadata: &anchor [a]",
        "metadata: !tag value",
        "metadata: { ? [a, b]: value }",
        "...\n---\nname: other\ndescription: other",
    ] {
        assert!(
            header(&format!("name: review\ndescription: ok\n{extra}\n")).is_err(),
            "{extra}"
        );
    }
}

#[test]
fn identity_discovery_keeps_yaml_names_independent_of_other_metadata() {
    for metadata in [
        "'name': review\ndescription: ok",
        "\"na\\u006de\": review\ndescription: ok",
        "description: ok\nname: \"re\\\n  view\"",
        "{description: ok, name: review}",
        "name: >-\n  review\ndescription: ok",
    ] {
        let text = format!("---\n{metadata}\n---\nbody");
        assert_eq!(identity(text.as_bytes()).unwrap(), "review", "{text}");
        assert_eq!(parse(&text, Path::new(".")).unwrap().0, "review");
    }
    for metadata in [
        "description: [unfinished\n'name': review",
        "  description: [unfinished\n  \"name\": review",
        "{name: review, description: [unfinished}",
    ] {
        let text = format!("---\n{metadata}\n---\nbody");
        assert_eq!(identity(text.as_bytes()).unwrap(), "review", "{text}");
        assert!(parse(&text, Path::new(".")).is_err());
    }
    assert_eq!(
        identity(b"---\ndescription: \xff\n\"name\": review\n---\n").unwrap(),
        "review"
    );
    let error =
        identity(b"---\n# comment\ndescription: [unfinished\nname: INVALID\n---\n").unwrap_err();
    assert_eq!(error.line, 4);
}
