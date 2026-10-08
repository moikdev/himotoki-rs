"""Binding tests. Most need a built database: set HIMOTOKI_DB_PATH (skipped otherwise)."""

import os
from concurrent.futures import ThreadPoolExecutor

import pytest

import himotoki_rs

DB = os.environ.get("HIMOTOKI_DB_PATH")
needs_db = pytest.mark.skipif(
    not DB or not os.path.isfile(DB), reason="HIMOTOKI_DB_PATH not set to a built database"
)


@needs_db
def test_analyze_shape():
    [(words, score)] = himotoki_rs.analyze("猫が好き", db_path=DB)
    assert [w["text"] for w in words] == ["猫", "が", "好き"]
    assert score > 0


@needs_db
def test_input_validation():
    with pytest.raises(ValueError):
        himotoki_rs.analyze("   ", db_path=DB)
    with pytest.raises(ValueError):
        himotoki_rs.analyze("猫", limit=0, db_path=DB)
    with pytest.raises(ValueError):
        himotoki_rs.analyze("あ" * 101, db_path=DB)


@needs_db
def test_switching_database_is_rejected():
    himotoki_rs.warm_up(DB)
    with pytest.raises(RuntimeError):
        himotoki_rs.analyze("猫", db_path=DB + ".other")


@needs_db
def test_threads_match_sequential():
    texts = ["猫が好き", "彼女は終わってしまった", "イライラする", "学校で勉強しています"] * 8
    seq = [himotoki_rs.analyze(t, limit=3, db_path=DB) for t in texts]
    with ThreadPoolExecutor(4) as ex:
        par = list(ex.map(lambda t: himotoki_rs.analyze(t, limit=3, db_path=DB), texts))
    assert par == seq


def test_missing_db_raises_catchable_error(tmp_path):
    # Fresh interpreter: the engine binds to the first database it opens.
    import subprocess
    import sys

    missing = tmp_path / "missing.db"
    code = (
        "import himotoki_rs\n"
        "try:\n"
        f"    himotoki_rs.analyze('猫', db_path={str(missing)!r})\n"
        "except RuntimeError as e:\n"
        "    assert 'database not found' in str(e), e\n"
        "else:\n"
        "    raise SystemExit('no error')\n"
    )
    subprocess.run([sys.executable, "-c", code], check=True)
    assert not missing.exists()


@needs_db
def test_relative_db_path_survives_chdir(tmp_path):
    # Worker threads open their own connections; a relative path must not be
    # re-resolved against a later working directory.
    import subprocess
    import sys

    db = os.path.abspath(DB)
    code = (
        "import os, threading, himotoki_rs\n"
        f"os.chdir({os.path.dirname(db)!r})\n"
        f"himotoki_rs.warm_up({os.path.basename(db)!r})\n"
        f"os.chdir({str(tmp_path)!r})\n"
        "out = []\n"
        "t = threading.Thread(target=lambda: out.append(himotoki_rs.analyze('猫')))\n"
        "t.start(); t.join()\n"
        "assert out and out[0][0][0][0]['text'] == '猫', out\n"
    )
    subprocess.run([sys.executable, "-c", code], check=True)
