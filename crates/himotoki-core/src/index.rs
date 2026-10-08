//! Word-surface index — replaces the Python marisa trie (`word_index.py`).
//!
//! Python only ever calls `trie_has_prefix(...)`'s sibling `trie_match` as an
//! exact-membership gate (trie_has_prefix is dead code there), so we use an
//! `fst::Set` of all kanji_text/kana_text surfaces, built from the DB.
//!
//! The set is cached on disk as `<db file name>.fst` beside the database, or
//! in the temp dir (named by a hash of the database's identity) when that
//! directory is read-only. Each cache file starts with the identity it was
//! built from — the database's real path, size and mtime — and is published
//! by renaming a uniquely named temp file, so the bytes and their identity
//! can never mismatch, even with concurrent builders. While a non-empty
//! `-wal` file holds changes the main file's size/mtime don't reflect, no
//! persisted index is used or written.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use fst::{Set, SetBuilder};

/// Cache file header; bump the version if the layout changes.
const MAGIC: &[u8] = b"himotoki-fst-v1\n";

pub struct WordIndex {
    /// `None` = index unavailable: every surface "may exist", which is the
    /// same as running without an index (slower, identical results).
    set: Option<Set<Vec<u8>>>,
}

impl WordIndex {
    pub fn load_or_build(db_path: &Path, conn: &rusqlite::Connection) -> Self {
        let cache = crate::db::resolve(db_path)
            .ok()
            .filter(|real| !crate::db::has_pending_wal(real))
            .and_then(|real| Some((fingerprint(&real)?, real)));
        let candidates: Vec<PathBuf> = match &cache {
            Some((key, real)) => vec![beside_path(real), temp_path(key)],
            None => Vec::new(),
        };
        if let Some((key, _)) = &cache {
            for path in &candidates {
                if let Some(set) = load_cached(path, key) {
                    return WordIndex { set: Some(set) };
                }
            }
        }
        let bytes = match build_from_db(conn) {
            Ok(bytes) => bytes,
            Err(_) => return WordIndex { set: None },
        };
        // Cache beside the DB, else in the temp dir; failing to cache only
        // costs a rebuild next time.
        if let Some((key, _)) = &cache {
            let mut file = Vec::with_capacity(MAGIC.len() + key.len() + 1 + bytes.len());
            file.extend_from_slice(MAGIC);
            file.extend_from_slice(key.as_bytes());
            file.push(b'\n');
            file.extend_from_slice(&bytes);
            for path in &candidates {
                if write_atomic(path, &file).is_ok() {
                    break;
                }
            }
        }
        WordIndex {
            set: Set::new(bytes).ok(),
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

/// `<dir>/<db file name>.fst` — distinct for `x.db` and `x.sqlite`.
fn beside_path(real_db: &Path) -> PathBuf {
    let mut p = real_db.as_os_str().to_owned();
    p.push(".fst");
    PathBuf::from(p)
}

/// Temp-dir cache location, unique per database identity.
fn temp_path(key: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    std::env::temp_dir().join(format!("himotoki-index-{:016x}.fst", h.finish()))
}

/// `size:mtime_ns:real_path` of the (resolved) database file.
fn fingerprint(real_db: &Path) -> Option<String> {
    let m = std::fs::metadata(real_db).ok()?;
    let mtime = m
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!("{}:{}:{}", m.len(), mtime, real_db.display()))
}

/// The cached set at `path`, if it was built from the database `key` names.
fn load_cached(path: &Path, key: &str) -> Option<Set<Vec<u8>>> {
    let file = std::fs::read(path).ok()?;
    let rest = file.strip_prefix(MAGIC)?;
    let nl = rest.iter().position(|&b| b == b'\n')?;
    if &rest[..nl] != key.as_bytes() {
        return None;
    }
    Set::new(rest[nl + 1..].to_vec()).ok()
}

/// Write to a uniquely named sibling temp file, then rename over `path`, so
/// readers never see a partial file and concurrent writers never interleave.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
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

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("himotoki-index-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A tiny dictionary containing `words`.
    fn dict(path: &Path, words: &[&str]) {
        let _ = std::fs::remove_file(path);
        let c = rusqlite::Connection::open(path).unwrap();
        c.execute_batch("CREATE TABLE kanji_text(text TEXT); CREATE TABLE kana_text(text TEXT);")
            .unwrap();
        for w in words {
            c.execute("INSERT INTO kanji_text VALUES (?1)", [w])
                .unwrap();
        }
    }

    fn index_for(db: &Path) -> WordIndex {
        let c = crate::db::open(db).unwrap();
        WordIndex::load_or_build(db, &c)
    }

    #[test]
    fn unavailable_index_admits_everything() {
        assert!(WordIndex::none().contains("猫"));
    }

    #[test]
    fn failed_build_does_not_gate_out_words() {
        let dir = scratch("empty");
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
        let root = scratch("ro");
        let mtime = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let mut dbs = Vec::new();
        for (dir, word) in [("a", "猫"), ("b", "犬")] {
            let d = root.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            let db = d.join("x.db");
            dict(&db, &[word]);
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
            assert!(index_for(db).contains(word), "{}", db.display());
            assert!(index_for(db).contains(word), "cached {}", db.display());
        }
        for (d, db, _) in &dbs {
            let real = crate::db::resolve(db).unwrap();
            let _ = std::fs::remove_file(temp_path(&fingerprint(&real).unwrap()));
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// `shared.db` and `shared.sqlite` used to share `shared.fst`, and the
    /// index bytes and their key were separate files: concurrent builders
    /// could pair one database's index with the other's key.
    #[test]
    fn concurrent_builders_keep_indexes_separate() {
        let dir = scratch("race");
        let a = dir.join("shared.db");
        let b = dir.join("shared.sqlite");
        dict(&a, &["猫"]);
        dict(&b, &["犬"]);
        std::thread::scope(|s| {
            for i in 0..8 {
                let (db, word, other) = if i % 2 == 0 {
                    (&a, "猫", "犬")
                } else {
                    (&b, "犬", "猫")
                };
                s.spawn(move || {
                    for _ in 0..10 {
                        let ix = index_for(db);
                        assert!(ix.contains(word) && !ix.contains(other));
                    }
                });
            }
        });
        for (db, word, other) in [(&a, "猫", "犬"), (&b, "犬", "猫")] {
            let ix = index_for(db);
            assert!(ix.contains(word) && !ix.contains(other), "{}", db.display());
        }
    }

    /// A WAL commit can add words without changing the main file's size or
    /// mtime; a persisted index must not hide them.
    #[test]
    fn pending_wal_bypasses_cached_index() {
        let dir = scratch("wal");
        let db = dir.join("w.db");
        dict(&db, &["猫"]);
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch("PRAGMA journal_mode=WAL;")
            .unwrap();
        assert!(index_for(&db).contains("猫")); // persisted, no pending WAL
        let w = rusqlite::Connection::open(&db).unwrap();
        w.execute_batch("PRAGMA wal_autocheckpoint=0; INSERT INTO kanji_text VALUES ('犬');")
            .unwrap();
        let ix = index_for(&db);
        assert!(ix.contains("犬") && ix.contains("猫"));
        drop(w);
    }

    /// Opening through a symlink uses the target's cache and WAL state.
    #[cfg(unix)]
    #[test]
    fn symlinked_database_sees_wal_words() {
        let dir = scratch("sym");
        let db = dir.join("real.db");
        dict(&db, &["猫"]);
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch("PRAGMA journal_mode=WAL;")
            .unwrap();
        let alias = dir.join("alias.db");
        std::os::unix::fs::symlink(&db, &alias).unwrap();
        assert!(index_for(&alias).contains("猫"));
        let w = rusqlite::Connection::open(&db).unwrap();
        w.execute_batch("PRAGMA wal_autocheckpoint=0; INSERT INTO kanji_text VALUES ('犬');")
            .unwrap();
        assert!(index_for(&alias).contains("犬"));
        drop(w);
    }
}
