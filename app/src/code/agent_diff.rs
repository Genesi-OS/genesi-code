//! Keeping an agent's diff on screen while the user edits the same file.
//!
//! The editor shows an agent edit as a diff against a base: the file as it
//! was before the agent touched it. When the user then types, their text is
//! also "different from the base", so without help it would show up green
//! like the agent's, and the agent's own changes could no longer be told apart.
//! The fix is to carry each edit the user makes over to the base as well: the
//! base then follows the user and only the agent's changes stay marked.

use std::ops::Range;
use std::time::Duration;

use similar::{Algorithm, DiffOp, TextDiff};

/// How long either diff may run before it settles for a coarser answer. A
/// coarser answer is still correct, it just keeps a little more marked.
const DIFF_TIMEOUT: Duration = Duration::from_millis(50);

/// Apply the user's edit (`before` -> `after`) to `base` wherever it lands
/// in text the agent did not change.
///
/// An edit inside the agent's changes is left out of the base, so it reads as
/// part of that change: accepting the change keeps it, rejecting the change
/// drops it, as it would if the agent had written it. All three texts are
/// expected with LF line endings.
pub fn carry_user_edit(base: &str, before: &str, after: &str) -> String {
    if before == after {
        return base.to_string();
    }
    let regions = unchanged_regions(base, before);
    let mut carried: Vec<(Range<usize>, &str)> = user_edits(before, after)
        .into_iter()
        .filter_map(|(range, text)| {
            let region = regions
                .iter()
                .find(|region| region.contains(&range, before.len()))?;
            let start = region.base_start + (range.start - region.before.start);
            Some((start..start + range.len(), text))
        })
        .collect();
    if carried.is_empty() {
        return base.to_string();
    }

    // Back to front, so the ranges still to apply are not shifted.
    carried.sort_by(|a, b| b.0.start.cmp(&a.0.start));
    let mut base = base.to_string();
    for (range, text) in carried {
        base.replace_range(range, text);
    }
    base
}

/// A run of whole lines that is the same in the base and in the buffer.
struct UnchangedRegion {
    base_start: usize,
    /// Byte range in the buffer.
    before: Range<usize>,
}

impl UnchangedRegion {
    /// Whether an edit of `range` (in the buffer) falls entirely in this region.
    ///
    /// A region's end is the start of the next line, which belongs to whatever
    /// comes next: typing at the start of a line the agent added extends the
    /// agent's line. The end of the file is the exception, as nothing follows.
    fn contains(&self, range: &Range<usize>, before_len: usize) -> bool {
        self.before.start <= range.start
            && range.end <= self.before.end
            && (range.start < self.before.end || range.start == before_len)
    }
}

fn unchanged_regions(base: &str, before: &str) -> Vec<UnchangedRegion> {
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .timeout(DIFF_TIMEOUT)
        .diff_lines(base, before);
    let base_offsets = offsets(diff.old_slices().iter().map(|line| line.len()));
    let before_offsets = offsets(diff.new_slices().iter().map(|line| line.len()));

    diff.ops()
        .iter()
        .filter_map(|op| match *op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => Some(UnchangedRegion {
                base_start: base_offsets[old_index],
                before: before_offsets[new_index]..before_offsets[new_index + len],
            }),
            _ => None,
        })
        .collect()
}

/// The places `after` differs from `before`: each a byte range of `before` and
/// what replaced it. Several of them come from multiple cursors.
fn user_edits<'a>(before: &str, after: &'a str) -> Vec<(Range<usize>, &'a str)> {
    // Most edits are a keystroke in a large file: strip what is shared at both
    // ends, so the character diff only looks at what actually changed.
    let prefix = common_prefix(before, after);
    let suffix = common_suffix(&before[prefix..], &after[prefix..]);
    let old = &before[prefix..before.len() - suffix];
    let new = &after[prefix..after.len() - suffix];

    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .timeout(DIFF_TIMEOUT)
        .diff_chars(old, new);
    let old_offsets = offsets(diff.old_slices().iter().map(|c| c.len()));
    let new_offsets = offsets(diff.new_slices().iter().map(|c| c.len()));

    let mut edits = Vec::new();
    let mut pending: Option<(Range<usize>, Range<usize>)> = None;
    for op in diff.ops() {
        if let DiffOp::Equal { .. } = op {
            if let Some((old_range, new_range)) = pending.take() {
                edits.push((old_range, new_range));
            }
            continue;
        }
        let old_range = old_offsets[op.old_range().start]..old_offsets[op.old_range().end];
        let new_range = new_offsets[op.new_range().start]..new_offsets[op.new_range().end];
        pending = Some(match pending.take() {
            Some((o, n)) => (o.start..old_range.end, n.start..new_range.end),
            None => (old_range, new_range),
        });
    }
    edits.extend(pending);

    edits
        .into_iter()
        .map(|(old_range, new_range)| {
            (
                prefix + old_range.start..prefix + old_range.end,
                &new[new_range],
            )
        })
        .collect()
}

/// Running byte offsets: entry `i` is where token `i` starts, and the last
/// entry is the total length.
fn offsets(lengths: impl Iterator<Item = usize>) -> Vec<usize> {
    let mut offsets = vec![0];
    let mut total = 0;
    for len in lengths {
        total += len;
        offsets.push(total);
    }
    offsets
}

/// Length in bytes of the shared start, on a character boundary.
fn common_prefix(a: &str, b: &str) -> usize {
    a.char_indices()
        .zip(b.chars())
        .find(|((_, x), y)| x != y)
        .map(|((i, _), _)| i)
        .unwrap_or_else(|| a.len().min(b.len()))
}

/// Length in bytes of the shared end, on a character boundary.
fn common_suffix(a: &str, b: &str) -> usize {
    a.chars()
        .rev()
        .zip(b.chars().rev())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.len_utf8())
        .sum()
}

#[cfg(test)]
#[path = "agent_diff_tests.rs"]
mod tests;
