use std::fs;

use tempfile::tempdir;

use super::*;

fn rust_source() -> &'static str {
    r#"pub fn parse_headers(raw: &str) -> Vec<String> {
    raw.lines().map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Doc comment between the attribute and the fn.
    #[test]
    fn parses_headers_into_lines() {
        assert_eq!(parse_headers("a\nb").len(), 2);
    }

    fn helper_not_a_test() -> u8 {
        7
    }
}
"#
}

#[test]
fn a_cursor_inside_a_rust_test_finds_it_with_its_module() {
    let target = find_test_at(rust_source(), 12, Path::new("src/headers.rs"))
        .expect("cursor is inside the test body");
    assert_eq!(target.name, "parses_headers_into_lines");
    assert_eq!(target.suite.as_deref(), Some("tests"));
    assert_eq!(target.qualified_name(), "tests::parses_headers_into_lines");
}

#[test]
fn a_cursor_inside_a_helper_finds_no_test_at_all() {
    // Line 17 is inside `helper_not_a_test`, which has no #[test] above it —
    // and a real `#[test]` sits a few lines further up. The backward scan must
    // stop at the helper rather than latching onto that test: running an
    // unrelated test and presenting it as the user's is worse than finding
    // nothing.
    assert_eq!(find_test_at(rust_source(), 17, Path::new("src/headers.rs")), None);
}

#[test]
fn a_declaration_inside_a_javascript_test_does_not_end_the_search() {
    // `const total = …` is a declaration, but it encloses nothing. Treating it
    // as the enclosing block would lose the test one line into its own body.
    let source = r#"
describe('cart', () => {
  it('adds an item to the cart', () => {
    const total = 2;
    expect(total).toBe(2);
  });
});
"#;
    let target = find_test_at(source, 5, Path::new("src/cart.test.ts")).expect("test found");
    assert_eq!(target.name, "adds an item to the cart");
}

#[test]
fn the_symbol_under_the_cursor_is_the_enclosing_function() {
    let symbol = symbol_at(rust_source(), 2, Path::new("src/headers.rs"));
    assert_eq!(symbol.as_deref(), Some("parse_headers"));
}

#[test]
fn python_tests_carry_their_class() {
    let source = r#"
class TestCheckout:
    def test_totals_include_tax(self):
        assert total(100) == 110

def helper():
    return 1
"#;
    let target = find_test_at(source, 4, Path::new("tests/test_checkout.py")).expect("test found");
    assert_eq!(target.name, "test_totals_include_tax");
    assert_eq!(target.suite.as_deref(), Some("TestCheckout"));
}

#[test]
fn javascript_tests_are_found_by_their_string_name() {
    let source = r#"
describe('cart', () => {
  it('adds an item to the cart', async () => {
    expect(add()).toBe(1);
  });
});
"#;
    let target = find_test_at(source, 4, Path::new("src/cart.test.ts")).expect("test found");
    assert_eq!(target.name, "adds an item to the cart");
    // Jest addresses tests by name alone, so a suite would only add noise.
    assert_eq!(target.suite, None);
}

#[test]
fn javascript_symbols_are_found_across_declaration_forms() {
    let arrow = "export const calculateShipping = (weight) => {\n  return weight * 2;\n};\n";
    assert_eq!(
        symbol_at(arrow, 2, Path::new("src/ship.ts")).as_deref(),
        Some("calculateShipping")
    );

    let function = "export default async function loadUser(id) {\n  return id;\n}\n";
    assert_eq!(
        symbol_at(function, 2, Path::new("src/user.js")).as_deref(),
        Some("loadUser")
    );

    let class_declaration = "class ShoppingCart {\n  add() {}\n}\n";
    assert_eq!(
        symbol_at(class_declaration, 2, Path::new("src/cart.js")).as_deref(),
        Some("ShoppingCart")
    );
}

#[test]
fn go_tests_are_recognised_only_by_their_prefix() {
    let source = "func TestSum(t *testing.T) {\n\tt.Fatal()\n}\n\nfunc sum(a int) int {\n\treturn a\n}\n";
    let target = find_test_at(source, 2, Path::new("math/sum_test.go")).expect("test found");
    assert_eq!(target.name, "TestSum");
    assert!(
        find_test_at(source, 6, Path::new("math/sum_test.go")).is_none(),
        "a plain func is not a Go test"
    );
}

#[test]
fn runners_come_from_project_markers_not_from_the_extension() {
    let dir = tempdir().expect("temp dir");
    // A stray .py in a Rust project must not produce a pytest command that
    // could never work.
    fs::write(dir.path().join("Cargo.toml"), "[package]\nname='x'\n").expect("write manifest");
    assert_eq!(
        detect_runner(dir.path(), Path::new("src/main.rs")),
        Some(TestRunner::Cargo)
    );
    assert_eq!(detect_runner(dir.path(), Path::new("script.py")), None);
}

