use std::path::Path;

use super::*;

fn jsx(source: &str, offset_of: &str) -> Option<HoverTarget> {
    let offset = source.find(offset_of).expect("anchor present");
    target_at_offset(Path::new("/project/src/App.jsx"), source, offset)
}

fn html(source: &str, offset_of: &str) -> Option<HoverTarget> {
    let offset = source.find(offset_of).expect("anchor present");
    target_at_offset(Path::new("/project/index.html"), source, offset)
}

// ── identifier resolution ───────────────────────────────────────────────────

#[test]
fn finds_a_component_reference_under_the_cursor() {
    let source = "export default function App() { return <Card title=\"hi\" />; }";
    assert_eq!(
        jsx(source, "Card"),
        Some(HoverTarget::Component("Card".to_string()))
    );
}

#[test]
fn finds_a_component_from_the_middle_of_its_name() {
    let source = "return <UserProfile />;";
    let offset = source.find("Profile").expect("anchor");
    assert_eq!(
        target_at_offset(Path::new("/project/src/App.jsx"), source, offset),
        Some(HoverTarget::Component("UserProfile".to_string()))
    );
}

#[test]
fn lowercase_identifiers_are_not_components() {
    let source = "const total = count + 1;";
    assert_eq!(jsx(source, "count"), None);
}

#[test]
fn plain_html_tags_in_jsx_are_not_components() {
    let source = "return <div className=\"row\" />;";
    assert_eq!(jsx(source, "div"), None);
}

#[test]
fn whitespace_is_not_a_target() {
    let source = "const a = 1;   ";
    let offset = source.len() - 1;
    assert_eq!(
        target_at_offset(Path::new("/project/src/App.jsx"), source, offset),
        None
    );
}

#[test]
fn unknown_extensions_resolve_to_nothing() {
    let source = "<Card />";
    let offset = source.find("Card").expect("anchor");
    assert_eq!(
        target_at_offset(Path::new("/project/notes.txt"), source, offset),
        None
    );
}

// ── HTML element ranges ─────────────────────────────────────────────────────

#[test]
fn finds_the_innermost_enclosing_element() {
    let source = r#"<html><body><section class="hero"><p>hello</p></section></body></html>"#;
    let Some(HoverTarget::HtmlElement { start, end }) = html(source, "hello") else {
        panic!("expected an element");
    };
    assert_eq!(&source[start..end], "<p>hello</p>");
}

#[test]
fn finds_the_section_when_hovering_its_own_tag() {
    let source = r#"<body><section class="hero"><p>hi</p></section></body>"#;
    let Some(HoverTarget::HtmlElement { start, end }) = html(source, "hero") else {
        panic!("expected an element");
    };
    assert_eq!(
        &source[start..end],
        r#"<section class="hero"><p>hi</p></section>"#
    );
}

#[test]
fn void_elements_are_their_own_target() {
    let source = r#"<div><img src="a.png" alt="cat"></div>"#;
    let Some(HoverTarget::HtmlElement { start, end }) = html(source, "cat") else {
        panic!("expected an element");
    };
    assert_eq!(&source[start..end], r#"<img src="a.png" alt="cat">"#);
}

#[test]
fn structural_tags_are_skipped_as_targets() {
    // Hovering inside <body> but outside any real element gives nothing rather
    // than offering to preview the whole document.
    let source = "<html><body>   </body></html>";
    let offset = source.find("   ").expect("anchor") + 1;
    assert_eq!(
        target_at_offset(Path::new("/project/index.html"), source, offset),
        None
    );
}

#[test]
fn attribute_values_containing_angle_brackets_do_not_break_the_walk() {
    let source = r#"<div><span title="a > b">x</span></div>"#;
    let Some(HoverTarget::HtmlElement { start, end }) = html(source, ">x<") else {
        panic!("expected an element");
    };
    assert_eq!(&source[start..end], r#"<span title="a > b">x</span>"#);
}

#[test]
fn unclosed_tags_do_not_hang_the_walk() {
    let source = "<div><p>text";
    assert_eq!(
        target_at_offset(Path::new("/project/index.html"), source, 8),
        None
    );
}

// ── labels ──────────────────────────────────────────────────────────────────

#[test]
fn labels_read_like_selectors() {
    assert_eq!(element_label(r#"<section class="hero big">x</section>"#), "section.hero");
    assert_eq!(element_label(r#"<div id="main">x</div>"#), "div#main");
    assert_eq!(element_label("<p>x</p>"), "p");
}

// ── import binding ──────────────────────────────────────────────────────────

#[test]
fn recognises_default_and_named_imports() {
    assert!(import_clause_binds("import Card", "Card"));
    assert!(import_clause_binds("import { Card, Badge }", "Badge"));
    assert!(import_clause_binds("import React, { useState }", "useState"));
}

#[test]
fn recognises_renamed_imports_by_their_local_name() {
    assert!(import_clause_binds("import { Card as Panel }", "Panel"));
    assert!(!import_clause_binds("import { Card as Panel }", "Card"));
}

#[test]
fn unrelated_imports_do_not_bind() {
    assert!(!import_clause_binds("import { Badge }", "Card"));
}

#[test]
fn package_imports_are_not_resolved_to_source() {
    // A bare specifier is a dependency: there is no project source to preview.
    let source = "import { Button } from 'some-ui-kit';\nreturn <Button />;";
    assert_eq!(
        resolve_import(
            Path::new("/project"),
            Path::new("/project/src/App.jsx"),
            source,
            "Button"
        ),
        None
    );
}
