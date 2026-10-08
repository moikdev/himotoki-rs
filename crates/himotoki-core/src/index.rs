//! Word-surface index — replaces the Python marisa trie (`word_index.py`).
//!
//! Python only ever calls `trie_has_prefix(...)`'s sibling `trie_match` as an
//! exact-membership gate (trie_has_prefix is dead code there), so we use an
//! `fst::Set` of all kanji_text/kana_text surfaces, built from the DB and
//! cached as `himotoki.fst` beside it (or in the temp dir when the DB's
//! directory is read-only). A `himotoki.fst.key` sidecar records the DB's
//! size and mtime so a replaced database never reuses a stale index.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use fst::{Set, SetBuilder};

pub struct WordIndex {
    /// `None` = index unavailable: every surface "may exist", which is the
    /// same as running without an index (slower, identical results).
    set: Option<Set<Vec<u8>>>,
}

impl WordIndex {
    pub fn load_or_build(db_path: &Path, conn: &rusqlite::Connection) -> Self {
        let key = db_fingerprint(db_path);
        let candidates = [fst_path_for(db_path), temp_fst_path(key.as_deref())];
        for fst_path in &candidates {
            if let Some(set) = load_fresh(db_path, fst_path, key.as_deref()) {
                return WordIndex { set: Some(set) };
            }
        }
        match build_from_db(conn) {
            Ok(bytes) => {
                // Cache beside the DB, else in the temp dir; failure to cache
                // only costs a rebuild next time.
                for fst_path in &candidates {
                    if std::fs::write(fst_path, &bytes).is_ok() {
                        if let Some(k) = &key {
                            let _ = std::fs::write(key_path_for(fst_path), k);
                        }
                        break;
                    }
                }
                WordIndex {
                    set: Set::new(bytes).ok(),
                }
            }
            Err(_) => WordIndex { set: None },
        }
    }

    pub fn none() -> Self {
        WordIndex { set: None }
    }

    /// `word_match` / `trie_match` — exact surface membership.
    pub fn contains(&self, text: &str) -> bool {
        self.set.as_ref().map(|s| s.contains(text)).unwrap_or(true)
    }
}

fn fst_path_for(db_path: &Path) -> PathBuf {
    db_path.with_extension("fst")
}

fn key_path_for(fst_path: &Path) -> PathBuf {
    fst_path.with_extension("fst.key")
}

fn temp_fst_path(key: Option<&str>) -> PathBuf {
    let tag = key.unwrap_or("unknown").replace(':', "-");
    std::env::temp_dir().join(format!("himotoki-index-{tag}.fst"))
}

/// `size:mtime_ns` of the database file.
fn db_fingerprint(db_path: &Path) -> Option<String> {
    let m = std::fs::metadata(db_path).ok()?;
    let mtime = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos();
    Some(format!("{}:{}", m.len(), mtime))
}

fn load_fresh(db_path: &Path, fst_path: &Path, key: Option<&str>) -> Option<Set<Vec<u8>>> {
    let fresh = match std::fs::read_to_string(key_path_for(fst_path)) {
        // Sidecar present: must match exactly.
        Ok(stored) => Some(stored.trim()) == key,
        // Index written before sidecars existed: fall back to mtime order.
        Err(_) => match (
            std::fs::metadata(db_path).and_then(|m| m.modified()),
            std::fs::metadata(fst_path).and_then(|m| m.modified()),
        ) {
            (Ok(db_m), Ok(fst_m)) => fst_m >= db_m,
            _ => false,
        },
    };
    if !fresh {
        return None;
    }
    Set::new(std::fs::read(fst_path).ok()?).ok()
}

fn build_from_db(conn: &rusqlite::Connection) -> anyhow::Result<Vec<u8>> {
    let mut words = BTreeSet::new();
    for table in ["kanji_text", "kana_text"] {
        let mut stmt = conn.prepare(&format!("SELECT DISTINCT text FROM {}", table))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for r in rows {
            words.insert(r?);
        }
    }
    // FST requires sorted insert — BTreeSet iteration is sorted.
    let mut builder = SetBuilder::memory();
    builder.extend_iter(words)?;
    Ok(builder.into_inner()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_index_admits_everything() {
        assert!(WordIndex::none().contains("猫"));
    }

    #[test]
    fn failed_build_does_not_gate_out_words() {
        let dir = std::env::temp_dir().join(format!("himotoki-index-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("empty.db");
        let conn = rusqlite::Connection::open(&db).unwrap(); // no tables → build fails
        let ix = WordIndex::load_or_build(&db, &conn);
        assert!(ix.contains("猫"));
    }
}
