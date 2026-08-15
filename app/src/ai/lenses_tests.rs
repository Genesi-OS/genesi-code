use std::fs;

use tempfile::tempdir;

use super::*;

fn titles(findings: &[Finding]) -> Vec<String> {
    findings
        .iter()
        .map(|finding| finding.title.clone())
        .collect()
}

#[test]
fn an_await_inside_a_loop_is_flagged_as_scaling() {
    let source = r#"async function load(ids) {
  const out = [];
  for (const id of ids) {
    const user = await fetchUser(id);
    out.push(user);
  }
  return out;
}"#;
    let findings = scan_performance(source);
    let awaited = findings
        .iter()
        .find(|finding| finding.title == "await inside a loop")
        .expect("the sequential-await shape is the whole point");
    assert_eq!(awaited.line, 4);
    assert_eq!(awaited.severity, Severity::Scaling);
}

#[test]
fn an_await_outside_a_loop_is_left_alone() {
    // The single most important negative: awaiting once is just awaiting.
    let source = r#"async function load(id) {
  const user = await fetchUser(id);
  return user;
}"#;
    assert!(
        !titles(&scan_performance(source)).contains(&"await inside a loop".to_string()),
        "a plain await must not be flagged"
    );
}

#[test]
fn a_loop_that_has_closed_no_longer_counts_as_enclosing() {
    // The await here is AFTER the loop. Getting this wrong would flag most
    // async functions in a codebase.
    let source = r#"async function load(ids) {
  for (const id of ids) {
    collect(id);
  }
  const summary = await summarise();
  return summary;
}"#;
    assert!(
        !titles(&scan_performance(source)).contains(&"await inside a loop".to_string()),
        "the loop had closed: {:?}",
        scan_performance(source)
    );
}

#[test]
fn a_query_inside_a_loop_is_named_as_the_n_plus_one_shape() {
    let source = r#"for (const order of orders) {
  const items = db.findMany({ orderId: order.id });
  total += items.length;
}"#;
    let findings = scan_performance(source);
    let query = findings
        .iter()
        .find(|finding| finding.title == "query inside a loop")
        .expect("N+1 detected");
    assert!(query.detail.contains("N+1"), "{}", query.detail);
}

#[test]
fn nested_loops_are_reported_and_three_deep_is_worse() {
    let two = "for (const a of xs) {\n  for (const b of ys) {\n    touch(a, b);\n  }\n}";
    assert!(titles(&scan_performance(two)).contains(&"nested loop".to_string()));

    let three = "for (const a of xs) {\n  for (const b of ys) {\n    for (const c of zs) {\n      touch(a, b, c);\n    }\n  }\n}";
    assert!(
        titles(&scan_performance(three)).contains(&"loop nested three deep".to_string()),
        "got {:?}",
        titles(&scan_performance(three))
    );
}

#[test]
fn a_regex_built_inside_a_loop_is_flagged_as_waste_not_as_scaling() {
    let source = "for (const line of lines) {\n  const re = new RegExp(pattern);\n  re.test(line);\n}";
    let findings = scan_performance(source);
    let regex = findings
        .iter()
        .find(|finding| finding.title == "regex compiled inside a loop")
        .expect("flagged");
    // It is wasteful, but it does not change the complexity class.
    assert_eq!(regex.severity, Severity::Waste);
}

#[test]
fn blocking_io_is_flagged_wherever_it_appears() {
    let source = "async function boot() {\n  const config = readFileSync('./config.json');\n  return config;\n}";
    assert!(titles(&scan_performance(source)).contains(&"blocking I/O".to_string()));
}

#[test]
fn a_spread_accumulator_in_a_reduce_is_flagged_as_quadratic() {
    let source = "const merged = items.reduce((acc, item) => [...acc, item], []);";
    let findings = scan_performance(source);
    let spread = findings
        .iter()
        .find(|finding| finding.title == "spread into an accumulator")
        .expect("flagged");
    assert_eq!(spread.severity, Severity::Scaling);
}

#[test]
fn a_clean_file_produces_nothing() {
    let source = "export function add(a, b) {\n  return a + b;\n}\n";
    let lens = PerformanceLens {
        file: PathBuf::from("a.ts"),
        findings: scan_performance(source),
    };
    assert!(lens.findings.is_empty(), "got {:?}", lens.findings);
    assert_eq!(lens.summary(), "Nothing worth flagging in this file");
}

#[test]
fn findings_come_back_worst_first() {
    let source = "for (const a of xs) {\n  const re = new RegExp(p);\n  const u = await get(a);\n}";
    let findings = scan_performance(source);
    assert!(findings.len() >= 2);
    // Scaling sorts before Waste, whatever order the lines were in.
    assert_eq!(findings[0].severity, Severity::Scaling);
}

