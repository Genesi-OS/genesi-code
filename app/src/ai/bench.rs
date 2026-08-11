//! Genesi Bench: run ONE unit test, in whatever language the project is in,
//! without leaving the editor.
//!
//! The whole feature rests on a fact every test runner shares: each of them can
//! address a single test by name. `cargo test`, `pytest`, `jest`, `go test`,
//! `rspec`, `phpunit`, `mvn`, `dotnet test` all take a filter. So Bench never
//! needs to understand a language deeply enough to *execute* a fragment of it —
//! it needs to name the test the cursor is in and hand that name to the tool the
//! project already uses. That is what makes "run just this part" work everywhere
//! instead of only where we wrote an interpreter.
//!
//! Detection is deliberately regex-and-convention rather than a full parse. A
//! test declaration is one of the most regular things in any codebase, and the
//! cost of being wrong is a filter that matches nothing — a message, not a
//! wrong answer. Nothing here executes project code; the only thing that runs is
//! the command the user presses Run on.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use walkdir::{DirEntry, WalkDir};

/// How far back from the cursor we look for the declaration that encloses it.
/// A test longer than this is not a unit test, and scanning the whole file
/// upwards would happily attach the cursor to something a screen away.
const MAX_LOOKBACK_LINES: usize = 400;

const MAX_TEST_FILES: usize = 800;
const MAX_FILE_BYTES: u64 = 512 * 1024;

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".venv",
    "venv",
    "vendor",
    "coverage",
    "__pycache__",
    ".idea",
    ".vscode",
];

/// A snapshot of the editor Bench was invoked from. Assembled by the workspace,
/// which is the only thing that can see the focused pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchContext {
    pub root: PathBuf,
    /// Project-relative, because that is what a runner's command line takes.
    pub file: PathBuf,
    pub source: String,
    /// 1-indexed, matching the editor and every runner's idea of a line.
    pub line: usize,
}

/// A test runner Bench knows how to drive with a single-test filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestRunner {
    Cargo,
    Pytest,
    Jest,
    Vitest,
    Mocha,
    GoTest,
    Maven,
    Gradle,
    RSpec,
    PhpUnit,
    DotNet,
}

impl TestRunner {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cargo => "cargo test",
            Self::Pytest => "pytest",
            Self::Jest => "jest",
            Self::Vitest => "vitest",
            Self::Mocha => "mocha",
            Self::GoTest => "go test",
            Self::Maven => "maven",
            Self::Gradle => "gradle",
            Self::RSpec => "rspec",
            Self::PhpUnit => "phpunit",
            Self::DotNet => "dotnet test",
        }
    }
}

/// A single test Bench can address by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestTarget {
    pub name: String,
    /// Enclosing class/suite/module, when the runner needs it to disambiguate.
    pub suite: Option<String>,
    /// Project-relative, because that is what every runner wants on its command
    /// line and what reads sensibly in the UI.
    pub file: PathBuf,
    pub line: usize,
}

impl TestTarget {
    /// `tests::parses_headers` — how the test is addressed, for display.
    pub fn qualified_name(&self) -> String {
        match &self.suite {
            Some(suite) => format!("{suite}::{}", self.name),
            None => self.name.clone(),
        }
    }

    pub fn location(&self) -> String {
        format!("{}:{}", self.file.display(), self.line)
    }
}

/// Why this target is what Bench is about to run — the difference between "your
/// cursor is in this test" and "no test here, but this one names your function".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOrigin {
    /// The cursor sits inside this test.
    AtCursor,
    /// Found by name convention from the symbol under the cursor.
    CoversSymbol,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchPlan {
    pub runner: TestRunner,
    pub target: TestTarget,
    pub origin: TargetOrigin,
    /// The exact shell command, shown to the user before it runs. Bench never
    /// runs something the user cannot read first.
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchOutcome {
    pub passed: bool,
    pub exit_code: Option<i32>,
    /// One line for the header: "1 passed", "1 failed", "no tests matched".
    pub summary: String,
    pub output: String,
    pub elapsed_ms: u128,
}

/// What Bench found when asked about the current selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BenchSubject {
    /// A test to run.
    Runnable(Box<BenchPlan>),
    /// A symbol with no test anywhere — the case the generator exists for.
    Untested {
        runner: Option<TestRunner>,
        symbol: String,
        file: PathBuf,
        line: usize,
    },
    /// Nothing recognisable under the cursor.
    Nothing(String),
}

