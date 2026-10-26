#!/usr/bin/env python3
"""Pre-synced encrypted Sticky2025 interop with an unmodified official RC peer.

Runs only on ephemeral Linux GitHub runners after the shared MatrixRTC fixture
is ready. Browser/SDK logs, accounts, captures and tool installations remain in
the private external root. No screenshots, traces or raw logs are published.
"""
from __future__ import annotations

import argparse
import contextlib
import json
import math
import os
from pathlib import Path
import re
import signal
import stat
import struct
import subprocess
import sys
import time
import wave

from fixture_paths import external_path, private_directory, write_private
from element_peer_fixture import down, up
from element_process_namespace import assert_contained

REPO = Path(__file__).resolve().parent.parent
TEST_NAME = "element_call_browser_peer_encrypted_media_and_departure"
CHECKS = frozenset((
    "official_image",
    "browser_identity", "browser_presynced", "browser_roster", "browser_video", "browser_audio", "browser_leave",
    "native_presynced_baseline", "native_roster", "native_audio", "native_video", "native_departure",
    "native_group_roster", "native_late_join_media", "native_group_departure", "native_key_rotation",
    "browser_group_roster", "browser_group_audio", "browser_screen", "browser_group_departure",
    "browser_key_rotation", "browser_post_rotation_screen", "browser_post_rotation_audio",
))
STAGES = frozenset((
    "configuration", "tool_install", "browser_install", "native_test_build", "media_fixture",
    "browser_launch", "browser_login", "browser_identity", "browser_room_sync", "native_join_wait",
    "browser_join", "browser_roster", "browser_receive_video", "browser_receive_audio", "native_media_wait", "browser_leave",
    "browser_group_roster", "browser_group_audio", "browser_receive_screen",
    "browser_group_departure", "browser_key_rotation",
    "element_fixture_root", "element_configuration", "element_image_pull", "element_image_verify",
    "element_container_create", "element_container_start", "element_ready", "element_cleanup",
    "native_test", "log_scan", "complete",
))
FAILURES = frozenset(("fixture_operation", "browser_acceptance", "native_acceptance", "deadline", "evidence", "log_scan"))
NATIVE_STAGES = ("microphone_cryptor", "late_login", "late_sync", "late_bootstrap", "late_join", "group_roster")
NATIVE_STAGE_PREFIX = "MORPHEUS_ELEMENT_NATIVE_STAGE "
MAX_LOG_BYTES = 64 * 1024 * 1024
MAX_LOG_FILES = 128
MAX_LOG_ENTRIES = 65_536
MAX_LOG_DEPTH = 32
CHECK_PATTERN = re.compile(r"^MORPHEUS_ELEMENT_CHECK ([a-z_]+)=(true|false)$", re.MULTILINE)
STAGE_PATTERN = re.compile(r"^MORPHEUS_ELEMENT_STAGE ([a-z_]+)$", re.MULTILINE)
FAILURE_PATTERN = re.compile(r"^MORPHEUS_ELEMENT_FAILURE ([a-z_]+)$", re.MULTILINE)


CONSOLE_START = b"MORPHEUS_ELEMENT_CONSOLE_CAPTURE started\n"
CONSOLE_CAP = b"MORPHEUS_ELEMENT_CONSOLE_CAPTURE capped\n"
CONSOLE_FRAME = b"MORPHEUS_ELEMENT_CONSOLE_FRAME "
CONSOLE_END = b"\nMORPHEUS_ELEMENT_CONSOLE_END\n"
SENDER_DETAIL_FIELDS = ("original_roster", "late_roster", "continuity_detail", "late_current_bob")
SENDER_FIELDS = ("capture", "continuity", "device", "attempted", "queued", "http_mark", "original", "receiver") + SENDER_DETAIL_FIELDS
SENDER_VALUES = {
    "capture": frozenset(("observed_window", "unknown")),
    "continuity": frozenset(("same", "changed", "unknown")),
    "device": frozenset(("observed", "unknown")),
    "attempted": frozenset(("observed", "unknown")),
    "queued": frozenset(("observed", "unknown")),
    "http_mark": frozenset(("observed", "unknown")),
    "original": frozenset(("decrypted", "missing_key", "withheld", "unknown_index", "trust",
                            "other_utd", "not_encrypted", "timeout", "error", "unknown")),
    "receiver": frozenset(("decrypted", "missing_key", "withheld", "unknown_index", "trust",
                            "other_utd", "not_encrypted", "timeout", "error", "unknown")),
    "original_roster": frozenset(("exact", "missing_alice", "missing_bob", "missing_carol", "missing_multiple",
                                  "unexpected_or_duplicate", "unavailable", "unknown")),
    "late_roster": frozenset(("exact", "missing_alice", "missing_bob", "missing_carol", "missing_multiple",
                              "unexpected_or_duplicate", "unavailable", "unknown")),
    "continuity_detail": frozenset(("same", "not_unique", "different_live", "live_changed", "wire_changed", "unknown")),
    "late_current_bob": frozenset(("present", "missing", "ambiguous", "unknown")),
}
SENDER_PREFIX = "MORPHEUS_ELEMENT_SENDER "
# Exact finite output budget from closed literal names/categories; never raw IDs.
SENDER_MAX_LINE = len(SENDER_PREFIX) + len(SENDER_FIELDS) - 1 + sum(
    len(key) + 1 + max(map(len, SENDER_VALUES[key])) for key in SENDER_FIELDS)


