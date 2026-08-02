//! Tests for the layout module.

use super::{ConditionalClass, EventBlock, Primitive, parse_layout, parse_layout_with_urls};
use crate::core::errors::MizuError;
use crate::core::types::StringInterner;
use crate::parser::logic::parse_action;
use crate::parser::urls::UrlRegistry;

#[test]
fn deeply_nested_layout_is_rejected_before_reaching_the_dom() {
    // Regression: a document nested well beyond MAX_LAYOUT_DEPTH must be
    // rejected at parse time, before it can reach the recursive DOM
    // walkers (build_taffy_tree, taffy's layout, paint_node) and overflow
    // the UI thread's stack.
    let mut layout = String::from("doc\n");
    for depth in 0..(super::MAX_LAYOUT_DEPTH + 10) {
        layout.push_str(&" ".repeat((depth + 1) * 4));
        layout.push_str("box\n");
    }
    let mut interner = StringInterner::new();
    let result = parse_layout(&layout, &mut interner);
    assert!(
        matches!(result, Err(MizuError::ParseError(ref msg)) if msg.contains("nesting too deep")),
        "over-deep layout must be rejected with a nesting error, got: {result:?}"
    );
}

#[test]
fn layout_nesting_at_the_limit_is_accepted() {
    // The boundary itself (exactly MAX_LAYOUT_DEPTH levels, including the
    // root `doc`) must still parse — the cap must not be off-by-one.
    let mut layout = String::from("doc\n");
    for depth in 0..(super::MAX_LAYOUT_DEPTH - 1) {
        layout.push_str(&" ".repeat((depth + 1) * 4));
        layout.push_str("box\n");
    }
    let mut interner = StringInterner::new();
    let result = parse_layout(&layout, &mut interner);
    assert!(
        result.is_ok(),
        "layout at the depth limit must parse: {result:?}"
    );
}

#[test]
fn media_guard_rejects_undeclared_image_alias() {
    // Mirrors the navigation path: an `image src` alias that is not present
    // in the (here empty) `urls` registry must be rejected at parse time.
    let mut interner = StringInterner::new();
    let registry: UrlRegistry = rustc_hash::FxHashMap::default();
    let layout = "doc\n    image src \"undeclared_alias\"\n";
    let result = parse_layout_with_urls(
        layout,
        &mut interner,
        Some(&registry),
        false,
        &rustc_hash::FxHashMap::default(),
    );
    match result {
        Err(MizuError::ParseError(msg)) => {
            assert!(
                msg.contains("undeclared_alias") && msg.contains("not declared"),
                "error should name the undeclared media alias: {msg}"
            );
        }
        other => panic!("expected ParseError for undeclared media alias, got: {other:?}"),
    }
}

#[test]
fn media_guard_skipped_without_registry() {
    // Without a registry (`None`), the guard must not fire — `parse_layout`
    // keeps its lenient behaviour.
    let mut interner = StringInterner::new();
    let layout = "doc\n    image src \"anything\"\n";
    let result = parse_layout(layout, &mut interner);
    assert!(result.is_ok(), "no registry → no media guard: {result:?}");
}

#[test]
fn direct_path_src_skips_media_guard() {
    // A plain filename with an extension is a direct path, not an alias —
    // the registry guard must not fire even when a registry is present.
    let mut interner = StringInterner::new();
    let registry: UrlRegistry = rustc_hash::FxHashMap::default();
    let layout = "doc\n    image src \"test.png\"\n";
    let result = parse_layout_with_urls(
        layout,
        &mut interner,
        Some(&registry),
        false,
        &rustc_hash::FxHashMap::default(),
    );
    assert!(
        result.is_ok(),
        "direct filename with extension must bypass guard: {result:?}"
    );
}

#[test]
fn direct_path_with_slash_skips_guard() {
    // A path containing `/` is always a direct path — guard skipped.
    let mut interner = StringInterner::new();
    let registry: UrlRegistry = rustc_hash::FxHashMap::default();
    let layout = "doc\n    image src \"./img/logo.png\"\n";
    let result = parse_layout_with_urls(
        layout,
        &mut interner,
        Some(&registry),
        false,
        &rustc_hash::FxHashMap::default(),
    );
    assert!(
        result.is_ok(),
        "relative path with slash must bypass guard: {result:?}"
    );
}

#[test]
fn file_url_src_blocked_from_remote_origin() {
    // When is_remote_origin=true, file:// in `image src` must be rejected
    // at parse time with a SecurityViolation message.
    let mut interner = StringInterner::new();
    let registry: UrlRegistry = rustc_hash::FxHashMap::default();
    let layout = "doc\n    image src \"file:///etc/passwd\"\n";
    let result = parse_layout_with_urls(
        layout,
        &mut interner,
        Some(&registry),
        true,
        &rustc_hash::FxHashMap::default(),
    );
    match result {
        Err(MizuError::ParseError(msg)) => {
            assert!(
                msg.contains("SecurityViolation"),
                "error must mention SecurityViolation: {msg}"
            );
            assert!(
                msg.contains("file://"),
                "error must contain the offending URL prefix: {msg}"
            );
        }
        other => panic!("expected ParseError(SecurityViolation), got: {other:?}"),
    }
}

#[test]
fn file_url_src_allowed_from_local_origin() {
    // When is_remote_origin=false, file:// in `image src` must be treated as
    // a direct path and not trigger the registry validation error.
    let mut interner = StringInterner::new();
    let registry: UrlRegistry = rustc_hash::FxHashMap::default();
    let layout = "doc\n    image src \"file:///home/user/img.png\"\n";
    let result = parse_layout_with_urls(
        layout,
        &mut interner,
        Some(&registry),
        false,
        &rustc_hash::FxHashMap::default(),
    );
    assert!(
        result.is_ok(),
        "file:// must be allowed in local-origin documents: {result:?}"
    );
}