/// Language family, derived from the file extension. Detection rules differ per
/// family, and nothing else about the language matters here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Rust,
    Python,
    JavaScript,
    Go,
    Jvm,
    Ruby,
    Php,
    CSharp,
}

fn family_of(file: &Path) -> Option<Family> {
    let extension = file.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "rs" => Family::Rust,
        "py" => Family::Python,
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Family::JavaScript,
        "go" => Family::Go,
        "java" | "kt" | "kts" => Family::Jvm,
        "rb" => Family::Ruby,
        "php" => Family::Php,
        "cs" => Family::CSharp,
        _ => return None,
    })
}

/// Which runner this project drives, for a file of this language.
///
/// Deliberately requires a project marker: guessing `pytest` for a stray `.py`
/// in a Rust repo produces a command that cannot work, and a wrong command is
/// worse than an honest "no runner detected".
pub fn detect_runner(root: &Path, file: &Path) -> Option<TestRunner> {
    match family_of(file)? {
        Family::Rust => root.join("Cargo.toml").exists().then_some(TestRunner::Cargo),
        // pytest runs plain unittest classes too, so it is the right default
        // whenever the project looks like Python at all.
        Family::Python => (root.join("pytest.ini").exists()
            || root.join("pyproject.toml").exists()
            || root.join("setup.py").exists()
            || root.join("setup.cfg").exists()
            || root.join("tox.ini").exists()
            || root.join("conftest.py").exists()
            || root.join("requirements.txt").exists())
        .then_some(TestRunner::Pytest),
        Family::JavaScript => javascript_runner(root),
        Family::Go => root.join("go.mod").exists().then_some(TestRunner::GoTest),
        Family::Jvm => {
            if root.join("pom.xml").exists() {
                Some(TestRunner::Maven)
            } else if root.join("build.gradle").exists()
                || root.join("build.gradle.kts").exists()
            {
                Some(TestRunner::Gradle)
            } else {
                None
            }
        }
        Family::Ruby => (root.join("Gemfile").exists() || root.join("spec").is_dir())
            .then_some(TestRunner::RSpec),
        Family::Php => root
            .join("composer.json")
            .exists()
            .then_some(TestRunner::PhpUnit),
        Family::CSharp => has_csproj(root).then_some(TestRunner::DotNet),
    }
}

/// Read the JS runner out of package.json rather than guessing. Vitest wins over
/// Jest when both are present, because a project that added Vitest is running
/// Vitest — Jest is usually the leftover.
fn javascript_runner(root: &Path) -> Option<TestRunner> {
    let manifest = fs::read_to_string(root.join("package.json")).ok()?;
    let lower = manifest.to_ascii_lowercase();
    if lower.contains("\"vitest\"") {
        Some(TestRunner::Vitest)
    } else if lower.contains("\"jest\"") {
        Some(TestRunner::Jest)
    } else if lower.contains("\"mocha\"") {
        Some(TestRunner::Mocha)
    } else {
        None
    }
}

fn has_csproj(root: &Path) -> bool {
    fs::read_dir(root)
        .map(|entries| {
            entries.flatten().any(|entry| {
                entry
                    .path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("csproj"))
            })
        })
        .unwrap_or(false)
}

/// The test declaration enclosing `line`, if the cursor is inside one.
///
/// `line` is 1-indexed, matching the editor and every runner's idea of a line.
pub fn find_test_at(source: &str, line: usize, file: &Path) -> Option<TestTarget> {
    let family = family_of(file)?;
    let lines = source.lines().collect::<Vec<_>>();
    let cursor = line.saturating_sub(1).min(lines.len().saturating_sub(1));
    let floor = cursor.saturating_sub(MAX_LOOKBACK_LINES);

    for index in (floor..=cursor).rev() {
        if let Some(name) = test_name_on_line(family, &lines, index) {
            return Some(TestTarget {
                suite: enclosing_suite(family, &lines, index),
                name,
                file: file.to_path_buf(),
                line: index + 1,
            });
        }
        // Stop at the nearest declaration that encloses the cursor. Without
        // this the scan walks straight out of the helper the cursor is in and
        // latches onto whatever test happens to sit above it — so pressing Run
        // inside a helper would run an unrelated test and report it as yours.
        if encloses_cursor(family, lines[index]) {
            return None;
        }
    }
    None
}