def console_window(data: bytes):
    """After scanning raw bytes, remove length-framed console DATA from facts.

    Actual owned child termination closes the observed window. It does not prove
    all SDK logging was available/delivered, so absent records remain UNKNOWN.
    Partial framing, duplicate starts, caps or invalid UTF-8 cannot yield evidence.
    """
    clean, records = [], []
    cursor = total = 0
    started = capped = False
    try:
        while cursor < len(data):
            end = data.find(b"\n", cursor)
            end = len(data) if end < 0 else end + 1
            line = data[cursor:end]
            cursor = end
            if line == CONSOLE_START:
                if started:
                    raise ValueError
                started = True
            elif line == CONSOLE_CAP:
                if not started or capped:
                    raise ValueError
                capped = True
            elif line.startswith(CONSOLE_FRAME):
                if not started or capped or len(line) > 64 or len(records) >= 4_096:
                    raise ValueError
                value = line[len(CONSOLE_FRAME):-1]
                if not re.fullmatch(rb"0|[1-9][0-9]{0,4}", value):
                    raise ValueError
                length = int(value)
                if length > 65_536 or total + length > 2 * 1024 * 1024:
                    raise ValueError
                stop = cursor + length
                if data[stop:stop + len(CONSOLE_END)] != CONSOLE_END:
                    raise ValueError
                records.append(data[cursor:stop].decode("utf8", errors="strict"))
                total += length
                cursor = stop + len(CONSOLE_END)
            elif line.startswith(b"MORPHEUS_ELEMENT_CONSOLE_"):
                raise ValueError
            else:
                clean.append(line)
        return b"".join(clean), records if started and not capped else [], started and not capped
    except (ValueError, UnicodeError):
        # No raw fallback: malformed framing could contain forged fact lines.
        return b"", [], False


def private_sender_records(data: bytes, prefix: bytes, keys, limit=1, optional_keys=()):
    """One bounded flat private record, never a public ID/path/error payload."""
    found = []
    cursor = 0
    while (start := data.find(prefix, cursor)) >= 0:
        cursor = start + len(prefix)
        if start and data[start - 1:start] != b"\n":
            continue
        end = data.find(b"\n", cursor)
        end = len(data) if end < 0 else end
        if end - cursor > 8_192 or len(found) >= limit:
            return []
        try:
            def unique(pairs):
                result = {}
                for key, value in pairs:
                    if key in result:
                        raise ValueError
                    result[key] = value
                return result
            value = json.loads(data[cursor:end], object_pairs_hook=unique)
            if (not isinstance(value, dict) or set(value) not in (set(keys), set(keys) | set(optional_keys))
                    or any(not isinstance(item, str) or not 0 < len(item) <= 1_024
                           or not item.isascii() or any(ord(char) < 32 for char in item)
                           for item in value.values())):
                return []
        except (ValueError, RecursionError):
            return []
        found.append(value)
    return found


def debug_string(text, name):
    values = re.findall(r"(?:^|\s)" + re.escape(name) + r'=("(?:[^"\\]|\\.)*")(?=\s|$)', text)
    if not values:
        raise ValueError
    parsed = [json.loads(value) for value in values]
    if any(not isinstance(value, str) or not 0 < len(value) <= 1_024 for value in parsed) or len(set(parsed)) != 1:
        raise ValueError
    return parsed[0]


