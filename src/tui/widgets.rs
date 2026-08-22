use ratatui::layout::Rect;

/// Fuzzy-filters `items` by `query`, returning indices into `items` in
/// best-match-first order. `text_of` extracts the matchable text for each
/// item (never the same as the rendered columns — e.g. the repo screen
/// matches against `"owner/name"` even though `OWNER`/`NAME` render as
/// separate table cells).
///
/// Empty query short-circuits to the identity ordering (`0..items.len()`)
/// rather than round-tripping through frizbee: "no filter typed yet" must
/// show the caller's own ordering (repo recency, worktree scan order)
/// untouched, not whatever an empty-pattern match happens to produce.
pub fn filter_indices<T>(items: &[T], text_of: impl Fn(&T) -> &str, query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..items.len()).collect();
    }
    let haystacks: Vec<&str> = items.iter().map(&text_of).collect();
    let mut matcher = frizbee::Matcher::new(query, &frizbee::Config::default());
    matcher
        .match_list(&haystacks)
        .into_iter()
        .map(|m| m.index as usize)
        .collect()
}

/// Resolves a mouse click at terminal coordinates `(click_col, click_row)`
/// to a row index into the *filtered* item list, given the table's
/// last-rendered content `Rect` (header row first, data rows after) and the
/// current scroll `offset` (`TableState::offset()`). Pure and independent of
/// any live terminal — the regression test drives it directly against a
/// fixture `Rect`.
///
/// Returns `None` for a click outside the table's columns, on the header
/// row, or past the last actually-rendered row.
pub fn row_at(table_area: Rect, offset: usize, click_col: u16, click_row: u16) -> Option<usize> {
    if table_area.width == 0 || table_area.height == 0 {
        return None;
    }
    if click_col < table_area.x || click_col >= table_area.x + table_area.width {
        return None;
    }
    if click_row <= table_area.y {
        return None; // on, or above, the header row
    }
    let body_row = click_row - table_area.y - 1; // 0-based within the visible body
    if body_row >= table_area.height.saturating_sub(1) {
        return None; // below the last visible row
    }
    Some(offset + body_row as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_area() -> Rect {
        // x=2,y=1: a table drawn inside a bordered block starting at (1,1),
        // so the inner content area (what `row_at` receives) starts one
        // cell in from the border. height=6 => header + 5 visible rows.
        Rect {
            x: 2,
            y: 1,
            width: 20,
            height: 6,
        }
    }

    #[test]
    fn row_at_resolves_within_table_bounds() {
        let area = table_area();
        // First data row sits directly under the header.
        assert_eq!(row_at(area, 0, 5, 2), Some(0));
        // Third visible data row.
        assert_eq!(row_at(area, 0, 5, 4), Some(2));
        // Last visible row (height=6 => header at row 1, body rows 2..=6).
        assert_eq!(row_at(area, 0, 5, 6), Some(4));
    }

    #[test]
    fn row_at_rejects_header_and_out_of_bounds() {
        let area = table_area();
        assert_eq!(row_at(area, 0, 5, 1), None, "header row itself");
        assert_eq!(row_at(area, 0, 5, 0), None, "above the table entirely");
        assert_eq!(row_at(area, 0, 5, 7), None, "past the last visible row");
        assert_eq!(row_at(area, 0, 1, 2), None, "left of the table");
        assert_eq!(row_at(area, 0, 22, 2), None, "right of the table");
    }

    #[test]
    fn row_at_accounts_for_scroll_offset() {
        let area = table_area();
        // Scrolled down 10 rows: clicking the first visible data row now
        // resolves to item index 10, not 0.
        assert_eq!(row_at(area, 10, 5, 2), Some(10));
        assert_eq!(row_at(area, 10, 5, 4), Some(12));
    }

    #[test]
    fn filter_indices_empty_query_is_identity_order() {
        let items = vec!["zebra", "apple", "mango"];
        assert_eq!(filter_indices(&items, |s| s, ""), vec![0, 1, 2]);
    }

    #[test]
    fn filter_indices_matches_and_excludes() {
        let items = vec!["owner/repo-alpha", "owner/repo-beta", "other/thing"];
        let hits = filter_indices(&items, |s| s, "alpha");
        assert_eq!(hits, vec![0]);

        let none = filter_indices(&items, |s| s, "zzz-nomatch-zzz");
        assert!(none.is_empty());
    }

    #[test]
    fn filter_indices_matches_noncontiguous_subsequence() {
        // Pins frizbee::Config::default() to actual fuzzy (subsequence)
        // matching, not plain substring matching — "orc" has no contiguous
        // occurrence in "owner/repo-cw" but is a subsequence of it. This is
        // the entire UX skim provided; a config change that narrows it to
        // substring-only would pass every other test here silently.
        let items = vec!["owner/repo-cw"];
        let hits = filter_indices(&items, |s| s, "orc");
        assert_eq!(
            hits,
            vec![0],
            "must match a non-contiguous subsequence, not just a substring"
        );
    }
}