/// Whether this line opens the declaration the cursor is sitting inside.
///
/// Only used as a stopping rule, so it errs toward stopping: refusing to find a
/// test costs the user a click on the sidebar, while running the wrong test
/// costs them a wrong answer they may believe.
fn encloses_cursor(family: Family, line: &str) -> bool {
    let pattern = match family {
        Family::Rust => rust_fn(),
        Family::Python => python_def(),
        Family::Go => go_fn(),
        Family::Jvm => jvm_method(),
        Family::Ruby => ruby_def(),
        Family::Php => php_method(),
        Family::CSharp => csharp_method(),
        // JavaScript test bodies are full of declarations that enclose nothing
        // — `const total = 2;` inside an `it(…)` would end the scan one line
        // into the test. Only a declaration that opens a block can be what the
        // cursor is inside of.
        Family::JavaScript => {
            return js_declaration().is_match(line) && line.trim_end().ends_with('{');
        }
    };
    pattern.is_match(line)
}

/// Whether line `index` declares a test, and its name.
fn test_name_on_line(family: Family, lines: &[&str], index: usize) -> Option<String> {
    let line = lines[index];
    match family {
        // A bare `fn` is not a test; the attribute above it is what makes it
        // one. Without this check every helper in the file looks runnable.
        Family::Rust => {
            let name = capture(&rust_fn(), line)?;
            rust_has_test_attribute(lines, index).then_some(name)
        }
        Family::Python => capture(&python_test_def(), line),
        Family::JavaScript => capture(&js_test_call(), line),
        Family::Go => capture(&go_test_fn(), line),
        Family::Jvm => {
            let name = capture(&jvm_method(), line)?;
            jvm_has_test_annotation(lines, index).then_some(name)
        }
        Family::Ruby => capture(&ruby_test(), line),
        Family::Php => capture(&php_test_method(), line),
        Family::CSharp => {
            let name = capture(&csharp_method(), line)?;
            csharp_has_test_attribute(lines, index).then_some(name)
        }
    }
}

/// Rust marks tests with an attribute on one of the lines just above `fn`,
/// possibly with doc comments or other attributes in between.
fn rust_has_test_attribute(lines: &[&str], index: usize) -> bool {
    let floor = index.saturating_sub(8);
    for candidate in (floor..index).rev() {
        let line = lines[candidate].trim();
        if line.contains("#[test]") || (line.starts_with("#[") && line.contains("test")) {
            return true;
        }
        // Keep walking past attributes and comments; anything else means we
        // left the declaration's header and there was no test attribute.
        if !line.is_empty()
            && !line.starts_with('#')
            && !line.starts_with("//")
            && !line.starts_with("/*")
            && !line.starts_with('*')
        {
            return false;
        }
    }
    false
}

fn jvm_has_test_annotation(lines: &[&str], index: usize) -> bool {
    let floor = index.saturating_sub(8);
    (floor..index).rev().any(|candidate| {
        let line = lines[candidate].trim();
        line.starts_with("@Test")
            || line.starts_with("@ParameterizedTest")
            || line.starts_with("@RepeatedTest")
    })
}

fn csharp_has_test_attribute(lines: &[&str], index: usize) -> bool {
    let floor = index.saturating_sub(8);
    (floor..index).rev().any(|candidate| {
        let line = lines[candidate].trim();
        line.starts_with("[Fact")
            || line.starts_with("[Test")
            || line.starts_with("[Theory")
            || line.starts_with("[TestMethod")
    })
}