#[test]
fn vitest_wins_over_jest_when_a_project_has_both() {
    let dir = tempdir().expect("temp dir");
    fs::write(
        dir.path().join("package.json"),
        r#"{"devDependencies":{"jest":"29","vitest":"1"}}"#,
    )
    .expect("write manifest");
    assert_eq!(
        detect_runner(dir.path(), Path::new("src/a.test.ts")),
        Some(TestRunner::Vitest)
    );
}

#[test]
fn commands_address_exactly_one_test() {
    let target = TestTarget {
        name: "parses_headers".to_string(),
        suite: Some("tests".to_string()),
        file: PathBuf::from("src/headers.rs"),
        line: 12,
    };
    assert_eq!(
        build_command(TestRunner::Cargo, &target),
        "cargo test tests::parses_headers -- --exact --nocapture"
    );

    let python = TestTarget {
        name: "test_totals".to_string(),
        suite: Some("TestCheckout".to_string()),
        file: PathBuf::from("tests/test_checkout.py"),
        line: 3,
    };
    assert_eq!(
        build_command(TestRunner::Pytest, &python),
        "pytest 'tests/test_checkout.py::TestCheckout::test_totals' -v"
    );

    let js = TestTarget {
        name: "adds an item".to_string(),
        suite: None,
        file: PathBuf::from("src/cart.test.ts"),
        line: 4,
    };
    assert_eq!(
        build_command(TestRunner::Jest, &js),
        "npx jest 'src/cart.test.ts' -t 'adds an item'"
    );

    let go = TestTarget {
        name: "TestSum".to_string(),
        suite: None,
        file: PathBuf::from("math/sum_test.go"),
        line: 1,
    };
    assert_eq!(
        build_command(TestRunner::GoTest, &go),
        "go test -run '^TestSum$' -v ./math/"
    );
}

#[test]
fn a_cargo_test_without_a_module_drops_exact_rather_than_matching_nothing() {
    let target = TestTarget {
        name: "parses_headers".to_string(),
        suite: None,
        file: PathBuf::from("tests/headers.rs"),
        line: 3,
    };
    let command = build_command(TestRunner::Cargo, &target);
    assert!(!command.contains("--exact"), "got {command}");
    assert!(command.contains("cargo test parses_headers"), "got {command}");
}

#[test]
fn rspec_addresses_a_test_by_line_so_the_name_never_matters() {
    let target = TestTarget {
        name: "does a thing".to_string(),
        suite: None,
        file: PathBuf::from("spec/cart_spec.rb"),
        line: 42,
    };
    assert_eq!(
        build_command(TestRunner::RSpec, &target),
        "bundle exec rspec 'spec/cart_spec.rb:42'"
    );
}

#[test]
fn a_covering_test_is_found_across_naming_conventions() {
    let dir = tempdir().expect("temp dir");
    fs::create_dir_all(dir.path().join("tests")).expect("create tests dir");
    fs::write(
        dir.path().join("tests/test_shipping.py"),
        "def test_calculate_shipping_adds_handling():\n    assert True\n",
    )
    .expect("write test");
    fs::write(dir.path().join("shipping.py"), "def calculate_shipping():\n    pass\n")
        .expect("write source");

    // camelCase symbol, snake_case test: normalisation is what bridges them.
    let target = find_covering_test(dir.path(), "calculateShipping", Path::new("shipping.py"))
        .expect("the convention-named test is found");
    assert_eq!(target.name, "test_calculate_shipping_adds_handling");
}

#[test]
fn a_covering_test_in_the_same_file_is_found() {
    // Rust's dominant convention: the test lives in `mod tests` right beside
    // the code, in a file that looks nothing like a test file. Canonicalising
    // the current file before stripping the root used to drop it from the
    // search entirely (a `\\?\` path never strips), so this exact — and most
    // common — case silently found nothing.
    let dir = tempdir().expect("temp dir");
    fs::create_dir_all(dir.path().join("src")).expect("create src");
    fs::write(dir.path().join("src/headers.rs"), rust_source()).expect("write source");

    let target = find_covering_test(dir.path(), "parse_headers", Path::new("src/headers.rs"))
        .expect("the neighbouring mod tests is searched");
    assert_eq!(target.name, "parses_headers_into_lines");
    assert_eq!(target.file, PathBuf::from("src/headers.rs"));
}

#[test]
fn very_short_symbols_do_not_match_half_the_codebase() {
    let dir = tempdir().expect("temp dir");
    fs::create_dir_all(dir.path().join("tests")).expect("create tests dir");
    fs::write(
        dir.path().join("tests/test_thing.py"),
        "def test_id_is_generated():\n    assert True\n",
    )
    .expect("write test");

    assert!(
        find_covering_test(dir.path(), "id", Path::new("thing.py")).is_none(),
        "a two-letter symbol must not claim an unrelated test"
    );
}

