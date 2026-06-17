use std::path::Path;

use warp_core::ui::appearance::Appearance;
use warpui::assets::asset_cache::AssetSource;
use warpui::color::ColorU;
use warpui::elements::{CacheOption, Icon, Image};
use warpui::Element;

fn bundled_image(path: &'static str) -> Box<dyn Element> {
    Image::new(AssetSource::Bundled { path }, CacheOption::BySize).finish()
}

fn bundled_icon(path: &'static str, color: ColorU) -> Box<dyn Element> {
    Icon::new(path, color).finish()
}

/// Returns a special icon for the given file path, if any.
pub fn icon_from_file_path(path: &str, appearance: &Appearance) -> Option<Box<dyn Element>> {
    let theme = appearance.theme();
    let parsed_path = Path::new(path);
    let extension = parsed_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());
    let file_name = parsed_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_ascii_lowercase());
    let main_text = theme.main_text_color(theme.background());

    let image = match file_name.as_deref() {
        Some("package.json") | Some("package-lock.json") | Some("npm-shrinkwrap.json") => {
            bundled_image("bundled/svg/file_type/npm.svg")
        }
        Some("dockerfile") | Some("containerfile") => {
            bundled_icon("bundled/svg/docker.svg", ColorU::new(36, 150, 237, 255))
        }
        Some(name) if name == "docker-compose.yml" || name == "docker-compose.yaml" => {
            bundled_icon("bundled/svg/docker.svg", ColorU::new(36, 150, 237, 255))
        }
        _ => match extension.as_deref() {
            Some("rs") => bundled_image("bundled/svg/file_type/rust.svg"),
            Some("json") | Some("jsonc") | Some("webmanifest") => {
                bundled_image("bundled/svg/file_type/json.svg")
            }
            Some("ts") | Some("tsx") | Some("mts") | Some("cts") => {
                bundled_image("bundled/svg/file_type/typescript.svg")
            }
            Some("js") | Some("jsx") | Some("mjs") | Some("cjs") | Some("jsm") => {
                bundled_image("bundled/svg/file_type/javascript.svg")
            }
            Some("py") | Some("pyi") | Some("pyw") => {
                bundled_image("bundled/svg/file_type/python.svg")
            }
            Some("cpp") | Some("hpp") | Some("cc") | Some("cxx") | Some("hh") | Some("hxx") => {
                bundled_image("bundled/svg/file_type/cpp.svg")
            }
            Some("go") => bundled_image("bundled/svg/file_type/go.svg"),
            Some("md") | Some("mdx") => {
                bundled_icon("bundled/svg/file_type/markdown.svg", main_text.into())
            }
            Some("html") | Some("htm") | Some("xhtml") => bundled_icon(
                "bundled/svg/file-code-02.svg",
                ColorU::new(227, 109, 48, 255),
            ),
            Some("css") | Some("scss") | Some("sass") | Some("less") => {
                bundled_icon("bundled/svg/brush-01.svg", ColorU::new(77, 148, 255, 255))
            }
            Some("xml") | Some("svg") => {
                bundled_icon("bundled/svg/code-02.svg", ColorU::new(232, 136, 62, 255))
            }
            Some("sh") | Some("bash") | Some("zsh") | Some("fish") => {
                bundled_icon("bundled/svg/terminal.svg", main_text.into())
            }
            Some("kt") | Some("kts") => bundled_image("bundled/svg/file_type/kotlin.svg"),
            Some("php") => bundled_image("bundled/svg/file_type/php.svg"),
            Some("pl") | Some("pm") => bundled_image("bundled/svg/file_type/perl.svg"),
            Some("c") | Some("h") => bundled_image("bundled/svg/file_type/c.svg"),
            Some("pyx") | Some("pxd") => bundled_image("bundled/svg/file_type/cython.svg"),
            Some("swf") => bundled_image("bundled/svg/file_type/flash.svg"),
            Some("wasm") => bundled_image("bundled/svg/file_type/wasm.svg"),
            Some("zig") => bundled_image("bundled/svg/file_type/zig.svg"),
            Some("sql") => bundled_image("bundled/svg/file_type/sql.svg"),
            Some("ng") | Some("ngml") => bundled_image("bundled/svg/file_type/angular.svg"),
            Some("tf") | Some("hcl") | Some("tfvars") => {
                bundled_image("bundled/svg/file_type/terraform.svg")
            }
            Some("toml") | Some("lock") | Some("ini") | Some("cfg") | Some("conf")
            | Some("env") | Some("yaml") | Some("yml") => {
                bundled_icon("bundled/svg/sliders-04.svg", ColorU::new(197, 157, 62, 255))
            }
            Some("java") | Some("gradle") => bundled_icon(
                "bundled/svg/file-code-02.svg",
                ColorU::new(198, 89, 71, 255),
            ),
            Some("rb") | Some("gemspec") => bundled_icon(
                "bundled/svg/file-code-02.svg",
                ColorU::new(194, 70, 85, 255),
            ),
            Some("cs") | Some("csproj") | Some("sln") => bundled_icon(
                "bundled/svg/file-code-02.svg",
                ColorU::new(158, 110, 255, 255),
            ),
            Some("swift") => bundled_icon(
                "bundled/svg/file-code-02.svg",
                ColorU::new(245, 138, 48, 255),
            ),
            Some("lua") => bundled_icon(
                "bundled/svg/file-code-02.svg",
                ColorU::new(92, 123, 255, 255),
            ),
            Some("vue") => bundled_icon(
                "bundled/svg/file-code-02.svg",
                ColorU::new(64, 184, 131, 255),
            ),
            Some("svelte") => bundled_icon(
                "bundled/svg/file-code-02.svg",
                ColorU::new(255, 107, 62, 255),
            ),
            Some("mmd") | Some("mermaid") => bundled_image("bundled/svg/file_type/mermaid.svg"),
            _ => {
                return None;
            }
        },
    };
    Some(image)
}
