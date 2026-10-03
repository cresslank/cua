//! Off-screen row collapse adapted from upstream PR #3787 by Will Bogusz,
//! final commit 135165af3 (issue #3906).

use super::bindings::{copy_element_array_attr, release_all, AXUIElementRef};
use core_foundation::base::{CFEqual, CFHash, CFTypeRef};
use cua_driver_core::walk_budget::WalkBudget;
use std::collections::HashMap;

pub fn is_row_container(role: &str) -> bool {
    matches!(role, "AXTable" | "AXOutline" | "AXList" | "AXBrowser")
}

fn hides_any_rows(row_count: usize, visible_count: usize) -> bool {
    visible_count > 0 && visible_count < row_count
}

pub trait RowIdentity: Copy {
    fn hash_key(self) -> usize;
    fn same_row(self, other: Self) -> bool;
}

impl RowIdentity for AXUIElementRef {
    fn hash_key(self) -> usize {
        // SAFETY: every pointer reaching a row set is owned by the `CollapsedRows`
        // that holds the arrays it came from, so it is live for that borrow.
        unsafe { CFHash(self as CFTypeRef) as usize }
    }

    fn same_row(self, other: Self) -> bool {
        // SAFETY: as in `hash_key` — both pointers are live AX elements.
        unsafe { CFEqual(self as CFTypeRef, other as CFTypeRef) != 0 }
    }
}

pub struct RowIdentitySet<T: RowIdentity> {
    buckets: HashMap<usize, Vec<T>>,
    count: usize,
}

impl<T: RowIdentity> RowIdentitySet<T> {
    fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            count: 0,
        }
    }

    fn insert(&mut self, row: T) {
        let bucket = self.buckets.entry(row.hash_key()).or_default();
        if bucket.iter().any(|&seen| seen.same_row(row)) {
            return;
        }
        bucket.push(row);
        self.count += 1;
    }

    fn contains(&self, row: T) -> bool {
        self.buckets
            .get(&row.hash_key())
            .is_some_and(|bucket| bucket.iter().any(|&seen| seen.same_row(row)))
    }

    fn count(&self) -> usize {
        self.count
    }
}

fn rows_not_shown<T: RowIdentity>(rows: &[T], visible: &[T], selected: &[T]) -> RowIdentitySet<T> {
    // Cardinality alone cannot prove a subset when an app returns stale or
    // duplicate proxies. Only collapse rows when AX identity proves it.
    let mut all = RowIdentitySet::new();
    for &row in rows {
        all.insert(row);
    }
    let mut visible_set = RowIdentitySet::new();
    for &row in visible {
        if !all.contains(row) {
            return RowIdentitySet::new();
        }
        visible_set.insert(row);
    }
    if !hides_any_rows(all.count(), visible_set.count()) {
        return RowIdentitySet::new();
    }
    let mut shown = RowIdentitySet::new();
    for &row in visible.iter().chain(selected) {
        shown.insert(row);
    }
    let mut hidden = RowIdentitySet::new();
    for &row in rows {
        if !shown.contains(row) {
            hidden.insert(row);
        }
    }
    hidden
}

pub struct CollapsedRows {
    rows: Vec<AXUIElementRef>,
    hidden: RowIdentitySet<AXUIElementRef>,
    total: usize,
}

impl CollapsedRows {
    pub fn hides(&self, row: AXUIElementRef) -> bool {
        self.hidden.contains(row)
    }

    pub fn count(&self) -> usize {
        self.hidden.count()
    }

    pub fn total(&self) -> usize {
        self.total
    }
}

impl Drop for CollapsedRows {
    fn drop(&mut self) {
        // SAFETY: `copy_element_array_attr` retained each row once and the
        // hidden set only borrows those pointers, so this releases them once.
        unsafe { release_all(std::mem::take(&mut self.rows)) };
    }
}

