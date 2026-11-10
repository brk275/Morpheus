"""Full-engine differential check; synthetic DB/WAL only, no Matrix profiles."""
import hashlib
import json
import os
from pathlib import Path
import selectors
import signal
import stat
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[2]
ORIGINAL_SHA = "0a409f1633283fa31a9126b11fbfd64a1991c5d30defad07e5745d4667f5e23d"
PATCHED_SHA = "478a6a723c88f339b98361e341e1bd7457dfa08508f79beec3f5286849c23139"
HEADER_SHA = "9e69a1353a4288450b0d5239ede11fc7f1f4c8e5eb07491fc8317eacb5b7de7e"
ORIGINAL = b"""  sz = (sz&0xfe00) + ((sz&0x0001)<<16);
  testcase( sz<=32768 );
  testcase( sz>=65536 );
  iOffset = walFrameOffset(iRead, sz) + WAL_FRAME_HDRSIZE;
  /* testcase( IS_BIG_INT(iOffset) ); // requires a 4GiB WAL */
  return sqlite3OsRead(pWal->pWalFd, pOut, (nOut>sz ? sz : nOut), iOffset);"""
PATCHED = b"""  sz = (sz&0xfe00) + ((sz&0x0001)<<16);
  if( nOut>sz ){
    memset(pOut+sz, 0, nOut-sz);
    nOut = sz;
  }
  testcase( sz<=32768 );
  testcase( sz>=65536 );
  iOffset = walFrameOffset(iRead, sz) + WAL_FRAME_HDRSIZE;
  /* testcase( IS_BIG_INT(iOffset) ); // requires a 4GiB WAL */
  return sqlite3OsRead(pWal->pWalFd, pOut, nOut, iOffset);"""


def bounded_file(path, maximum):
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or not 0 < info.st_size <= maximum:
        raise RuntimeError("sqlite_regression_source_bounds")
    data = path.read_bytes()
    if len(data) != info.st_size:
        raise RuntimeError("sqlite_regression_source_changed")
    return data



def closed_json(data):
    if len(data) > 4096:
        raise ValueError("protocol_bounds")

    def unique_pairs(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("protocol_duplicate")
            result[key] = value
        return result

    def invalid_constant(value):
        raise ValueError("protocol_constant")

    return json.loads(data, object_pairs_hook=unique_pairs,
                      parse_constant=invalid_constant)


def run_owned(arguments, directory, timeout, child_case=None):
    """Keep child output private/bounded and reap its owned process group."""
    if signal.getsignal(signal.SIGCHLD) != signal.SIG_DFL:
        raise RuntimeError("sqlite_regression_child_ownership")
    environment = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8",
                   "TMPDIR": str(directory)}
    deadline = time.monotonic() + timeout
    output = {"out": bytearray(), "err": bytearray()}
    selector = None
    completed = False
    child = subprocess.Popen(arguments, cwd=directory, env=environment,
                             stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, start_new_session=True)
    try:
        selector = selectors.DefaultSelector()
        selector.register(child.stdout, selectors.EVENT_READ, "out")
        selector.register(child.stderr, selectors.EVENT_READ, "err")
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise RuntimeError("sqlite_regression_child_timeout")
            for key, _ in selector.select(min(remaining, 0.25)):
                block = os.read(key.fileobj.fileno(), 65536)
                if not block:
                    selector.unregister(key.fileobj)
                    continue
                output[key.data].extend(block)
                if len(output[key.data]) > 65536:
                    raise RuntimeError("sqlite_regression_output_bounds")
        # Observe without reaping: the group ID cannot be reused before cleanup.
        while time.monotonic() < deadline:
            observed = os.waitid(os.P_PID, child.pid,
                                 os.WEXITED | os.WNOHANG | os.WNOWAIT)
            if observed is not None:
                completed = True
                break
            time.sleep(0.01)
        if not completed:
            raise RuntimeError("sqlite_regression_child_timeout")
    finally:
        primary_failure = sys.exc_info()[0] is not None
        cleanup_failed = False
        try:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            except BaseException:
                cleanup_failed = True
            try:
                child.wait(timeout=5)
            except BaseException:
                cleanup_failed = True
        finally:
            for resource in (selector, child.stdout, child.stderr):
                if resource is None:
                    continue
                try:
                    resource.close()
                except BaseException:
                    cleanup_failed = True
        if cleanup_failed and not primary_failure:
            raise RuntimeError("sqlite_regression_cleanup_failed")
    if child.returncode != 0:
        # Reflect only closed synthetic-driver phases, never arbitrary output.
        phases = {"arguments", "path_length", "path_exists", "allocator_config",
                  "lookaside_config", "pagecache_config", "create_database",
                  "write_wal", "fixture_prefix", "open_fixture", "query_config",
                  "prepare", "first_row", "column_type", "column_size",
                  "prefix_mismatch", "tail_mismatch", "extra_row", "finalize",
                  "close", "shutdown"}
        if child_case in {"baseline", "patched"}:
            try:
                failure = closed_json(bytes(output["out"]))
                if (type(failure) is dict and
                    set(failure) == {"case", "ok", "phase", "code"} and
                    failure["case"] == child_case and failure["ok"] is False and
                    type(failure["phase"]) is str and failure["phase"] in phases and
                    type(failure["code"]) is int and
                    -1000000 <= failure["code"] <= 1000000):
                    raise RuntimeError("sqlite_regression_%s_%s_%d" %
                                       (child_case, failure["phase"], failure["code"]))
            except (ValueError, UnicodeDecodeError, TypeError, RecursionError):
                pass
        raise RuntimeError("sqlite_regression_child_failed")
    return bytes(output["out"])


