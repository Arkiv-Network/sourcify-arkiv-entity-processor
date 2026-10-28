//! Paging contracts by `matchid` windows (docs/udc-proposal.md, "Paging"), the rule both nodes run.
//!
//! Neither node sorts: our node returns matches in creation order (record ID), Jalebi in block order and, within a
//! block, by transaction and key. A contract patched in place (a re-verification under a new `matchId`) keeps its
//! creation position on both (measured), so node order is not `matchid` order and a key set cannot rely on it.
//! The rule relies on one property only, which both nodes give whatever their order:
//!
//! > a window query `… AND matchid > u64(A) AND matchid <= u64(B)` that returns fewer than `limit` rows, or no
//! > cursor, returned every match in that window.
//!
//! A page of `page` contracts after `A` (ascending): query the window `(A, A + width]`. If it is complete, its rows
//! sorted by `matchid` are exact; take what the page needs and, while it is short, move to the next window with a
//! wider width. If it is full (incomplete), its `need`-th smallest `matchid` `M` bounds a window `(A, M]` that holds
//! the `need` smallest matches overall (a subset's k-th smallest is never below the whole set's), so query that
//! instead. `M` is strictly below `B` while `need < limit`, so the walk ends. Descending pages mirror it with
//! `matchid < u64(B) AND matchid >= u64(A)`. Cursors and node order are never used across queries.
use anyhow::{ensure, Result};

/// One match: its `matchid` and entity key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub matchid: u64,
    pub key: [u8; 32],
}
/// A window: the bounds of `matchid` (`lower <= matchid <= upper`, both inclusive; `None` is open).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub lower: Option<u64>,
    pub upper: Option<u64>,
}
impl Window {
    /// The window as query clauses (both nodes: `>=`/`<=` pairs, never `BETWEEN`).
    pub fn clauses(&self) -> String {
        let mut out = vec![];
        if let Some(l) = self.lower {
            out.push(format!("matchid >= u64({l})"));
        }
        if let Some(u) = self.upper {
            out.push(format!("matchid <= u64({u})"));
        }
        out.join(" AND ")
    }
}
/// Runs one window query: its rows (in any order) and whether the response was complete (fewer
/// than `limit` rows, or no cursor).
pub type Query<'a> = dyn FnMut(Window, usize) -> Result<(Vec<Row>, bool)> + 'a;
/// A page and where the next one starts: `next` is the last row's `matchid` (exclusive bound of the
/// next page), None at the end; `width` is a window width that suits the next page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub rows: Vec<Row>,
    pub next: Option<u64>,
    pub width: u64,
    pub queries: usize,
}

/// Default first window width (in `matchid` units); later pages pass the previous page's width.
pub const WIDTH: u64 = 1 << 16;
/// Windows are open beyond this `matchid` (2^40, above 10^12): the mirror keeps Sourcify's match
/// IDs below 10^12 (higher ones are a node's own).
pub const TOP: u64 = 1 << 40;

/// The `size` matches with the smallest `matchid` above `after` (None: from the start), ascending.
pub fn ascending(
    query: &mut Query,
    after: Option<u64>,
    size: usize,
    limit: usize,
    width: u64,
) -> Result<Page> {
    walk(query, after, size, limit, width, true)
}
/// The `size` matches with the largest `matchid` below `before` (None: from the top), descending.
pub fn descending(
    query: &mut Query,
    before: Option<u64>,
    size: usize,
    limit: usize,
    width: u64,
) -> Result<Page> {
    walk(query, before, size, limit, width, false)
}