#[test]
fn every_test_in_a_file_is_listed_in_order() {
    let tests = tests_in(rust_source(), Path::new("src/headers.rs"));
    assert_eq!(tests.len(), 1);
    assert_eq!(tests[0].name, "parses_headers_into_lines");
}

#[test]
fn a_green_exit_with_no_matching_test_is_not_a_pass() {
    // The dangerous case: the filter matched nothing, the runner exits 0, and a
    // naive reading calls it green.
    let outcome = read_outcome(
        TestRunner::Pytest,
        Some(0),
        "collected 0 items\n\n= no tests ran in 0.01s =",
        12,
    );
    assert!(!outcome.passed);
    assert_eq!(outcome.summary, "no test matched the filter");
}

#[test]
fn a_real_pass_and_a_real_failure_read_correctly() {
    let pass = read_outcome(TestRunner::Cargo, Some(0), "test result: ok. 1 passed; 0 failed", 30);
    assert!(pass.passed);
    assert_eq!(pass.summary, "passed");

    let fail = read_outcome(TestRunner::Cargo, Some(101), "test result: FAILED. 0 passed; 1 failed", 30);
    assert!(!fail.passed);
    assert_eq!(fail.summary, "failed (exit 101)");

    let killed = read_outcome(TestRunner::Cargo, None, "", 30);
    assert!(!killed.passed);
    assert_eq!(killed.summary, "killed before it finished");
}

#[test]
fn generated_code_is_pulled_out_of_the_models_fence() {
    let reply = "Sure! Here is a test:\n\n```rust\n#[test]\nfn works() {\n    assert!(true);\n}\n```\n\nHope that helps.";
    let code = extract_code_block(reply).expect("code found");
    assert!(code.starts_with("#[test]"), "got {code}");
    assert!(code.ends_with('}'), "got {code}");
    assert!(!code.contains("Hope that helps"));
}

#[test]
fn an_unfenced_reply_is_still_usable() {
    let reply = "#[test]\nfn works() {\n    assert!(true);\n}";
    let code = extract_code_block(reply).expect("code found");
    assert!(code.starts_with("#[test]"));
}

#[test]
fn generated_tests_land_where_each_ecosystem_keeps_them() {
    let dir = tempdir().expect("temp dir");
    assert_eq!(
        suggested_test_path(dir.path(), Path::new("src/headers.rs"), Some(TestRunner::Cargo)),
        PathBuf::from("src/headers_tests.rs")
    );
    assert_eq!(
        suggested_test_path(dir.path(), Path::new("src/cart.ts"), Some(TestRunner::Vitest)),
        PathBuf::from("src/cart.test.ts")
    );
    assert_eq!(
        suggested_test_path(dir.path(), Path::new("math/sum.go"), Some(TestRunner::GoTest)),
        PathBuf::from("math/sum_test.go")
    );
    // With a tests/ directory present, Python tests go there rather than beside
    // the module.
    fs::create_dir_all(dir.path().join("tests")).expect("create tests dir");
    assert_eq!(
        suggested_test_path(dir.path(), Path::new("shipping.py"), Some(TestRunner::Pytest)),
        PathBuf::from("tests/test_shipping.py")
    );
}

#[test]
fn a_cursor_in_a_test_wins_over_symbol_lookup() {
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join("Cargo.toml"), "[package]\nname='x'\n").expect("write manifest");
    fs::create_dir_all(dir.path().join("src")).expect("create src");
    fs::write(dir.path().join("src/headers.rs"), rust_source()).expect("write source");

    let subject = subject_for(dir.path(), Path::new("src/headers.rs"), rust_source(), 12);
    let BenchSubject::Runnable(plan) = subject else {
        panic!("expected a runnable test, got {subject:?}");
    };
    assert_eq!(plan.origin, TargetOrigin::AtCursor);
    assert_eq!(plan.runner, TestRunner::Cargo);
    assert_eq!(plan.target.name, "parses_headers_into_lines");
}

#[test]
fn a_symbol_with_no_test_anywhere_is_offered_to_the_generator() {
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join("Cargo.toml"), "[package]\nname='x'\n").expect("write manifest");
    fs::create_dir_all(dir.path().join("src")).expect("create src");
    let source = "pub fn untested_thing(a: u8) -> u8 {\n    a + 1\n}\n";
    fs::write(dir.path().join("src/lonely.rs"), source).expect("write source");

    let subject = subject_for(dir.path(), Path::new("src/lonely.rs"), source, 2);
    let BenchSubject::Untested { symbol, runner, .. } = subject else {
        panic!("expected an untested symbol, got {subject:?}");
    };
    assert_eq!(symbol, "untested_thing");
    assert_eq!(runner, Some(TestRunner::Cargo));
}

#[test]
fn an_unrecognised_file_says_so_instead_of_guessing() {
    let dir = tempdir().expect("temp dir");
    let subject = subject_for(dir.path(), Path::new("notes.txt"), "just text\n", 1);
    assert!(matches!(subject, BenchSubject::Nothing(_)), "got {subject:?}");
}