#[test]
fn mizu_url_src_is_rejected() {
    // An absolute mizu:// URL bypasses the urls registry and is now a hard
    // compile error (see test_absolute_url_src_is_rejected for the message).
    let mut interner = StringInterner::new();
    let registry: UrlRegistry = rustc_hash::FxHashMap::default();
    let layout = "doc\n    image src \"mizu://cdn.example.com/img.png\"\n";
    let result = parse_layout_with_urls(
        layout,
        &mut interner,
        Some(&registry),
        false,
        &rustc_hash::FxHashMap::default(),
    );
    assert!(
        matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("absolute URLs are not allowed in src")),
        "mizu:// URL in src must be rejected: {result:?}"
    );
}

#[test]
fn symbolic_alias_still_validated() {
    // A pure identifier (no `.` or `/`) is treated as a symbolic alias and
    // must still be rejected when absent from the registry.
    let mut interner = StringInterner::new();
    let registry: UrlRegistry = rustc_hash::FxHashMap::default();
    let layout = "doc\n    image src \"cdn_icons\"\n";
    let result = parse_layout_with_urls(
        layout,
        &mut interner,
        Some(&registry),
        false,
        &rustc_hash::FxHashMap::default(),
    );
    match result {
        Err(MizuError::ParseError(msg)) => {
            assert!(
                msg.contains("cdn_icons") && msg.contains("not declared"),
                "error must name the undeclared alias: {msg}"
            );
        }
        other => panic!("expected ParseError for undeclared symbolic alias, got: {other:?}"),
    }
}

#[test]
fn test_empty_layout_fails() {
    let result = parse_layout("   \n  \n", &mut StringInterner::new());
    assert!(matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("empty")));
}

#[test]
fn test_root_must_be_doc() {
    let result = parse_layout("    box\n", &mut StringInterner::new());
    assert!(
        matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("root element must be `doc`"))
    );
}

#[test]
fn old_window_root_keyword_is_a_clear_parse_error_naming_doc() {
    let result = parse_layout("    window\n", &mut StringInterner::new());
    assert!(
        matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("window") && m.contains("doc")),
        "expected a ParseError naming both `window` and `doc`, got: {result:?}"
    );
}

#[test]
fn test_multi_tiered_dom_tree() {
    let layout = r#"
doc title "Mizu App"
    box class container
        text "Welcome to Mizu"
        button "Submit"
            click -> Redirect("/home")
"#;
    let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
    let root = tree.root();
    assert_eq!(root.value().primitive, Primitive::Doc);
    assert_eq!(
        root.value().attributes.get("title").map(|s| s.as_str()),
        Some("Mizu App")
    );
    assert!(
        !root
            .children()
            .any(|n| n.value().primitive == Primitive::Text),
        "the explicit `title` attribute must not create a visible child Text node"
    );

    let mut children = root.children();
    let box_node = children.next().unwrap();
    assert_eq!(box_node.value().primitive, Primitive::Box);
    assert_eq!(
        box_node.value().attributes.get("class").map(|s| s.as_str()),
        Some("container")
    );

    let mut box_children = box_node.children();
    let text_node = box_children.next().unwrap();
    assert_eq!(text_node.value().primitive, Primitive::Text);
    // "Welcome to Mizu" is stored as "content" attribute on the Text node itself
    assert_eq!(
        text_node
            .value()
            .attributes
            .get("content")
            .map(|s| s.as_str()),
        Some("Welcome to Mizu")
    );

    let button_node = box_children.next().unwrap();
    assert_eq!(button_node.value().primitive, Primitive::Button);
    // "Submit" is a child Text node of the button
    let btn_text = button_node
        .children()
        .find(|n| n.value().primitive == Primitive::Text);
    assert_eq!(
        btn_text
            .and_then(|n| n.value().attributes.get("content"))
            .map(|s| s.as_str()),
        Some("Submit")
    );
    assert_eq!(
        button_node.value().events.get("click"),
        Some(&EventBlock::Click {
            action: parse_action("Redirect(\"/home\")", &mut StringInterner::new()).unwrap()
        })
    );
}

#[test]
fn doc_positional_inline_text_is_a_parse_error() {
    // The old `window "Title"` positional-text-sets-title sugar is
    // removed: `doc` no longer accepts positional inline text at all,
    // closing the "bare string after the tag means something different
    // per primitive" inconsistency. `title` must be an explicit
    // attribute.
    let layout = "\n    doc \"Hello, Mizu!\"\n";
    let result = parse_layout(layout, &mut StringInterner::new());
    assert!(
        matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("title")),
        "expected a ParseError pointing to the explicit `title` attribute, got: {result:?}"
    );
}

#[test]
fn doc_explicit_title_attribute_sets_title_and_no_visible_child() {
    let layout = "\n    doc title \"Explicit\"\n";
    let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
    let root = tree.root();
    assert_eq!(
        root.value().attributes.get("title").map(|s| s.as_str()),
        Some("Explicit")
    );
    assert_eq!(root.children().count(), 0);
}

#[test]
fn title_attribute_rejected_on_non_doc_primitive() {
    let layout = "\n    doc\n        box title \"not allowed here\"\n";
    let result = parse_layout(layout, &mut StringInterner::new());
    assert!(
        matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("title") && m.contains("doc")),
        "expected a ParseError naming `title` and `doc`, got: {result:?}"
    );
}