/// The class/module the test sits in, where the runner needs it to address the
/// test. Found by indentation for Python and by nesting for the rest.
fn enclosing_suite(family: Family, lines: &[&str], index: usize) -> Option<String> {
    let floor = index.saturating_sub(MAX_LOOKBACK_LINES);
    let pattern = match family {
        Family::Rust => rust_mod(),
        Family::Python => python_class(),
        Family::Jvm => jvm_class(),
        Family::Php => php_class(),
        Family::CSharp => csharp_class(),
        // Jest/Mocha/RSpec address tests by name, and `go test -run` takes the
        // function alone, so a suite would only add a way to be wrong.
        Family::JavaScript | Family::Go | Family::Ruby => return None,
    };
    (floor..index)
        .rev()
        .find_map(|candidate| capture(&pattern, lines[candidate]))
}

/// The function, method, or class the cursor is in — what the user means by
/// "this part" when it is not itself a test.
pub fn symbol_at(source: &str, line: usize, file: &Path) -> Option<String> {
    let family = family_of(file)?;
    let lines = source.lines().collect::<Vec<_>>();
    let cursor = line.saturating_sub(1).min(lines.len().saturating_sub(1));
    let floor = cursor.saturating_sub(MAX_LOOKBACK_LINES);

    let pattern = match family {
        Family::Rust => rust_fn(),
        Family::Python => python_def(),
        Family::JavaScript => js_declaration(),
        Family::Go => go_fn(),
        Family::Jvm => jvm_method(),
        Family::Ruby => ruby_def(),
        Family::Php => php_method(),
        Family::CSharp => csharp_method(),
    };
    (floor..=cursor)
        .rev()
        .find_map(|index| capture(&pattern, lines[index]))
}

/// Find a test whose name refers to `symbol`.
///
/// This is the convention every codebase already follows without being told:
/// `parse_headers` is tested by `test_parse_headers`, `parses_headers`,
/// `ParseHeadersTest`, or `describe("parseHeaders")`. Matching is done on
/// normalised names so casing and underscores stop mattering.
pub fn find_covering_test(root: &Path, symbol: &str, from_file: &Path) -> Option<TestTarget> {
    let needle = tokens(symbol);
    if needle.iter().map(String::len).sum::<usize>() < 3 {
        // Two-letter helpers match half the codebase; better to offer to
        // generate a test than to run something unrelated.
        return None;
    }

    // The file the user is in comes first: Rust keeps `mod tests` beside the
    // code, and even in other languages the nearest test is the likely one.
    //
    // Everything stays project-relative. Canonicalising first looks tidier but
    // returns a `\\?\` UNC path on Windows that `strip_prefix(root)` then fails
    // to match, which silently dropped the user's own file from the search —
    // so a Rust symbol tested by its neighbouring `mod tests` found nothing.
    let mut candidates = vec![from_file.to_path_buf()];
    candidates.extend(
        test_files(root)
            .into_iter()
            .filter_map(|file| file.strip_prefix(root).map(Path::to_path_buf).ok()),
    );

    let mut seen = HashSet::new();
    for relative in candidates {
        if !seen.insert(relative.clone()) {
            continue;
        }
        let Ok(source) = fs::read_to_string(root.join(&relative)) else {
            continue;
        };
        // Every word of the symbol has to appear in the test's name. Requiring
        // all of them is what keeps `parse_headers` from claiming a test that
        // merely mentions `parse`.
        if let Some(target) = tests_in(&source, &relative).into_iter().find(|target| {
            let candidate = tokens(&target.name);
            needle.iter().all(|word| candidate.contains(word))
        }) {
            return Some(target);
        }
    }
    None
}

/// Every test declared in `source`, in declaration order.
pub fn tests_in(source: &str, file: &Path) -> Vec<TestTarget> {
    let Some(family) = family_of(file) else {
        return Vec::new();
    };
    let lines = source.lines().collect::<Vec<_>>();
    (0..lines.len())
        .filter_map(|index| {
            let name = test_name_on_line(family, &lines, index)?;
            Some(TestTarget {
                suite: enclosing_suite(family, &lines, index),
                name,
                file: file.to_path_buf(),
                line: index + 1,
            })
        })
        .collect()
}

