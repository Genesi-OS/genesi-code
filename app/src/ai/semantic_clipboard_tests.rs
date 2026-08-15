use std::fs;

use tempfile::tempdir;

use super::*;

/// A small React project: a component typed against an imported interface,
/// styled with a class defined in a stylesheet.
fn scaffold() -> tempfile::TempDir {
    let dir = tempdir().expect("temp dir");
    fs::create_dir_all(dir.path().join("src/types")).expect("create types dir");
    fs::create_dir_all(dir.path().join("src/styles")).expect("create styles dir");

    fs::write(
        dir.path().join("src/types/cart.ts"),
        r#"export interface CartItem {
  id: string;
  name: string;
  price: number;
}

export type Currency = "BRL" | "USD";

export const TAX_RATE = 0.1;
"#,
    )
    .expect("write types");

    fs::write(
        dir.path().join("src/styles/cart.css"),
        r#".cart-row {
  display: flex;
  gap: 8px;
}

.unused-thing {
  color: red;
}
"#,
    )
    .expect("write styles");

    fs::write(
        dir.path().join("src/Cart.tsx"),
        r#"import { CartItem, Currency } from "./types/cart";
import "./styles/cart.css";

interface CartProps {
  items: CartItem[];
}

export function Cart({ items }: CartProps) {
  return (
    <div className="cart-row">
      {items.map((item: CartItem) => (
        <span key={item.id}>{item.name}</span>
      ))}
    </div>
  );
}
"#,
    )
    .expect("write component");
    dir
}

const SELECTION: &str = r#"export function Cart({ items }: CartProps) {
  return (
    <div className="cart-row">
      {items.map((item: CartItem) => (
        <span key={item.id}>{item.name}</span>
      ))}
    </div>
  );
}"#;

fn names_of(resolved: &ResolvedSelection) -> Vec<String> {
    resolved
        .definitions
        .iter()
        .map(|definition| definition.name.clone())
        .collect()
}

#[test]
fn an_imported_interface_is_resolved_through_the_import() {
    let dir = scaffold();
    let resolved = resolve(dir.path(), Path::new("src/Cart.tsx"), SELECTION, 9);

    let cart_item = resolved
        .definitions
        .iter()
        .find(|definition| definition.name == "CartItem")
        .expect("CartItem resolved");
    assert_eq!(cart_item.kind, DefinitionKind::Interface);
    assert_eq!(cart_item.source, PathBuf::from("src/types/cart.ts"));
    assert!(cart_item.text.contains("price: number;"), "{}", cart_item.text);
    // The whole interface body, and nothing after it.
    assert!(!cart_item.text.contains("Currency"), "{}", cart_item.text);
}

#[test]
fn an_interface_declared_in_the_same_file_is_resolved() {
    let dir = scaffold();
    let resolved = resolve(dir.path(), Path::new("src/Cart.tsx"), SELECTION, 9);
    let props = resolved
        .definitions
        .iter()
        .find(|definition| definition.name == "CartProps")
        .expect("CartProps resolved");
    assert_eq!(props.source, PathBuf::from("src/Cart.tsx"));
    assert!(props.text.contains("items: CartItem[];"), "{}", props.text);
}

#[test]
fn a_referenced_css_class_is_resolved_and_unused_ones_are_not() {
    let dir = scaffold();
    let resolved = resolve(dir.path(), Path::new("src/Cart.tsx"), SELECTION, 9);

    let rule = resolved
        .definitions
        .iter()
        .find(|definition| definition.name == ".cart-row")
        .expect("the class the selection actually uses");
    assert_eq!(rule.kind, DefinitionKind::CssRule);
    assert!(rule.text.contains("display: flex;"), "{}", rule.text);

    assert!(
        !names_of(&resolved).iter().any(|name| name == ".unused-thing"),
        "a class the selection never mentions must not be pulled in"
    );
}

#[test]
fn imports_the_selection_does_not_use_are_left_out() {
    let dir = scaffold();
    // `Currency` is imported by the file but never appears in this selection.
    let resolved = resolve(dir.path(), Path::new("src/Cart.tsx"), SELECTION, 9);
    assert!(
        !names_of(&resolved).iter().any(|name| name == "Currency"),
        "resolution follows the SELECTION, not the file's whole import list: {:?}",
        names_of(&resolved)
    );
}

#[test]
fn a_renamed_import_is_resolved_under_its_local_name() {
    let dir = scaffold();
    fs::write(
        dir.path().join("src/Renamed.tsx"),
        "import { CartItem as LineItem } from \"./types/cart\";\n\nexport function Row(item: LineItem) { return null; }\n",
    )
    .expect("write file");

    let resolved = resolve(
        dir.path(),
        Path::new("src/Renamed.tsx"),
        "export function Row(item: LineItem) { return null; }",
        3,
    );
    // The local name is what the selection says, so that is what must resolve —
    // to the definition behind it.
    let found = resolved
        .definitions
        .iter()
        .find(|definition| definition.name == "LineItem");
    assert!(
        found.is_some(),
        "expected LineItem to resolve, got {:?}",
        names_of(&resolved)
    );
}

