//! Tailwind utility classes, resolved to ordinary declarations.
//!
//! Tailwind emits its CSS at build time, and with JIT the rules for a given
//! class may exist in no file the preview can read — so a component styled
//! entirely with utilities would otherwise render unstyled. Translating the
//! utilities directly is what makes those components look right.
//!
//! The mapping is applied *before* the project's own stylesheets, so real CSS
//! (including a compiled `output.css`, which the preview still reads when it
//! finds one) always wins.

/// Tailwind's spacing scale: `p-4` is `4 * 0.25rem`.
const SPACING_STEP: f32 = 4.;

/// Declarations for one class, or empty when it is not a utility we know.
pub fn declarations_for_class(class: &str) -> Vec<(String, String)> {
    // Variants (`hover:`, `md:`, `dark:`) describe a state or viewport the
    // preview is not in. Responsive prefixes are the exception: the sheet is
    // desktop-width, so the largest matching one should win — but applying
    // them all in order gets the same result far more simply.
    let bare = match class.rsplit_once(':') {
        Some((prefix, rest)) => {
            if is_responsive_prefix(prefix) {
                rest
            } else {
                return Vec::new();
            }
        }
        None => class,
    };

    let declaration = |property: &str, value: String| vec![(property.to_string(), value)];

    // Arbitrary values: `text-[#61dafb]`, `w-[32px]`, `bg-[rgb(1,2,3)]`.
    if let Some((head, value)) = arbitrary_value(bare) {
        return match head {
            "text" => declaration("color", value),
            "bg" => declaration("background-color", value),
            "border" => declaration("border-color", value),
            "w" => declaration("width", value),
            "h" => declaration("height", value),
            "min-h" => declaration("min-height", value),
            "max-w" => declaration("max-width", value),
            "p" => declaration("padding", value),
            "m" => declaration("margin", value),
            "gap" => declaration("gap", value),
            "rounded" => declaration("border-radius", value),
            "leading" => declaration("line-height", value),
            _ => Vec::new(),
        };
    }

    match bare {
        // ── display and flow ────────────────────────────────────────────────
        "flex" => return declaration("display", "flex".into()),
        "inline-flex" => return declaration("display", "flex".into()),
        "grid" => return declaration("display", "grid".into()),
        "block" => return declaration("display", "block".into()),
        "inline-block" => return declaration("display", "inline-block".into()),
        "inline" => return declaration("display", "inline".into()),
        "hidden" => return declaration("display", "none".into()),
        "flex-row" => return declaration("flex-direction", "row".into()),
        "flex-col" => return declaration("flex-direction", "column".into()),
        "flex-1" | "grow" | "flex-grow" => return declaration("flex", "1".into()),

        // ── alignment ───────────────────────────────────────────────────────
        "justify-start" => return declaration("justify-content", "flex-start".into()),
        "justify-center" => return declaration("justify-content", "center".into()),
        "justify-end" => return declaration("justify-content", "flex-end".into()),
        "justify-between" => return declaration("justify-content", "space-between".into()),
        "justify-around" => return declaration("justify-content", "space-around".into()),
        "justify-evenly" => return declaration("justify-content", "space-evenly".into()),
        "items-start" => return declaration("align-items", "flex-start".into()),
        "items-center" => return declaration("align-items", "center".into()),
        "items-end" => return declaration("align-items", "flex-end".into()),
        "items-stretch" => return declaration("align-items", "stretch".into()),
        "text-left" => return declaration("text-align", "left".into()),
        "text-center" => return declaration("text-align", "center".into()),
        "text-right" => return declaration("text-align", "right".into()),
        "mx-auto" => return vec![("margin-left".into(), "auto".into()), ("margin-right".into(), "auto".into())],

        // ── type ────────────────────────────────────────────────────────────
        "text-xs" => return declaration("font-size", "12px".into()),
        "text-sm" => return declaration("font-size", "14px".into()),
        "text-base" => return declaration("font-size", "16px".into()),
        "text-lg" => return declaration("font-size", "18px".into()),
        "text-xl" => return declaration("font-size", "20px".into()),
        "text-2xl" => return declaration("font-size", "24px".into()),
        "text-3xl" => return declaration("font-size", "30px".into()),
        "text-4xl" => return declaration("font-size", "36px".into()),
        "text-5xl" => return declaration("font-size", "48px".into()),
        "text-6xl" => return declaration("font-size", "60px".into()),
        "font-thin" => return declaration("font-weight", "100".into()),
        "font-light" => return declaration("font-weight", "300".into()),
        "font-normal" => return declaration("font-weight", "400".into()),
        "font-medium" => return declaration("font-weight", "500".into()),
        "font-semibold" => return declaration("font-weight", "600".into()),
        "font-bold" => return declaration("font-weight", "700".into()),
        "font-extrabold" => return declaration("font-weight", "800".into()),
        "font-black" => return declaration("font-weight", "900".into()),
        "font-mono" => return declaration("font-family", "monospace".into()),
        "italic" => return declaration("font-style", "italic".into()),
        "underline" => return declaration("text-decoration", "underline".into()),
        "uppercase" => return declaration("text-transform", "uppercase".into()),

        // ── borders and shape ───────────────────────────────────────────────
        "border" => return declaration("border-width", "1px".into()),
        "border-0" => return declaration("border-width", "0".into()),
        "border-2" => return declaration("border-width", "2px".into()),
        "border-4" => return declaration("border-width", "4px".into()),
        "rounded-none" => return declaration("border-radius", "0".into()),
        "rounded-sm" => return declaration("border-radius", "2px".into()),
        "rounded" => return declaration("border-radius", "4px".into()),
        "rounded-md" => return declaration("border-radius", "6px".into()),
        "rounded-lg" => return declaration("border-radius", "8px".into()),
        "rounded-xl" => return declaration("border-radius", "12px".into()),
        "rounded-2xl" => return declaration("border-radius", "16px".into()),
        "rounded-3xl" => return declaration("border-radius", "24px".into()),
        "rounded-full" => return declaration("border-radius", "9999px".into()),

        // ── sizing keywords ─────────────────────────────────────────────────
        "w-full" => return declaration("width", "100%".into()),
        "h-full" => return declaration("height", "100%".into()),
        "w-screen" => return declaration("width", "100vw".into()),
        "h-screen" => return declaration("height", "100vh".into()),
        "min-h-screen" => return declaration("min-height", "100vh".into()),
        "w-auto" | "h-auto" => return Vec::new(),
        _ => {}
    }

    // ── scale-driven utilities ──────────────────────────────────────────────
    if let Some(rest) = bare.strip_prefix("opacity-") {
        if let Ok(percent) = rest.parse::<f32>() {
            return declaration("opacity", format!("{}", percent / 100.));
        }
    }
    for (prefix, properties) in [
        ("p-", &["padding"][..]),
        ("px-", &["padding-left", "padding-right"][..]),
        ("py-", &["padding-top", "padding-bottom"][..]),
        ("pt-", &["padding-top"][..]),
        ("pr-", &["padding-right"][..]),
        ("pb-", &["padding-bottom"][..]),
        ("pl-", &["padding-left"][..]),
        ("m-", &["margin"][..]),
        ("mx-", &["margin-left", "margin-right"][..]),
        ("my-", &["margin-top", "margin-bottom"][..]),
        ("mt-", &["margin-top"][..]),
        ("mr-", &["margin-right"][..]),
        ("mb-", &["margin-bottom"][..]),
        ("ml-", &["margin-left"][..]),
        ("gap-", &["gap"][..]),
        ("w-", &["width"][..]),
        ("h-", &["height"][..]),
        ("min-w-", &["min-width"][..]),
        ("min-h-", &["min-height"][..]),
        ("max-w-", &["max-width"][..]),
    ] {
        let Some(rest) = bare.strip_prefix(prefix) else {
            continue;
        };
        let Some(pixels) = spacing_value(rest) else {
            continue;
        };
        return properties
            .iter()
            .map(|property| (property.to_string(), pixels.clone()))
            .collect();
    }

    // ── palette utilities ───────────────────────────────────────────────────
    for (prefix, property) in [
        ("text-", "color"),
        ("bg-", "background-color"),
        ("border-", "border-color"),
    ] {
        let Some(rest) = bare.strip_prefix(prefix) else {
            continue;
        };
        if let Some(color) = palette_color(rest) {
            return declaration(property, color.to_string());
        }
    }

    Vec::new()
}