@unittest.skipUnless(sys.platform == "linux", "Linux full-engine regression")
class SqliteWalPageTailTests(unittest.TestCase):
    def test_original_engine_exposes_poison_and_backport_zero_fills_tail(self):
        sqlite = ROOT / "vendor/libsqlite3-sys/sqlite3"
        driver = ROOT / "scripts/fixtures/sqlite_wal_page_tail.c"
        source = bounded_file(sqlite / "sqlite3.c", 16 * 1024 * 1024)
        header = bounded_file(sqlite / "sqlite3.h", 1024 * 1024)
        bounded_file(driver, 65536)
        self.assertEqual(hashlib.sha256(source).hexdigest(), PATCHED_SHA)
        self.assertEqual(hashlib.sha256(header).hexdigest(), HEADER_SHA)
        self.assertEqual(source.count(PATCHED), 1)
        original = source.replace(PATCHED, ORIGINAL, 1)
        self.assertEqual(hashlib.sha256(original).hexdigest(), ORIGINAL_SHA)
        compiler = Path("/usr/bin/cc")
        self.assertTrue(compiler.is_file(), "sqlite_regression_compiler_missing")
        parent = os.environ.get("RUNNER_TEMP")
        with tempfile.TemporaryDirectory(prefix="morpheus-sqlite-wal-",
                                         dir=parent) as temporary:
            work = Path(temporary)
            baseline_source = work / "upstream.c"
            baseline_source.write_bytes(original)
            evidence = {}
            for case, engine in (("baseline", baseline_source),
                                 ("patched", sqlite / "sqlite3.c")):
                executable = work / case
                run_owned([str(compiler), "-std=c11", "-D_GNU_SOURCE",
                           "-DSQLITE_THREADSAFE=1", "-DSQLITE_OMIT_LOAD_EXTENSION",
                           "-O0", "-g0", "-I", str(sqlite), str(driver),
                           str(engine), "-o", str(executable), "-pthread", "-lm"],
                          work, 180)
                data = run_owned([str(executable), case, str(work / (case + ".db"))],
                                 work, 15, child_case=case)
                try:
                    result = closed_json(data)
                except (ValueError, UnicodeDecodeError, RecursionError):
                    self.fail("sqlite_regression_invalid_protocol")
                expected = {"case": case, "ok": True, "prefix_bytes": 378,
                            "tail_bytes": 1670,
                            "expected_tail": 165 if case == "baseline" else 0}
                # Do not include untrusted output in assertion diagnostics.
                valid_types = (type(result) is dict and
                               set(result) == set(expected) and
                               type(result.get("case")) is str and
                               type(result.get("ok")) is bool and
                               all(type(result.get(key)) is int for key in
                                   ("prefix_bytes", "tail_bytes", "expected_tail")))
                if not valid_types or result != expected:
                    self.fail("sqlite_regression_engine_evidence_mismatch")
                evidence[case] = result
            self.assertEqual(set(evidence), {"baseline", "patched"})