#[test]
fn bare_module_specifiers_are_never_followed() {
    // Following `react` would mean pasting node_modules into a chat.
    let dir = tempdir().expect("temp dir");
    assert!(resolve_module(dir.path(), Path::new("src/a.tsx"), "react").is_empty());
    assert!(resolve_module(dir.path(), Path::new("src/a.tsx"), "@scope/pkg").is_empty());
}

#[test]
fn relative_module_paths_resolve_across_the_usual_shapes() {
    let dir = tempdir().expect("temp dir");
    fs::create_dir_all(dir.path().join("src/lib/nested")).expect("create dirs");
    fs::write(dir.path().join("src/lib/util.ts"), "export const a = 1;\n").expect("write");
    fs::write(
        dir.path().join("src/lib/nested/index.tsx"),
        "export const b = 2;\n",
    )
    .expect("write");

    // Extensionless sibling.
    assert_eq!(
        resolve_module(dir.path(), Path::new("src/app.tsx"), "./lib/util"),
        vec![PathBuf::from("src/lib/util.ts")]
    );
    // Directory with an index file.
    assert_eq!(
        resolve_module(dir.path(), Path::new("src/app.tsx"), "./lib/nested"),
        vec![PathBuf::from("src/lib/nested/index.tsx")]
    );
    // Parent traversal.
    assert_eq!(
        resolve_module(dir.path(), Path::new("src/deep/app.tsx"), "../lib/util"),
        vec![PathBuf::from("src/lib/util.ts")]
    );
}

#[test]
fn a_one_line_type_alias_is_captured_whole_without_running_on() {
    let dir = tempdir().expect("temp dir");
    fs::write(
        dir.path().join("types.ts"),
        "export type Id = string;\nexport type Other = number;\n",
    )
    .expect("write");

    let resolved = resolve(dir.path(), Path::new("types.ts"), "const x: Id = 'a';", 1);
    let id = resolved
        .definitions
        .iter()
        .find(|definition| definition.name == "Id")
        .expect("Id resolved");
    assert_eq!(id.text, "export type Id = string;");
    assert!(!id.text.contains("Other"), "ran on into the next line");
}

#[test]
fn names_that_are_not_in_the_project_are_reported_rather_than_hidden() {
    let dir = scaffold();
    let resolved = resolve(
        dir.path(),
        Path::new("src/Cart.tsx"),
        "const widget: SomethingNobodyDefined = build();",
        1,
    );
    assert!(
        resolved
            .unresolved
            .iter()
            .any(|name| name == "SomethingNobodyDefined"),
        "got {:?}",
        resolved.unresolved
    );
}

#[test]
fn the_clipboard_payload_carries_the_selection_and_its_definitions() {
    let dir = scaffold();
    let resolved = resolve(dir.path(), Path::new("src/Cart.tsx"), SELECTION, 9);
    let text = resolved.to_clipboard_text();

    assert!(text.contains("src/Cart.tsx:9"), "{text}");
    assert!(text.contains("export function Cart"), "{text}");
    assert!(text.contains("Referenced definitions"), "{text}");
    assert!(text.contains("interface CartItem"), "{text}");
    assert!(text.contains("price: number;"), "{text}");
    assert!(text.contains("display: flex;"), "{text}");
    // Fenced so a chat renders it as code.
    assert!(text.contains("```tsx"), "{text}");
    assert!(text.contains("```css"), "{text}");
}

#[test]
fn a_selection_that_references_nothing_copies_exactly_as_it_was() {
    let dir = scaffold();
    let plain = "const total = 1 + 2;";
    let resolved = resolve(dir.path(), Path::new("src/Cart.tsx"), plain, 1);
    assert!(!resolved.is_enriched());
    // No definitions means no wrapper: a plain copy must stay a plain copy.
    assert_eq!(resolved.to_clipboard_text(), plain);
}

#[test]
fn an_empty_selection_resolves_to_nothing() {
    let dir = scaffold();
    let resolved = resolve(dir.path(), Path::new("src/Cart.tsx"), "   ", 1);
    assert!(resolved.definitions.is_empty());
    assert!(resolved.unresolved.is_empty());
}

#[test]
fn common_noise_is_never_resolved() {
    let dir = scaffold();
    let resolved = resolve(
        dir.path(),
        Path::new("src/Cart.tsx"),
        "if (React && Boolean(x)) { return String(y); }",
        1,
    );
    for noise in ["React", "String", "Boolean"] {
        assert!(
            !names_of(&resolved).iter().any(|name| name == noise),
            "{noise} should not be resolved"
        );
    }
}