/// Files that look like they hold tests, by the naming conventions each
/// ecosystem uses. Bounded, because this runs on a keystroke-adjacent path.
fn test_files(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .follow_links(false)
        .max_depth(8)
        .into_iter()
        .filter_entry(should_visit)
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            entry
                .metadata()
                .map(|metadata| metadata.len() <= MAX_FILE_BYTES)
                .unwrap_or(false)
        })
        .filter(|entry| looks_like_test_file(entry.path()))
        .take(MAX_TEST_FILES)
        .map(DirEntry::into_path)
        .collect()
}

fn looks_like_test_file(path: &Path) -> bool {
    if family_of(path).is_none() {
        return false;
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let in_test_dir = path.components().any(|component| {
        let component = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        matches!(component.as_str(), "test" | "tests" | "spec" | "__tests__")
    });
    in_test_dir
        || name.starts_with("test_")
        || name.ends_with("_test.rs")
        || name.ends_with("_test.py")
        || name.ends_with("_test.go")
        || name.ends_with("_test.rb")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.ends_with("test.java")
        || name.ends_with("tests.cs")
        || name.ends_with("test.php")
}

fn should_visit(entry: &DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    !SKIP_DIRS.contains(&name.as_ref())
}

/// Split an identifier into stemmed word tokens.
///
/// `parse_headers`, `parseHeaders` and `ParseHeaders` all become
/// `["pars…", "header"]`, which is what lets a symbol be matched against a test
/// whose name is a sentence (`"adds an item to the cart"`) or a conjugation
/// (`parses_headers_into_lines`). Plain substring matching looks equivalent and
/// is not: `parse_headers` is not a substring of `parses_headers`, and that one
/// letter covers a large share of how tests are actually named.
fn tokens(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for character in name.chars() {
        if !character.is_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        // A capital opens a new word, so camelCase splits with no separator.
        if character.is_uppercase() && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        current.extend(character.to_lowercase());
    }
    if !current.is_empty() {
        words.push(current);
    }
    words.iter().map(|word| stem(word)).collect()
}

/// Crude singularisation — enough to bridge `parse`/`parses` and
/// `header`/`headers`. Left alone below four letters so short words (`is`,
/// `as`) survive, and `ss` endings (`class`, `pass`) are never truncated.
fn stem(word: &str) -> String {
    if word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") {
        return word[..word.len() - 1].to_string();
    }
    word.to_string()
}

/// Build the command that runs exactly this one test.
pub fn build_command(runner: TestRunner, target: &TestTarget) -> String {
    let file = target.file.to_string_lossy().replace('\\', "/");
    let name = &target.name;
    match runner {
        // `--exact` needs the full path, so it is only safe with the module we
        // actually found. Without one, fall back to the substring filter rather
        // than an exact match that would find nothing.
        TestRunner::Cargo => match &target.suite {
            Some(module) => format!("cargo test {module}::{name} -- --exact --nocapture"),
            None => format!("cargo test {name} -- --nocapture"),
        },
        TestRunner::Pytest => match &target.suite {
            Some(class) => format!("pytest '{file}::{class}::{name}' -v"),
            None => format!("pytest '{file}::{name}' -v"),
        },
        // `-t` filters by test name, the file argument stops it loading the
        // rest of the suite. Both together are what makes this fast.
        TestRunner::Jest => format!("npx jest '{file}' -t '{name}'"),
        TestRunner::Vitest => format!("npx vitest run '{file}' -t '{name}'"),
        TestRunner::Mocha => format!("npx mocha '{file}' --grep '{name}'"),
        TestRunner::GoTest => {
            let package = target
                .file
                .parent()
                .map(|parent| parent.to_string_lossy().replace('\\', "/"))
                .filter(|parent| !parent.is_empty())
                .map(|parent| format!("./{parent}/"))
                .unwrap_or_else(|| "./".to_string());
            format!("go test -run '^{name}$' -v {package}")
        }
        TestRunner::Maven => match &target.suite {
            Some(class) => format!("mvn -q test -Dtest='{class}#{name}'"),
            None => format!("mvn -q test -Dtest='{name}'"),
        },
        TestRunner::Gradle => match &target.suite {
            Some(class) => format!("./gradlew test --tests '{class}.{name}'"),
            None => format!("./gradlew test --tests '*{name}'"),
        },
        // RSpec addresses a test by line, which is exact and needs no name at
        // all — the one runner where ambiguity is impossible.
        TestRunner::RSpec => format!("bundle exec rspec '{file}:{}'", target.line),
        TestRunner::PhpUnit => format!("./vendor/bin/phpunit --filter '{name}' '{file}'"),
        TestRunner::DotNet => format!("dotnet test --filter 'FullyQualifiedName~{name}'"),
    }
}