/// # Safety
///
/// `element` must be valid, and must stay valid while the returned
/// [`CollapsedRows`] is alive.
pub unsafe fn collapse_offscreen_rows(
    element: AXUIElementRef,
    role: &str,
    budget: &mut WalkBudget,
) -> Option<CollapsedRows> {
    if !is_row_container(role) || row_budget_expired(budget) {
        return None;
    }
    let rows = copy_element_array_attr(element, "AXRows")?;
    if row_budget_expired(budget) {
        release_all(rows);
        return None;
    }
    let visible = copy_element_array_attr(element, "AXVisibleRows").unwrap_or_default();
    if row_budget_expired(budget) || !hides_any_rows(rows.len(), visible.len()) {
        release_all(rows);
        release_all(visible);
        return None;
    }
    let selected = copy_element_array_attr(element, "AXSelectedRows").unwrap_or_default();
    if row_budget_expired(budget) {
        release_all(rows);
        release_all(visible);
        release_all(selected);
        return None;
    }
    let hidden = rows_not_shown(&rows, &visible, &selected);
    release_all(visible);
    release_all(selected);
    if hidden.count() == 0 {
        release_all(rows);
        return None;
    }
    let total = rows.len();
    Some(CollapsedRows {
        rows,
        hidden,
        total,
    })
}

fn row_budget_expired(budget: &mut WalkBudget) -> bool {
    if budget.expired() {
        budget.stop_for_timeout();
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    struct FakeRow {
        bucket: usize,
        row: u32,
    }

    impl RowIdentity for FakeRow {
        fn hash_key(self) -> usize {
            self.bucket
        }

        fn same_row(self, other: Self) -> bool {
            self.row == other.row
        }
    }

    fn rows_sharing_one_bucket(count: u32) -> Vec<FakeRow> {
        (0..count).map(|row| FakeRow { bucket: 0, row }).collect()
    }

    #[test]
    fn rows_the_container_shows_survive_a_hash_collision() {
        let rows = rows_sharing_one_bucket(8);
        let visible = vec![rows[3], rows[4]];

        let hidden = rows_not_shown(&rows, &visible, &[]);

        assert_eq!(hidden.count(), 6);
        assert!(!hidden.contains(rows[3]));
        assert!(!hidden.contains(rows[4]));
        assert!(hidden.contains(rows[0]));
        assert!(hidden.contains(rows[7]));
    }

    #[test]
    fn a_selected_row_is_kept_even_when_it_is_off_screen() {
        let rows = rows_sharing_one_bucket(8);
        let visible = vec![rows[0]];
        let selected = vec![rows[6], rows[0]];

        let hidden = rows_not_shown(&rows, &visible, &selected);

        assert_eq!(hidden.count(), 6);
        assert!(!hidden.contains(rows[6]));
    }

    #[test]
    fn identity_not_pointer_decides_membership() {
        let rows = rows_sharing_one_bucket(3);
        let restated_row = FakeRow {
            bucket: 0,
            row: rows[1].row,
        };

        let hidden = rows_not_shown(&rows, &[restated_row], &[]);

        assert_eq!(hidden.count(), 2);
        assert!(!hidden.contains(rows[1]));
    }

    #[test]
    fn only_a_partial_visible_set_collapses_anything() {
        assert!(hides_any_rows(667, 19));
        assert!(!hides_any_rows(667, 0));
        assert!(!hides_any_rows(667, 667));
        assert!(!hides_any_rows(19, 19));
        assert!(!hides_any_rows(0, 0));
    }

    #[test]
    fn fully_visible_list_collapses_nothing() {
        let rows = rows_sharing_one_bucket(8);
        let hidden = rows_not_shown(&rows, &rows, &[]);
        assert_eq!(hidden.count(), 0);
        assert!(rows.iter().all(|&row| !hidden.contains(row)));
    }

    #[test]
    fn stale_visible_rows_do_not_prove_a_subset() {
        let rows = rows_sharing_one_bucket(8);
        let visible = [rows[0], FakeRow { bucket: 0, row: 99 }];
        assert_eq!(rows_not_shown(&rows, &visible, &[]).count(), 0);
    }

    #[test]
    fn expired_budget_does_not_read_a_container() {
        let mut budget = WalkBudget::new(0, 100);
        assert!(!budget.admit());
        // Null is never dereferenced: budget refusal must precede native work.
        assert!(
            unsafe { collapse_offscreen_rows(std::ptr::null_mut(), "AXList", &mut budget) }
                .is_none()
        );
        assert_eq!(budget.outcome().reason(), Some("timeout"));
    }

    #[test]
    fn only_row_modelling_containers_are_asked_about_visibility() {
        for role in ["AXTable", "AXOutline", "AXList", "AXBrowser"] {
            assert!(is_row_container(role), "{role} models rows");
        }
        for role in ["AXGroup", "AXScrollArea", "AXWindow", "AXRow", "AXCell"] {
            assert!(!is_row_container(role), "{role} does not model rows");
        }
    }
}