def recipients_field(text, name, tuples=False):
    """Only the pinned Debug map-of-sets / list-of-triples grammar, no eval."""
    lines = [line.strip() for line in text.splitlines() if line.startswith("    ") and name + "=" in line]
    if len(lines) != 1 or lines[0].count(name + "=") != 1:
        raise ValueError
    value = lines[0].split(name + "=", 1)[1]
    pos = 0
    decoder = json.JSONDecoder()
    def token(char):
        nonlocal pos
        while pos < len(value) and value[pos].isspace():
            pos += 1
        if not value.startswith(char, pos):
            raise ValueError
        pos += len(char)
    def string():
        nonlocal pos
        while pos < len(value) and value[pos].isspace():
            pos += 1
        item, end = decoder.raw_decode(value, pos)
        if not isinstance(item, str) or not 0 < len(item) <= 1_024:
            raise ValueError
        pos = end
        return item
    result = set()
    if tuples:
        token("[")
        for count in range(129):
            if value[pos:].lstrip().startswith("]"):
                token("]")
                break
            if count == 128:
                raise ValueError
            if count:
                token(",")
            token("(")
            string()  # message ID is private; only exact user/device are selected.
            token(",")
            user = string()
            token(",")
            device = string()
            token(")")
            if (user, device) in result:
                raise ValueError
            result.add((user, device))
        else:
            raise ValueError
    else:
        token("{")
        users = set()
        for count in range(65):
            if value[pos:].lstrip().startswith("}"):
                token("}")
                break
            if count == 64:
                raise ValueError
            if count:
                token(",")
            user = string()
            if user in users:
                raise ValueError
            users.add(user)
            token(":")
            token("{")
            for index in range(129):
                if value[pos:].lstrip().startswith("}"):
                    token("}")
                    break
                if index == 128 or len(result) >= 128:
                    raise ValueError
                if index:
                    token(",")
                device = string()
                if (user, device) in result:
                    raise ValueError
                result.add((user, device))
            else:
                raise ValueError
        else:
            raise ValueError
    # Remaining fields are parsed independently; delimiters cannot be smuggled.
    if pos < len(value) and not value[pos].isspace():
        raise ValueError
    return result


def sender_detail_values_valid(value):
    if any(value.get(key) not in SENDER_VALUES[key] for key in SENDER_DETAIL_FIELDS):
        return False
    detail = value["continuity_detail"]
    if detail == "unknown":
        return all(value[key] == "unknown" for key in SENDER_DETAIL_FIELDS)
    if value.get("continuity") != ("same" if detail == "same" else "changed"):
        return False
    return (detail not in ("not_unique", "live_changed")
            or value["late_current_bob"] == "unknown")


