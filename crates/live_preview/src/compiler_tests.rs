use std::path::Path;

use super::*;

fn compile(html: &str) -> PreviewDocument {
    compile_html(Path::new("/project"), Path::new("/project/index.html"), html)
}

fn first_box(node: &PreviewNode) -> Option<&PreviewBox> {
    match node {
        PreviewNode::Box(preview_box) => Some(preview_box),
        _ => None,
    }
}

/// Depth-first search for the first box carrying `tag`.
fn find_box<'a>(node: &'a PreviewNode, tag: &str) -> Option<&'a PreviewBox> {
    let preview_box = first_box(node)?;
    if preview_box.tag == tag {
        return Some(preview_box);
    }
    preview_box
        .children
        .iter()
        .find_map(|child| find_box(child, tag))
}

fn collect_text(node: &PreviewNode) -> String {
    match node {
        PreviewNode::Box(preview_box) => preview_box
            .children
            .iter()
            .map(collect_text)
            .collect::<Vec<_>>()
            .join(" "),
        PreviewNode::Inline { fragments, .. } => fragments
            .iter()
            .map(|fragment| fragment.text.as_str())
            .collect::<Vec<_>>()
            .join(""),
        PreviewNode::Placeholder { label, .. } => label.clone(),
        PreviewNode::Image { alt, .. } => alt.clone(),
        PreviewNode::Rule { .. } => String::new(),
        PreviewNode::Svg { .. } => String::new(),
    }
}

// ── colors ──────────────────────────────────────────────────────────────────

#[test]
fn parses_hex_colors_of_every_length() {
    assert_eq!(parse_color("#fff"), Some(Rgba::new(255, 255, 255, 255)));
    assert_eq!(parse_color("#0f8f6a"), Some(Rgba::new(15, 143, 106, 255)));
    assert_eq!(parse_color("#0f8f6a80"), Some(Rgba::new(15, 143, 106, 128)));
}

#[test]
fn parses_functional_colors() {
    assert_eq!(parse_color("rgb(10, 20, 30)"), Some(Rgba::new(10, 20, 30, 255)));
    assert_eq!(
        parse_color("rgba(10, 20, 30, 0.5)"),
        Some(Rgba::new(10, 20, 30, 127))
    );
    assert_eq!(parse_color("hsl(0, 100%, 50%)"), Some(Rgba::new(255, 0, 0, 255)));
}

#[test]
fn gradients_fall_back_to_their_first_stop() {
    assert_eq!(
        parse_color("linear-gradient(90deg, #ff0000 0%, #00ff00 100%)"),
        Some(Rgba::new(255, 0, 0, 255))
    );
}

#[test]
fn transparent_is_not_treated_as_a_missing_color() {
    assert_eq!(parse_color("transparent"), Some(Rgba::new(0, 0, 0, 0)));
    assert_eq!(parse_color("currentColor"), None);
}

// ── lengths ─────────────────────────────────────────────────────────────────

#[test]
fn resolves_length_units() {
    assert_eq!(parse_length("12px", 16., None), Some(12.));
    assert_eq!(parse_length("2rem", 16., None), Some(32.));
    assert_eq!(parse_length("1.5em", 20., None), Some(30.));
    assert_eq!(parse_length("50%", 16., Some(400.)), Some(200.));
    assert_eq!(parse_length("auto", 16., None), None);
}

// ── the cascade ─────────────────────────────────────────────────────────────