#[test]
fn test_attribute_extraction() {
    let layout = r#"
doc
    input type "text" placeholder "Enter Username" class input-field val 42
"#;
    let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
    let input_node = tree
        .root()
        .children()
        .find(|n| n.value().primitive == Primitive::Input)
        .unwrap();
    let attrs = &input_node.value().attributes;
    assert_eq!(attrs.get("type").map(|s| s.as_str()), Some("text"));
    assert_eq!(
        attrs.get("placeholder").map(|s| s.as_str()),
        Some("Enter Username")
    );
    assert_eq!(attrs.get("class").map(|s| s.as_str()), Some("input-field"));
    assert_eq!(attrs.get("val").map(|s| s.as_str()), Some("42"));
}

#[test]
fn input_type_file_parses_with_and_without_accept() {
    // `type "file"` needs no dedicated grammar support: `type` is
    // already a generic recognised attribute (as this test's sibling,
    // `test_attribute_extraction`, demonstrates for `type "text"`), and
    // `accept` is just another ordinary `key "value"` attribute pair.
    let layout = r#"
doc
    input type "file" name "avatar" accept ".png,.jpg"
    input type "file" name "resume"
"#;
    let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
    let mut inputs = tree
        .root()
        .children()
        .filter(|n| n.value().primitive == Primitive::Input);

    let with_accept = inputs.next().unwrap();
    let attrs = &with_accept.value().attributes;
    assert_eq!(attrs.get("type").map(|s| s.as_str()), Some("file"));
    assert_eq!(attrs.get("name").map(|s| s.as_str()), Some("avatar"));
    assert_eq!(attrs.get("accept").map(|s| s.as_str()), Some(".png,.jpg"));

    let without_accept = inputs.next().unwrap();
    let attrs = &without_accept.value().attributes;
    assert_eq!(attrs.get("type").map(|s| s.as_str()), Some("file"));
    assert_eq!(attrs.get("accept"), None);
}

#[test]
fn class_with_leading_dot_is_a_parse_error() {
    let layout = "\n    doc\n        box class .foo\n";
    let result = parse_layout(layout, &mut StringInterner::new());
    assert!(
        matches!(result, Err(MizuError::ParseError(ref m)) if m.contains(".foo") && m.contains("class foo")),
        "expected a ParseError pointing to the dot-free form, got: {result:?}"
    );
}

#[test]
fn class_without_leading_dot_still_parses() {
    let layout = "\n    doc\n        box class foo\n";
    let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
    let box_node = tree
        .root()
        .children()
        .find(|n| n.value().primitive == Primitive::Box)
        .unwrap();
    assert_eq!(
        box_node.value().attributes.get("class").map(|s| s.as_str()),
        Some("foo")
    );
}

#[test]
fn h1_through_h6_parse_as_heading_with_correct_level() {
    for (tag, expected_level) in [
        ("h1", "1"),
        ("h2", "2"),
        ("h3", "3"),
        ("h4", "4"),
        ("h5", "5"),
        ("h6", "6"),
    ] {
        let layout = format!("\n    doc\n        {tag} \"Section title\"\n");
        let tree = parse_layout(&layout, &mut StringInterner::new()).unwrap();
        let heading = tree
            .root()
            .children()
            .find(|n| n.value().primitive == Primitive::Heading)
            .unwrap_or_else(|| panic!("{tag} did not parse as Primitive::Heading"));
        assert_eq!(
            heading.value().attributes.get("level").map(|s| s.as_str()),
            Some(expected_level),
            "{tag} must set level={expected_level}"
        );
        let text_child = heading
            .children()
            .find(|n| n.value().primitive == Primitive::Text)
            .expect("inline text must become a child Text node, like every non-doc primitive");
        assert_eq!(
            text_child
                .value()
                .attributes
                .get("content")
                .map(|s| s.as_str()),
            Some("Section title")
        );
    }
}

#[test]
fn test_event_blocks() {
    let layout = r#"
doc
    button "Submit"
        click -> ActionPerform
    box
        submit -> FormSubmit
"#;
    // Use a shared interner so symbols match between parse_layout and parse_action.
    let mut interner = StringInterner::new();
    let tree = parse_layout(layout, &mut interner).unwrap();
    // Skip the Text child inserted for "App"
    let mut children = tree
        .root()
        .children()
        .filter(|n| n.value().primitive != Primitive::Text);

    let btn = children.next().unwrap();
    assert_eq!(
        btn.value().events.get("click"),
        Some(&EventBlock::Click {
            action: parse_action("ActionPerform", &mut interner).unwrap()
        })
    );

    let bx = children.next().unwrap();
    assert_eq!(
        bx.value().events.get("submit"),
        Some(&EventBlock::Submit {
            action: parse_action("FormSubmit", &mut interner).unwrap()
        })
    );
}

#[test]
fn test_bind_keyword_produces_error() {
    let layout = "doc\n    input\n        bind -> user.name\n";
    let result = parse_layout(layout, &mut StringInterner::new());
    assert!(
        matches!(result, Err(MizuError::ParseError(msg)) if msg.contains("bind is no longer supported"))
    );
}

#[test]
fn test_every_child_line_is_rejected_with_line_number() {
    // Node timers were removed: the child-line form must be a hard error
    // that carries the offending line number and points at root timers.
    let layout = r#"
doc
    box
        every 500ms -> count = count + 1
"#;
    let result = parse_layout(layout, &mut StringInterner::new());
    match result {
        Err(MizuError::ParseError(msg)) => {
            assert!(
                msg.contains("node timers are not allowed"),
                "error must state that node timers are not allowed, got: {msg}"
            );
            assert!(
                msg.contains("root timer in the logic block"),
                "error must point at root timers, got: {msg}"
            );
            assert!(
                msg.contains("line 4"),
                "error must carry the offending line number, got: {msg}"
            );
        }
        other => panic!("expected ParseError for child-line `every`, got: {other:?}"),
    }
}

