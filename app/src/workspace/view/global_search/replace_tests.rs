use super::*;

#[test]
fn a_literal_query_is_matched_literally() {
    // `.` and `(` are regex syntax; a literal search for `foo.bar(` must not
    // treat them as such, or `fooXbar(` would be rewritten too.
    let result = replace_in_text("foo.bar( fooXbar(", "foo.bar(", "baz(", false, true).unwrap();
    assert_eq!(result.text, "baz( fooXbar(");
    assert_eq!(result.count, 1);
}

#[test]
fn a_literal_replacement_keeps_its_dollar_signs() {
    // In literal mode `$1` is text, not a capture reference.
    let result = replace_in_text("price", "price", "$1.00", false, true).unwrap();
    assert_eq!(result.text, "$1.00");
}

#[test]
fn a_regex_replacement_can_use_capture_groups() {
    let result = replace_in_text(
        "let a = 1;\nlet b = 2;",
        r"let (\w+)",
        "const $1",
        true,
        true,
    )
    .unwrap();
    assert_eq!(result.text, "const a = 1;\nconst b = 2;");
    assert_eq!(result.count, 2);
}

#[test]
fn case_insensitive_means_case_insensitive() {
    let result = replace_in_text("Foo foo FOO", "foo", "x", false, false).unwrap();
    assert_eq!(result.text, "x x x");
    assert_eq!(result.count, 3);
}

#[test]
fn case_sensitive_leaves_other_cases_alone() {
    let result = replace_in_text("Foo foo FOO", "foo", "x", false, true).unwrap();
    assert_eq!(result.text, "Foo x FOO");
}

#[test]
fn line_anchors_match_every_line_like_ripgrep() {
    // ripgrep searches line by line, so `^` is the start of each line. Run
    // over a whole file, the regex crate's default would only match the first.
    let result = replace_in_text("a\na\na", "^a", "b", true, true).unwrap();
    assert_eq!(result.text, "b\nb\nb");
}

#[test]
fn crlf_line_endings_survive() {
    let result = replace_in_text("old\r\nold\r\n", "old", "new", false, true).unwrap();
    assert_eq!(result.text, "new\r\nnew\r\n");
}

#[test]
fn no_match_returns_the_text_untouched() {
    let result = replace_in_text("nothing here", "absent", "x", false, true).unwrap();
    assert_eq!(result.count, 0);
    assert_eq!(result.text, "nothing here");
}

#[test]
fn a_pattern_that_matches_empty_text_is_refused() {
    // `a*` matches between every character; replacing it would rewrite every
    // position in every file.
    assert!(replace_in_text("abc", "a*", "x", true, true).is_err());
    assert!(replace_in_text("abc", "^", "x", true, true).is_err());
}

#[test]
fn an_invalid_regex_is_an_error_not_a_panic() {
    assert!(replace_in_text("abc", "(unclosed", "x", true, true).is_err());
}

#[test]
fn an_empty_search_is_refused() {
    assert!(replace_in_text("abc", "", "x", false, true).is_err());
}