/// Decide what Bench should offer for the current selection.
///
/// The order matters: a cursor inside a test means run THAT test, even if the
/// symbol lookup would have found another one. Only when there is no test under
/// the cursor do we go looking for one that covers the symbol.
pub fn subject_for(
    root: &Path,
    file: &Path,
    source: &str,
    line: usize,
) -> BenchSubject {
    let runner = detect_runner(root, file);

    if let Some(target) = find_test_at(source, line, file) {
        return match runner {
            Some(runner) => BenchSubject::Runnable(Box::new(BenchPlan {
                command: build_command(runner, &target),
                runner,
                target,
                origin: TargetOrigin::AtCursor,
            })),
            None => BenchSubject::Nothing(format!(
                "Found the test `{}`, but no test runner for this project. Bench looks for Cargo.toml, package.json, go.mod, pom.xml, and the like in {}.",
                target.name,
                root.display()
            )),
        };
    }

    let Some(symbol) = symbol_at(source, line, file) else {
        return BenchSubject::Nothing(
            "Put the cursor inside a test, or inside the function you want tested.".to_string(),
        );
    };

    if let Some(target) = find_covering_test(root, &symbol, file) {
        if let Some(runner) = runner {
            return BenchSubject::Runnable(Box::new(BenchPlan {
                command: build_command(runner, &target),
                runner,
                target,
                origin: TargetOrigin::CoversSymbol,
            }));
        }
    }

    BenchSubject::Untested {
        runner,
        symbol,
        file: file.to_path_buf(),
        line,
    }
}

/// Turn a finished command into a verdict.
///
/// The exit code is the truth for pass/fail — every one of these runners returns
/// non-zero on failure. The output is only read to say something better than
/// "exit 1", and to catch the case that looks like success but is not: a filter
/// that matched no test at all, which several runners report with exit 0.
pub fn read_outcome(
    runner: TestRunner,
    exit_code: Option<i32>,
    output: &str,
    elapsed_ms: u128,
) -> BenchOutcome {
    let lower = output.to_ascii_lowercase();
    let matched_nothing = match runner {
        TestRunner::Cargo => lower.contains("0 passed") && lower.contains("0 failed"),
        TestRunner::Pytest => lower.contains("no tests ran") || lower.contains("collected 0 items"),
        TestRunner::Jest | TestRunner::Vitest => {
            lower.contains("no tests found") || lower.contains("0 passed") && lower.contains("0 failed")
        }
        TestRunner::Mocha => lower.contains("0 passing"),
        TestRunner::GoTest => lower.contains("no tests to run") || lower.contains("testing: warning: no tests to run"),
        TestRunner::RSpec => lower.contains("0 examples"),
        TestRunner::PhpUnit => lower.contains("no tests executed"),
        TestRunner::Maven | TestRunner::Gradle | TestRunner::DotNet => false,
    };

    let passed = exit_code == Some(0) && !matched_nothing;
    let summary = if matched_nothing {
        // Silent success is the worst outcome a test runner can produce: it
        // looks green and proves nothing.
        "no test matched the filter".to_string()
    } else if passed {
        "passed".to_string()
    } else {
        match exit_code {
            Some(code) => format!("failed (exit {code})"),
            None => "killed before it finished".to_string(),
        }
    };

    BenchOutcome {
        passed,
        exit_code,
        summary,
        output: output.to_string(),
        elapsed_ms,
    }
}