#[test]
fn test_every_inline_is_rejected() {
    // The inline form (`t "x" every 1s -> …`) must be rejected too.
    let layout = r#"
doc
    text "Time" every 1s -> ticks = ticks + 1
"#;
    let result = parse_layout(layout, &mut StringInterner::new());
    match result {
        Err(MizuError::ParseError(msg)) => {
            assert!(
                msg.contains("node timers are not allowed"),
                "error must state that node timers are not allowed, got: {msg}"
            );
        }
        other => panic!("expected ParseError for inline `every`, got: {other:?}"),
    }
}

#[test]
fn test_every_inside_each_is_rejected() {
    // Regression guard for the resource-amplification vector: an `every`
    // nested in an `each` would have multiplied into one timer per list
    // element, driven by remote data. It must fail to compile.
    let layout = r#"
doc
    each item in items
        box
            every 100ms -> n = n + 1
"#;
    let result = parse_layout(layout, &mut StringInterner::new());
    match result {
        Err(MizuError::ParseError(msg)) => {
            assert!(
                msg.contains("node timers are not allowed"),
                "error must state that node timers are not allowed, got: {msg}"
            );
            assert!(
                msg.contains("line 5"),
                "error must carry the offending line number, got: {msg}"
            );
        }
        other => panic!("expected ParseError for `every` inside `each`, got: {other:?}"),
    }
}

#[test]
fn test_markdown_multiline_block() {
    let layout = r#"
doc
    markdown """
        # Header
        This is a multi-line markdown block.
        - Item 1
        - Item 2
    """
"#;
    let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
    let markdown_node = tree
        .root()
        .children()
        .find(|n| n.value().primitive == Primitive::Markdown)
        .unwrap();
    assert_eq!(markdown_node.value().primitive, Primitive::Markdown);
    let content = markdown_node.value().attributes.get("content").unwrap();
    assert!(content.contains("# Header"));
    assert!(content.contains("This is a multi-line markdown block."));
    assert!(content.contains("- Item 1"));
}

#[test]
fn test_illegal_primitive_fails() {
    let layout = r#"
doc
    invalid_primitive "Error"
"#;
    let result = parse_layout(layout, &mut StringInterner::new());
    assert!(
        matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("Illegal primitive name"))
    );
}

#[test]
fn test_multiple_roots_fail() {
    let layout = r#"
doc
box
"#;
    let result = parse_layout(layout, &mut StringInterner::new());
    assert!(
        matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("multiple root elements"))
    );
}

#[test]
fn test_badly_formatted_attributes_fail() {
    let layout = r#"
doc
    button class.btn
"#;
    let result = parse_layout(layout, &mut StringInterner::new());
    assert!(
        matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("Invalid attribute key"))
    );
}

#[test]
fn test_missing_event_payload_fails() {
    let layout = r#"
doc
    button
        click ->
"#;
    let result = parse_layout(layout, &mut StringInterner::new());
    assert!(
        matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("missing its action or variable payload"))
    );
}

#[test]
fn test_case_insensitive_primitives_and_equal_sign_attributes() {
    let layout = r#"
DOC class=title-bar
    BOX class = container
        text "Hello"
"#;
    let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
    let root = tree.root();
    assert_eq!(root.value().primitive, Primitive::Doc);
    assert_eq!(
        root.value().attributes.get("class").map(|s| s.as_str()),
        Some("title-bar")
    );

    let box_node = root
        .children()
        .find(|n| n.value().primitive == Primitive::Box)
        .unwrap();
    assert_eq!(box_node.value().primitive, Primitive::Box);
    assert_eq!(
        box_node.value().attributes.get("class").map(|s| s.as_str()),
        Some("container")
    );
}

// ────────────────────────────────────────────────────────────────────────
// Trailing layout keywords after actions must be hard errors, not silent loss
// ────────────────────────────────────────────────────────────────────────

#[test]
fn test_trailing_class_after_click_action_is_error() {
    // Before the fix this silently dropped `class "btn"`.
    let layout = r#"
doc
    button class "expected-btn"
        click -> count = count + 1 class "wrong"
"#;
    let result = parse_layout(layout, &mut StringInterner::new());
    assert!(
        result.is_err(),
        "expected error for trailing `class` after action, but parse succeeded: {result:?}"
    );
    if let Err(MizuError::ParseError(ref msg)) = result {
        assert!(
            msg.contains("class"),
            "error message should mention `class`, got: {msg}"
        );
    }
}

#[test]
fn test_trailing_id_after_click_action_is_error() {
    let layout = r#"
doc
    button
        click -> x = 1 id "my-btn"
"#;
    let result = parse_layout(layout, &mut StringInterner::new());
    assert!(
        result.is_err(),
        "expected error for trailing `id` after action"
    );
}

#[test]
fn test_trailing_src_after_click_action_is_error() {
    let layout = r#"
doc
    button
        click -> x = 1 src "image.png"
"#;
    let result = parse_layout(layout, &mut StringInterner::new());
    assert!(
        result.is_err(),
        "expected error for trailing `src` after action"
    );
}

