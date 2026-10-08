//! Counter lookup regression tests — `年` has two counter seqs (2084840 ねん,
//! 2220370 とせ); `find_counter_in_text` must yield both.

mod common;

use himotoki_core::grammar::counters::find_counter_in_text;

#[test]
fn nen_counter_yields_all_seqs() {
    let env = require_db!();
    let found = find_counter_in_text(&env.conn, "24年");
    let seqs: Vec<i64> = found
        .iter()
        .filter(|(s, e, _)| *s == 0 && *e == 3)
        .filter_map(|(_, _, c)| c.seq())
        .collect();
    assert!(seqs.contains(&2084840), "expected 2084840 in {seqs:?}");
    assert!(seqs.contains(&2220370), "expected 2220370 in {seqs:?}");
}
