// SPDX-License-Identifier: GPL-3.0-or-later
//! Layer order for a selection of strokes: bring forward, send backward, bring to front, send
//! to back. A page draws its strokes in order, so this is a reordering of the page's list;
//! the history applies it as `ReplaceStrokes` on the positions that change.

/// Where the selection goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrange {
    /// Above the next stroke it overlaps.
    Forward,
    /// Below the previous stroke it overlaps.
    Backward,
    /// Above everything on the page.
    Front,
    /// Below everything on the page.
    Back,
}

/// A new order for `len` strokes with the ascending indices `selected`. `overlaps(i)` says
/// whether unselected stroke `i` overlaps the selection: one step forward or backward passes
/// the nearest such stroke, since passing one it doesn't touch would change nothing on the
/// page. The selection moves as a group, keeping its own order.
///
/// Returns `order` (the stroke now at position `p` was at `order[p]`) and the selection's new
/// indices, or `None` if nothing would move.
pub fn arrange(
    len: usize,
    selected: &[usize],
    overlaps: impl Fn(usize) -> bool,
    how: Arrange,
) -> Option<(Vec<usize>, Vec<usize>)> {
    let mut is_selected = vec![false; len];
    for &i in selected {
        *is_selected.get_mut(i)? = true;
    }
    let (&first, &last) = (selected.first()?, selected.last()?);
    let others = |range: std::ops::Range<usize>| range.filter(|&i| !is_selected[i]);
    let chosen = selected.iter().copied();
    let order: Vec<usize> = match how {
        Arrange::Front => others(0..len).chain(chosen).collect(),
        Arrange::Back => chosen.chain(others(0..len)).collect(),
        Arrange::Forward => {
            let k = others(last + 1..len).find(|&i| overlaps(i))?;
            others(0..k + 1)
                .chain(chosen)
                .chain(others(k + 1..len))
                .collect()
        }
        Arrange::Backward => {
            let k = others(0..first).rev().find(|&i| overlaps(i))?;
            others(0..k).chain(chosen).chain(others(k..len)).collect()
        }
    };
    if order.iter().enumerate().all(|(p, &i)| p == i) {
        return None;
    }
    let moved = (0..len).filter(|&p| is_selected[order[p]]).collect();
    Some((order, moved))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(len: usize, sel: &[usize], touching: &[usize], how: Arrange) -> Option<Vec<usize>> {
        let (order, moved) = arrange(len, sel, |i| touching.contains(&i), how)?;
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..len).collect::<Vec<_>>(), "a permutation");
        let expect: Vec<usize> = moved.iter().map(|&p| order[p]).collect();
        assert_eq!(
            expect, sel,
            "moved indices point at the selection, in order"
        );
        Some(order)
    }

    #[test]
    fn front_and_back() {
        let all = [0, 1, 2, 3, 4];
        assert_eq!(
            run(5, &[1, 3], &all, Arrange::Front),
            Some(vec![0, 2, 4, 1, 3])
        );
        assert_eq!(
            run(5, &[1, 3], &all, Arrange::Back),
            Some(vec![1, 3, 0, 2, 4])
        );
        assert_eq!(run(5, &[3, 4], &all, Arrange::Front), None);
        assert_eq!(run(5, &[0, 1], &all, Arrange::Back), None);
    }

    #[test]
    fn forward_passes_the_next_overlapping_stroke() {
        // 3 doesn't touch the selection, 4 does.
        assert_eq!(
            run(6, &[1, 2], &[0, 4, 5], Arrange::Forward),
            Some(vec![0, 3, 4, 1, 2, 5])
        );
        // Nothing above touches it.
        assert_eq!(run(4, &[1], &[0], Arrange::Forward), None);
    }

    #[test]
    fn backward_passes_the_previous_overlapping_stroke() {
        assert_eq!(
            run(6, &[3, 4], &[0, 1, 5], Arrange::Backward),
            Some(vec![0, 3, 4, 1, 2, 5])
        );
        assert_eq!(run(4, &[2], &[3], Arrange::Backward), None);
    }

    #[test]
    fn gaps_in_the_selection_close_up() {
        // Unselected 2 sat between selected 1 and 3; stepping forward past 4 groups them.
        assert_eq!(
            run(5, &[1, 3], &[4], Arrange::Forward),
            Some(vec![0, 2, 4, 1, 3])
        );
    }

    #[test]
    fn bad_input() {
        assert_eq!(arrange(3, &[], |_| true, Arrange::Front), None);
        assert_eq!(arrange(3, &[5], |_| true, Arrange::Front), None);
    }
}