#[test]
fn test_trailing_class_after_every_action_is_error() {
    // `every` is rejected outright now; the error must still be a clean
    // ParseError (not a panic) even with trailing garbage on the line.
    let layout = r#"
doc
    box class "timer"
        every 1s -> tick = tick + 1 class "wrong"
"#;
    let result = parse_layout(layout, &mut StringInterner::new());
    assert!(
        result.is_err(),
        "expected error for trailing `class` after every action"
    );
}

#[test]
fn test_clean_action_with_class_on_element_line_is_ok() {
    // Regression: the correct form must still parse successfully.
    let layout = r#"
doc
    button class "btn"
        click -> count = count + 1
"#;
    let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
    let root = tree.root();
    let btn = root
        .children()
        .find(|n| n.value().primitive == Primitive::Button)
        .unwrap();
    assert_eq!(btn.value().primitive, Primitive::Button);
    assert_eq!(
        btn.value().attributes.get("class").map(|s| s.as_str()),
        Some("btn")
    );
    assert!(btn.value().events.contains_key("click"));
}

#[test]
fn test_t_alias_for_text() {
    // Use window without inline text so the only Text child is the one from `t "hello"`
    let layout = r#"
doc
    t "hello"
"#;
    let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
    let root = tree.root();
    let text_node = root
        .children()
        .find(|n| n.value().primitive == Primitive::Text)
        .unwrap();
    assert_eq!(text_node.value().primitive, Primitive::Text);
    let content_child = text_node
        .value()
        .attributes
        .get("content")
        .map(|s| s.as_str());
    assert_eq!(content_child, Some("hello"));
}

#[test]
fn test_each_parsing() {
    let layout = r#"
doc
    each article in articles
        text "item"
"#;
    let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
    let root = tree.root();
    let each_node = root
        .children()
        .find(|n| n.value().primitive == Primitive::Each)
        .unwrap();
    assert_eq!(each_node.value().primitive, Primitive::Each);
    assert_eq!(
        each_node.value().iterator_context,
        Some(("article".to_string(), "articles".to_string()))
    );
}

#[test]
fn test_each_invalid_syntax_fails() {
    let layout = r#"
doc
    each item
"#;
    let result = parse_layout(layout, &mut StringInterner::new());
    assert!(result.is_err(), "expected error for invalid each syntax");
}

// ────────────────────────────────────────────────────────────────────────
// `download(alias)` built-in function
// ────────────────────────────────────────────────────────────────────────

#[test]
fn media_alias_resolved_to_absolute_url() {
    use crate::parser::urls::{EndpointKind, UrlEndpoint};

    let mut interner = StringInterner::new();
    let logo_sym = interner.get_or_intern("logo");
    let mut registry: UrlRegistry = rustc_hash::FxHashMap::default();
    registry.insert(
        logo_sym,
        UrlEndpoint {
            kind: EndpointKind::Media,
            raw_target: "mizu://cdn.local/logo.png".to_string(),
        },
    );

    let layout = "doc\n    image src \"logo\"\n";
    let tree = parse_layout_with_urls(
        layout,
        &mut interner,
        Some(&registry),
        false,
        &rustc_hash::FxHashMap::default(),
    )
    .unwrap();
    let img = tree
        .root()
        .children()
        .find(|n| n.value().primitive == Primitive::Image)
        .expect("image node not found");
    assert_eq!(
        img.value().attributes.get("src").map(String::as_str),
        Some("mizu://cdn.local/logo.png"),
        "media alias must be rewritten to its absolute URL at parse time"
    );
}

#[test]
fn test_absolute_url_src_is_rejected() {
    // A literal absolute network URL in `src` bypasses the urls registry
    // and must be a hard compile error, with or without a registry present.
    let layout = "doc\n    image src \"mizu://evil.example/pixel.png\"\n";

    let no_registry = parse_layout(layout, &mut StringInterner::new());
    match no_registry {
        Err(MizuError::ParseError(msg)) => {
            assert!(
                msg.contains("absolute URLs are not allowed in src"),
                "error must reject absolute src, got: {msg}"
            );
            assert!(
                msg.contains("media alias"),
                "error must point at media aliases, got: {msg}"
            );
            assert!(
                msg.contains("line 2"),
                "error must carry line number, got: {msg}"
            );
        }
        other => panic!("expected ParseError for absolute src (no registry), got: {other:?}"),
    }

    // Same rejection even when a registry is supplied.
    let mut interner = StringInterner::new();
    let registry: UrlRegistry = rustc_hash::FxHashMap::default();
    let with_registry = parse_layout_with_urls(
        layout,
        &mut interner,
        Some(&registry),
        false,
        &rustc_hash::FxHashMap::default(),
    );
    assert!(
        matches!(with_registry, Err(MizuError::ParseError(ref m)) if m.contains("absolute URLs are not allowed in src")),
        "absolute src must be rejected with a registry too, got: {with_registry:?}"
    );
}

#[test]
fn test_relative_src_still_allowed() {
    // A relative path must keep working (used as-is by the renderer).
    let layout = "doc\n    image src \"assets/logo.png\"\n";
    let tree = parse_layout(layout, &mut StringInterner::new()).unwrap();
    let img = tree
        .root()
        .children()
        .find(|n| n.value().primitive == Primitive::Image)
        .expect("image node not found");
    assert_eq!(
        img.value().attributes.get("src").map(String::as_str),
        Some("assets/logo.png"),
        "relative src must be preserved unchanged"
    );
}

