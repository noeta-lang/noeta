//! The **observed-order sort** both engines run.
//!
//! A list is sorted under one of two comparators. Ordinarily it is the runtime's own structural
//! order, which is a total order by construction — the fields of a value, compared in declared
//! order, all the way down. But a type may write its own `compare` (`impl Comparable`), and then
//! *that* is what every observed order of its values means, so the sort has to call back into user
//! code for each comparison.
//!
//! A user comparator is not a total order, because nothing can make it one: a `compare` that
//! reports both `a < b` and `b < a` is a program the language accepts and runs. `slice::sort_by` is
//! documented as permitted to **panic** on such a comparator, and a panic inside the sort would
//! take the process down with a message about a Rust invariant rather than about the program.
//! [`stable_order_by`] is a plain bottom-up merge sort instead: it never inspects its comparator's
//! consistency, so an inconsistent one yields *some* permutation of the input and nothing else
//! happens. That permutation is deterministic — the same merge order for the same input on both
//! engines — which is what keeps the differential meaningful.
//!
//! It answers with the **permutation** rather than reordering in place, because the two engines
//! hold their elements differently (the VM's are refcounted words it must retain as it copies) and
//! a permutation is the one answer both can apply.

use std::cmp::Ordering;

/// The **stable** ordering of `0..items.len()` under `cmp`, without ever assuming `cmp` is a total
/// order — `out[k]` is the index of the element that belongs in position `k`.
///
/// Bottom-up merge sort: runs of 1 are merged into runs of 2, 4, 8, … Ties (`Ordering::Equal`, and
/// every answer `cmp` declines to give) keep the earlier element, which is what makes it stable.
/// Both engines call this one body, so a user `compare` orders a list identically under the
/// reference interpreter and the VM even when it is inconsistent.
pub fn stable_order_by<T>(items: &[T], mut cmp: impl FnMut(&T, &T) -> Ordering) -> Vec<usize> {
    let n = items.len();
    let mut order: Vec<usize> = (0..n).collect();
    if n < 2 {
        return order;
    }
    let mut scratch: Vec<usize> = order.clone();
    let mut width = 1;
    while width < n {
        let mut start = 0;
        while start < n {
            let mid = (start + width).min(n);
            let end = (start + 2 * width).min(n);
            merge(
                &order[start..mid],
                &order[mid..end],
                &mut scratch[start..end],
                items,
                &mut cmp,
            );
            start = end;
        }
        order.copy_from_slice(&scratch);
        width *= 2;
    }
    order
}

/// Merge two adjacent sorted runs of indices into `out`, taking from `left` on a tie so the sort is
/// stable.
fn merge<T>(
    left: &[usize],
    right: &[usize],
    out: &mut [usize],
    items: &[T],
    cmp: &mut impl FnMut(&T, &T) -> Ordering,
) {
    let (mut i, mut j, mut k) = (0, 0, 0);
    while i < left.len() && j < right.len() {
        if cmp(&items[right[j]], &items[left[i]]) == Ordering::Less {
            out[k] = right[j];
            j += 1;
        } else {
            out[k] = left[i];
            i += 1;
        }
        k += 1;
    }
    for &idx in &left[i..] {
        out[k] = idx;
        k += 1;
    }
    for &idx in &right[j..] {
        out[k] = idx;
        k += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apply a permutation, for the assertions below.
    fn permute<T: Copy>(items: &[T], order: &[usize]) -> Vec<T> {
        order.iter().map(|&i| items[i]).collect()
    }

    #[test]
    fn sorts_and_keeps_equal_elements_in_input_order() {
        // Sorted on the first component; the second says where each element started, so a stable
        // sort leaves the pairs sharing a key in ascending second-component order.
        let items = [(2, 0), (1, 1), (2, 2), (0, 3), (1, 4), (2, 5)];
        let order = stable_order_by(&items, |a, b| a.0.cmp(&b.0));
        assert_eq!(
            permute(&items, &order),
            [(0, 3), (1, 1), (1, 4), (2, 0), (2, 2), (2, 5)]
        );
    }

    #[test]
    fn an_inconsistent_comparator_permutes_rather_than_panicking() {
        // "Everything is less than everything" is not a total order; `slice::sort_by` is allowed to
        // panic on it. This must simply return, with every input element present exactly once.
        let items = [3, 1, 4, 1, 5, 9, 2, 6];
        let order = stable_order_by(&items, |_, _| Ordering::Less);
        let mut seen = order.clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..items.len()).collect::<Vec<_>>());
    }

    #[test]
    fn already_sorted_and_reversed_inputs_both_land_sorted() {
        for input in [[1, 2, 3, 4, 5, 6, 7], [7, 6, 5, 4, 3, 2, 1]] {
            let order = stable_order_by(&input, |a: &i32, b: &i32| a.cmp(b));
            assert_eq!(permute(&input, &order), [1, 2, 3, 4, 5, 6, 7]);
        }
    }

    #[test]
    fn an_empty_or_single_list_is_the_identity() {
        let empty: [i32; 0] = [];
        assert!(stable_order_by(&empty, |a: &i32, b: &i32| a.cmp(b)).is_empty());
        assert_eq!(stable_order_by(&[42], |a: &i32, b: &i32| a.cmp(b)), vec![0]);
    }

    /// The permutation is a function of the input and the comparator alone — no randomness, no
    /// length-dependent algorithm switch — so both engines running one program agree element for
    /// element even when the comparator is inconsistent.
    #[test]
    fn the_permutation_is_deterministic() {
        let items: Vec<i32> = (0..64).map(|i| (i * 37) % 11).collect();
        let odd_cmp = |a: &i32, b: &i32| (a % 3).cmp(&(b % 3)).reverse();
        let first = stable_order_by(&items, odd_cmp);
        assert_eq!(first, stable_order_by(&items, odd_cmp));
    }
}
