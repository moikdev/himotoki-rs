//! Database connection management — port of `himotoki/db/connection.py`.
//!
//! Path resolution priority:
//!   1. `HIMOTOKI_DB_PATH` / `HIMOTOKI_DB` env vars
//!   2. `~/.himotoki/himotoki.db`
//!   3. `<repo>/data/himotoki.db` (dev tree)

pub mod rows;

use std::path::PathBuf;

use rusqlite::Connection;

/// Resolve the default database path, mirroring `_get_default_db_path`.
pub fn default_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("HIMOTOKI_DB_PATH").or_else(|_| std::env::var("HIMOTOKI_DB")) {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return pb;
        }
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let user_db = home.join(".himotoki").join("himotoki.db");
        if user_db.exists() {
            return user_db;
        }
    }
    // Dev-tree data dir: <repo>/data/himotoki.db (manifest is crates/himotoki-core)
    let dev_db = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/himotoki.db");
    if dev_db.exists() {
        return dev_db;
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".himotoki")
        .join("himotoki.db")
}

/// Open the dictionary read-only. A missing file is an error instead of
/// silently creating an empty database, and read-only files/filesystems work.
///
/// Normally the file is opened `immutable=1`, so SQLite skips file locking
/// entirely (the analyzer never writes; don't rebuild the file in place while
/// a process has it open). If a non-empty `-wal` file holds committed but
/// uncheckpointed changes, immutable mode would ignore them, so it falls back
/// to an ordinary read-only open.
///
/// SQLite's global memory-status accounting (a process-wide mutex on every
/// allocation) is disabled at build time via `.cargo/config.toml`.
pub fn open(path: &std::path::Path) -> rusqlite::Result<Connection> {
    use rusqlite::OpenFlags;
    if !path.is_file() {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
            Some(format!("database not found: {}", path.display())),
        ));
    }
    let abs = std::path::absolute(path).map_err(|e| {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
            Some(format!("cannot resolve {}: {e}", path.display())),
        )
    })?;
    let conn = Connection::open_with_flags(
        sqlite_uri(&abs, !has_pending_wal(&abs)),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "cache_size", "-64000")?;
    conn.pragma_update(None, "mmap_size", "268435456")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(conn)
}

/// A non-empty `<db>-wal` may hold commits not yet in the main file.
fn has_pending_wal(db: &std::path::Path) -> bool {
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    std::fs::metadata(wal).map(|m| m.len() > 0).unwrap_or(false)
}

/// `file:` URI for an absolute `path`. Always emits an empty authority
/// (`file://` + `/path`), so paths starting with `//` are not misread as a
/// host; percent-encodes the characters SQLite's URI parser treats specially;
/// on Windows uses forward slashes and `file:///C:/...` for drive paths
/// (UNC paths become `file:////server/share/...`).
fn sqlite_uri(path: &std::path::Path, immutable: bool) -> String {
    let mut raw = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        raw = raw.replace('\\', "/");
    }
    let mut out = String::from("file://");
    if !raw.starts_with('/') {
        out.push('/');
    }
    for c in raw.chars() {
        match c {
            '%' => out.push_str("%25"),
            '?' => out.push_str("%3f"),
            '#' => out.push_str("%23"),
            _ => out.push(c),
        }
    }
    if immutable {
        out.push_str("?immutable=1");
    }
    out
}

pub fn apply_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "cache_size", "-64000")?;
    conn.pragma_update(None, "mmap_size", "268435456")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_db(dir: &std::path::Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        let c = Connection::open(&p).unwrap();
        c.execute_batch("CREATE TABLE entry(seq INTEGER); INSERT INTO entry VALUES (7);")
            .unwrap();
        p
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("himotoki-db-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn missing_file_is_an_error_and_is_not_created() {
        let d = scratch("missing");
        let p = d.join("nope.db");
        let err = open(&p).unwrap_err().to_string();
        assert!(err.contains("database not found"), "{err}");
        assert!(!p.exists());
    }

    #[test]
    fn opens_read_only_file() {
        let d = scratch("ro");
        let p = tiny_db(&d, "ro.db");
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&p, perm).unwrap();
        let c = open(&p).unwrap();
        let n: i64 = c
            .query_row("SELECT seq FROM entry", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 7);
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        std::fs::set_permissions(&p, perm).unwrap();
    }

    #[test]
    fn uri_escapes_special_characters() {
        let d = scratch("uri");
        // `?` is not a legal file-name character on Windows.
        let name = if cfg!(windows) {
            "a#b%c.db"
        } else {
            "a?b#c%d.db"
        };
        let p = tiny_db(&d, name);
        let c = open(&p).unwrap();
        let n: i64 = c
            .query_row("SELECT seq FROM entry", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 7);
    }

    #[cfg(unix)]
    #[test]
    fn double_slash_and_relative_paths() {
        let d = scratch("paths");
        let p = tiny_db(&d, "x.db");
        let doubled = PathBuf::from(format!("/{}", p.display()));
        assert!(open(&doubled).is_ok(), "{}", doubled.display());
        let rel = pathdiff_from_cwd(&p);
        assert!(open(&rel).is_ok(), "{}", rel.display());
    }

    #[cfg(unix)]
    fn pathdiff_from_cwd(p: &std::path::Path) -> PathBuf {
        let cwd = std::env::current_dir().unwrap();
        let ups = cwd.components().count() - 1;
        let mut rel = PathBuf::new();
        for _ in 0..ups {
            rel.push("..");
        }
        rel.join(p.strip_prefix("/").unwrap())
    }

    #[test]
    fn sees_uncheckpointed_wal_commits() {
        let d = scratch("wal");
        let p = d.join("wal.db");
        let _ = std::fs::remove_file(&p);
        let w = Connection::open(&p).unwrap();
        w.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
             CREATE TABLE entry(seq INTEGER); INSERT INTO entry VALUES (7);",
        )
        .unwrap();
        // `w` stays open, so the commit lives only in wal.db-wal.
        let c = open(&p).unwrap();
        let n: i64 = c
            .query_row("SELECT count(*) FROM entry", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        drop(w);
    }

    #[cfg(windows)]
    #[test]
    fn uri_handles_windows_paths() {
        assert_eq!(
            sqlite_uri(std::path::Path::new("C:\\x\\y.db"), true),
            "file:///C:/x/y.db?immutable=1"
        );
        assert_eq!(
            sqlite_uri(std::path::Path::new("\\\\srv\\share\\y.db"), false),
            "file:////srv/share/y.db"
        );
    }
}