#[test]
fn test_download_builtin_parsed() {
    use crate::parser::logic::{Action, Expr};
    use crate::parser::urls::{EndpointKind, UrlEndpoint};

    let mut interner = StringInterner::new();
    let backup_sym = interner.get_or_intern("backup_alias");
    let mut registry: UrlRegistry = rustc_hash::FxHashMap::default();
    registry.insert(
        backup_sym,
        UrlEndpoint {
            kind: EndpointKind::Media,
            raw_target: "mizu://cdn.local/backup.zip".to_string(),
        },
    );

    let layout = "doc\n    button\n        click -> download(backup_alias)\n";
    let tree = parse_layout_with_urls(
        layout,
        &mut interner,
        Some(&registry),
        false,
        &rustc_hash::FxHashMap::default(),
    )
    .unwrap();
    let btn = tree
        .root()
        .children()
        .find(|n| n.value().primitive == Primitive::Button)
        .expect("button node not found");
    let click_event = btn
        .value()
        .events
        .get("click")
        .expect("click event not found");
    match click_event {
        EventBlock::Click {
            action: Action::Eval(tree),
        } => {
            let Expr::FunctionCall {
                name,
                args_start,
                args_len,
            } = tree.root()
            else {
                panic!("expected FunctionCall root, got {:?}", tree.root());
            };
            let args = tree.arena.args(*args_start, *args_len);
            assert_eq!(
                interner.resolve(*name),
                Some("download"),
                "function name should be 'download'"
            );
            assert_eq!(args.len(), 1, "download should have 1 argument");
            match &tree.arena[args[0]] {
                Expr::Variable(sym) => assert_eq!(interner.resolve(*sym), Some("backup_alias")),
                other => panic!("expected Variable arg, got {other:?}"),
            }
        }
        other => panic!("expected Click {{ Action::Eval(FunctionCall) }}, got {other:?}"),
    }
}

#[test]
fn test_download_old_syntax_error() {
    // `button download -> backup_alias` → ParseError with migration hint
    let layout = "doc\n    button download -> backup_alias\n";
    let mut interner = StringInterner::new();
    let result = parse_layout(layout, &mut interner);
    match result {
        Err(MizuError::ParseError(msg)) => {
            assert!(
                msg.contains("download") && msg.contains("click"),
                "error should mention download and the new syntax: {msg}"
            );
        }
        other => panic!("expected ParseError for old download syntax, got: {other:?}"),
    }
}

#[test]
fn test_download_api_alias_rejected() {
    use crate::parser::urls::{EndpointKind, UrlEndpoint};

    let mut interner = StringInterner::new();
    let api_sym = interner.get_or_intern("api_alias");
    let mut registry: UrlRegistry = rustc_hash::FxHashMap::default();
    registry.insert(
        api_sym,
        UrlEndpoint {
            kind: EndpointKind::Api,
            raw_target: "mizu://api.local/v1/data".to_string(),
        },
    );

    let layout = "doc\n    button\n        click -> download(api_alias)\n";
    let result = parse_layout_with_urls(
        layout,
        &mut interner,
        Some(&registry),
        false,
        &rustc_hash::FxHashMap::default(),
    );
    match result {
        Err(MizuError::ParseError(msg)) => {
            assert!(
                msg.contains("api_alias") && (msg.contains("api") || msg.contains("media")),
                "error should mention alias and endpoint kind: {msg}"
            );
        }
        other => panic!("expected ParseError for api download alias, got: {other:?}"),
    }
}

// ────────────────────────────────────────────────────────────────────────
// Conditional classes
// ────────────────────────────────────────────────────────────────────────

#[test]
fn test_conditional_class_parsed() {
    // A `class active if flag` child line should produce a non-empty
    // conditional_classes vec on the parent box node.
    let layout = "doc\n    box class base\n        class active if flag\n";
    let mut interner = StringInterner::new();
    let tree = parse_layout(layout, &mut interner).unwrap();
    let box_node = tree
        .root()
        .children()
        .find(|n| n.value().primitive == Primitive::Box)
        .expect("box node not found");
    assert_eq!(
        box_node.value().attributes.get("class").map(|s| s.as_str()),
        Some("base")
    );
    assert_eq!(box_node.value().conditional_classes.len(), 1);
    assert!(matches!(
        &box_node.value().conditional_classes[0],
        ConditionalClass::Toggle { class_name, .. } if class_name == "active"
    ));
}

#[test]
fn test_conditional_class_applied() {
    // Condition evaluates to true → the expression result is Bool(true).
    use crate::core::types::{Value, VariableStore};
    use crate::parser::logic::evaluate;
    use rustc_hash::FxHashMap;

    let layout = "doc\n    box class base\n        class active if flag\n";
    let mut interner = StringInterner::new();
    let tree = parse_layout(layout, &mut interner).unwrap();
    let box_node = tree
        .root()
        .children()
        .find(|n| n.value().primitive == Primitive::Box)
        .unwrap();
    let ConditionalClass::Toggle { condition, .. } = &box_node.value().conditional_classes[0]
    else {
        panic!("expected ConditionalClass::Toggle");
    };

    let mut store = VariableStore::with_interner(interner.freeze());
    store.set_runtime("flag", Value::Bool(true));
    let result = evaluate(
        condition.root(),
        &condition.arena,
        &mut store,
        &FxHashMap::default(),
        0,
    )
    .unwrap();
    assert_eq!(
        result,
        Value::Bool(true),
        "condition with flag=true should be truthy"
    );
}