/// The prompt handed to the local model when a symbol has no test.
///
/// Deliberately narrow: one test, the project's own framework, no prose. A model
/// asked for "tests for this" writes a suite with mocks it invented; asked for
/// exactly one test in a named file, it writes something that runs.
pub fn generation_prompt(
    symbol: &str,
    source_excerpt: &str,
    file: &Path,
    runner: Option<TestRunner>,
    target_file: &Path,
) -> String {
    let framework = runner
        .map(|runner| runner.label())
        .unwrap_or("the project's existing test framework");
    format!(
        "Write exactly ONE unit test for `{symbol}` from `{}`.\n\n\
         Rules:\n\
         - Use {framework}, matching how tests are already written in this project.\n\
         - The test goes in `{}`.\n\
         - Name it so it clearly refers to `{symbol}`, and assert real behaviour \
           rather than that the function merely runs.\n\
         - Output ONLY the code, in a single fenced block. No explanation.\n\n\
         Here is the code under test:\n\n```\n{}\n```",
        file.display(),
        target_file.display(),
        source_excerpt.trim()
    )
}

/// Where a generated test should be written, following the project's own layout.
pub fn suggested_test_path(root: &Path, file: &Path, runner: Option<TestRunner>) -> PathBuf {
    let stem = file
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "module".to_string());
    let extension = file
        .extension()
        .map(|ext| ext.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent = file.parent().unwrap_or(Path::new(""));

    match runner {
        // Rust keeps unit tests in the same file, under `mod tests`. Warp's own
        // codebase splits them into `<name>_tests.rs` and includes them with
        // `#[path]`, which is the convention this project actually uses.
        Some(TestRunner::Cargo) => parent.join(format!("{stem}_tests.rs")),
        Some(TestRunner::Pytest) => {
            let tests_dir = root.join("tests");
            if tests_dir.is_dir() {
                PathBuf::from("tests").join(format!("test_{stem}.py"))
            } else {
                parent.join(format!("test_{stem}.py"))
            }
        }
        Some(TestRunner::GoTest) => parent.join(format!("{stem}_test.go")),
        Some(TestRunner::Jest) | Some(TestRunner::Vitest) | Some(TestRunner::Mocha) => {
            parent.join(format!("{stem}.test.{extension}"))
        }
        Some(TestRunner::RSpec) => PathBuf::from("spec").join(format!("{stem}_spec.rb")),
        Some(TestRunner::PhpUnit) => PathBuf::from("tests").join(format!("{stem}Test.php")),
        Some(TestRunner::Maven) | Some(TestRunner::Gradle) => PathBuf::from("src/test/java")
            .join(format!("{}Test.{extension}", capitalize(&stem))),
        Some(TestRunner::DotNet) => parent.join(format!("{}Tests.cs", capitalize(&stem))),
        None => parent.join(format!("{stem}_test.{extension}")),
    }
}

fn capitalize(value: &str) -> String {
    let mut characters = value.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

/// Pull the code out of the model's reply. A local model will wrap it in a fence
/// and often narrate around it however firmly it was told not to.
pub fn extract_code_block(reply: &str) -> Option<String> {
    let mut inside = false;
    let mut collected: Vec<&str> = Vec::new();
    for line in reply.lines() {
        if line.trim_start().starts_with("```") {
            if inside {
                break;
            }
            inside = true;
            continue;
        }
        if inside {
            collected.push(line);
        }
    }
    if collected.is_empty() {
        // No fence at all: if the reply looks like code rather than prose, take
        // it whole rather than losing a usable test to a formatting miss.
        let trimmed = reply.trim();
        return (!trimmed.is_empty() && trimmed.lines().count() > 1).then(|| trimmed.to_string());
    }
    Some(collected.join("\n").trim_end().to_string())
}

/// First group that actually matched. Some patterns spell out alternatives that
/// each carry the name in a different group — `function f`, `const f =`, and
/// `class F` in JavaScript — so reading group 1 alone would find nothing for
/// two of the three.
fn capture(pattern: &Regex, line: &str) -> Option<String> {
    let captures = pattern.captures(line)?;
    (1..captures.len())
        .filter_map(|index| captures.get(index))
        .map(|value| value.as_str().to_string())
        .next()
}

// ── declaration patterns, one per language family ──
//
// Built on each call rather than cached in a static: these run over at most a
// few hundred lines, and a `OnceLock` per pattern would be more machinery than
// the cost it saves.

fn rust_fn() -> Regex {
    Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+\S+\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)")
        .expect("valid Rust fn regex")
}