#[test]
fn class_rules_beat_tag_rules() {
    let document = compile(
        r#"<html><head><style>
            p { color: #ff0000; }
            .accent { color: #00ff00; }
        </style></head><body><p class="accent">hi</p></body></html>"#,
    );
    let paragraph = find_box(&document.root, "p").expect("paragraph");
    assert_eq!(paragraph.style.color, Some(Rgba::new(0, 255, 0, 255)));
}

#[test]
fn inline_style_beats_every_rule() {
    let document = compile(
        r#"<html><head><style>#main { color: #ff0000; }</style></head>
           <body><div id="main" style="color:#0000ff">hi</div></body></html>"#,
    );
    let div = find_box(&document.root, "div").expect("div");
    assert_eq!(div.style.color, Some(Rgba::new(0, 0, 255, 255)));
}

#[test]
fn descendant_selectors_match_through_ancestors() {
    let document = compile(
        r#"<html><head><style>.card p { color: #123456; }</style></head>
           <body><div class="card"><section><p>hi</p></section></div></body></html>"#,
    );
    let paragraph = find_box(&document.root, "p").expect("paragraph");
    assert_eq!(paragraph.style.color, Some(Rgba::new(0x12, 0x34, 0x56, 255)));
}

#[test]
fn descendant_selectors_do_not_match_outside_their_ancestor() {
    let document = compile(
        r#"<html><head><style>.card p { color: #123456; }</style></head>
           <body><p>hi</p></body></html>"#,
    );
    let paragraph = find_box(&document.root, "p").expect("paragraph");
    assert_eq!(paragraph.style.color, None);
}

#[test]
fn hover_rules_never_apply_to_the_resting_render() {
    let document = compile(
        r#"<html><head><style>
            .btn { color: #111111; }
            .btn:hover { color: #ff0000; }
        </style></head><body><div class="btn">go</div></body></html>"#,
    );
    let button = find_box(&document.root, "div").expect("div");
    assert_eq!(button.style.color, Some(Rgba::new(0x11, 0x11, 0x11, 255)));
}

#[test]
fn custom_properties_resolve_through_var() {
    let document = compile(
        r#"<html><head><style>
            html { --brand: #0f8f6a; }
            .hero { background: var(--brand); }
        </style></head><body><div class="hero">hi</div></body></html>"#,
    );
    let hero = find_box(&document.root, "div").expect("div");
    assert_eq!(hero.style.background, Some(Rgba::new(15, 143, 106, 255)));
}

#[test]
fn var_falls_back_when_undefined() {
    let document = compile(
        r#"<html><head><style>.hero { color: var(--missing, #abcdef); }</style></head>
           <body><div class="hero">hi</div></body></html>"#,
    );
    let hero = find_box(&document.root, "div").expect("div");
    assert_eq!(hero.style.color, Some(Rgba::new(0xab, 0xcd, 0xef, 255)));
}

#[test]
fn nested_rules_resolve_against_their_parent() {
    // SCSS, and plain CSS since nesting landed. Without this the inner rule
    // was parsed as garbage and the whole file after it could be lost.
    let document = compile(
        r#"<html><head><style>
            .card {
                color: #111111;
                .title { color: #00ff00; }
            }
        </style></head><body>
            <div class="card"><p class="title">hi</p></div>
        </body></html>"#,
    );
    let card = find_box(&document.root, "div").expect("card");
    assert_eq!(card.style.color, Some(Rgba::new(0x11, 0x11, 0x11, 255)));
    let title = find_box(&document.root, "p").expect("title");
    assert_eq!(title.style.color, Some(Rgba::new(0, 255, 0, 255)));
}

#[test]
fn ampersand_nesting_attaches_to_the_parent_itself() {
    let document = compile(
        r#"<html><head><style>
            .btn {
                color: #111111;
                &.primary { color: #0000ff; }
            }
        </style></head><body><div class="btn primary">go</div></body></html>"#,
    );
    let button = find_box(&document.root, "div").expect("div");
    assert_eq!(button.style.color, Some(Rgba::new(0, 0, 255, 255)));
}

#[test]
fn nested_state_rules_still_never_apply() {
    let document = compile(
        r#"<html><head><style>
            .btn { color: #111111; &:hover { color: #ff0000; } }
        </style></head><body><div class="btn">go</div></body></html>"#,
    );
    let button = find_box(&document.root, "div").expect("div");
    assert_eq!(button.style.color, Some(Rgba::new(0x11, 0x11, 0x11, 255)));
}

#[test]
fn a_rule_after_a_nested_block_is_not_lost() {
    let document = compile(
        r#"<html><head><style>
            .a { .inner { color: #00ff00; } }
            .b { color: #0000ff; }
        </style></head><body><div class="b">hi</div></body></html>"#,
    );
    let node = find_box(&document.root, "div").expect("div");
    assert_eq!(node.style.color, Some(Rgba::new(0, 0, 255, 255)));
}

#[test]
fn media_nested_inside_a_rule_applies_to_that_rule() {
    let document = compile(
        r#"<html><head><style>
            .hero { color: #111111; @media (min-width: 600px) { color: #00ff00; } }
        </style></head><body><div class="hero">hi</div></body></html>"#,
    );
    let hero = find_box(&document.root, "div").expect("div");
    assert_eq!(hero.style.color, Some(Rgba::new(0, 255, 0, 255)));
}

#[test]
fn media_blocks_contribute_their_rules() {
    let document = compile(
        r#"<html><head><style>
            @media (min-width: 600px) { .wide { color: #00ff00; } }
        </style></head><body><div class="wide">hi</div></body></html>"#,
    );
    let wide = find_box(&document.root, "div").expect("div");
    assert_eq!(wide.style.color, Some(Rgba::new(0, 255, 0, 255)));
}

#[test]
fn comments_do_not_break_parsing() {
    let document = compile(
        r#"<html><head><style>
            /* the brand color */
            .a { color: #ff0000; } /* trailing */
        </style></head><body><div class="a">hi</div></body></html>"#,
    );
    let node = find_box(&document.root, "div").expect("div");
    assert_eq!(node.style.color, Some(Rgba::new(255, 0, 0, 255)));
}

#[test]
fn shorthand_padding_expands_to_each_edge() {
    let document = compile(
        r#"<html><head><style>.a { padding: 4px 8px 12px 16px; }</style></head>
           <body><div class="a">hi</div></body></html>"#,
    );
    let node = find_box(&document.root, "div").expect("div");
    assert_eq!(node.style.padding.top, 4.);
    assert_eq!(node.style.padding.right, 8.);
    assert_eq!(node.style.padding.bottom, 12.);
    assert_eq!(node.style.padding.left, 16.);
}

#[test]
fn flex_layout_properties_survive_the_cascade() {
    let document = compile(
        r#"<html><head><style>.row {
            display: flex; flex-direction: row; justify-content: space-between;
            align-items: center; gap: 12px;
        }</style></head><body><div class="row"><span>a</span></div></body></html>"#,
    );
    let row = find_box(&document.root, "div").expect("div");
    assert_eq!(row.style.display, Display::Flex);
    assert_eq!(row.style.direction, FlexDirection::Row);
    assert_eq!(row.style.justify, Justify::SpaceBetween);
    assert_eq!(row.style.align, Align::Center);
    assert_eq!(row.style.gap, 12.);
}

#[test]
fn display_none_subtrees_are_dropped() {
    let document = compile(
        r#"<html><head><style>.gone { display: none; }</style></head>
           <body><div class="gone">secret</div><p>shown</p></body></html>"#,
    );
    let text = collect_text(&document.root);
    assert!(!text.contains("secret"), "hidden text leaked: {text}");
    assert!(text.contains("shown"));
}

#[test]
fn font_size_inherits_and_em_resolves_against_the_parent() {
    let document = compile(
        r#"<html><head><style>
            .outer { font-size: 20px; }
            .inner { font-size: 1.5em; }
        </style></head><body><div class="outer"><div class="inner">hi</div></div></body></html>"#,
    );
    let outer = find_box(&document.root, "div").expect("outer");
    assert_eq!(outer.style.font_size, 20.);
    let inner = outer
        .children
        .iter()
        .find_map(|child| find_box(child, "div"))
        .expect("inner");
    assert_eq!(inner.style.font_size, 30.);
}

// ── inline runs ─────────────────────────────────────────────────────────────

#[test]
fn phrasing_content_folds_into_one_wrappable_run() {
    let document = compile("<html><body><p>Hello <strong>world</strong>!</p></body></html>");
    let paragraph = find_box(&document.root, "p").expect("paragraph");
    assert_eq!(paragraph.children.len(), 1);
    let PreviewNode::Inline { fragments, .. } = &paragraph.children[0] else {
        panic!("expected an inline run, got {:?}", paragraph.children[0]);
    };
    assert_eq!(fragments.len(), 3);
    assert_eq!(fragments[1].text, "world");
    assert!(fragments[1].bold);
    assert!(!fragments[0].bold);
}

#[test]
fn whitespace_between_words_collapses_to_one_space() {
    let document = compile("<html><body><p>a\n   b</p></body></html>");
    assert_eq!(collect_text(&document.root).trim(), "a b");
}

#[test]
fn scripts_and_styles_never_reach_the_rendered_text() {
    let document = compile(
        "<html><body><script>var secret = 1;</script><p>visible</p></body></html>",
    );
    let text = collect_text(&document.root);
    assert!(!text.contains("secret"), "script body leaked: {text}");
    assert!(text.contains("visible"));
}

#[test]
fn line_breaks_survive_as_hard_breaks() {
    let document = compile("<html><body><p>a<br>b</p></body></html>");
    let paragraph = find_box(&document.root, "p").expect("paragraph");
    let PreviewNode::Inline { fragments, .. } = &paragraph.children[0] else {
        panic!("expected an inline run");
    };
    assert!(fragments.iter().any(|fragment| fragment.text.contains('\n')));
}

#[test]
fn text_transform_uppercase_applies_to_the_rendered_text() {
    let document = compile(
        r#"<html><head><style>.shout { text-transform: uppercase; }</style></head>
           <body><p class="shout">quiet</p></body></html>"#,
    );
    assert!(collect_text(&document.root).contains("QUIET"));
}

// ── JSX ─────────────────────────────────────────────────────────────────────

#[test]
fn extracts_a_function_component() {
    let source = r#"
        import React from 'react';
        export default function Card() {
          return (
            <div className="card">
              <h2>Title</h2>
            </div>
          );
        }
    "#;
    let components = extract_jsx_components(
        Path::new("/project"),
        Path::new("/project/src/Card.jsx"),
        source,
    );
    assert_eq!(components.len(), 1);
    assert_eq!(components[0].name, "Card");
    assert!(components[0].markup.contains("<h2>Title</h2>"));
}

#[test]
fn extracts_a_component_whose_body_opens_with_hooks() {
    // The shape that used to defeat extraction: walking back from `return`
    // landed on `const [count, setCount]` and read `[count,` as the name.
    let source = r#"
        import { useState } from 'react';
        export default function App() {
          const [count, setCount] = useState(0);
          const label = 'hi';
          return (
            <div className="app">
              <Counter value={count} />
            </div>
          );
        }
    "#;
    let components = extract_jsx_components(
        Path::new("/project"),
        Path::new("/project/src/App.tsx"),
        source,
    );
    assert_eq!(
        components.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        vec!["App"]
    );
    assert!(components[0].markup.contains("<Counter"));
}

#[test]
fn extracts_a_typed_arrow_component() {
    let source = r#"
        const Card: React.FC<Props> = ({ title }) => {
          const cls = 'card';
          return <div className={cls}>{title}</div>;
        };
    "#;
    let components = extract_jsx_components(
        Path::new("/project"),
        Path::new("/project/src/Card.tsx"),
        source,
    );
    assert_eq!(components.len(), 1);
    assert_eq!(components[0].name, "Card");
}

#[test]
fn extracts_several_components_from_one_file() {
    let source = r#"
        export function Badge() {
          return <span className="badge">new</span>;
        }
        export function Panel() {
          const x = 1;
          return <section className="panel"><Badge /></section>;
        }
    "#;
    let components = extract_jsx_components(
        Path::new("/project"),
        Path::new("/project/src/ui.tsx"),
        source,
    );
    let names: Vec<&str> = components.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["Badge", "Panel"]);
    // Each component must claim only its own markup.
    assert!(components[0].markup.contains("badge"));
    assert!(!components[0].markup.contains("panel"));
    assert!(components[1].markup.contains("<Badge />"));
}

#[test]
fn destructuring_declarations_are_not_components() {
    let source = "const [Alpha, setAlpha] = useState(null);
const { Beta } = props;";
    let components = extract_jsx_components(
        Path::new("/project"),
        Path::new("/project/src/x.tsx"),
        source,
    );
    assert!(components.is_empty(), "got {components:?}");
}

#[test]
fn extracts_an_arrow_component() {
    let source = r#"const Badge = () => <span className="badge">new</span>;"#;
    let components = extract_jsx_components(
        Path::new("/project"),
        Path::new("/project/src/Badge.tsx"),
        source,
    );
    assert_eq!(components.len(), 1);
    assert_eq!(components[0].name, "Badge");
}

#[test]
fn lowercase_helpers_are_not_mistaken_for_components() {
    let source = r#"function helper() { return <div>nope</div>; }"#;
    let components = extract_jsx_components(
        Path::new("/project"),
        Path::new("/project/src/helper.js"),
        source,
    );
    assert!(components.is_empty(), "got {components:?}");
}

#[test]
fn jsx_compiles_class_names_through_the_cascade() {
    let component = JsxComponentSource {
        name: "Card".to_string(),
        path: "/project/src/Card.jsx".into(),
        line: 1,
        markup: r#"<div className="card"><p>hi</p></div>"#.to_string(),
        stylesheets: Vec::new(),
    };
    let sheet = Stylesheet::parse(".card { background: #101418; padding: 16px; }");
    let document = compile_jsx_component(
        Path::new("/project"),
        &component,
        &HashMap::new(),
        &sheet,
    );
    let card = find_box(&document.root, "div").expect("card");
    assert_eq!(card.style.background, Some(Rgba::new(0x10, 0x14, 0x18, 255)));
    assert_eq!(card.style.padding.top, 16.);
}

#[test]
fn jsx_inline_style_objects_become_declarations() {
    let component = JsxComponentSource {
        name: "Chip".to_string(),
        path: "/project/src/Chip.jsx".into(),
        line: 1,
        markup: r#"<div style={{ backgroundColor: '#ff0000', fontSize: 20 }}>hi</div>"#.to_string(),
        stylesheets: Vec::new(),
    };
    let document = compile_jsx_component(
        Path::new("/project"),
        &component,
        &HashMap::new(),
        &Stylesheet::default(),
    );
    let chip = find_box(&document.root, "div").expect("chip");
    assert_eq!(chip.style.background, Some(Rgba::new(255, 0, 0, 255)));
    assert_eq!(chip.style.font_size, 20.);
}

#[test]
fn jsx_expressions_become_labelled_placeholders() {
    let component = JsxComponentSource {
        name: "Greeting".to_string(),
        path: "/project/src/Greeting.jsx".into(),
        line: 1,
        markup: "<div>{user.name}</div>".to_string(),
        stylesheets: Vec::new(),
    };
    let document = compile_jsx_component(
        Path::new("/project"),
        &component,
        &HashMap::new(),
        &Stylesheet::default(),
    );
    assert!(collect_text(&document.root).contains("user.name"));
}

#[test]
fn child_components_expand_in_place() {
    let badge = JsxComponentSource {
        name: "Badge".to_string(),
        path: "/project/src/Badge.jsx".into(),
        line: 1,
        markup: "<span>NEW</span>".to_string(),
        stylesheets: Vec::new(),
    };
    let card = JsxComponentSource {
        name: "Card".to_string(),
        path: "/project/src/Card.jsx".into(),
        line: 1,
        markup: "<div><Badge /></div>".to_string(),
        stylesheets: Vec::new(),
    };
    let index = HashMap::from([("Badge".to_string(), badge)]);
    let document = compile_jsx_component(
        Path::new("/project"),
        &card,
        &index,
        &Stylesheet::default(),
    );
    assert!(collect_text(&document.root).contains("NEW"));
}

#[test]
fn unresolved_child_components_render_as_placeholders() {
    let card = JsxComponentSource {
        name: "Card".to_string(),
        path: "/project/src/Card.jsx".into(),
        line: 1,
        markup: "<div><Missing /></div>".to_string(),
        stylesheets: Vec::new(),
    };
    let document = compile_jsx_component(
        Path::new("/project"),
        &card,
        &HashMap::new(),
        &Stylesheet::default(),
    );
    assert!(collect_text(&document.root).contains("<Missing />"));
}

// ── budgets ─────────────────────────────────────────────────────────────────

/// Depth of the deepest chain of boxes in the tree.
fn tree_depth(node: &PreviewNode) -> usize {
    match node {
        PreviewNode::Box(preview_box) => {
            1 + preview_box
                .children
                .iter()
                .map(tree_depth)
                .max()
                .unwrap_or(0)
        }
        _ => 1,
    }
}

#[test]
fn deeply_nested_markup_stays_within_the_node_budget() {
    let mut html = String::from("<html><body>");
    for _ in 0..5_000 {
        html.push_str("<div>");
    }
    html.push_str("deep");
    for _ in 0..5_000 {
        html.push_str("</div>");
    }
    html.push_str("</body></html>");
    let document = compile(&html);
    assert!(
        document.root.node_count() <= 1_600,
        "budget exceeded: {}",
        document.root.node_count()
    );
    // The budget alone does not bound recursion; without a depth cap this
    // input overflows the stack rather than producing a truncated tree.
    assert!(
        tree_depth(&document.root) <= 70,
        "nesting not bounded: {}",
        tree_depth(&document.root)
    );
}

// ── CSS Modules and friends ─────────────────────────────────────────────────

#[test]
fn css_module_member_access_becomes_a_class() {
    let component = JsxComponentSource {
        name: "Card".to_string(),
        path: "/project/src/Card.jsx".into(),
        line: 1,
        markup: "<div className={styles.card}>hi</div>".to_string(),
        stylesheets: Vec::new(),
    };
    // A module's source selector is the plain name; hashing happens later.
    let sheet = Stylesheet::parse(".card { background: #101418; }");
    let document =
        compile_jsx_component(Path::new("/project"), &component, &HashMap::new(), &sheet);
    let card = find_box(&document.root, "div").expect("card");
    assert_eq!(card.style.background, Some(Rgba::new(0x10, 0x14, 0x18, 255)));
}

#[test]
fn template_literals_combining_classes_all_apply() {
    let component = JsxComponentSource {
        name: "Card".to_string(),
        path: "/project/src/Card.jsx".into(),
        line: 1,
        markup: "<div className={`${styles.card} ${styles.active}`}>hi</div>".to_string(),
        stylesheets: Vec::new(),
    };
    let sheet = Stylesheet::parse(".card { padding: 8px; } .active { color: #00ff00; }");
    let document =
        compile_jsx_component(Path::new("/project"), &component, &HashMap::new(), &sheet);
    let card = find_box(&document.root, "div").expect("card");
    assert_eq!(card.style.padding.top, 8.);
    assert_eq!(card.style.color, Some(Rgba::new(0, 255, 0, 255)));
}

#[test]
fn clsx_style_calls_pick_up_literals_and_members() {
    let component = JsxComponentSource {
        name: "Card".to_string(),
        path: "/project/src/Card.jsx".into(),
        line: 1,
        markup: "<div className={clsx('base', styles.card)}>hi</div>".to_string(),
        stylesheets: Vec::new(),
    };
    let sheet = Stylesheet::parse(".base { padding: 4px; } .card { color: #0000ff; }");
    let document =
        compile_jsx_component(Path::new("/project"), &component, &HashMap::new(), &sheet);
    let card = find_box(&document.root, "div").expect("card");
    assert_eq!(card.style.padding.top, 4.);
    assert_eq!(card.style.color, Some(Rgba::new(0, 0, 255, 255)));
}

#[test]
fn a_plain_class_list_still_works() {
    let component = JsxComponentSource {
        name: "Card".to_string(),
        path: "/project/src/Card.jsx".into(),
        line: 1,
        markup: r#"<div className="card wide">hi</div>"#.to_string(),
        stylesheets: Vec::new(),
    };
    let sheet = Stylesheet::parse(".card { color: #ff0000; } .wide { padding: 12px; }");
    let document =
        compile_jsx_component(Path::new("/project"), &component, &HashMap::new(), &sheet);
    let card = find_box(&document.root, "div").expect("card");
    assert_eq!(card.style.color, Some(Rgba::new(255, 0, 0, 255)));
    assert_eq!(card.style.padding.top, 12.);
}

// ── CSS-in-JS ───────────────────────────────────────────────────────────────

#[test]
fn styled_components_carry_their_template_literal_css() {
    let source = r#"
        import styled from 'styled-components';
        const Button = styled.button`
          background: #0000ff;
          padding: 12px;
        `;
    "#;
    let components = extract_jsx_components(
        Path::new("/project"),
        Path::new("/project/src/Button.jsx"),
        source,
    );
    assert_eq!(components.len(), 1);
    assert_eq!(components[0].name, "Button");

    let document = compile_jsx_component(
        Path::new("/project"),
        &components[0],
        &HashMap::new(),
        &Stylesheet::default(),
    );
    let button = find_box(&document.root, "button").expect("button");
    assert_eq!(button.style.background, Some(Rgba::new(0, 0, 255, 255)));
    assert_eq!(button.style.padding.top, 12.);
}

#[test]
fn styled_interpolations_and_nested_states_are_dropped() {
    let source = r#"
        const Button = styled.button`
          color: #111111;
          background: ${props => props.bg};
          &:hover { color: #ff0000; }
        `;
    "#;
    let components = extract_jsx_components(
        Path::new("/project"),
        Path::new("/project/src/Button.jsx"),
        source,
    );
    let document = compile_jsx_component(
        Path::new("/project"),
        &components[0],
        &HashMap::new(),
        &Stylesheet::default(),
    );
    let button = find_box(&document.root, "button").expect("button");
    assert_eq!(button.style.color, Some(Rgba::new(0x11, 0x11, 0x11, 255)));
    // The prop-dependent background and the hover state must not be guessed at.
    assert_eq!(button.style.background, None);
}

// ── CDN stylesheets ─────────────────────────────────────────────────────────

#[test]
fn resolves_jsdelivr_urls_to_their_package() {
    assert_eq!(
        cdn_package_and_path(
            "https://cdn.jsdelivr.net/npm/bootstrap@5.3.3/dist/css/bootstrap.min.css"
        ),
        Some((
            "bootstrap".to_string(),
            "dist/css/bootstrap.min.css".to_string()
        ))
    );
}

#[test]
fn resolves_unpkg_urls() {
    assert_eq!(
        cdn_package_and_path("https://unpkg.com/bulma@0.9.4/css/bulma.min.css"),
        Some(("bulma".to_string(), "css/bulma.min.css".to_string()))
    );
}

#[test]
fn resolves_scoped_packages_without_eating_the_scope() {
    // The leading `@` is a scope, not a version.
    assert_eq!(
        cdn_package_and_path("https://cdn.jsdelivr.net/npm/@fortawesome/fontawesome-free@6.5.1/css/all.min.css"),
        Some((
            "@fortawesome/fontawesome-free".to_string(),
            "css/all.min.css".to_string()
        ))
    );
}

#[test]
fn resolves_an_unversioned_package() {
    assert_eq!(
        cdn_package_and_path("https://unpkg.com/bootstrap-icons/font/bootstrap-icons.css"),
        Some((
            "bootstrap-icons".to_string(),
            "font/bootstrap-icons.css".to_string()
        ))
    );
}

#[test]
fn query_strings_do_not_become_part_of_the_path() {
    assert_eq!(
        cdn_package_and_path("https://cdn.jsdelivr.net/npm/bootstrap@5.3.3/dist/css/bootstrap.css?v=2"),
        Some((
            "bootstrap".to_string(),
            "dist/css/bootstrap.css".to_string()
        ))
    );
}

#[test]
fn non_cdn_and_bare_package_urls_resolve_to_nothing() {
    assert_eq!(cdn_package_and_path("https://example.com/style.css"), None);
    assert_eq!(cdn_package_and_path("./style.css"), None);
    // No file named inside the package: nothing to read.
    assert_eq!(cdn_package_and_path("https://unpkg.com/bootstrap@5.3.3"), None);
}

// ── pseudo-elements ─────────────────────────────────────────────────────────

#[test]
fn a_scrollbar_pseudo_element_does_not_paint_its_host() {
    // Taken from a real stylesheet: treating `::-webkit-scrollbar-thumb` as its
    // host painted the whole sidebar red with 99px corners.
    let document = compile(
        r#"<html><head><style>
            #nav-content { background: #18283b; }
            #nav-content::-webkit-scrollbar-thumb {
                border-radius: 99px;
                background-color: #D62929;
            }
        </style></head><body><div id="nav-content">hi</div></body></html>"#,
    );
    let nav = find_box(&document.root, "div").expect("nav");
    assert_eq!(nav.style.background, Some(Rgba::new(0x18, 0x28, 0x3b, 255)));
    assert_eq!(nav.style.radius, 0.);
}

#[test]
fn before_and_after_never_style_their_host() {
    let document = compile(
        r#"<html><head><style>
            .badge { color: #111111; }
            .badge::before { color: #ff0000; content: "x"; }
            .badge::after { background: #ff0000; }
        </style></head><body><span class="badge">hi</span></body></html>"#,
    );
    let badge = find_box(&document.root, "span").expect("badge");
    assert_eq!(badge.style.color, Some(Rgba::new(0x11, 0x11, 0x11, 255)));
    assert_eq!(badge.style.background, None);
}

#[test]
fn single_colon_legacy_pseudo_elements_are_dropped_too() {
    let document = compile(
        r#"<html><head><style>
            .a { color: #111111; }
            .a:-webkit-scrollbar { background: #ff0000; }
        </style></head><body><div class="a">hi</div></body></html>"#,
    );
    let node = find_box(&document.root, "div").expect("div");
    assert_eq!(node.style.background, None);
}

#[test]
fn ordinary_structural_pseudo_classes_still_apply() {
    // `:first-child` describes position, not state, so dropping it would lose
    // styles the page really does show.
    let document = compile(
        r#"<html><head><style>
            li:first-child { color: #00ff00; }
        </style></head><body><ul><li>one</li></ul></body></html>"#,
    );
    let item = find_box(&document.root, "li").expect("li");
    assert_eq!(item.style.color, Some(Rgba::new(0, 255, 0, 255)));
}

#[test]
fn resolves_cdnjs_urls() {
    // cdnjs does not use npm specifiers; font-awesome is routinely loaded here.
    assert_eq!(
        cdn_package_and_path(
            "https://cdnjs.cloudflare.com/ajax/libs/font-awesome/5.15.3/css/all.min.css"
        ),
        Some(("font-awesome".to_string(), "css/all.min.css".to_string()))
    );
}

#[test]
fn a_blurred_element_is_drawn_faintly_rather_than_solid() {
    let document = compile(
        r#"<html><head><style>
            .glow { background: #de004b; filter: blur(20px); opacity: 0.5; }
        </style></head><body><div class="glow">x</div></body></html>"#,
    );
    let glow = find_box(&document.root, "div").expect("div");
    assert!(
        glow.style.opacity < 0.2,
        "blurred decoration drawn too strongly: {}",
        glow.style.opacity
    );
}

// ── custom properties reach fragments and components ────────────────────────

#[test]
fn a_fragment_inherits_the_root_custom_properties() {
    // The shape of a real stylesheet: every colour named by a variable on
    // `:root`. Compiling a fragment without them left `var(--…)` unresolved,
    // so the hovered element came out with no styling at all.
    let sheet = Stylesheet::parse(
        r#"
        :root { --navbar-dark-primary: #18283b; --navbar-light-primary: #f5f6fa; }
        #nav-bar { background: var(--navbar-dark-primary); color: var(--navbar-light-primary); }
        "#,
    );
    let document = compile_html_fragment(
        Path::new("/project"),
        Path::new("/project/index.html"),
        r#"<div id="nav-bar">hi</div>"#,
        &sheet,
    );
    let bar = find_box(&document.root, "div").expect("nav-bar");
    assert_eq!(bar.style.background, Some(Rgba::new(0x18, 0x28, 0x3b, 255)));
    assert_eq!(bar.style.color, Some(Rgba::new(0xf5, 0xf6, 0xfa, 255)));
}

#[test]
fn a_component_inherits_the_root_custom_properties() {
    let component = JsxComponentSource {
        name: "Card".to_string(),
        path: "/project/src/Card.tsx".into(),
        line: 1,
        markup: r#"<div className="card">hi</div>"#.to_string(),
        stylesheets: Vec::new(),
    };
    let sheet = Stylesheet::parse(
        ":root { --surface: #101418; } .card { background: var(--surface); }",
    );
    let document =
        compile_jsx_component(Path::new("/project"), &component, &HashMap::new(), &sheet);
    let card = find_box(&document.root, "div").expect("card");
    assert_eq!(card.style.background, Some(Rgba::new(0x10, 0x14, 0x18, 255)));
}

#[test]
fn a_fragment_variable_falls_back_when_the_root_never_defines_it() {
    let sheet = Stylesheet::parse(".a { color: var(--missing, #abcdef); }");
    let document = compile_html_fragment(
        Path::new("/project"),
        Path::new("/project/index.html"),
        r#"<div class="a">hi</div>"#,
        &sheet,
    );
    let node = find_box(&document.root, "div").expect("div");
    assert_eq!(node.style.color, Some(Rgba::new(0xab, 0xcd, 0xef, 255)));
}

// ── flex growth ─────────────────────────────────────────────────────────────

#[test]
fn a_growing_child_is_not_dropped_when_the_parent_has_no_height() {
    // The sidebar shape: `height: calc(…)` is a length the preview cannot
    // resolve, so the column is content-sized. A `flex: 1` child there has no
    // leftover space to take and collapsed, taking the whole nav body with it.
    let document = compile(
        r#"<html><head><style>
            #nav-bar { display: flex; flex-direction: column; height: calc(100% - 2vw); }
            #nav-content { flex: 1; }
        </style></head><body>
            <div id="nav-bar"><div id="nav-content">Dashboard</div></div>
        </body></html>"#,
    );
    let bar = find_box(&document.root, "div").expect("nav-bar");
    let content = bar
        .children
        .iter()
        .find_map(|child| find_box(child, "div"))
        .expect("nav-content");
    assert_eq!(content.style.grow, 0.);
    assert!(collect_text(&document.root).contains("Dashboard"));
}

#[test]
fn a_growing_child_keeps_growing_when_the_parent_is_sized() {
    let document = compile(
        r#"<html><head><style>
            .col { display: flex; flex-direction: column; height: 400px; }
            .body { flex: 1; }
        </style></head><body>
            <div class="col"><div class="body">hi</div></div>
        </body></html>"#,
    );
    let column = find_box(&document.root, "div").expect("col");
    let body = column
        .children
        .iter()
        .find_map(|child| find_box(child, "div"))
        .expect("body");
    assert_eq!(body.style.grow, 1.);
}

#[test]
fn a_row_uses_its_width_to_decide_whether_growth_is_satisfiable() {
    let document = compile(
        r#"<html><head><style>
            .row { display: flex; flex-direction: row; width: 600px; }
            .fill { flex: 1; }
        </style></head><body><div class="row"><div class="fill">hi</div></div></body></html>"#,
    );
    let row = find_box(&document.root, "div").expect("row");
    let fill = row
        .children
        .iter()
        .find_map(|child| find_box(child, "div"))
        .expect("fill");
    assert_eq!(fill.style.grow, 1.);
}

// ── JSX expressions ─────────────────────────────────────────────────────────

fn compile_markup(markup: &str, css: &str) -> PreviewDocument {
    let component = JsxComponentSource {
        name: "X".to_string(),
        path: "/p/src/X.tsx".into(),
        line: 1,
        markup: markup.to_string(),
        stylesheets: Vec::new(),
    };
    compile_jsx_component(
        Path::new("/p"),
        &component,
        &HashMap::new(),
        &Stylesheet::parse(css),
    )
}

#[test]
fn a_jsx_comment_is_not_rendered() {
    // Taken from a real navbar: a commented-out block was being printed as a
    // chip of source, putting dead markup on screen.
    let document = compile_markup(
        r#"<nav>
             {/* <div className="flex items-center gap-4">
                   <span>Dashboard</span>
                 </div> */}
             <span>Corressay</span>
           </nav>"#,
        "",
    );
    let text = collect_text(&document.root);
    assert!(text.contains("Corressay"));
    assert!(!text.contains("className"), "comment leaked: {text}");
    assert!(!text.contains("Dashboard"), "comment leaked: {text}");
}

#[test]
fn a_conditional_renders_the_markup_it_guards() {
    let document = compile_markup(
        r#"<div>{showDropdown && (<button className="item">Sair</button>)}</div>"#,
        ".item { color: #00ff00; }",
    );
    assert!(collect_text(&document.root).contains("Sair"));
    let button = find_box(&document.root, "button").expect("button");
    assert_eq!(button.style.color, Some(Rgba::new(0, 255, 0, 255)));
}

#[test]
fn a_ternary_renders_one_of_its_branches() {
    let document = compile_markup(
        "<div>{loading ? <span>Carregando</span> : <span>Pronto</span>}</div>",
        "",
    );
    let text = collect_text(&document.root);
    assert!(text.contains("Carregando"), "got {text}");
}

#[test]
fn a_map_renders_one_row() {
    let document = compile_markup(
        "<ul>{items.map((item) => (<li key={item.id}>Item</li>))}</ul>",
        "",
    );
    assert!(collect_text(&document.root).contains("Item"));
}

#[test]
fn a_comparison_is_not_mistaken_for_markup() {
    // `a < b` must not be read as a tag.
    let document = compile_markup("<div>{count < limit}</div>", "");
    let text = collect_text(&document.root);
    assert!(text.contains("count"), "expected a placeholder, got {text}");
}

#[test]
fn a_plain_value_expression_still_shows_as_a_placeholder() {
    let document = compile_markup("<span>{userName}</span>", "");
    assert!(collect_text(&document.root).contains("userName"));
}

#[test]
fn a_fallback_literal_renders_as_text() {
    let document = compile_markup("<span>{userName || 'Usuário'}</span>", "");
    let text = collect_text(&document.root);
    assert!(text.contains("Usuário"), "got {text}");
    assert!(!text.contains("userName"), "raw source leaked: {text}");
}

// ── inline SVG ──────────────────────────────────────────────────────────────

fn svg_source(node: &PreviewNode) -> Option<&str> {
    match node {
        PreviewNode::Svg { source, .. } => Some(source),
        PreviewNode::Box(preview_box) => preview_box.children.iter().find_map(svg_source),
        _ => None,
    }
}

#[test]
fn an_inline_svg_is_kept_as_renderable_source() {
    let document = compile(
        r#"<html><body><div><svg viewBox="0 0 24 24"><path d="M0 0h24"></path></svg></div></body></html>"#,
    );
    let svg = svg_source(&document.root).expect("svg kept");
    assert!(svg.starts_with("<svg"), "got {svg}");
    assert!(svg.contains("viewBox=\"0 0 24 24\""), "viewBox lost: {svg}");
    assert!(svg.contains("<path"), "got {svg}");
    assert!(svg.contains("xmlns="), "a standalone svg needs its namespace: {svg}");
}

#[test]
fn jsx_camel_case_svg_names_are_restored() {
    // JSX writes `strokeWidth` and `clipPath`; SVG needs `stroke-width` and
    // `clipPath`. Serialising the flattened names produces markup a rasteriser
    // silently ignores.
    let component = JsxComponentSource {
        name: "Logo".to_string(),
        path: "/p/src/Logo.tsx".into(),
        line: 1,
        markup: r#"<svg viewBox="0 0 200 200"><defs><clipPath id="e"><rect height={30} width={30} /></clipPath></defs><circle strokeWidth={2} strokeLinecap="round" r={70} /></svg>"#.to_string(),
        stylesheets: Vec::new(),
    };
    let document = compile_jsx_component(
        Path::new("/p"),
        &component,
        &HashMap::new(),
        &Stylesheet::default(),
    );
    let svg = svg_source(&document.root).expect("svg kept");
    assert!(svg.contains("stroke-width=\"2\""), "got {svg}");
    assert!(svg.contains("stroke-linecap=\"round\""), "got {svg}");
    assert!(svg.contains("<clipPath"), "got {svg}");
    assert!(svg.contains("viewBox="), "got {svg}");
}

#[test]
fn current_color_is_resolved_against_the_element() {
    // A standalone rasteriser has no notion of `currentColor`.
    let document = compile(
        r#"<html><head><style>.icon { color: #ff0000; }</style></head>
           <body><svg class="icon"><path stroke="currentColor" d="M0 0"/></svg></body></html>"#,
    );
    let svg = svg_source(&document.root).expect("svg kept");
    assert!(svg.contains("#ff0000"), "currentColor not resolved: {svg}");
    assert!(!svg.contains("currentColor"), "got {svg}");
}

// ── box-shadow ──────────────────────────────────────────────────────────────

#[test]
fn a_box_shadow_is_resolved() {
    let document = compile(
        r#"<html><head><style>
            .card { box-shadow: 0 4px 12px rgba(0,0,0,0.1); }
        </style></head><body><div class="card">hi</div></body></html>"#,
    );
    let card = find_box(&document.root, "div").expect("card");
    let shadow = card.style.shadow.expect("a shadow");
    assert_eq!(shadow.offset_x, 0.);
    assert_eq!(shadow.offset_y, 4.);
    assert_eq!(shadow.blur, 12.);
    assert_eq!(shadow.color, Rgba::new(0, 0, 0, 25));
}

#[test]
fn a_spread_radius_is_kept() {
    let document = compile(
        r#"<html><head><style>.a { box-shadow: 1px 2px 3px 4px #000000; }</style></head>
           <body><div class="a">hi</div></body></html>"#,
    );
    let node = find_box(&document.root, "div").expect("div");
    let shadow = node.style.shadow.expect("a shadow");
    assert_eq!(shadow.spread, 4.);
}

#[test]
fn inset_and_none_shadows_are_not_drawn() {
    for value in ["inset 0 2px 4px #000000", "none"] {
        let document = compile(&format!(
            r#"<html><head><style>.a {{ box-shadow: {value}; }}</style></head>
               <body><div class="a">hi</div></body></html>"#
        ));
        let node = find_box(&document.root, "div").expect("div");
        assert!(node.style.shadow.is_none(), "{value} should not draw");
    }
}

#[test]
fn only_the_first_of_a_shadow_list_is_used() {
    let document = compile(
        r#"<html><head><style>
            .a { box-shadow: 0 1px 2px #ff0000, 0 8px 24px #00ff00; }
        </style></head><body><div class="a">hi</div></body></html>"#,
    );
    let node = find_box(&document.root, "div").expect("div");
    let shadow = node.style.shadow.expect("a shadow");
    assert_eq!(shadow.color, Rgba::new(255, 0, 0, 255));
}

// ── a real hovered element (Competitivo.tsx) ────────────────────────────────

#[test]
fn a_real_tailwind_element_resolves_its_utilities() {
    let markup = r#"<div className="text-center mb-6 md:mb-10">
        <h1 className="text-[24px] md:text-[32px] font-bold text-[#1a1a1a] mb-1 md:mb-2">Melhores Redacoes</h1>
        <p className="text-sm md:text-base text-[#666]">Ranking dos melhores escritores</p>
    </div>"#;
    let document = compile_markup(markup, "");

    let outer = find_box(&document.root, "div").expect("div");
    assert_eq!(outer.style.text_align, TextAlign::Center);
    assert_eq!(outer.style.margin.bottom, 40.); // md:mb-10 wins over mb-6

    let h1 = find_box(&document.root, "h1").expect("h1");
    assert!(h1.style.bold);
    assert_eq!(h1.style.color, Some(Rgba::new(0x1a, 0x1a, 0x1a, 255)));
    // The arbitrary font size must come from `text-[32px]`, not the h1 default.
    assert_eq!(h1.style.font_size, 32.);
}

#[test]
fn an_arbitrary_text_value_is_a_font_size_when_it_is_a_length() {
    let document = compile_markup(r#"<span className="text-[14px]">hi</span>"#, "");
    let span = find_box(&document.root, "span").expect("span");
    assert_eq!(span.style.font_size, 14.);
    // …and still a colour when it is one.
    let document = compile_markup(r#"<span className="text-[#00ff00]">hi</span>"#, "");
    let span = find_box(&document.root, "span").expect("span");
    assert_eq!(span.style.color, Some(Rgba::new(0, 255, 0, 255)));
}
