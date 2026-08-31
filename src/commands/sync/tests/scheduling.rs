//! decisions/0046 addendum, Finding C: `select_next_branch_by_mapping_distance`
//! computes each remaining branch's distance exactly once per round and
//! converts a single branch's distance-computation failure into that
//! branch's own halt rather than the whole round's.

use std::cell::Cell;

use super::*;

#[test]
fn each_branch_distance_is_computed_exactly_once_and_the_minimum_is_selected() {
    let remaining = vec!["b".to_owned(), "a".to_owned(), "c".to_owned()];
    let calls: std::collections::HashMap<&str, Cell<usize>> = [
        ("a", Cell::new(0)),
        ("b", Cell::new(0)),
        ("c", Cell::new(0)),
    ]
    .into_iter()
    .collect();

    let (halted, selected) = select_next_branch_by_mapping_distance(&remaining, |branch| {
        calls[branch].set(calls[branch].get() + 1);
        Ok(match branch {
            "a" => Some(5),
            "b" => Some(1),
            "c" => None,
            _ => unreachable!(),
        })
    });

    assert!(halted.is_empty());
    assert_eq!(selected.as_deref(), Some("b"), "the smallest distance wins");
    for branch in ["a", "b", "c"] {
        assert_eq!(
            calls[branch].get(),
            1,
            "{branch:?}'s distance must be computed exactly once this round, \
             not once per pairwise comparison"
        );
    }
}

#[test]
fn a_branch_with_no_mapping_yet_sorts_after_every_branch_that_has_one() {
    let remaining = vec!["no-mapping".to_owned(), "far".to_owned()];
    let (halted, selected) = select_next_branch_by_mapping_distance(&remaining, |branch| {
        Ok(match branch {
            "no-mapping" => None,
            "far" => Some(1_000),
            _ => unreachable!(),
        })
    });
    assert!(halted.is_empty());
    assert_eq!(selected.as_deref(), Some("far"));
}

#[test]
fn ties_break_by_branch_name() {
    let remaining = vec!["zebra".to_owned(), "apple".to_owned()];
    let (halted, selected) = select_next_branch_by_mapping_distance(&remaining, |_| Ok(Some(3)));
    assert!(halted.is_empty());
    assert_eq!(selected.as_deref(), Some("apple"));
}

#[test]
fn a_single_branchs_distance_error_becomes_its_own_halt_not_a_whole_round_abort() {
    // Finding C: a distance-computation failure for one branch (e.g.
    // Finding A's own-scan-horizon refusal) must not prevent every other
    // branch's turn this run — it comes back as a named halt for the
    // caller to report and remove, not a propagated error.
    let remaining = vec!["ok".to_owned(), "broken".to_owned()];
    let (halted, selected) = select_next_branch_by_mapping_distance(&remaining, |branch| {
        if branch == "broken" {
            anyhow::bail!("this branch's first-parent history exceeds the scan limit");
        }
        Ok(Some(1))
    });
    assert!(
        selected.is_none(),
        "nothing is selected from a round that saw an error"
    );
    assert_eq!(halted.len(), 1);
    assert_eq!(halted[0].0, "broken");
    assert!(halted[0].1.contains("broken"));
    assert!(halted[0].1.contains("scan limit"));
}

#[test]
fn every_remaining_branch_erroring_halts_every_one_of_them() {
    let remaining = vec!["a".to_owned(), "b".to_owned()];
    let (halted, selected) =
        select_next_branch_by_mapping_distance(&remaining, |_| anyhow::bail!("boom"));
    assert!(selected.is_none());
    assert_eq!(halted.len(), 2);
}

#[test]
fn an_empty_remaining_list_selects_nothing_and_halts_nothing() {
    let remaining: Vec<String> = Vec::new();
    let (halted, selected) = select_next_branch_by_mapping_distance(&remaining, |_| Ok(Some(0)));
    assert!(halted.is_empty());
    assert!(selected.is_none());
}

#[test]
fn a_branchs_none_distance_is_computed_once_across_rounds_while_a_some_distance_is_recomputed_every_round()
 {
    // decisions/0046 addendum, Finding I: a branch that never gains a
    // mapping this run has its expensive full-history walk paid exactly
    // once, not once per scheduling round, while a branch with a mapping is
    // still asked every round since an earlier push in the same run can
    // shorten its distance.
    let mut no_mapping_memo: std::collections::HashSet<String> = std::collections::HashSet::new();
    let calls: std::collections::HashMap<&str, Cell<usize>> =
        [("no-mapping", Cell::new(0)), ("mapped", Cell::new(0))]
            .into_iter()
            .collect();

    for _round in 0..3 {
        for branch in ["no-mapping", "mapped"] {
            let distance =
                mapping_distance_with_none_memo(branch, &mut no_mapping_memo, |queried| {
                    calls[queried].set(calls[queried].get() + 1);
                    Ok(match queried {
                        "no-mapping" => None,
                        "mapped" => Some(1),
                        _ => unreachable!(),
                    })
                })
                .unwrap();
            match branch {
                "no-mapping" => assert_eq!(distance, None),
                "mapped" => assert_eq!(distance, Some(1)),
                _ => unreachable!(),
            }
        }
    }

    assert_eq!(
        calls["no-mapping"].get(),
        1,
        "a None distance must be memoized for the rest of the run"
    );
    assert_eq!(
        calls["mapped"].get(),
        3,
        "a Some distance must be recomputed every round"
    );
}