def sender_observation(records, native: bytes, browser: bytes, window: bool):
    result = dict.fromkeys(SENDER_FIELDS, "unknown")
    if window:
        result["capture"] = "observed_window"
    bindings = private_sender_records(native, b"MORPHEUS_ELEMENT_SENDER_PRIVATE ", (
        "room_id", "bob", "carol", "carol_device", "carol_curve", "event_id", "session_id", "continuity", "original", "receiver"),
        optional_keys=SENDER_DETAIL_FIELDS)
    devices = private_sender_records(browser, b"MORPHEUS_ELEMENT_DEVICE_PRIVATE ", (
        "room_id", "bob", "bob_device", "carol", "carol_device", "carol_curve", "known_user", "known_device", "known_curve", "sample"), 8)
    if len(bindings) != 1:
        return result
    binding = bindings[0]
    # A legacy private binding can establish only its old fields. An extended
    # binding must contain the complete closed group and consistent continuity.
    if any(key in binding for key in SENDER_DETAIL_FIELDS):
        if not sender_detail_values_valid(binding):
            return result
        for key in SENDER_DETAIL_FIELDS:
            result[key] = binding[key]
    if binding["continuity"] in SENDER_VALUES["continuity"]:
        result["continuity"] = binding["continuity"]
    if binding["original"] in SENDER_VALUES["original"]:
        result["original"] = binding["original"]
    if binding["receiver"] in SENDER_VALUES["receiver"]:
        result["receiver"] = binding["receiver"]
    # The native same-cipher/receiver receipt is independent of stock logging.
    # The bounded cached-device receipt is independent of console capture too.
    samples = [device["sample"] for device in devices]
    if (not devices or len(samples) != len(set(samples)) or any(sample not in tuple(str(index) for index in range(1, 9)) for sample in samples)
            or binding["continuity"] != "same" or binding["original"] != "decrypted"
            or any(binding[key] != device[key] for device in devices for key in ("room_id", "bob", "carol", "carol_device", "carol_curve"))
            or not any((device["known_user"], device["known_device"], device["known_curve"]) ==
                       (binding["carol"], binding["carol_device"], binding["carol_curve"]) for device in devices)):
        return result
    result["device"] = "observed"
    # No console capture means no attempted/queued/HTTP-marked sharing claim.
    if not window:
        return result
    expected = (binding["carol"], binding["carol_device"])
    queued = {}
    seen_queued = set()
    marked = set()
    pending_recipients = set()
    try:
        for text in records:
            if "INFO matrix_sdk_crypto::session_manager::group_sessions: Trying to encrypt a room key\n" in text:
                pending_recipients = set()
                if (debug_string(text, "session_id") == binding["session_id"]
                        and debug_string(text, "room_id") == binding["room_id"]):
                    pending_recipients = recipients_field(text, "recipients")
                    if expected in pending_recipients:
                        result["attempted"] = "observed"
            elif "INFO matrix_sdk_crypto::session_manager::group_sessions: Created batch of to-device messages of type m.room.encrypted\n" in text:
                if debug_string(text, "session_id") != binding["session_id"]:
                    continue
                request = debug_string(text, "request_id")
                if request in seen_queued:
                    raise ValueError
                seen_queued.add(request)
                recipients = recipients_field(text, "message_list", tuples=True)
                # Require the actual-session attempt, not just a span label:
                # the pinned SDK records its span before possible key rotation.
                if expected in recipients and recipients <= pending_recipients:
                    queued[request] = recipients
                    result["queued"] = "observed"
            elif "INFO matrix_sdk_crypto::olm::group_sessions::outbound: Marking to-device request carrying a room key or a withheld as sent\n" in text:
                request = debug_string(text, "request_id")
                if request in marked:
                    raise ValueError
                marked.add(request)
                recipients = recipients_field(text, "recipients")
                if request in queued and expected in queued[request] and recipients == queued[request]:
                    result["http_mark"] = "observed"
    except (ValueError, RecursionError):
        # Unsupported/malformed/ambiguous formats yield no sender claims.
        for key in ("attempted", "queued", "http_mark"):
            result[key] = "unknown"
    return result


def sender_diagnostics(output: str):
    values = []
    for line in output.splitlines():
        if line.startswith(SENDER_PREFIX):
            if len(line) > SENDER_MAX_LINE or values:
                return dict.fromkeys(("sender_" + key for key in SENDER_FIELDS), "unknown")
            fields = line[len(SENDER_PREFIX):].split(" ")
            if len(fields) != len(SENDER_FIELDS):
                return dict.fromkeys(("sender_" + key for key in SENDER_FIELDS), "unknown")
            result = {}
            for field, key in zip(fields, SENDER_FIELDS):
                head, sep, value = field.partition("=")
                if not sep or head != key or value not in SENDER_VALUES[key]:
                    return dict.fromkeys(("sender_" + name for name in SENDER_FIELDS), "unknown")
                result["sender_" + key] = value
            if not sender_detail_values_valid({key: result["sender_" + key] for key in SENDER_FIELDS}):
                return dict.fromkeys(("sender_" + key for key in SENDER_FIELDS), "unknown")
            values.append(result)
    return values[0] if values else {}


def evidence(output: str, *, complete=False) -> dict[str, bool]:
    result = {}
    for key, value in CHECK_PATTERN.findall(output):
        if key in CHECKS:
            result[key] = result.get(key, True) and value == "true"
    if complete and (set(result) != CHECKS or not all(result.values())
                     or any(value in FAILURES for value in FAILURE_PATTERN.findall(output))):
        raise RuntimeError("evidence")
    return result


def native_stages(output: str) -> tuple[str, ...]:
    """Bound each candidate line before copying a fixed native stage."""
    result = []
    cursor = 0
    while (start := output.find("MORPHEUS_ELEMENT_NATIVE_STAGE", cursor)) >= 0:
        cursor = start + len("MORPHEUS_ELEMENT_NATIVE_STAGE")
        if start and output[start - 1] != "\n":
            continue
        end = output.find("\n", start)
        end = len(output) if end < 0 else end
        if (len(result) >= len(NATIVE_STAGES) or end - start > 96
                or output[start:end] != NATIVE_STAGE_PREFIX + NATIVE_STAGES[len(result)]):
            return ("invalid",)
        result.append(NATIVE_STAGES[len(result)])
    return tuple(result)


