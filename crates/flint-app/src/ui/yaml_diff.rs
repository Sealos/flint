//! A review's YAML diff, shared by the detail pane's edit and the fleet apply's per-member plan.
//! The texts are what a review shows: `status` and `managedFields` left out, Secret values
//! hidden by the core.

use std::time::Duration;

/// The longest a diff may take: past it, the diff is coarser but still correct.
const DIFF_DEADLINE: Duration = Duration::from_secs(1);

fn diff<'a>(before: &'a str, after: &'a str) -> similar::TextDiff<'a, 'a, str> {
    similar::TextDiff::configure()
        .timeout(DIFF_DEADLINE)
        .diff_lines(before, after)
}

/// The change as a unified patch, or `None` if it changes nothing. Off the UI thread: a large
/// value replaced can take long.
pub(in crate::ui) fn patch(name: &str, before: &str, after: &str) -> Option<String> {
    if before == after {
        return None;
    }
    let patch = diff(before, after)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{name}.yaml"), &format!("b/{name}.yaml"))
        .to_string();
    Some(patch)
}

/// The lines added and removed.
pub(in crate::ui) fn counts(before: &str, after: &str) -> (usize, usize) {
    let diff = diff(before, after);
    let count = |tag| diff.iter_all_changes().filter(|c| c.tag() == tag).count();
    (
        count(similar::ChangeTag::Insert),
        count(similar::ChangeTag::Delete),
    )
}

/// The change as its hunks' lines ("@@", then " ", "+" or "-" and the line), at most `max` of
/// them, then how many more there are. Empty if it changes nothing.
pub(in crate::ui) fn hunk_lines(before: &str, after: &str, max: usize) -> Vec<String> {
    let Some(patch) = patch("object", before, after) else {
        return Vec::new();
    };
    let lines: Vec<&str> = patch
        .lines()
        .filter(|line| !line.starts_with("--- ") && !line.starts_with("+++ "))
        .collect();
    let mut shown: Vec<String> = lines.iter().take(max).map(|l| (*l).to_owned()).collect();
    if lines.len() > max {
        shown.push(format!("… {} more lines", lines.len() - max));
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_review_without_a_change_has_no_diff() {
        assert!(patch("a", "x: 1\n", "x: 1\n").is_none());
        assert!(patch("a", "x: 1\n", "x: 2\n").is_some());
        assert!(hunk_lines("x: 1\n", "x: 1\n", 10).is_empty());
    }

    #[test]
    fn hunks_show_each_line_and_are_capped() {
        assert_eq!(counts("a: 1\nb: 2\n", "a: 3\n"), (1, 2));
        assert_eq!(
            hunk_lines("a: 1\nb: 2\n", "a: 3\n", 10),
            ["@@ -1,2 +1 @@", "-a: 1", "-b: 2", "+a: 3"]
        );
        assert_eq!(
            hunk_lines("a: 1\nb: 2\n", "a: 3\n", 2),
            ["@@ -1,2 +1 @@", "-a: 1", "… 2 more lines"]
        );
    }
}