fn is_responsive_prefix(prefix: &str) -> bool {
    matches!(prefix, "sm" | "md" | "lg" | "xl" | "2xl")
}

/// `text-[#61dafb]` → `("text", "#61dafb")`. Underscores stand for spaces in
/// Tailwind's arbitrary-value syntax.
fn arbitrary_value(class: &str) -> Option<(&str, String)> {
    let open = class.find("-[")?;
    let value = class[open + 2..].strip_suffix(']')?;
    Some((&class[..open], value.replace('_', " ")))
}

/// `4` → `16px`, `0.5` → `2px`, plus the fraction and keyword forms.
fn spacing_value(rest: &str) -> Option<String> {
    match rest {
        "px" => return Some("1px".to_string()),
        "full" => return Some("100%".to_string()),
        "auto" => return None,
        _ => {}
    }
    if let Some((numerator, denominator)) = rest.split_once('/') {
        let numerator = numerator.parse::<f32>().ok()?;
        let denominator = denominator.parse::<f32>().ok()?;
        if denominator == 0. {
            return None;
        }
        return Some(format!("{}%", numerator / denominator * 100.));
    }
    let steps = rest.parse::<f32>().ok()?;
    Some(format!("{}px", steps * SPACING_STEP))
}

/// `gray-200` → the hex from Tailwind's default palette. `white`/`black`/
/// `transparent` are the keyword entries.
fn palette_color(rest: &str) -> Option<&'static str> {
    match rest {
        "white" => return Some("#ffffff"),
        "black" => return Some("#000000"),
        "transparent" => return Some("transparent"),
        "current" | "inherit" => return None,
        _ => {}
    }
    let (family, shade) = rest.rsplit_once('-')?;
    let shades = PALETTE
        .iter()
        .find(|(name, _)| *name == family)
        .map(|(_, shades)| shades)?;
    let index = match shade {
        "50" => 0,
        "100" => 1,
        "200" => 2,
        "300" => 3,
        "400" => 4,
        "500" => 5,
        "600" => 6,
        "700" => 7,
        "800" => 8,
        "900" => 9,
        "950" => 10,
        _ => return None,
    };
    Some(shades[index])
}