def diagnostics(output: str, *, include_native=True) -> dict[str, str]:
    result = {}
    stages = [value for value in STAGE_PATTERN.findall(output) if value in STAGES]
    failures = [value for value in FAILURE_PATTERN.findall(output) if value in FAILURES]
    if stages:
        result["stage"] = stages[-1]
    if failures:
        result["failure"] = failures[-1]
    if include_native and (native := native_stages(output)):
        result["native_stage"] = native[-1]
    result.update(sender_diagnostics(output))
    return result


def exact_native_pass(output: str) -> bool:
    return (not re.search(r"\bskipped\b", output, re.I)
            and re.search(r"^test " + TEST_NAME + r" \.\.\. ok$", output, re.M) is not None
            and re.search(r"^test result: ok\. 1 passed; 0 failed; 0 ignored;", output, re.M) is not None)


def native_executable(output: str) -> Path:
    matches = []
    for line in output.splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (value.get("reason") == "compiler-artifact" and value.get("target", {}).get("name") == "element_call_peer"
                and value.get("profile", {}).get("test") is True and value.get("executable")):
            path = Path(value["executable"]).resolve()
            if path.is_file() and path.is_relative_to((REPO / "target").resolve()):
                matches.append(path)
    if len(matches) != 1:
        raise RuntimeError("native_acceptance")
    return matches[0]


