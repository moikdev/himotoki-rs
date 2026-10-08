//! Regressions for bugs found in the 2026-10 audit. Expected values come from
//! the Python reference implementation on the same database.

mod common;

use himotoki_core::output::format::segment_to_json;
use himotoki_core::segment::segment_text;

fn best_score(env: &common::Env, text: &str) -> f64 {
    segment_text(&env.conn, text, Some(&env.index), 1)[0].1
}

#[test]
fn shime_is_a_kanji_character() {
    // 〆 is U+3006; it was mistyped as U+3030 (〰).
    assert!(himotoki_core::chars::is_kanji_char('〆'));
    assert!(!himotoki_core::chars::is_kanji_char('〰'));
    let env = require_db!();
    assert_eq!(best_score(&env, "〆切"), 65.0);
}

#[test]
fn suffix_compound_sees_nokanji_reading() {
    // find_word dropped the nokanji column, failing the primary-reading check.
    let env = require_db!();
    assert_eq!(best_score(&env, "イライラする"), 682.0);
}

#[test]
fn index_does_not_change_results() {
    // A repeated non-dictionary substring (やで) used to get duplicate
    // suffix compounds only when the word index was in use.
    let env = require_db!();
    for text in [
        "ややでにやよのへやもへやで",
        "しししししししししし",
        "猫が好き",
    ] {
        let with = segment_to_json(&env.conn, text, Some(&env.index), 5);
        let without = segment_to_json(&env.conn, text, None, 5);
        assert_eq!(with, without, "{text}");
    }
}

#[test]
fn pathological_input_is_rejected_quickly() {
    let env = require_db!();
    let t0 = std::time::Instant::now();
    let err = himotoki_core::analyze(&env.conn, &"て".repeat(100), Some(&env.index), 5, None)
        .unwrap_err();
    assert!(err.is::<himotoki_core::TextTooComplexError>(), "{err}");
    assert!(t0.elapsed().as_secs_f64() < 2.0);
    // Dense but natural text of the same length is accepted.
    let natural =
        "国境の長いトンネルを抜けると雪国であった。夜の底が白くなった。信号所に汽車が止まった。"
            .repeat(3);
    let natural: String = natural.chars().take(100).collect();
    assert!(himotoki_core::analyze(&env.conn, &natural, Some(&env.index), 5, None).is_ok());
}
