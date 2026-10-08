//! Shared test setup. Integration tests need a built database: set
//! HIMOTOKI_DB_PATH (they skip, with a note, when it is unset).

use himotoki_core::{db, index::WordIndex};

#[allow(dead_code)] // not every test target uses every field
pub struct Env {
    pub conn: rusqlite::Connection,
    pub index: WordIndex,
}

pub fn env() -> Option<Env> {
    let path = std::path::PathBuf::from(std::env::var_os("HIMOTOKI_DB_PATH")?);
    let conn = db::open(&path).expect("open HIMOTOKI_DB_PATH");
    himotoki_core::warm_up(&conn);
    let index = WordIndex::load_or_build(&path, &conn);
    Some(Env { conn, index })
}

#[macro_export]
macro_rules! require_db {
    () => {
        match common::env() {
            Some(e) => e,
            None => {
                eprintln!("skipped: HIMOTOKI_DB_PATH not set");
                return;
            }
        }
    };
}