#[test]
fn test_conditional_class_not_applied() {
    // Condition evaluates to false → the expression result is Bool(false).
    use crate::core::types::{Value, VariableStore};
    use crate::parser::logic::evaluate;
    use rustc_hash::FxHashMap;

    let layout = "doc\n    box class base\n        class active if flag\n";
    let mut interner = StringInterner::new();
    let tree = parse_layout(layout, &mut interner).unwrap();
    let box_node = tree
        .root()
        .children()
        .find(|n| n.value().primitive == Primitive::Box)
        .unwrap();
    let ConditionalClass::Toggle { condition, .. } = &box_node.value().conditional_classes[0]
    else {
        panic!("expected ConditionalClass::Toggle");
    };

    let mut store = VariableStore::with_interner(interner.freeze());
    store.set_runtime("flag", Value::Bool(false));
    let result = evaluate(
        condition.root(),
        &condition.arena,
        &mut store,
        &FxHashMap::default(),
        0,
    )
    .unwrap();
    assert_eq!(
        result,
        Value::Bool(false),
        "condition with flag=false should be falsy"
    );
}

#[test]
fn test_multiple_conditional_classes() {
    // Three classes: two conditions true, one false → 2 truthy, 1 falsy.
    use crate::core::types::{Value, VariableStore};
    use crate::parser::logic::evaluate;
    use rustc_hash::FxHashMap;

    let layout = "doc\n    box\n        class a if flag_a\n        class b if flag_b\n        class c if flag_c\n";
    let mut interner = StringInterner::new();
    let tree = parse_layout(layout, &mut interner).unwrap();
    let box_node = tree
        .root()
        .children()
        .find(|n| n.value().primitive == Primitive::Box)
        .unwrap();
    assert_eq!(box_node.value().conditional_classes.len(), 3);

    let mut store = VariableStore::with_interner(interner.freeze());
    store.set_runtime("flag_a", Value::Bool(true));
    store.set_runtime("flag_b", Value::Bool(false));
    store.set_runtime("flag_c", Value::Bool(true));

    let fns: FxHashMap<_, _> = FxHashMap::default();
    let ccs = &box_node.value().conditional_classes;
    let truthy_count = ccs
        .iter()
        .filter(|cc| {
            let ConditionalClass::Toggle { condition, .. } = cc else {
                return false;
            };
            matches!(
                evaluate(condition.root(), &condition.arena, &mut store, &fns, 0),
                Ok(Value::Bool(true))
            )
        })
        .count();
    assert_eq!(truthy_count, 2, "two of three conditions should be truthy");
}

#[test]
fn test_conditional_class_with_field_access() {
    // Condition `item.done` on a Record value resolves correctly.
    use crate::core::types::{Value, VariableStore};
    use crate::parser::logic::evaluate;
    use rustc_hash::FxHashMap;
    use std::sync::Arc;

    let layout = "doc\n    box\n        class active if item.done\n";
    let mut interner = StringInterner::new();
    let tree = parse_layout(layout, &mut interner).unwrap();
    let box_node = tree
        .root()
        .children()
        .find(|n| n.value().primitive == Primitive::Box)
        .unwrap();
    assert!(matches!(
        &box_node.value().conditional_classes[0],
        ConditionalClass::Toggle { class_name, .. } if class_name == "active"
    ));

    let mut record_map: Vec<(Arc<str>, Value)> =
        Vec::<(std::sync::Arc<str>, crate::core::types::Value)>::new();
    record_map.push((Arc::from("done"), Value::Bool(true)));

    let mut store = VariableStore::with_interner(interner.freeze());
    store.set_runtime("item", {
        record_map.sort_by(|a, b| a.0.cmp(&b.0));
        Value::record_from_unsorted(record_map)
    });

    let ConditionalClass::Toggle { condition, .. } = &box_node.value().conditional_classes[0]
    else {
        panic!("expected ConditionalClass::Toggle");
    };
    let result = evaluate(
        condition.root(),
        &condition.arena,
        &mut store,
        &FxHashMap::default(),
        0,
    )
    .unwrap();
    assert_eq!(
        result,
        Value::Bool(true),
        "item.done should resolve to true"
    );
}