/// Tailwind's default palette, shades 50 → 950.
///
/// A project with a customised theme will have its generated CSS read as an
/// ordinary stylesheet, and that overrides these.
#[rustfmt::skip]
const PALETTE: &[(&str, [&str; 11])] = &[
    ("slate",   ["#f8fafc","#f1f5f9","#e2e8f0","#cbd5e1","#94a3b8","#64748b","#475569","#334155","#1e293b","#0f172a","#020617"]),
    ("gray",    ["#f9fafb","#f3f4f6","#e5e7eb","#d1d5db","#9ca3af","#6b7280","#4b5563","#374151","#1f2937","#111827","#030712"]),
    ("zinc",    ["#fafafa","#f4f4f5","#e4e4e7","#d4d4d8","#a1a1aa","#71717a","#52525b","#3f3f46","#27272a","#18181b","#09090b"]),
    ("neutral", ["#fafafa","#f5f5f5","#e5e5e5","#d4d4d4","#a3a3a3","#737373","#525252","#404040","#262626","#171717","#0a0a0a"]),
    ("stone",   ["#fafaf9","#f5f5f4","#e7e5e4","#d6d3d1","#a8a29e","#78716c","#57534e","#44403c","#292524","#1c1917","#0c0a09"]),
    ("red",     ["#fef2f2","#fee2e2","#fecaca","#fca5a5","#f87171","#ef4444","#dc2626","#b91c1c","#991b1b","#7f1d1d","#450a0a"]),
    ("orange",  ["#fff7ed","#ffedd5","#fed7aa","#fdba74","#fb923c","#f97316","#ea580c","#c2410c","#9a3412","#7c2d12","#431407"]),
    ("amber",   ["#fffbeb","#fef3c7","#fde68a","#fcd34d","#fbbf24","#f59e0b","#d97706","#b45309","#92400e","#78350f","#451a03"]),
    ("yellow",  ["#fefce8","#fef9c3","#fef08a","#fde047","#facc15","#eab308","#ca8a04","#a16207","#854d0e","#713f12","#422006"]),
    ("lime",    ["#f7fee7","#ecfccb","#d9f99d","#bef264","#a3e635","#84cc16","#65a30d","#4d7c0f","#3f6212","#365314","#1a2e05"]),
    ("green",   ["#f0fdf4","#dcfce7","#bbf7d0","#86efac","#4ade80","#22c55e","#16a34a","#15803d","#166534","#14532d","#052e16"]),
    ("emerald", ["#ecfdf5","#d1fae5","#a7f3d0","#6ee7b7","#34d399","#10b981","#059669","#047857","#065f46","#064e3b","#022c22"]),
    ("teal",    ["#f0fdfa","#ccfbf1","#99f6e4","#5eead4","#2dd4bf","#14b8a6","#0d9488","#0f766e","#115e59","#134e4a","#042f2e"]),
    ("cyan",    ["#ecfeff","#cffafe","#a5f3fc","#67e8f9","#22d3ee","#06b6d4","#0891b2","#0e7490","#155e75","#164e63","#083344"]),
    ("sky",     ["#f0f9ff","#e0f2fe","#bae6fd","#7dd3fc","#38bdf8","#0ea5e9","#0284c7","#0369a1","#075985","#0c4a6e","#082f49"]),
    ("blue",    ["#eff6ff","#dbeafe","#bfdbfe","#93c5fd","#60a5fa","#3b82f6","#2563eb","#1d4ed8","#1e40af","#1e3a8a","#172554"]),
    ("indigo",  ["#eef2ff","#e0e7ff","#c7d2fe","#a5b4fc","#818cf8","#6366f1","#4f46e5","#4338ca","#3730a3","#312e81","#1e1b4b"]),
    ("violet",  ["#f5f3ff","#ede9fe","#ddd6fe","#c4b5fd","#a78bfa","#8b5cf6","#7c3aed","#6d28d9","#5b21b6","#4c1d95","#2e1065"]),
    ("purple",  ["#faf5ff","#f3e8ff","#e9d5ff","#d8b4fe","#c084fc","#a855f7","#9333ea","#7e22ce","#6b21a8","#581c87","#3b0764"]),
    ("fuchsia", ["#fdf4ff","#fae8ff","#f5d0fe","#f0abfc","#e879f9","#d946ef","#c026d3","#a21caf","#86198f","#701a75","#4a044e"]),
    ("pink",    ["#fdf2f8","#fce7f3","#fbcfe8","#f9a8d4","#f472b6","#ec4899","#db2777","#be185d","#9d174d","#831843","#500724"]),
    ("rose",    ["#fff1f2","#ffe4e6","#fecdd3","#fda4af","#fb7185","#f43f5e","#e11d48","#be123c","#9f1239","#881337","#4c0519"]),
];

#[cfg(test)]
#[path = "tailwind_tests.rs"]
mod tests;
