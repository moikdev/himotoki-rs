//! Word-surface index — replaces the Python marisa trie (`word_index.py`).
//!
//! Python only ever calls `trie_has_prefix(...)`'s sibling `trie_match` as an
//! exact-membership gate (trie_has_prefix is dead code there), so we use an
//! `fst::Set` of all kanji_text/kana_text surfaces, built from the DB and
//! cached as `himotoki.fst` beside it, or in the temp dir (named by a hash of
//! the database's identity) when the DB's directory is read-only. A `.key`
//! sidecar records the DB's size, mtime and canonical path, so a replaced or
//! different database never reuses a stale index. Files are published by
//! write-then-rename.

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
        // (path, trust a sidecar-less legacy index by mtime)
        let mut candidates = vec![(fst_path_for(db_path), true)];
        if let Some(k) = &key {
            candidates.push((temp_fst_path(k), false));
        }
        for (fst_path, legacy_ok) in &candidates {
            if let Some(set) = load_fresh(db_path, fst_path, key.as_deref(), *legacy_ok) {
                return WordIndex { set: Some(set) };
            }
        }
        match build_from_db(conn) {
            Ok(bytes) => {
                // Cache beside the DB, else in the temp dir; failure to cache
                // only costs a rebuild next time. Without a fingerprint there
                // is nothing to validate a cached copy against, so skip it.
                if let Some(k) = &key {
                    for (fst_path, _) in &candidates {
                        if write_atomic(fst_path, &bytes).is_ok()
                            && write_atomic(&key_path_for(fst_path), k.as_bytes()).is_ok()
                        {
                            break;
                        }
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

/// Temp-dir cache location, unique per database identity.
fn temp_fst_path(key: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    std::env::temp_dir().join(format!("himotoki-index-{:016x}.fst", h.finish()))
}

/// `size:mtime_ns:canonical_path` of the database file.
fn db_fingerprint(db_path: &Path) -> Option<String> {
    let m = std::fs::metadata(db_path).ok()?;
    let mtime = m
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let canon = std::fs::canonicalize(db_path).ok()?;
    Some(format!("{}:{}:{}", m.len(), mtime, canon.display()))
}

fn load_fresh(
    db_path: &Path,
    fst_path: &Path,
    key: Option<&str>,
    legacy_ok: bool,
) -> Option<Set<Vec<u8>>> {
    let fresh = match std::fs::read_to_string(key_path_for(fst_path)) {
        // Sidecar present: must match exactly.
        Ok(stored) => key.is_some() && Some(stored.trim()) == key,
        // Index written beside the DB before sidecars existed: fall back to
        // mtime order.
        Err(_) if legacy_ok => match (
            std::fs::metadata(db_path).and_then(|m| m.modified()),
            std::fs::metadata(fst_path).and_then(|m| m.modified()),
        ) {
            (Ok(db_m), Ok(fst_m)) => fst_m >= db_m,
            _ => false,
        },
        Err(_) => false,
    };
    if !fresh {
        return None;
    }
    Set::new(std::fs::read(fst_path).ok()?).ok()
}

/// Write to a sibling temp file, then rename over `path`, so readers never
/// see a partially written file.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
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

    /// Two databases with identical size and mtime in read-only directories
    /// must not share a temp-dir index.
    #[cfg(unix)]
    #[test]
    fn temp_cache_is_per_database() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("himotoki-index-ro-{}", std::process::id()));
        let mtime = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let mut dbs = Vec::new();
        for (dir, word) in [("a", "猫"), ("b", "犬")] {
            let d = root.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            let db = d.join("x.db");
            let _ = std::fs::remove_file(&db);
            let c = rusqlite::Connection::open(&db).unwrap();
            c.execute_batch(&format!(
                "CREATE TABLE kanji_text(text TEXT); CREATE TABLE kana_text(text TEXT);
                 INSERT INTO kanji_text VALUES ('{word}');"
            ))
            .unwrap();
            drop(c);
            std::fs::File::options()
                .write(true)
                .open(&db)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o555)).unwrap();
            dbs.push((d, db, word));
        }
        assert_eq!(
            std::fs::metadata(&dbs[0].1).unwrap().len(),
            std::fs::metadata(&dbs[1].1).unwrap().len()
        );
        for (_, db, word) in &dbs {
            let c = crate::db::open(db).unwrap();
            let ix = WordIndex::load_or_build(db, &c);
            assert!(ix.contains(word), "{} should contain {word}", db.display());
            // Reload from the cache too.
            let ix = WordIndex::load_or_build(db, &c);
            assert!(
                ix.contains(word),
                "cached {} should contain {word}",
                db.display()
            );
        }
        for (d, db, _) in &dbs {
            if let Some(k) = db_fingerprint(db) {
                let t = temp_fst_path(&k);
                let _ = std::fs::remove_file(key_path_for(&t));
                let _ = std::fs::remove_file(t);
            }
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