fn walk(
    query: &mut Query,
    from: Option<u64>,
    size: usize,
    limit: usize,
    width: u64,
    up: bool,
) -> Result<Page> {
    ensure!(
        size >= 1 && size < limit,
        "a page must be smaller than the query limit"
    );
    let mut width = width.max(1);
    let mut rows: Vec<Row> = vec![];
    let mut queries = 0;
    // The next unread value (inclusive), moving up or down.
    let mut near = match (up, from) {
        (true, None) => Some(0),
        (true, Some(a)) => a.checked_add(1),
        (false, None) => Some(u64::MAX),
        (false, Some(b)) => b.checked_sub(1),
    };
    while let Some(start) = near {
        let need = size - rows.len();
        // The window's far end is open once it reaches past TOP (ascending; no match ID is expected
        // beyond it) or 0 (descending), so the last window ends the walk in one query wherever the
        // last match is. The near end is always the cursor's.
        let mut window = if up {
            let far = start.saturating_add(width - 1);
            Window {
                lower: Some(start).filter(|s| *s > 0),
                upper: Some(far).filter(|f| *f < TOP),
            }
        } else {
            let far = start.min(TOP).saturating_sub(width - 1);
            Window {
                lower: Some(far).filter(|f| *f > 0),
                upper: Some(start).filter(|s| *s < u64::MAX),
            }
        };
        let found = loop {
            queries += 1;
            let (mut found, complete) = query(window, limit)?;
            ensure!(
                found.iter().all(|r| within(window, r.matchid)),
                "the node returned a row outside its window"
            );
            if complete {
                break found;
            }
            ensure!(
                found.len() >= need,
                "an incomplete window returned too few rows"
            );
            // A full window: its need-th row bounds a window that holds the need first matches.
            found.sort_by_key(|r| r.matchid);
            if !up {
                found.reverse();
            }
            let bound = found[need - 1].matchid;
            ensure!(
                Some(bound) != if up { window.upper } else { window.lower },
                "a full window did not shrink (duplicate matchids?)"
            );
            if up {
                window.upper = Some(bound);
            } else {
                window.lower = Some(bound);
            }
            width = bound.abs_diff(start) + 1;
        };
        let empty = found.is_empty();
        let mut found = found;
        found.sort_by_key(|r| r.matchid);
        if !up {
            found.reverse();
        }
        rows.extend(found.into_iter().take(need));
        if rows.len() >= size {
            let last = rows.last().unwrap().matchid;
            return Ok(Page {
                rows,
                next: Some(last),
                width,
                queries,
            });
        }
        // Short: continue past this window with a wider one (an open end finishes the walk).
        near = match (up, window.upper, window.lower) {
            (true, None, _) | (false, _, None) => None,
            (true, Some(u), _) => u.checked_add(1),
            (false, _, Some(l)) => l.checked_sub(1),
        };
        width = width.saturating_mul(if empty { 16 } else { 4 });
    }
    Ok(Page {
        rows,
        next: None,
        width,
        queries,
    })
}
fn within(w: Window, m: u64) -> bool {
    w.lower.is_none_or(|l| m >= l) && w.upper.is_none_or(|u| m <= u)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node: rows in its own order (`order`), answering `limit` rows and saying whether more remain.
    fn node(
        rows: Vec<Row>,
        log: &mut Vec<Window>,
    ) -> impl FnMut(Window, usize) -> Result<(Vec<Row>, bool)> + '_ {
        move |w, limit| {
            log.push(w);
            let all: Vec<Row> = rows
                .iter()
                .copied()
                .filter(|r| within(w, r.matchid))
                .collect();
            let complete = all.len() <= limit;
            Ok((all.into_iter().take(limit).collect(), complete))
        }
    }
    fn key(n: u64) -> [u8; 32] {
        let mut k = [0u8; 32];
        k[24..].copy_from_slice(&n.to_be_bytes());
        k
    }
    /// Contracts created in matchid order (creation order is the node's order), then some patched
    /// to new, higher matchids in place: they keep their creation position.
    fn dataset(n: u64, patched: &[u64]) -> Vec<Row> {
        let mut next = n * 10;
        (0..n)
            .map(|i| {
                let matchid = if patched.contains(&i) {
                    next += 7;
                    next
                } else {
                    i * 10 + 3
                };
                Row {
                    matchid,
                    key: key(i),
                }
            })
            .collect()
    }
    fn expected(rows: &[Row], up: bool) -> Vec<u64> {
        let mut m: Vec<u64> = rows.iter().map(|r| r.matchid).collect();
        m.sort();
        if !up {
            m.reverse();
        }
        m
    }
    fn all_pages(
        rows: Vec<Row>,
        size: usize,
        limit: usize,
        width: u64,
        up: bool,
    ) -> (Vec<u64>, usize) {
        let mut log = vec![];
        let mut q = node(rows, &mut log);
        let (mut out, mut cursor, mut width, mut queries) = (vec![], None, width, 0);
        loop {
            let page = if up {
                ascending(&mut q, cursor, size, limit, width).unwrap()
            } else {
                descending(&mut q, cursor, size, limit, width).unwrap()
            };
            queries += page.queries;
            out.extend(page.rows.iter().map(|r| r.matchid));
            width = page.width;
            match page.next {
                Some(n) if page.rows.len() == size => cursor = Some(n),
                _ => break,
            }
        }
        (out, queries)
    }

    #[test]
    fn creation_order_with_patched_rows_pages_in_matchid_order() {
        for patched in [
            vec![],
            vec![0],
            vec![0, 1, 2, 500, 999],
            (0..1000).step_by(3).collect(),
        ] {
            let rows = dataset(1000, &patched);
            for (size, limit, width) in [
                (20, 100, 1 << 16),
                (20, 100, 1),
                (50, 100, 300),
                (1, 2, 5),
                (99, 100, 10),
            ] {
                for up in [true, false] {
                    let (got, _) = all_pages(rows.clone(), size, limit, width, up);
                    assert_eq!(
                        got,
                        expected(&rows, up),
                        "patched {} size {size} up {up}",
                        patched.len()
                    );
                }
            }
        }
    }

    #[test]
    fn any_node_order_gives_the_same_pages() {
        let mut rows = dataset(700, &[3, 4, 5, 300]);
        let want = expected(&rows, true);
        // Jalebi-like: block order, then key order within a block; and a fully shuffled order.
        rows.sort_by_key(|r| (r.key[31] / 16, r.matchid.wrapping_mul(2654435761) % 1000));
        assert_eq!(all_pages(rows.clone(), 20, 100, 1000, true).0, want);
        rows.reverse();
        assert_eq!(all_pages(rows, 20, 100, 1000, true).0, want);
    }

    #[test]
    fn a_page_takes_few_queries_once_the_width_settles() {
        let rows = dataset(10_000, &[]);
        let (got, queries) = all_pages(rows, 20, 100, 1 << 16, true);
        assert_eq!(got.len(), 10_000);
        // 500 pages; the first one shrinks its window, the rest mostly hit at once.
        assert!(queries < 1300, "{queries} queries for 500 pages");
    }

    #[test]
    fn empty_and_tiny_results_end_cleanly() {
        let mut log = vec![];
        let mut q = node(vec![], &mut log);
        let page = ascending(&mut q, None, 20, 100, 1 << 16).unwrap();
        assert!(page.rows.is_empty() && page.next.is_none());
        let rows = vec![
            Row {
                matchid: u64::MAX,
                key: key(1),
            },
            Row {
                matchid: 0,
                key: key(2),
            },
        ];
        assert_eq!(all_pages(rows.clone(), 1, 2, 1, true).0, vec![0, u64::MAX]);
        assert_eq!(all_pages(rows, 1, 2, 1, false).0, vec![u64::MAX, 0]);
    }

    #[test]
    fn a_node_that_breaks_the_window_is_refused() {
        let mut bad = |_: Window, _: usize| -> Result<(Vec<Row>, bool)> {
            Ok((
                vec![Row {
                    matchid: 5,
                    key: key(1),
                }],
                true,
            ))
        };
        assert!(ascending(&mut bad, Some(10), 1, 2, 4).is_err());
        assert!(ascending(&mut bad, None, 2, 2, 4).is_err());
    }

    #[test]
    fn sparse_matchids_take_few_queries() {
        // 16 contracts spread over 60 million IDs, like the test contracts on our node.
        let ids = [
            1u64, 32, 235, 452, 607, 673, 877, 881, 911, 990, 21_561, 5_888_028, 23_813_307,
            28_469_961, 28_759_260, 60_000_001,
        ];
        let rows: Vec<Row> = ids
            .iter()
            .map(|&m| Row {
                matchid: m,
                key: key(m),
            })
            .collect();
        let (got, queries) = all_pages(rows.clone(), 20, 100, WIDTH, true);
        assert_eq!(got, ids);
        assert!(queries <= 12, "{queries} queries");
        let (got, queries) = all_pages(rows, 20, 100, WIDTH, false);
        assert_eq!(got, ids.iter().rev().copied().collect::<Vec<_>>());
        assert!(queries <= 12, "{queries} queries");
    }

    #[test]
    fn clauses_use_inclusive_pairs() {
        let w = Window {
            lower: Some(5),
            upper: Some(9),
        };
        assert_eq!(w.clauses(), "matchid >= u64(5) AND matchid <= u64(9)");
        assert_eq!(
            Window {
                lower: None,
                upper: None
            }
            .clauses(),
            ""
        );
    }
}
