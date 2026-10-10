//! Project-wide replace.
//!
//! Search runs ripgrep over the files on disk; replace has to find exactly what
//! that search found. It does so by re-applying the same pattern, built the
//! same way, to each file's current contents -- rather than trusting the byte
//! offsets in the search results, which go stale the moment a file is edited
//! after the search ran.

use anyhow::{anyhow, Result};
use regex::{NoExpand, RegexBuilder};

/// The outcome of replacing in one piece of text.
#[derive(Debug, PartialEq, Eq)]
pub struct Replaced {
    pub text: String,
    pub count: usize,
}

/// Replace every match of `pattern` in `text`.
///
/// Mirrors the search's own rules: a literal query is escaped, a regex is used
/// as written, and turning case sensitivity off means plain case-insensitive
/// (not smart case). `^` and `$` match at every line, as they do in ripgrep's
/// line-oriented search, even though this runs over the whole file at once.
///
/// In regex mode the replacement may use `$1` / `${name}` for capture groups,
/// as in VS Code. In literal mode it is inserted exactly as typed, so a `$` in
/// it stays a `$`.
pub fn replace_in_text(
    text: &str,
    pattern: &str,
    replacement: &str,
    use_regex: bool,
    case_sensitive: bool,
) -> Result<Replaced> {
    let regex = build_pattern(pattern, use_regex, case_sensitive)?;
    let count = regex.find_iter(text).count();
    if count == 0 {
        return Ok(Replaced {
            text: text.to_string(),
            count: 0,
        });
    }
    let text = if use_regex {
        regex.replace_all(text, replacement).into_owned()
    } else {
        regex.replace_all(text, NoExpand(replacement)).into_owned()
    };
    Ok(Replaced { text, count })
}

/// Build the pattern the way the search does, and refuse one that would match
/// nothing in particular.
pub fn build_pattern(pattern: &str, use_regex: bool, case_sensitive: bool) -> Result<regex::Regex> {
    if pattern.is_empty() {
        return Err(anyhow!("Nothing to replace: the search is empty."));
    }
    let source = if use_regex {
        pattern.to_string()
    } else {
        regex::escape(pattern)
    };
    let regex = RegexBuilder::new(&source)
        .case_insensitive(!case_sensitive)
        .multi_line(true)
        .build()
        .map_err(|e| anyhow!("That search isn't a valid regular expression: {e}"))?;
    // A pattern that matches the empty string (`a*`, `\b`, `^`) matches between
    // every pair of characters. Replacing with it would insert the replacement
    // all through every file, which is never what was meant.
    if regex.is_match("") {
        return Err(anyhow!(
            "That pattern can match empty text, so replacing it would touch every position in every file."
        ));
    }
    Ok(regex)
}

#[cfg(test)]
#[path = "replace_tests.rs"]
mod tests;
