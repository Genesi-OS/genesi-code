use super::*;

fn one(class: &str) -> Option<(String, String)> {
    let mut declarations = declarations_for_class(class);
    (declarations.len() == 1).then(|| declarations.remove(0))
}

fn value(class: &str, property: &str) -> Option<String> {
    declarations_for_class(class)
        .into_iter()
        .find(|(name, _)| name == property)
        .map(|(_, value)| value)
}

#[test]
fn maps_layout_utilities() {
    assert_eq!(one("flex"), Some(("display".into(), "flex".into())));
    assert_eq!(one("flex-col"), Some(("flex-direction".into(), "column".into())));
    assert_eq!(one("hidden"), Some(("display".into(), "none".into())));
    assert_eq!(
        one("justify-between"),
        Some(("justify-content".into(), "space-between".into()))
    );
    assert_eq!(one("items-center"), Some(("align-items".into(), "center".into())));
}

#[test]
fn maps_the_spacing_scale() {
    assert_eq!(value("p-4", "padding").as_deref(), Some("16px"));
    assert_eq!(value("gap-2", "gap").as_deref(), Some("8px"));
    assert_eq!(value("mt-0", "margin-top").as_deref(), Some("0px"));
    assert_eq!(value("px-3", "padding-left").as_deref(), Some("12px"));
    assert_eq!(value("px-3", "padding-right").as_deref(), Some("12px"));
}

#[test]
fn maps_fractional_and_keyword_sizes() {
    assert_eq!(value("w-1/2", "width").as_deref(), Some("50%"));
    assert_eq!(value("w-full", "width").as_deref(), Some("100%"));
    assert_eq!(value("h-px", "height").as_deref(), Some("1px"));
    assert_eq!(value("w-5", "width").as_deref(), Some("20px"));
}

#[test]
fn maps_type_utilities() {
    assert_eq!(value("text-base", "font-size").as_deref(), Some("16px"));
    assert_eq!(value("font-semibold", "font-weight").as_deref(), Some("600"));
    assert_eq!(value("uppercase", "text-transform").as_deref(), Some("uppercase"));
}

#[test]
fn maps_palette_colors() {
    assert_eq!(value("text-gray-200", "color").as_deref(), Some("#e5e7eb"));
    assert_eq!(value("bg-blue-500", "background-color").as_deref(), Some("#3b82f6"));
    assert_eq!(value("text-white", "color").as_deref(), Some("#ffffff"));
    assert_eq!(value("border-red-600", "border-color").as_deref(), Some("#dc2626"));
}

#[test]
fn maps_arbitrary_values() {
    // The exact form in the user's markup: `text-[#61dafb]`.
    assert_eq!(value("text-[#61dafb]", "color").as_deref(), Some("#61dafb"));
    assert_eq!(value("w-[32px]", "width").as_deref(), Some("32px"));
    assert_eq!(
        value("bg-[rgb(1,2,3)]", "background-color").as_deref(),
        Some("rgb(1,2,3)")
    );
}

#[test]
fn responsive_prefixes_apply_but_state_variants_do_not() {
    // The sheet is desktop width, so `md:` styles are the ones in effect.
    assert_eq!(value("md:flex", "display").as_deref(), Some("flex"));
    // A hover style must never leak into the resting render.
    assert!(declarations_for_class("hover:bg-blue-500").is_empty());
    assert!(declarations_for_class("dark:text-white").is_empty());
}

#[test]
fn unknown_classes_yield_nothing() {
    assert!(declarations_for_class("my-custom-class").is_empty());
    assert!(declarations_for_class("").is_empty());
    assert!(declarations_for_class("text-notacolor-999").is_empty());
}

#[test]
fn rounded_and_border_shorthands() {
    assert_eq!(value("rounded-lg", "border-radius").as_deref(), Some("8px"));
    assert_eq!(value("rounded-full", "border-radius").as_deref(), Some("9999px"));
    assert_eq!(value("border", "border-width").as_deref(), Some("1px"));
}

#[test]
fn opacity_becomes_a_fraction() {
    assert_eq!(value("opacity-50", "opacity").as_deref(), Some("0.5"));
}