#[test]
fn layers_are_read_from_the_path() {
    assert_eq!(layer_of(Path::new("src/routes/users.ts")), Layer::Route);
    assert_eq!(
        layer_of(Path::new("src/services/billing.ts")),
        Layer::Service
    );
    assert_eq!(layer_of(Path::new("src/models/user.rs")), Layer::Model);
    assert_eq!(layer_of(Path::new("src/utils/format.ts")), Layer::Util);
    assert_eq!(layer_of(Path::new("src/components/Cart.tsx")), Layer::Component);
    // Capitalised .tsx outside components/ is still a component by convention.
    assert_eq!(layer_of(Path::new("src/Cart.tsx")), Layer::Component);
    // A test inside services/ is a test first.
    assert_eq!(
        layer_of(Path::new("src/services/billing.test.ts")),
        Layer::Test
    );
}

#[test]
fn dependencies_are_split_by_whether_they_leave_the_project() {
    let dir = tempdir().expect("temp dir");
    fs::create_dir_all(dir.path().join("src")).expect("create src");
    fs::write(
        dir.path().join("src/cart.ts"),
        r#"import React from "react";
import debounce from "lodash/debounce";
import merge from "lodash/merge";
import { CartItem } from "./types";
import { fmt } from "../utils/format";

export function total(items) { return 0; }
export const TAX = 0.1;
"#,
    )
    .expect("write");

    let lens = architecture(dir.path(), Path::new("src/cart.ts"));
    // Two entry points into lodash are ONE dependency.
    assert_eq!(lens.external_dependencies, vec!["lodash", "react"]);
    assert_eq!(
        lens.internal_dependencies,
        vec!["../utils/format", "./types"]
    );
    assert_eq!(lens.exports, vec!["TAX", "total"]);
    assert_eq!(lens.fan_out(), 4);
}

#[test]
fn dependents_are_the_files_that_import_this_one() {
    let dir = tempdir().expect("temp dir");
    fs::create_dir_all(dir.path().join("src/pages")).expect("create dirs");
    fs::write(dir.path().join("src/billing.ts"), "export const x = 1;\n").expect("write");
    fs::write(
        dir.path().join("src/pages/checkout.ts"),
        "import { x } from \"../billing\";\n",
    )
    .expect("write");
    fs::write(
        dir.path().join("src/pages/cart.ts"),
        "import { x } from \"../billing\";\n",
    )
    .expect("write");
    fs::write(dir.path().join("src/unrelated.ts"), "export const y = 2;\n").expect("write");

    let lens = architecture(dir.path(), Path::new("src/billing.ts"));
    assert_eq!(lens.fan_in(), 2, "got {:?}", lens.dependents);
    assert!(lens.dependents.contains(&PathBuf::from("src/pages/cart.ts")));
    assert!(lens
        .dependents
        .contains(&PathBuf::from("src/pages/checkout.ts")));
    assert!(!lens.dependents.contains(&PathBuf::from("src/unrelated.ts")));
}

#[test]
fn an_index_file_never_claims_the_whole_project_as_dependents() {
    // `index` appears in half the import paths in a JS project; matching on it
    // would report every file as a dependent of every index.
    let dir = tempdir().expect("temp dir");
    fs::create_dir_all(dir.path().join("src/a")).expect("create dirs");
    fs::write(dir.path().join("src/a/index.ts"), "export const x = 1;\n").expect("write");
    fs::write(
        dir.path().join("src/b.ts"),
        "import { x } from \"./a/index\";\n",
    )
    .expect("write");

    let lens = architecture(dir.path(), Path::new("src/a/index.ts"));
    assert_eq!(lens.fan_in(), 0, "index is too common a stem to match on");
}

#[test]
fn a_file_nothing_imports_says_so_plainly() {
    let dir = tempdir().expect("temp dir");
    // It depends on something, so it is not isolated — just unused.
    fs::write(
        dir.path().join("orphan.ts"),
        "import React from \"react\";\nexport const x = 1;\n",
    )
    .expect("write");
    let lens = architecture(dir.path(), Path::new("orphan.ts"));
    assert_eq!(lens.fan_in(), 0);
    assert_eq!(lens.external_dependencies, vec!["react"]);
    assert!(
        lens.summary().contains("Nothing in the project imports"),
        "{}",
        lens.summary()
    );
}

#[test]
fn a_file_with_no_edges_at_all_reads_as_isolated() {
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join("alone.ts"), "export const x = 1;\n").expect("write");
    let lens = architecture(dir.path(), Path::new("alone.ts"));
    assert!(lens.summary().starts_with("Isolated"), "{}", lens.summary());
}
