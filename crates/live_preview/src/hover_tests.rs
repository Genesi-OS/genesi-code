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
fn finds_the_element_whose_tag_is_under_the_pointer() {
    let source = r#"<html><body><section class="hero"><p>hello</p></section></body></html>"#;
    let Some(HoverTarget::HtmlElement { start, end }) = html(source, "<p>") else {
        panic!("expected an element");
    };
    assert_eq!(&source[start..end], "<p>hello</p>");
}

#[test]
fn pointing_at_text_content_is_not_a_target() {
    // Resting anywhere inside an element used to open its card, which made the
    // card appear whenever the pointer was merely somewhere on the line.
    let source = r#"<html><body><section class="hero"><p>hello</p></section></body></html>"#;
    assert_eq!(html(source, "hello"), None);
}

#[test]
fn the_closing_tag_is_also_a_target() {
    let source = "<div><p>hi</p></div>";
    let Some(HoverTarget::HtmlElement { start, end }) = html(source, "</p>") else {
        panic!("expected an element");
    };
    assert_eq!(&source[start..end], "<p>hi</p>");
}

#[test]
fn svg_internals_are_never_targets() {
    // These compile to nothing, so targeting them produced a blank card.
    let source = r#"<button><svg viewBox="0 0 24 24"><path fill="none" d="M0 0h24"></path></svg>Text</button>"#;
    assert_eq!(html(source, "<path"), None);
    assert_eq!(html(source, "<svg"), None);
    // The button around them is still a perfectly good target.
    assert!(matches!(
        html(source, "<button"),
        Some(HoverTarget::HtmlElement { .. })
    ));
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

// ── blank previews ──────────────────────────────────────────────────────────

#[test]
fn a_target_that_compiles_to_nothing_yields_no_card() {
    // An element made only of dropped children has nothing to draw; showing a
    // titled, empty card for it reads as a bug rather than as a preview.
    let document = compile_html_fragment(
        Path::new("/project"),
        Path::new("/project/index.html"),
        "<div><svg><path d=\"M0 0\"></path></svg></div>",
        &Stylesheet::default(),
    );
    assert!(document.is_blank());
}

#[test]
fn a_target_with_text_is_not_blank() {
    let document = compile_html_fragment(
        Path::new("/project"),
        Path::new("/project/index.html"),
        "<div><span>hello</span></div>",
        &Stylesheet::default(),
    );
    assert!(!document.is_blank());
}

#[test]
fn a_target_with_only_a_background_is_not_blank() {
    let document = compile_html_fragment(
        Path::new("/project"),
        Path::new("/project/index.html"),
        "<div style=\"background:#ff0000;height:40px\"></div>",
        &Stylesheet::default(),
    );
    assert!(!document.is_blank());
}

// ── single-file components ──────────────────────────────────────────────────

fn sfc(path: &str, source: &str, anchor: &str) -> Option<HoverTarget> {
    let offset = source.find(anchor).expect("anchor present");
    target_at_offset(Path::new(path), source, offset)
}

#[test]
fn vue_template_elements_are_targets() {
    let source = r#"
<template>
  <section class="hero"><p>hi</p></section>
</template>
<script>export default {}</script>
<style>.hero { background: #101418; }</style>
"#;
    let Some(HoverTarget::HtmlElement { start, end }) =
        sfc("/project/src/Hero.vue", source, "<section")
    else {
        panic!("expected an element");
    };
    assert!(source[start..end].contains("<p>hi</p>"));
}

#[test]
fn the_template_wrapper_itself_is_not_a_target() {
    let source = "<template>
  <p>hi</p>
</template>";
    assert_eq!(sfc("/project/src/A.vue", source, "<template"), None);
}

#[test]
fn svelte_markup_is_a_target() {
    let source = r#"
<script>let name = 'world';</script>
<h1 class="title">hello</h1>
<style>.title { color: #00ff00; }</style>
"#;
    assert!(matches!(
        sfc("/project/src/App.svelte", source, "<h1"),
        Some(HoverTarget::HtmlElement { .. })
    ));
}

#[test]
fn a_single_file_component_is_styled_by_its_own_style_block() {
    let source = r#"
<template>
  <section class="hero">hi</section>
</template>
<style>.hero { background: #0000ff; padding: 16px; }</style>
"#;
    let offset = source.find("<section").expect("anchor");
    let preview = preview_at_offset(
        Path::new("/project"),
        Path::new("/project/src/Hero.vue"),
        source,
        offset,
    )
    .expect("a preview");
    assert_eq!(preview.label, "section.hero");
    // The style block travels with the file, so no stylesheet lookup is needed.
    assert!(!preview.document.is_blank());
}

#[test]
fn scoped_and_scss_style_blocks_are_read_too() {
    let source = r#"
<template>
  <div class="card"><span class="tag">hi</span></div>
</template>
<style lang="scss" scoped>
.card { color: #111111; .tag { color: #00ff00; } }
</style>
"#;
    let offset = source.find("<div").expect("anchor");
    let preview = preview_at_offset(
        Path::new("/project"),
        Path::new("/project/src/Card.vue"),
        source,
        offset,
    )
    .expect("a preview");
    assert!(!preview.document.is_blank());
}

// ── module resolution ───────────────────────────────────────────────────────

#[test]
fn multi_line_imports_still_bind_their_names() {
    // Prettier splits an import as soon as the list is long enough, and a
    // line-based scan never matched these.
    let source = r#"
import React from 'react';
import {
  Card,
  Badge,
} from './ui/components';
"#;
    let bindings = module_bindings(source, "import");
    let card = bindings
        .iter()
        .find(|(clause, _)| import_clause_binds(clause, "Card"))
        .expect("Card is bound");
    assert_eq!(card.1, "./ui/components");

    let badge = bindings
        .iter()
        .find(|(clause, _)| import_clause_binds(clause, "Badge"))
        .expect("Badge is bound");
    assert_eq!(badge.1, "./ui/components");
}

#[test]
fn a_side_effect_import_binds_nothing() {
    let source = "import './styles.css';\nimport Card from './Card';";
    let bindings = module_bindings(source, "import");
    assert!(bindings
        .iter()
        .any(|(clause, specifier)| import_clause_binds(clause, "Card") && specifier == "./Card"));
}

#[test]
fn export_statements_are_read_the_same_way() {
    let source = r#"
export { default as Card } from './Card';
export { Badge } from './Badge';
export * from './misc';
"#;
    let bindings = module_bindings(source, "export");
    assert!(bindings
        .iter()
        .any(|(clause, specifier)| import_clause_binds(clause, "Card") && specifier == "./Card"));
    assert!(bindings
        .iter()
        .any(|(clause, specifier)| import_clause_binds(clause, "Badge") && specifier == "./Badge"));
    // A wildcard names nothing, so it must not claim to bind a name.
    assert!(bindings
        .iter()
        .any(|(clause, specifier)| clause.contains('*') && specifier == "./misc"));
}

#[test]
fn type_only_imports_do_not_shadow_a_value_import() {
    let source = "import type { Props } from './types';\nimport Card from './Card';";
    let bindings = module_bindings(source, "import");
    let card = bindings
        .iter()
        .find(|(clause, _)| import_clause_binds(clause, "Card"))
        .expect("Card is bound");
    assert_eq!(card.1, "./Card");
}