#[test]
fn test_conditional_class_with_action_rejected() {
    // A condition that calls a side-effecting built-in must produce ParseError.
    let layout = "doc\n    box\n        class active if GET(api_alias)\n";
    let mut interner = StringInterner::new();
    let result = parse_layout(layout, &mut interner);
    match result {
        Err(MizuError::ParseError(msg)) => {
            assert!(
                msg.contains("GET") || msg.contains("side-effect") || msg.contains("pure"),
                "error should mention GET or purity: {msg}"
            );
        }
        other => panic!("expected ParseError for side-effecting condition, got: {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Ternary-valued conditional classes
// ─────────────────────────────────────────────────────────────────────

#[test]
fn ternary_conditional_class_basic_two_branch() {
    use crate::core::types::{Value, VariableStore};
    use crate::parser::logic::evaluate;
    use rustc_hash::FxHashMap;

    let layout = "doc\n    box\n        class flag ? \"on\" : \"off\"\n";
    let mut interner = StringInterner::new();
    let tree = parse_layout(layout, &mut interner).unwrap();
    let box_node = tree
        .root()
        .children()
        .find(|n| n.value().primitive == Primitive::Box)
        .unwrap();
    let ConditionalClass::Ternary { expr } = &box_node.value().conditional_classes[0] else {
        panic!("expected ConditionalClass::Ternary");
    };

    let mut store = VariableStore::with_interner(interner.freeze());
    store.set_runtime("flag", Value::Bool(true));
    let result = evaluate(
        expr.root(),
        &expr.arena,
        &mut store,
        &FxHashMap::default(),
        0,
    )
    .unwrap();
    assert_eq!(result, Value::String(std::sync::Arc::from("on")));

    store.set_runtime("flag", Value::Bool(false));
    let result = evaluate(
        expr.root(),
        &expr.arena,
        &mut store,
        &FxHashMap::default(),
        0,
    )
    .unwrap();
    assert_eq!(result, Value::String(std::sync::Arc::from("off")));
}

#[test]
fn ternary_conditional_class_if_then_else_spelling_also_parses() {
    let layout = "doc\n    box\n        class if flag then \"on\" else \"off\"\n";
    let mut interner = StringInterner::new();
    let tree = parse_layout(layout, &mut interner).unwrap();
    let box_node = tree
        .root()
        .children()
        .find(|n| n.value().primitive == Primitive::Box)
        .unwrap();
    assert!(matches!(
        &box_node.value().conditional_classes[0],
        ConditionalClass::Ternary { .. }
    ));
}

#[test]
fn ternary_conditional_class_nested_ternary_is_accepted() {
    let layout = "doc\n    box\n        class a ? \"x\" : b ? \"y\" : \"z\"\n";
    let mut interner = StringInterner::new();
    let tree = parse_layout(layout, &mut interner).unwrap();
    let box_node = tree
        .root()
        .children()
        .find(|n| n.value().primitive == Primitive::Box)
        .unwrap();
    assert!(matches!(
        &box_node.value().conditional_classes[0],
        ConditionalClass::Ternary { .. }
    ));
}

#[test]
fn ternary_conditional_class_rejects_variable_branch() {
    let layout = "doc\n    box\n        class flag ? name : \"off\"\n";
    let mut interner = StringInterner::new();
    let result = parse_layout(layout, &mut interner);
    assert!(
        matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("variable")),
        "expected ParseError naming the variable branch, got: {result:?}"
    );
}

#[test]
fn ternary_conditional_class_rejects_field_access_branch() {
    let layout = "doc\n    box\n        class flag ? item.name : \"off\"\n";
    let mut interner = StringInterner::new();
    let result = parse_layout(layout, &mut interner);
    assert!(
        matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("field access")),
        "expected ParseError naming the field-access branch, got: {result:?}"
    );
}

#[test]
fn ternary_conditional_class_rejects_function_call_branch() {
    // `to_string` is a known-pure builtin (see `KNOWN_PURE_BUILTINS` in
    // purity.rs), so this exercises the literal-branch check
    // specifically -- not the separate purity check, which it passes.
    let layout = "doc\n    box\n        class flag ? to_string(1) : \"off\"\n";
    let mut interner = StringInterner::new();
    let result = parse_layout(layout, &mut interner);
    assert!(
        matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("function call")),
        "expected ParseError naming the function-call branch, got: {result:?}"
    );
}

#[test]
fn ternary_conditional_class_rejects_effectful_condition() {
    let layout = "doc\n    box\n        class GET(api_alias) ? \"on\" : \"off\"\n";
    let mut interner = StringInterner::new();
    let result = parse_layout(layout, &mut interner);
    assert!(
        matches!(result, Err(MizuError::ParseError(ref m)) if m.contains("GET") || m.contains("side-effect") || m.contains("pure")),
        "expected ParseError naming the side-effecting condition, got: {result:?}"
    );
}

#[test]
fn conditional_class_toggle_name_literally_if_still_parses() {
    // Edge case: the class name in the toggle form happens to be the
    // literal word `if` -- disambiguation keys off the *second* token's
    // position, not this one, so it must not misfire.
    let layout = "doc\n    box\n        class if if flag\n";
    let mut interner = StringInterner::new();
    let tree = parse_layout(layout, &mut interner).unwrap();
    let box_node = tree
        .root()
        .children()
        .find(|n| n.value().primitive == Primitive::Box)
        .unwrap();
    assert!(matches!(
        &box_node.value().conditional_classes[0],
        ConditionalClass::Toggle { class_name, .. } if class_name == "if"
    ));
}

#[test]
fn conditional_class_toggle_condition_with_question_mark_in_string_still_parses() {
    // Edge case: a `?` inside a quoted string within a toggle condition
    // must not be mistaken for a ternary -- the `if` keyword at the
    // second token position is checked before any ternary attempt.
    let layout = "doc\n    box\n        class active if content == \"is this ok?\"\n";
    let mut interner = StringInterner::new();
    let tree = parse_layout(layout, &mut interner).unwrap();
    let box_node = tree
        .root()
        .children()
        .find(|n| n.value().primitive == Primitive::Box)
        .unwrap();
    assert!(matches!(
        &box_node.value().conditional_classes[0],
        ConditionalClass::Toggle { class_name, .. } if class_name == "active"
    ));
}

#[test]
fn conditional_class_three_forms_coexist_and_disambiguate_correctly() {
    // Realistic neighboring examples: static-toggle, and both ternary
    // spellings, back to back on sibling nodes.
    let layout = "doc\n    box\n        class active if flag\n    box\n        class flag ? \"on\" : \"off\"\n    box\n        class if flag then \"on\" else \"off\"\n";
    let mut interner = StringInterner::new();
    let tree = parse_layout(layout, &mut interner).unwrap();
    let mut boxes = tree
        .root()
        .children()
        .filter(|n| n.value().primitive == Primitive::Box);

    let toggle_box = boxes.next().unwrap();
    assert!(matches!(
        &toggle_box.value().conditional_classes[0],
        ConditionalClass::Toggle { class_name, .. } if class_name == "active"
    ));

    let ternary_question_box = boxes.next().unwrap();
    assert!(matches!(
        &ternary_question_box.value().conditional_classes[0],
        ConditionalClass::Ternary { .. }
    ));

    let ternary_ifelse_box = boxes.next().unwrap();
    assert!(matches!(
        &ternary_ifelse_box.value().conditional_classes[0],
        ConditionalClass::Ternary { .. }
    ));
}