fn rust_mod() -> Regex {
    Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)").expect("valid Rust mod regex")
}

fn python_test_def() -> Regex {
    Regex::new(r"^\s*(?:async\s+)?def\s+(test[A-Za-z0-9_]*)").expect("valid Python test regex")
}

fn python_def() -> Regex {
    Regex::new(r"^\s*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)").expect("valid Python def regex")
}

fn python_class() -> Regex {
    Regex::new(r"^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)").expect("valid Python class regex")
}

/// `it('…')`, `test("…")`, and their `.only` / `.each` variants.
fn js_test_call() -> Regex {
    Regex::new(r#"^\s*(?:it|test)(?:\.\w+)*\s*\(\s*[\x22\x27`]([^\x22\x27`]+)"#)
        .expect("valid JS test regex")
}

fn js_declaration() -> Regex {
    Regex::new(
        r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?(?:function\s+([A-Za-z_$][\w$]*)|(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=|class\s+([A-Za-z_$][\w$]*))",
    )
    .expect("valid JS declaration regex")
}

fn go_test_fn() -> Regex {
    Regex::new(r"^\s*func\s+((?:Test|Benchmark|Fuzz|Example)[A-Za-z0-9_]*)\s*\(")
        .expect("valid Go test regex")
}

fn go_fn() -> Regex {
    Regex::new(r"^\s*func\s+(?:\([^)]*\)\s*)?([A-Za-z_][A-Za-z0-9_]*)\s*\(")
        .expect("valid Go fn regex")
}

fn jvm_method() -> Regex {
    Regex::new(
        r"^\s*(?:@\w+\s+)*(?:public|private|protected|internal)?\s*(?:static\s+)?(?:final\s+)?(?:fun\s+|[\w<>\[\], .]+\s+)([A-Za-z_][A-Za-z0-9_]*)\s*\(",
    )
    .expect("valid JVM method regex")
}

fn jvm_class() -> Regex {
    Regex::new(r"^\s*(?:public\s+|final\s+|abstract\s+|open\s+)*class\s+([A-Za-z_][A-Za-z0-9_]*)")
        .expect("valid JVM class regex")
}

fn ruby_test() -> Regex {
    Regex::new(r#"^\s*(?:it|specify)\s+[\x22\x27]([^\x22\x27]+)"#).expect("valid Ruby test regex")
}

fn ruby_def() -> Regex {
    Regex::new(r"^\s*def\s+([A-Za-z_][A-Za-z0-9_?!]*)").expect("valid Ruby def regex")
}

fn php_test_method() -> Regex {
    Regex::new(r"^\s*(?:public\s+)?function\s+(test[A-Za-z0-9_]*)\s*\(")
        .expect("valid PHP test regex")
}

fn php_method() -> Regex {
    Regex::new(r"^\s*(?:(?:public|private|protected|static)\s+)*function\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(")
        .expect("valid PHP method regex")
}

fn php_class() -> Regex {
    Regex::new(r"^\s*(?:final\s+|abstract\s+)*class\s+([A-Za-z_][A-Za-z0-9_]*)")
        .expect("valid PHP class regex")
}

fn csharp_method() -> Regex {
    Regex::new(
        r"^\s*(?:(?:public|private|protected|internal|static|async|override|virtual|sealed)\s+)*[\w<>\[\], .?]+\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(",
    )
    .expect("valid C# method regex")
}

fn csharp_class() -> Regex {
    Regex::new(r"^\s*(?:(?:public|internal|sealed|abstract|static|partial)\s+)*class\s+([A-Za-z_][A-Za-z0-9_]*)")
        .expect("valid C# class regex")
}

#[cfg(test)]
#[path = "bench_tests.rs"]
mod tests;
