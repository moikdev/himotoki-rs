//! himotoki_rs — PyO3 bindings for the Rust himotoki port.
//!
//! Mirrors `himotoki/__init__.py`: `analyze(text, limit, max_length)` returns
//! `List[Tuple[List[dict], int]]` where each dict is `word_info_gloss_json`'s
//! output (same shape as `himotoki -j`).

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

use himotoki_core::db;
use himotoki_core::index::WordIndex;
use himotoki_core::output::format::word_info_gloss_json;
use himotoki_core::output::golden::num_json;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Shared, immutable engine state (path + FST index). Each OS thread gets
/// its own SQLite connection so `analyze` calls run in parallel once the
/// GIL is released.
struct Shared {
    index: Option<WordIndex>,
    path: PathBuf,
}

static SHARED: OnceLock<Shared> = OnceLock::new();
static INIT: Mutex<()> = Mutex::new(());

thread_local! {
    static CONN: std::cell::RefCell<Option<rusqlite::Connection>> = const { std::cell::RefCell::new(None) };
}

fn shared(db_path: Option<&str>) -> PyResult<&'static Shared> {
    if let Some(s) = SHARED.get() {
        return Ok(s);
    }
    let _g = INIT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = SHARED.get() {
        return Ok(s);
    }
    let path = db_path
        .map(PathBuf::from)
        .unwrap_or_else(db::default_db_path);
    let conn = db::open(&path)
        .map_err(|e| PyRuntimeError::new_err(format!("failed to open {}: {e}", path.display())))?;
    // Per-thread connections reopen this path later, possibly after the
    // process changed directory: pin it to an absolute, canonical path.
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    himotoki_core::warm_up(&conn);
    let index = Some(WordIndex::load_or_build(&path, &conn));
    Ok(SHARED.get_or_init(|| Shared { index, path }))
}

fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

fn with_engine<T>(
    db_path: Option<&str>,
    f: impl FnOnce(&rusqlite::Connection, Option<&WordIndex>) -> anyhow::Result<T>,
) -> PyResult<T> {
    let sh = shared(db_path)?;
    if let Some(p) = db_path {
        if !same_file(&sh.path, std::path::Path::new(p)) {
            return Err(PyRuntimeError::new_err(format!(
                "engine already initialized with {}; cannot switch to {}",
                sh.path.display(),
                p
            )));
        }
    }
    CONN.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(db::open(&sh.path).map_err(|e| PyRuntimeError::new_err(e.to_string()))?);
        }
        f(slot.as_ref().unwrap(), sh.index.as_ref()).map_err(|e| {
            let msg = e.to_string();
            if e.is::<himotoki_core::TextTooLongError>()
                || e.is::<himotoki_core::TextTooComplexError>()
            {
                PyValueError::new_err(msg)
            } else {
                PyRuntimeError::new_err(msg)
            }
        })
    })
}

/// `analyze` — NFC-normalize, segment, and fill word infos.
///
/// Returns `List[Tuple[List[dict], int]]` matching Python's
/// `himotoki.analyze` (dicts are `word_info_gloss_json` output).
#[pyfunction]
#[pyo3(signature = (text, limit=1, db_path=None, max_length=None))]
fn analyze(
    py: Python<'_>,
    text: &str,
    limit: usize,
    db_path: Option<&str>,
    max_length: Option<usize>,
) -> PyResult<PyObject> {
    if text.trim().is_empty() {
        return Err(PyValueError::new_err(
            "text must be non-empty and not whitespace-only",
        ));
    }
    if limit < 1 {
        return Err(PyValueError::new_err("limit must be >= 1"));
    }
    let text_owned = text.to_string();
    py.allow_threads(move || {
        with_engine(db_path, |conn, index| {
            let results = himotoki_core::analyze(conn, &text_owned, index, limit, max_length)?;
            // Gloss JSON also hits SQLite, so build it before re-taking the GIL.
            Ok(results
                .iter()
                .map(|(wis, score)| {
                    (
                        serde_json::Value::Array(
                            wis.iter()
                                .map(|wi| word_info_gloss_json(conn, wi, false))
                                .collect(),
                        ),
                        num_json(*score),
                    )
                })
                .collect::<Vec<(serde_json::Value, serde_json::Value)>>())
        })
    })
    .and_then(|v| {
        pythonize::pythonize(py, &v)
            .map(|o| o.unbind())
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))
    })
}

/// `warm_up` — eagerly initialize caches (suffix map, counter cache,
/// archaic sets, rule registries, FST index) so the first `analyze` isn't
/// cold.
#[pyfunction]
#[pyo3(signature = (db_path=None))]
fn warm_up(db_path: Option<&str>) -> PyResult<()> {
    shared(db_path).map(|_| ())
}

/// Resolved database path (after env/default resolution).
#[pyfunction]
#[pyo3(signature = ())]
fn db_path() -> PyResult<String> {
    SHARED
        .get()
        .map(|s| s.path.display().to_string())
        .ok_or_else(|| PyRuntimeError::new_err("engine not initialized"))
}

#[pymodule]
fn himotoki_rs(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(analyze, m)?)?;
    m.add_function(wrap_pyfunction!(warm_up, m)?)?;
    m.add_function(wrap_pyfunction!(db_path, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