def media_fixtures(root: Path) -> None:
    """Browser fake capture uses red/green YUV video and a real nonzero tone."""
    root = private_directory(root)
    camera = root / "camera.y4m"
    fd = os.open(camera, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    # Exactly the color pattern the native receiver verifies, planar 4:2:0.
    pixels = (bytes([82]) * 320 + bytes([145]) * 320) * 360
    pixels += (bytes([90]) * 160 + bytes([54]) * 160) * 180
    pixels += (bytes([240]) * 160 + bytes([34]) * 160) * 180
    with os.fdopen(fd, "wb") as stream:
        stream.write(b"YUV4MPEG2 W640 H360 F15:1 Ip A1:1 C420jpeg\n")
        for _ in range(30):
            stream.write(b"FRAME\n" + pixels)
    audio = root / "microphone.wav"
    fd = os.open(audio, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as stream, wave.open(stream, "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(48_000)
        wav.writeframes(b"".join(struct.pack("<h", int(8_000 * math.sin(2 * math.pi * 440 * n / 48_000)))
                                  for n in range(96_000)))


class Child:
    """Keep the owned group leader unreaped until every group member is killed."""
    def __init__(self, args, log: Path, env, cwd):
        self.log = log
        self.started = time.monotonic()
        fd = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
        self.log_anchor = fd
        try:
            self.log_identity = self.file_identity(os.fstat(fd))
            with os.fdopen(os.dup(fd), "wb") as output:
                self.process = subprocess.Popen(args, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                                stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        except BaseException:
            os.close(fd)
            self.log_anchor = None
            raise
        self.closed = False
        self.joined = False

    @staticmethod
    def file_identity(info):
        return (info.st_dev, info.st_ino, info.st_uid, info.st_gid, info.st_mode, info.st_nlink)

    def release_log_anchor(self):
        if self.log_anchor is not None:
            if self.file_identity(os.fstat(self.log_anchor))[:2] != self.log_identity[:2]:
                raise RuntimeError("fixture_operation")
            os.close(self.log_anchor)
            self.log_anchor = None

    def status(self):
        result = os.waitid(os.P_PID, self.process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
        if result is None:
            return None
        return result.si_status if result.si_code == os.CLD_EXITED else -result.si_status

    def close(self):
        if self.closed:
            return
        # The waitable, unreaped direct child is the ownership anchor. Once
        # somebody else reaps it, a cached PID is no longer safe to signal.
        if self.process.returncode is not None:
            self.closed = True
            raise RuntimeError("fixture_operation")
        try:
            self.status()
        except ChildProcessError:
            self.closed = True
            raise RuntimeError("fixture_operation") from None
        try:
            os.killpg(self.process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        finally:
            self.process.wait(timeout=10)
            self.joined = True
            self.closed = True


def wait_peers(native: Child, browser: Child, seconds=360):
    while True:
        # Sample both terminal statuses even when both finish between polls.
        # A final browser.close error must not be hidden by earlier markers.
        native_status, browser_status = native.status(), browser.status()
        if native_status is not None and native_status != 0:
            raise RuntimeError("native_acceptance")
        if browser_status is not None and browser_status != 0:
            raise RuntimeError("browser_acceptance")
        if native_status == 0 and browser_status == 0:
            return
        if time.monotonic() - native.started >= seconds:
            raise RuntimeError("deadline")
        time.sleep(0.2)


class Runner:
    def __init__(self, root: Path, gitleaks: str):
        self.root = private_directory(root)
        self.logs = private_directory(root / "logs")
        self.gitleaks = gitleaks
        self.env = dict(os.environ)
        self.env.update({"RUST_BACKTRACE": "0", "PLAYWRIGHT_BROWSERS_PATH": str(root / "browsers"),
                         "npm_config_cache": str(root / "npm-cache"), "npm_config_userconfig": str(root / "npmrc")})
        write_private(root / "npmrc", "registry=https://registry.npmjs.org/\n")
        self.stage = "configuration"
        self.sequence = 0
        self.children = []
        self.sender_diagnostic = {}
        self.root_identity = Child.file_identity(self.root.stat())[:-1]

    def mark(self, value):
        assert value in STAGES
        self.stage = value
        print("MORPHEUS_ELEMENT_STAGE " + value, flush=True)

    def start(self, args, *, extra=None, cwd=REPO):
        if len(self.children) >= MAX_LOG_FILES:
            raise RuntimeError("fixture_operation")
        self.sequence += 1
        child = Child(args, self.logs / f"{self.sequence:02}.log", self.env | (extra or {}), cwd)
        self.children.append(child)
        return child

    def wait(self, child, seconds):
        while (status := child.status()) is None:
            if time.monotonic() - child.started >= seconds:
                raise RuntimeError("deadline")
            time.sleep(0.1)
        child.close()
        if status:
            raise RuntimeError("fixture_operation")
        return child.log.read_text(errors="replace")

    def command(self, args, *, seconds=300, cwd=REPO):
        return self.wait(self.start(args, cwd=cwd), seconds)

    def service(self, action):
        self.sequence += 1
        fd = os.open(self.logs / f"{self.sequence:02}.log", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as output, contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            return action()

    def close(self):
        failed = False
        for child in reversed(self.children):
            try:
                child.close()
            except BaseException:
                failed = True
        if failed or any(child.joined is not True for child in self.children):
            raise RuntimeError("fixture_operation")

    def release_log_anchors(self):
        failed = False
        for child in self.children:
            try:
                child.release_log_anchor()
            except BaseException:
                failed = True
        if failed:
            raise RuntimeError("fixture_operation")

    def log_snapshots(self):
        """Read only bounded regular logs through pinned no-follow directory FDs."""
        expected = {child.log.relative_to(self.root).as_posix(): child for child in self.children}
        found = set()
        snapshots = []
        total = entries = 0

        def stable(info):
            return (*Child.file_identity(info), info.st_size, info.st_mtime_ns, info.st_ctime_ns)

        def visit(directory, relative, depth):
            nonlocal total, entries
            if depth > MAX_LOG_DEPTH:
                raise RuntimeError("log_scan")
            with os.scandir(directory) as listing:
                for entry in listing:
                    entries += 1
                    if entries > MAX_LOG_ENTRIES:
                        raise RuntimeError("log_scan")
                    info = entry.stat(follow_symlinks=False)
                    name = relative + entry.name
                    if stat.S_ISDIR(info.st_mode):
                        fd = os.open(entry.name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                                     dir_fd=directory)
                        try:
                            if Child.file_identity(os.fstat(fd)) != Child.file_identity(info):
                                raise RuntimeError("log_scan")
                            visit(fd, name + "/", depth + 1)
                        finally:
                            os.close(fd)
                    elif entry.name.endswith(".log"):
                        if len(snapshots) >= MAX_LOG_FILES or not stat.S_ISREG(info.st_mode):
                            raise RuntimeError("log_scan")
                        fd = os.open(entry.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                                     dir_fd=directory)
                        try:
                            before = os.fstat(fd)
                            if (not stat.S_ISREG(before.st_mode) or stable(before) != stable(info)
                                    or not 0 <= before.st_size <= MAX_LOG_BYTES - total - bool(snapshots)):
                                raise RuntimeError("log_scan")
                            if child := expected.get(name):
                                if (child.log_anchor is None
                                        or Child.file_identity(os.fstat(child.log_anchor)) != child.log_identity
                                        or Child.file_identity(before) != child.log_identity):
                                    raise RuntimeError("log_scan")
                                found.add(name)
                            chunks, size = [], 0
                            while chunk := os.read(fd, min(65_536, before.st_size - size + 1)):
                                size += len(chunk)
                                if size > before.st_size:
                                    raise RuntimeError("log_scan")
                                chunks.append(chunk)
                            if (size != before.st_size or stable(os.fstat(fd)) != stable(before)
                                    or stable(os.stat(entry.name, dir_fd=directory, follow_symlinks=False)) != stable(before)):
                                raise RuntimeError("log_scan")
                            data = b"".join(chunks)
                            total += size + bool(snapshots)
                            snapshots.append((name, data))
                        finally:
                            os.close(fd)

        fd = os.open(self.root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
        try:
            if Child.file_identity(os.fstat(fd))[:-1] != self.root_identity:
                raise RuntimeError("log_scan")
            visit(fd, "", 0)
            if found != set(expected):
                raise RuntimeError("log_scan")
        finally:
            os.close(fd)
        return sorted(snapshots)

    def scan(self, native=None, browser=None):
        if any(child.joined is not True for child in self.children):
            raise RuntimeError("fixture_operation")
        if (native is not None and native is browser
                or any(child is not None and not any(owner is child for owner in self.children) for child in (native, browser))):
            raise RuntimeError("log_scan")
        try:
            snapshots = self.log_snapshots()
        except (OSError, ValueError):
            raise RuntimeError("log_scan") from None
        combined = b"\n".join(data for _, data in snapshots)
        native_output = next((data for name, data in snapshots
                              if native is not None and name == native.log.relative_to(self.root).as_posix()), b"")
        try:
            result = subprocess.run([self.gitleaks, "stdin", "--redact=100", "--ignore-gitleaks-allow", "--no-banner"],
                                    input=combined, capture_output=True, timeout=30, cwd=self.root)
        except (subprocess.TimeoutExpired, OSError):
            raise RuntimeError("log_scan") from None
        if result.returncode:
            raise RuntimeError("log_scan")
        if browser is not None:
            browser_name = browser.log.relative_to(self.root).as_posix()
            browser_output = next((data for name, data in snapshots if name == browser_name), b"")
            clean, records, window = console_window(browser_output)
            self.sender_diagnostic = sender_observation(records, native_output, clean, window)
            # Console text is never fed to acceptance/stage parsers, even if it
            # contains an exact forged success marker. Scan used the RAW bytes.
            combined = b"\n".join(clean if name == browser_name else data for name, data in snapshots)
        elif CONSOLE_FRAME in combined or CONSOLE_START in combined:
            raise RuntimeError("evidence")
        return (combined.decode(errors="replace").replace("\r\n", "\n").replace("\r", "\n"),
                native_output.decode(errors="replace").replace("\r\n", "\n").replace("\r", "\n"))


def execute(root: Path, fixture: Path, gitleaks: str):
    if sys.platform != "linux" or os.environ.get("GITHUB_ACTIONS") != "true" or os.environ.get("RUNNER_OS") != "Linux":
        raise RuntimeError("configuration")
    if "MORPHEUS_ELEMENT_DIAG_REFRESH" in os.environ:
        raise RuntimeError("configuration")
    assert_contained()
    fixture = external_path(fixture)
    if not fixture.is_file() or fixture.stat().st_mode & 0o077:
        raise RuntimeError("configuration")
    root = private_directory(root)
    runner = Runner(root, gitleaks)
    failure = None
    failed_stage = "configuration"
    native = browser = None
    sender = {}
    peers_complete = False
    children_joined = False
    cleanup_failure = None
    try:
        runner.mark("tool_install")
        tool = private_directory(root / "tool")
        for name in ("package.json", "package-lock.json"):
            write_private(tool / name, (REPO / "scripts/element-peer" / name).read_text(), create=True)
        runner.command(["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund", "--omit=optional"], cwd=tool)
        runner.mark("browser_install")
        # Trusted CI apt installs OS dependencies before the privilege drop.
        runner.command(["node", str(tool / "node_modules/playwright-core/cli.js"), "install", "chromium"], seconds=360, cwd=tool)
        runner.mark("native_test_build")
        built = runner.command(["cargo", "test", "--locked", "-p", "morpheus-rtc", "--features", "native-devices",
                                "--test", "element_call_peer", "--no-run", "--message-format=json"], seconds=600)
        binary = native_executable(built)
        peer = runner.service(lambda: up(root / "service"))
        if peer.get("official_image_identity_verified") is not True:
            raise RuntimeError("evidence")
        write_private(runner.logs / "image.log", "MORPHEUS_ELEMENT_CHECK official_image=true\n", create=True)
        runner.mark("media_fixture")
        media_fixtures(root)
        extra = {"MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS": str(fixture), "MORPHEUS_ELEMENT_PEER_ROOT": str(root),
                 "MORPHEUS_ELEMENT_PLAYWRIGHT_ROOT": str(tool), "MORPHEUS_ELEMENT_PEER_URL": peer["base_url"],
                 "MORPHEUS_ELEMENT_INTEROP_MODE": "sticky-2025", "MORPHEUS_ELEMENT_PEER_GO_FILE": str(root / "go"),
                 "MORPHEUS_ELEMENT_PEER_DONE_FILE": str(root / "done")}
        browser = runner.start(["node", "scripts/element_call_stock_peer.mjs"], extra=extra)
        # Browser keys and room sync exist before the native device joins the
        # room/call. Historical-membership key recovery is a separate test.
        while not (root / "browser-ready").is_file():
            if browser.status() is not None:
                raise RuntimeError("browser_acceptance")
            if time.monotonic() - browser.started >= 180:
                raise RuntimeError("deadline")
            time.sleep(0.2)
        runner.mark("native_test")
        native = runner.start([str(binary), "--exact", TEST_NAME, "--show-output", "--test-threads=1"], extra=extra)
        wait_peers(native, browser)
        peers_complete = True
    except BaseException as error:
        failed_stage = runner.stage
        failure = str(error) if isinstance(error, RuntimeError) and str(error) in FAILURES else "fixture_operation"
    finally:
        try:
            runner.close()
            children_joined = True
        except BaseException:
            cleanup_failure = "fixture_operation"
        try:
            runner.service(lambda: down(root / "service", keep_data=True))
        except BaseException:
            cleanup_failure = "fixture_operation"
    try:
        if not children_joined or cleanup_failure is not None:
            raise RuntimeError("fixture_operation")
        output, native_output = runner.scan(native, browser)
        sender = runner.sender_diagnostic
        # This is the exact native log snapshot included in the successful
        # scan, inspected only after owned group teardown and actual waits.
        if peers_complete and children_joined and not exact_native_pass(native_output):
            failure = "evidence"
        failure = failure or cleanup_failure
        facts = evidence(output, complete=not failure)
    except RuntimeError as error:
        failure = str(error)
        output, native_output, facts, sender = "", "", {}, {}
    finally:
        try:
            runner.release_log_anchors()
        except BaseException:
            failure = "fixture_operation"
            output, native_output, facts, sender = "", "", {}, {}
    for key, value in sorted(facts.items()):
        print(f"MORPHEUS_ELEMENT_CHECK {key}={str(value).lower()}", flush=True)
    if children_joined and cleanup_failure is None:
        for value in native_stages(native_output):
            print(NATIVE_STAGE_PREFIX + value, flush=True)
    if failure and sender:
        print(SENDER_PREFIX + " ".join(key + "=" + sender[key] for key in SENDER_FIELDS), flush=True)
    # Cleanup is required but must not erase the last useful failure stage.
    diagnostic_output = output.replace("MORPHEUS_ELEMENT_STAGE element_cleanup\n", "")
    if found := diagnostics(diagnostic_output, include_native=False):
        if "stage" in found:
            runner.mark(found["stage"])
    elif failure:
        runner.mark(failed_stage)
    if failure:
        print("MORPHEUS_ELEMENT_FAILURE " + failure, flush=True)
        raise RuntimeError(failure)
    runner.mark("complete")
    return facts


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--gitleaks", required=True)
    args = parser.parse_args()
    # The outer acceptance runner has a longer watchdog and also calls down.
    # A graceful cancellation still lets this process kill its owned groups.
    def interrupted(_signal, _frame):
        raise RuntimeError("deadline")
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    signal.signal(signal.SIGALRM, interrupted)
    signal.alarm(1_740)
    execute(args.root, args.fixture, args.gitleaks)


if __name__ == "__main__":
    try:
        main()
    except BaseException:
        # Exceptions can carry command arguments, passwords or URLs.
        print("Element Call acceptance failed; raw diagnostics withheld", file=sys.stderr)
        raise SystemExit(1)
