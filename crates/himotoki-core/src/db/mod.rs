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

/// Open the dictionary read-only. `immutable=1` lets SQLite skip file
/// locking entirely (the analyzer never writes; don't rebuild the file while
/// a process has it open), it works on read-only files and filesystems, and
/// a missing file is an error instead of silently creating an empty database.
pub fn open(path: &std::path::Path) -> rusqlite::Result<Connection> {
    use rusqlite::OpenFlags;
    // Memory-status accounting takes a process-global mutex on every SQLite
    // malloc, serializing otherwise independent connections. Must run before
    // SQLite initializes; later calls return SQLITE_MISUSE harmlessly.
    static MEMSTATUS_OFF: std::sync::Once = std::sync::Once::new();
    MEMSTATUS_OFF.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_config(
            rusqlite::ffi::SQLITE_CONFIG_MEMSTATUS,
            0 as std::os::raw::c_int,
        );
    });
    if !path.is_file() {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
            Some(format!("database not found: {}", path.display())),
        ));
    }
    let conn = Connection::open_with_flags(
        immutable_uri(path),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "cache_size", "-64000")?;
    conn.pragma_update(None, "mmap_size", "268435456")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(conn)
}

/// `file:` URI for `path` with `immutable=1`. Percent-encodes the characters
/// SQLite's URI parser treats specially and uses forward slashes, adding the
/// leading `/` SQLite expects before a Windows drive letter.
fn immutable_uri(path: &std::path::Path) -> String {
    let mut raw = path.to_string_lossy().into_owned();
    let mut out = String::from("file:");
    if cfg!(windows) {
        raw = raw.replace('\\', "/");
        if raw.as_bytes().get(1) == Some(&b':') {
            out.push('/');
        }
    }
    for c in raw.chars() {
        match c {
            '%' => out.push_str("%25"),
            '?' => out.push_str("%3f"),
            '#' => out.push_str("%23"),
            _ => out.push(c),
        }
    }
    out.push_str("?immutable=1");
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
        let p = tiny_db(&d, "a?b#c%d.db");
        let c = open(&p).unwrap();
        let n: i64 = c
            .query_row("SELECT seq FROM entry", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 7);
    }

    #[cfg(windows)]
    #[test]
    fn uri_handles_drive_letters() {
        assert_eq!(
            immutable_uri(std::path::Path::new("C:\\x\\y.db")),
            "file:/C:/x/y.db?immutable=1"
        );
    }
}
