//! Paging contracts by `matchid` windows (docs/udc-proposal.md, "Paging"), the rule both nodes run.
//!
//! Neither node sorts: our node returns matches in creation order (record ID), Jalebi in block order and, within a
//! block, by transaction and key. A contract patched in place (a re-verification under a new `matchId`) keeps its
//! creation position on both (measured), so node order is not `matchid` order and a key set cannot rely on it.
//! The rule relies on one property only, which both nodes give whatever their order:
//!
//! > a window query `… AND matchid >= u64(A) AND matchid <= u64(B)` (inclusive bounds) that returns no cursor
//! > returned every match in that window.
//!
//! A page of `size` contracts after `A` (ascending): query the window `[A + 1, A + width]`. If it is complete, its
//! rows sorted by `matchid` are exact; take what the page needs and, while it is short, move to the next window with
//! a wider width. If it is not complete, narrow it towards its near end and ask again: to the `need`-th smallest
//! `matchid` of the rows returned (a subset's k-th smallest is never below the whole set's, so the narrowed window
//! still holds the `need` first matches) or to the window's midpoint, whichever is nearer. The midpoint matters
//! when the rows come from the far end, as a descending walk's do on a prefilled chain (creation order is ascending
//! there); it also covers a node that returns a cursor with fewer rows than asked. Descending pages mirror it with
//! the windows below the cursor. Cursors and node order are never used across queries. `matchid`s are unique per
//! dataset (one contract per Sourcify match): two at a page boundary fail the walk instead of losing one.
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
/// Runs one window query: its rows (in any order) and whether the response was complete (no
/// cursor: both nodes return one exactly when rows remain).
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
    // Rows sorted from the near end (the cursor's side) to the far end.
    let toward_far = |rows: &mut Vec<Row>| {
        rows.sort_by_key(|r| r.matchid);
        if !up {
            rows.reverse();
        }
    };
    // The next unread value (inclusive), moving up or down.
    let mut near = match (up, from) {
        (true, None) => Some(0),
        (true, Some(a)) => a.checked_add(1),
        (false, None) => Some(u64::MAX),
        (false, Some(b)) => b.checked_sub(1),
    };
    // The far end (None: open) of the last window that came back full and was narrowed: the rest
    // of it holds rows, so the walk goes back to exactly that end instead of growing (a binary
    // search). Only the extent of the next window depends on it, never which rows are read.
    let mut full_far: Option<Option<u64>> = None;
    // The near part of the last full window came back empty, so its rest holds its rows: halve the
    // rest at once instead of asking for all of it first.
    let mut presumed_full = false;
    while let Some(start) = near {
        let need = size - rows.len();
        // The window's far end is open once it reaches past TOP (ascending; no match ID is expected
        // beyond it) or 0 (descending), so the last window ends the walk in one query wherever the
        // last match is. The near end is always the cursor's.
        let mut window = if let Some(far) = full_far {
            if up {
                Window {
                    lower: Some(start).filter(|s| *s > 0),
                    upper: far,
                }
            } else {
                Window {
                    lower: far,
                    upper: Some(start).filter(|s| *s < u64::MAX),
                }
            }
        } else if up {
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
        let mut skip = std::mem::take(&mut presumed_full)
            && (if up { window.upper } else { window.lower }) != Some(start);
        let mut found = loop {
            let (mut found, complete) = if std::mem::take(&mut skip) {
                (vec![], false)
            } else {
                queries += 1;
                let (found, complete) = query(window, limit)?;
                ensure!(
                    found.iter().all(|r| within(window, r.matchid)),
                    "the node returned a row outside its window"
                );
                (found, complete)
            };
            if complete {
                break found;
            }
            // Not complete: narrow towards the near end, to the need-th row from it or to the
            // midpoint, whichever is nearer; the narrowed window must be strictly smaller.
            full_far = Some(if up { window.upper } else { window.lower });
            toward_far(&mut found);
            let far = if up {
                window.upper.unwrap_or(u64::MAX)
            } else {
                window.lower.unwrap_or(0)
            };
            let mid = if up {
                start + (far - start) / 2
            } else {
                start - (start - far) / 2
            };
            let by_rows = found.get(need - 1).map(|r| r.matchid);
            let bound = match by_rows {
                Some(b) if up => b.min(mid),
                Some(b) => b.max(mid),
                None => mid,
            };
            ensure!(
                bound != far,
                "a window could not be narrowed (two contracts with one matchid?)"
            );
            if up {
                window.upper = Some(bound);
            } else {
                window.lower = Some(bound);
            }
            width = bound.abs_diff(start).saturating_add(1);
        };
        let empty = found.is_empty();
        toward_far(&mut found);
        if found.len() >= need {
            // The page fills here: a tie at its end would lose a row to the next page's cursor.
            ensure!(
                found.get(need).map(|r| r.matchid) != Some(found[need - 1].matchid),
                "two contracts share matchid {}",
                found[need - 1].matchid
            );
            rows.extend(found.into_iter().take(need));
            let (first, last) = (rows[0].matchid, rows.last().unwrap().matchid);
            // The next page's first window: twice this page's span, so it usually holds a page.
            let hint = first.abs_diff(last).saturating_add(1).saturating_mul(2);
            return Ok(Page {
                rows,
                next: Some(last),
                width: hint,
                queries,
            });
        }
        rows.extend(found);
        // Short: continue past this window (an open end finishes the walk): back to the rest of a
        // window that was full, else with a wider one.
        let end = if up { window.upper } else { window.lower };
        near = match (up, end) {
            (_, None) => None,
            (true, Some(u)) => u.checked_add(1),
            (false, Some(l)) => l.checked_sub(1),
        };
        if full_far == Some(end) {
            full_far = None;
        } else if full_far.is_some() && empty {
            presumed_full = true;
        }
        if full_far.is_none() {
            width = width.saturating_mul(if empty { 16 } else { 4 });
        }
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

    /// Dense data in creation order (ascending matchid, as on a prefilled chain): descending pages
    /// bisect towards their near end instead of creeping, and later pages hit at once.
    #[test]
    fn dense_creation_order_pages_cheaply_both_ways() {
        let rows: Vec<Row> = (0..60_000u64)
            .map(|i| Row {
                matchid: 1_000 + 3 * i,
                key: key(i),
            })
            .collect();
        for up in [true, false] {
            let mut log = vec![];
            let mut q = node(rows.clone(), &mut log);
            let (mut cursor, mut width, mut costs, mut got) = (None, WIDTH, vec![], vec![]);
            for _ in 0..10 {
                let page = if up {
                    ascending(&mut q, cursor, 20, 100, width).unwrap()
                } else {
                    descending(&mut q, cursor, 20, 100, width).unwrap()
                };
                costs.push(page.queries);
                got.extend(page.rows.iter().map(|r| r.matchid));
                (cursor, width) = (page.next, page.width);
            }
            let mut want = expected(&rows, up);
            want.truncate(200);
            assert_eq!(got, want, "up {up}");
            assert!(costs[0] <= 60, "first page {costs:?}");
            assert!(
                costs[1..].iter().all(|c| *c <= 3),
                "later pages {costs:?} (up {up})"
            );
        }
    }

    #[test]
    fn a_tie_at_a_page_end_fails_instead_of_losing_a_row() {
        let rows = vec![
            Row {
                matchid: 5,
                key: key(1),
            },
            Row {
                matchid: 5,
                key: key(2),
            },
            Row {
                matchid: 9,
                key: key(3),
            },
        ];
        let mut log = vec![];
        let mut q = node(rows, &mut log);
        let error = ascending(&mut q, None, 1, 10, 100).unwrap_err();
        assert!(error.to_string().contains("share matchid 5"), "{error}");
    }

    /// A node that returns a cursor with fewer rows than asked (a page trimmed by size) is
    /// narrowed by bisection and still gives exact pages.
    #[test]
    fn trimmed_pages_still_page_exactly() {
        let rows = dataset(2_000, &[1, 2, 3, 700]);
        let mut trimmed = |w: Window, limit: usize| -> Result<(Vec<Row>, bool)> {
            let all: Vec<Row> = rows
                .iter()
                .copied()
                .filter(|r| within(w, r.matchid))
                .collect();
            let complete = all.len() <= limit / 4;
            Ok((all.into_iter().take(limit / 4).collect(), complete))
        };
        let (mut cursor, mut out) = (None, vec![]);
        loop {
            let page = ascending(&mut trimmed, cursor, 20, 100, WIDTH).unwrap();
            out.extend(page.rows.iter().map(|r| r.matchid));
            match page.next {
                Some(n) if page.rows.len() == 20 => cursor = Some(n),
                _ => break,
            }
        }
        assert_eq!(out, expected(&rows, true));
    }

    /// Random datasets, node orders, trims, sizes, limits and widths: every walk returns exactly
    /// the matches in matchid order and ends.
    #[test]
    fn random_walks_are_exact() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..3_000 {
            let n = next() % 300;
            let spread = [1u64, 3, 1_000, 1 << 30, 1 << 41][(next() % 5) as usize];
            let mut ids: Vec<u64> = (0..n).map(|_| next() % (n * spread + 1)).collect();
            if next() % 4 == 0 {
                ids.push(u64::MAX - next() % 3);
                ids.push(next() % 2);
            }
            ids.sort();
            ids.dedup();
            let mut rows: Vec<Row> = ids
                .iter()
                .map(|&m| Row {
                    matchid: m,
                    key: key(m),
                })
                .collect();
            match next() % 3 {
                0 => {}
                1 => rows.reverse(),
                _ => rows.sort_by_key(|r| r.matchid.wrapping_mul(0x9e37_79b9_7f4a_7c15)),
            }
            let limit = 2 + (next() % 30) as usize;
            let size = 1 + (next() % (limit as u64 - 1)) as usize;
            let trim = 1 + (next() % limit as u64) as usize;
            let width = 1 + next() % (1 << (next() % 64));
            let up = next() % 2 == 0;
            let mut q = |w: Window, limit: usize| -> Result<(Vec<Row>, bool)> {
                let all: Vec<Row> = rows
                    .iter()
                    .copied()
                    .filter(|r| within(w, r.matchid))
                    .collect();
                let shown = all.len().min(limit).min(trim);
                Ok((all[..shown].to_vec(), shown == all.len()))
            };
            let (mut cursor, mut width, mut got) = (None, width, vec![]);
            for _ in 0..=rows.len() {
                let page = if up {
                    ascending(&mut q, cursor, size, limit, width).unwrap()
                } else {
                    descending(&mut q, cursor, size, limit, width).unwrap()
                };
                assert!(page.queries < 400, "{} queries", page.queries);
                got.extend(page.rows.iter().map(|r| r.matchid));
                width = page.width;
                match page.next {
                    Some(n) if page.rows.len() == size => cursor = Some(n),
                    _ => break,
                }
            }
            assert_eq!(got, expected(&rows, up));
        }
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
