import contextlib
import io
import hashlib
import json
import os
from pathlib import Path
import signal
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock
import wave

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import run_element_acceptance as target
import modified_sender_peer as modified
import run_matrix_acceptance as matrix


class EvidenceTests(unittest.TestCase):
    def test_group_roster_failure_receipt_is_fixed_bounded_and_keeps_failure(self):
        facts = {field: True for field in target.GROUP_ROSTER_FIELDS}
        raw = target.GROUP_ROSTER_PREFIX + json.dumps(facts) + "\n"
        self.assertEqual(target.group_roster_receipt(raw), facts)
        self.assertEqual(target.diagnostics(raw), {"group_roster": facts})
        partial = facts | {"alice_count_is_3": False, "alice_exactly_one_carol": False}
        self.assertEqual(target.group_roster_receipt(target.GROUP_ROSTER_PREFIX + json.dumps(partial)), partial)
        self.assertEqual(target.diagnostics(raw, include_native=False), {})
        self.assertIsNone(target.group_roster_receipt("private ordinary text"))
        self.assertIsNone(target.group_roster_receipt("embedded " + raw))
        invalid = [raw + raw, target.GROUP_ROSTER_PREFIX + "x" * 512,
                   target.GROUP_ROSTER_PREFIX + "[]\n",
                   target.GROUP_ROSTER_PREFIX + json.dumps(facts | {"private_owner": True}),
                   target.GROUP_ROSTER_PREFIX + json.dumps({key: value for key, value in facts.items()
                                                           if key != target.GROUP_ROSTER_FIELDS[0]}),
                   target.GROUP_ROSTER_PREFIX + json.dumps(facts)[:-1] + ',"alice_available":false}']
        for value in (0, 1, "true", None, [], {}):
            invalid.append(target.GROUP_ROSTER_PREFIX + json.dumps(facts | {"alice_available": value}))
        for malformed in invalid:
            with self.subTest(malformed=malformed[:40]):
                self.assertEqual(target.group_roster_receipt(malformed), "invalid")
                self.assertEqual(target.diagnostics(malformed), {"group_roster": "invalid"})
        unicode_oversize = target.GROUP_ROSTER_PREFIX + '{"unknown":"' + "é" * 250 + '"}'
        self.assertLessEqual(len(unicode_oversize), 512)
        self.assertGreater(len(unicode_oversize.encode("utf-8")), 512)
        with mock.patch.object(target.json, "loads") as parse:
            self.assertEqual(target.group_roster_receipt(unicode_oversize), "invalid")
            parse.assert_not_called()
        failed = raw + "MORPHEUS_ELEMENT_FAILURE native_acceptance\n"
        failed += "".join(f"MORPHEUS_ELEMENT_CHECK {name}=true\n" for name in target.CHECKS)
        with self.assertRaisesRegex(RuntimeError, "evidence"):
            target.evidence(failed, complete=True)
        self.assertEqual(target.diagnostics(failed), {"failure": "native_acceptance", "group_roster": facts})

    def test_initial_native_failure_reduction_is_source_bound_and_closed(self):
        header = ("thread 'element_call_browser_peer_encrypted_media_and_departure' (123) panicked at "
                  "crates/morpheus-rtc/tests/element_call_peer.rs:999:6:")
        for message, reason in target.INITIAL_NATIVE_PANICS.items():
            raw = header + "\n" + message + ": opaque-private-detail"
            self.assertEqual(target.initial_native_reason(raw), reason)
            published = target.NATIVE_REASON_PREFIX + target.initial_native_reason(raw) + "\n"
            self.assertEqual(target.diagnostics(published), {"native_reason": reason})
            self.assertNotIn("opaque", published)
            for wrong in (raw.replace("element_call_peer.rs", "other.rs"), message,
                          header + "\n" + message + "wrong-suffix", header + "\n" + message + ":" + "x" * 2_048):
                with self.subTest(message=message, wrong=wrong):
                    self.assertEqual(target.initial_native_reason(wrong), "unclassified")
        first, second = list(target.INITIAL_NATIVE_PANICS)[:2]
        self.assertEqual(target.initial_native_reason(header + "\n" + first + "\n" + header + "\n" + second), "multiple")
        for status, kind in ((1, "exit_nonzero"), (101, "exit_nonzero"), (-signal.SIGABRT, "signal_abort"),
                             (-signal.SIGSEGV, "signal_segv"), (-signal.SIGKILL, "signal_kill"),
                             (-999, "signal_other"), (None, "unclassified"), (0, "unclassified"), (True, "unclassified")):
            self.assertEqual(target.native_exit_kind(status), kind)
        for prefix, field, values in ((target.NATIVE_REASON_PREFIX, "native_reason", target.NATIVE_REASONS),
                                      (target.NATIVE_EXIT_PREFIX, "native_exit", target.NATIVE_EXITS)):
            for value in values:
                line = prefix + value
                self.assertEqual(target.diagnostics(line), {field: value})
                self.assertEqual(target.diagnostics(line, include_native=False), {})
                self.assertEqual(target.diagnostics(line + "\n" + line), {field: "invalid"})
            self.assertEqual(target.diagnostics(prefix + "opaque_private"), {field: "invalid"})
            self.assertEqual(target.diagnostics(prefix + "x" * 128), {field: "invalid"})
            self.assertEqual(target.diagnostics("embedded " + prefix + "unclassified"), {})

    def test_observer_profiles_are_bound_to_exact_verified_sdk_and_image(self):
        pairs = (
            ("fa4772a4ddd335a5a8bfbf8bb21df49b70f7a999", "sha256:6d83fe15c425e1139edafdffe2b2073256608dcafdc554da5bf840de496891fe", "sdk42"),
            ("3f05dc4d104a3dfc753c5e3549a18c74fab4b076", "sha256:c450902cf3f0dfa20ce23c9ab3b619959c571682dfa5cce16479a0c3d842896e", "sdk44"),
        )
        for index, (sdk, image, profile) in enumerate(pairs):
            peer = {"sdk_revision": sdk, "image_digest": image, "official_image_identity_verified": True}
            self.assertEqual(target.observer_profile(peer), profile)
            for changes in ({"sdk_revision": "unknown"}, {"image_digest": "unknown"},
                            {"official_image_identity_verified": False}, {"sdk_revision": pairs[1 - index][0]}):
                with self.subTest(profile=profile, changes=changes), self.assertRaisesRegex(RuntimeError, "evidence"):
                    target.observer_profile({**peer, **changes})

    def test_pinned_observer_binds_sdk44_wrappers_and_retains_legacy_raw_binding(self):
        # Execute only the exact observer function with disposable JS objects.
        # No browser, network, SDK import, media source, or package install.
        source = (target.REPO / "scripts/element_call_stock_peer.mjs").read_text()
        installer = source.split("// BEGIN pinned SDK44 public-wrapper observer.", 1)[1].split("\n", 1)[1].split(
            "// END pinned SDK44 public-wrapper observer.", 1)[0]
        legacy = source.split("async function observeAudio(", 1)[1].split("const binding = () => {", 1)[1].split(
            "\n        };", 1)[0]
        fixture = r'''
const vm = require("node:vm");
const [installer, legacy] = JSON.parse(require("node:fs").readFileSync(0, "utf8"));
const context = vm.createContext({ setTimeout, clearTimeout, queueMicrotask });
vm.runInContext(installer + `
class MediaStream {
  constructor(track) { this.track = track; }
  getAudioTracks() { return this.track.kind === "audio" ? [this.track] : []; }
  getVideoTracks() { return this.track.kind === "video" ? [this.track] : []; }
}
const owner = { room_id: "!room", user_id: "@owner", device_id: "DEVICE", member_id: "member", identity: "identity", track_sid: "SID" };
function setup(kind = "audio") {
  const stats = { type: "inbound-rtp", kind, trackIdentifier: "decoded", bytesReceived: 512, packetsReceived: 3 };
  const track = { kind, source: kind === "audio" ? "microphone" : "screenShare", id: "SID", encrypted: true,
    muted$: { value: false }, stats$: () => ({ subscribe(observer) { let live = true;
      queueMicrotask(() => { if (live) observer.next(stats); }); return { unsubscribe() { live = false; } }; } }) };
  const member = { local: false, userId: owner.user_id, deviceId: owner.device_id, memberId: owner.member_id,
    backendIdentities: [owner.identity], tracks$: { value: [track] } };
  const remote = { value: [member] };
  const media = { id: "decoded", kind, enabled: true, readyState: "live" };
  const props = kind === "audio" ? { track } : { video: track };
  const element = { srcObject: new MediaStream(media), paused: false, muted: false, volume: 1,
    __reactFiber$fixture: { memoizedProps: props, return: { memoizedProps: { tracks$: member.tracks$ }, return: {
      memoizedProps: { vm: { remoteMembers$: remote } }, return: null } } } };
  const membership = { userId: owner.user_id, deviceId: owner.device_id, memberId: owner.member_id,
    backendIdentities: [owner.identity], rtcBackendIdentity: "@owner:DEVICE" };
  const session = { room: { roomId: owner.room_id }, memberships: [membership, {}] };
  globalThis.window = { rtcSession: session, matrixclient: { getDeviceId: () => "BROWSER",
    matrixRTC: { roomSessions: new Map([[owner.room_id, session]]) } } };
  globalThis.document = { querySelectorAll: () => [element] };
  installStock44Observer();
  return { track, member, remote, media, props, element, membership, session, stats };
}
async function check(kind, mutate) {
  const value = setup(kind); if (mutate) mutate(value);
  return await window.__morpheusStock44Binding({ owner, expectedMembers: 2, kind, deadline: Date.now() + 2000 });
}
globalThis.run = async () => {
  for (const kind of ["audio", "video"]) {
    if (!(await check(kind))) throw new Error("legitimate pinned44 binding refused");
    for (const mutate of [
      (x) => x.member.userId = "@wrong", (x) => x.member.deviceId = "WRONG",
      (x) => x.member.memberId = "wrong", (x) => x.member.backendIdentities = ["wrong"],
      (x) => x.member.backendIdentities = [owner.identity, owner.identity], (x) => x.member.local = true,
      (x) => x.remote.value = [], (x) => x.member.tracks$.value = [{ ...x.track }],
      (x) => x.props[kind === "audio" ? "track" : "video"] = { ...x.track },
      (x) => x.element.__reactFiber$fixture = { memoizedProps: {}, return: null },
      (x) => x.element.srcObject = null, (x) => x.media.id = "unrelated-decoded",
      (x) => document.querySelectorAll = () => [x.element, { ...x.element }],
      (x) => x.media.readyState = "ended", (x) => x.media.enabled = false,
      (x) => x.track.id = "WRONG-SID", (x) => x.track.source = "camera",
      (x) => x.track.encrypted = false, (x) => x.track.muted$.value = true,
      (x) => x.stats.type = "outbound-rtp", (x) => x.stats.kind = "wrong",
      (x) => x.stats.trackIdentifier = "wrong", (x) => x.stats.trackIdentifier = "",
      (x) => x.stats.bytesReceived = 0, (x) => x.stats.packetsReceived = 0,
      (x) => x.track.stats$ = null,
      (x) => x.track.stats$ = () => ({ subscribe(observer) { queueMicrotask(() => observer.next(undefined)); return { unsubscribe() {} }; } }),
      (x) => x.track.stats$ = () => ({ subscribe(observer) { queueMicrotask(() => {
        x.member.tracks$.value = [{ ...x.track }]; observer.next(x.stats); }); return { unsubscribe() {} }; } }),
      (x) => x.session.memberships.push(x.membership),
      (x) => x.track.stats$ = () => ({ subscribe(observer) { queueMicrotask(() => {
        x.remote.value = [{ ...x.member }]; observer.next(x.stats); }); return { unsubscribe() {} }; } }),
    ]) if (await check(kind, mutate)) throw new Error("wrong or stale44 binding accepted");
  }
};
`, context);
(async () => {
  await context.run();
  vm.runInContext(`
const fixture = setup("audio");
const observerProfile = "sdk42", expectedMembers = 2, deadline = Date.now() + 2000;
const raw = { kind: "audio", source: "microphone", sid: "SID", mediaStreamTrack: fixture.media, attachedElements: [fixture.element] };
const publication = { track: raw, source: "microphone", kind: "audio", isLocal: false, isSubscribed: true,
  isEncrypted: true, isMuted: false, trackSid: "SID" };
const participant = { isLocal: false, identity: owner.identity, audioTrackPublications: new Map([["SID", publication]]),
  getTrackPublicationBySid: (sid) => sid === "SID" ? publication : undefined };
fixture.membership.rtcBackendIdentity = owner.identity;
fixture.element.__reactFiber$fixture = { memoizedProps: { trackRef: { participant, publication, source: "microphone" } }, return: null };
const binding = () => { ${legacy} };
if (!binding()) throw new Error("legacy42 raw binding refused");
participant.identity = "wrong";
if (binding()) throw new Error("legacy wrong owner accepted");
participant.identity = owner.identity; raw.attachedElements = [];
if (binding()) throw new Error("legacy unattached track accepted");
`, context);
})().catch(() => { process.exitCode = 1; });
'''
        node = shutil.which("node")
        self.assertIsNotNone(node, "Existing hosted Node runtime is unavailable")
        self.assertTrue(Path(node).is_absolute(), "Existing hosted Node runtime must resolve absolutely")
        result = subprocess.run([node, "-e", fixture], input=json.dumps([installer, legacy]),
                                text=True, capture_output=True, timeout=20, env={"PATH": os.defpath})
        self.assertEqual(result.returncode, 0, "Pinned synthetic observer contract failed")

    def test_requires_every_fixed_assertion_and_preserves_failure(self):
        complete = "\n".join(f"MORPHEUS_ELEMENT_CHECK {name}=true" for name in target.CHECKS)
        self.assertEqual(target.evidence(complete, complete=True), dict.fromkeys(target.CHECKS, True))
        for value in ("", complete.replace("browser_roster=true", "browser_roster=false"),
                      "MORPHEUS_ELEMENT_CHECK native_audio=false\n" + complete,
                      complete + "\nMORPHEUS_ELEMENT_FAILURE browser_acceptance"):
            with self.subTest(value=value), self.assertRaisesRegex(RuntimeError, "evidence"):
                target.evidence(value, complete=True)
        self.assertEqual(target.evidence("MORPHEUS_ELEMENT_CHECK secret_value=true"), {})
        # Historical eleven-fact evidence cannot establish reverse audio. A
        # later true must never erase a false reverse-audio observation.
        self.assertIn("browser_audio", target.CHECKS)
        for value in (complete.replace("MORPHEUS_ELEMENT_CHECK browser_audio=true", ""),
                      "MORPHEUS_ELEMENT_CHECK browser_audio=false\n" + complete,
                      complete + "\nMORPHEUS_ELEMENT_CHECK browser_audio=false"):
            with self.subTest(reverse_audio=value), self.assertRaisesRegex(RuntimeError, "evidence"):
                target.evidence(value, complete=True)

        # The original two-peer proof cannot satisfy the independently required
        # late third participant, same sender screen and post-rotation media.
        group = (
            "native_group_roster", "native_late_join_media", "native_group_departure", "native_key_rotation",
            "native_same_device_rejoin",
            "browser_group_roster", "browser_group_audio", "browser_screen", "browser_group_departure",
            "browser_key_rotation", "browser_post_rotation_screen", "browser_post_rotation_audio",
        )
        for name in group:
            self.assertIn(name, target.CHECKS)
            for value in (complete.replace(f"MORPHEUS_ELEMENT_CHECK {name}=true", ""),
                          f"MORPHEUS_ELEMENT_CHECK {name}=false\n" + complete):
                with self.subTest(group_fact=name), self.assertRaisesRegex(RuntimeError, "evidence"):
                    target.evidence(value, complete=True)

    def test_exact_named_nonempty_test_result_is_required(self):
        result = f"test {target.TEST_NAME} ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured;"
        self.assertTrue(target.exact_native_pass(result))
        for value in (result.replace(target.TEST_NAME, "other"), result.replace("1 passed", "0 passed"),
                      result.replace("0 ignored", "1 ignored"), result.replace("... ok", "... FAILED"),
                      result + "\nSkipped native fixture", result.splitlines()[-1]):
            with self.subTest(value=value):
                self.assertFalse(target.exact_native_pass(value))

    def test_diagnostics_are_allowlisted(self):
        result = target.diagnostics("\n".join((
            "MORPHEUS_ELEMENT_STAGE browser_roster", "MORPHEUS_ELEMENT_STAGE opaque_credential",
            "MORPHEUS_ELEMENT_FAILURE browser_acceptance", "MORPHEUS_ELEMENT_FAILURE private_password",
        )))
        self.assertEqual(result, {"stage": "browser_roster", "failure": "browser_acceptance"})

    def test_native_group_stages_are_closed_ordered_and_bounded(self):
        lines = [target.NATIVE_STAGE_PREFIX + value for value in target.NATIVE_STAGES]
        for count in range(1, len(lines) + 1):
            output = "\n".join(lines[:count])
            self.assertEqual(target.native_stages(output), target.NATIVE_STAGES[:count])
            self.assertEqual(target.diagnostics(output), {"native_stage": target.NATIVE_STAGES[count - 1]})
        self.assertEqual(target.native_stages("ordinary private log"), ())
        for output in (lines[1], lines[0] + "\n" + lines[0], "\n".join(lines + [lines[-1]]),
                       lines[0] + " private", target.NATIVE_STAGE_PREFIX + "opaque_secret",
                       target.NATIVE_STAGE_PREFIX + "x" * 97):
            with self.subTest(output=output):
                self.assertEqual(target.native_stages(output), ("invalid",))
                self.assertEqual(target.diagnostics(output), {"native_stage": "invalid"})

    def test_native_failure_facts_follow_owned_cleanup_and_successful_scan(self):
        for scanner_status, join_failure, service_failure in ((0, False, False), (1, False, False),
                                                            (0, True, False), (0, False, True)):
            with self.subTest(scanner_status=scanner_status, join_failure=join_failure, service_failure=service_failure), \
                    tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                fixture = root / "fixture.json"
                fixture.write_text("{}")
                fixture.chmod(0o600)
                (root / "browser-ready").touch()
                runner = target.Runner(root, "scanner")
                roster_facts = {field: True for field in target.GROUP_ROSTER_FIELDS}
                roster_line = target.GROUP_ROSTER_PREFIX + json.dumps(roster_facts) + "\n"
                native = self.anchored_log(runner, "native.log", (
                    "opaque-fixture-panic\nMORPHEUS_ELEMENT_CHECK native_audio=true\n"
                    "thread 'element_call_browser_peer_encrypted_media_and_departure' (123) panicked at "
                    "crates/morpheus-rtc/tests/element_call_peer.rs:999:6:\n"
                    "Browser microphone track was not subscribed: opaque-private-detail\n"
                    + target.NATIVE_STAGE_PREFIX + "microphone_cryptor\n"
                    + target.NATIVE_STAGE_PREFIX + "late_login\n" + roster_line).encode())
                browser = self.anchored_log(runner, "browser.log", b"MORPHEUS_ELEMENT_STAGE browser_group_roster\n")
                native.joined = browser.joined = False
                native.status = mock.Mock(return_value=1)
                browser.status = mock.Mock(return_value=None)
                events = []
                def native_join():
                    events.append("native_join")
                    if join_failure:
                        raise RuntimeError("fixture_operation")
                    native.joined = True
                def browser_join():
                    events.append("browser_join")
                    browser.joined = True
                native.close = native_join
                browser.close = browser_join
                runner.children = [browser, native]
                def scanned(*args, **kwargs):
                    events.append("scan")
                    return subprocess.CompletedProcess([], scanner_status)
                original_snapshots = runner.log_snapshots
                def snapshot():
                    events.append("read")
                    return original_snapshots()
                visible = io.StringIO()
                with mock.patch.object(target.sys, "platform", "linux"), mock.patch.dict(os.environ, {
                    "GITHUB_ACTIONS": "true", "RUNNER_OS": "Linux",
                }, clear=True), mock.patch.object(target, "assert_contained"), \
                        mock.patch.object(target, "Runner", return_value=runner), \
                        mock.patch.object(runner, "command", return_value=""), \
                        mock.patch.object(runner, "service", side_effect=[{"official_image_identity_verified": True, "base_url": "http://127.0.0.1:18080",
                                                                         "sdk_revision": "fa4772a4ddd335a5a8bfbf8bb21df49b70f7a999",
                                                                         "image_digest": "sha256:6d83fe15c425e1139edafdffe2b2073256608dcafdc554da5bf840de496891fe"},
                                                                      RuntimeError("fixture_operation") if service_failure else None]), \
                        mock.patch.object(runner, "start", side_effect=[browser, native]), \
                        mock.patch.object(target, "native_executable", return_value=Path("native")), \
                        mock.patch.object(target, "media_fixtures"), \
                        mock.patch.object(target.subprocess, "run", side_effect=scanned), \
                        mock.patch.object(runner, "log_snapshots", side_effect=snapshot), \
                        contextlib.redirect_stdout(visible), self.assertRaisesRegex(
                            RuntimeError, "fixture_operation" if join_failure or service_failure else (
                                "native_acceptance" if scanner_status == 0 else "log_scan")):
                    target.execute(root, fixture, "scanner")
                self.assertEqual(events, ["native_join", "browser_join"] + ([] if join_failure or service_failure else ["read", "scan"]))
                self.assertIsNone(native.log_anchor)
                self.assertIsNone(browser.log_anchor)
                output = visible.getvalue()
                self.assertNotIn("opaque", output)
                admitted_roster = scanner_status == 0 and not join_failure and not service_failure
                self.assertEqual(target.diagnostics(output).get("group_roster"),
                                 roster_facts if admitted_roster else None)
                self.assertEqual(target.evidence(output),
                                 {"native_audio": True, "official_image": True} if scanner_status == 0 and not join_failure and not service_failure else {})
                self.assertEqual(target.diagnostics(output).get("native_stage"),
                                 "late_login" if scanner_status == 0 and not join_failure and not service_failure else None)
                admitted = scanner_status == 0 and not join_failure and not service_failure
                self.assertEqual(target.diagnostics(output).get("native_reason"),
                                 "remote_microphone_subscription" if admitted else None)
                self.assertEqual(target.diagnostics(output).get("native_exit"), "exit_nonzero" if admitted else None)
                self.assertNotIn("opaque-private-detail", output)

    @staticmethod
    def anchored_log(runner, name, data):
        child = target.Child.__new__(target.Child)
        child.log = runner.logs / name
        child.log_anchor = os.open(child.log, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        child.log_identity = child.file_identity(os.fstat(child.log_anchor))
        os.write(child.log_anchor, data)
        child.joined = True
        runner.children.append(child)
        return child

    def test_log_scan_uses_exact_original_binary_snapshot_before_decode(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = target.Runner(Path(directory), "scanner")
            raw = b"\xffopaque-fixture-log\nMORPHEUS_ELEMENT_NATIVE_STAGE microphone_cryptor\n"
            child = self.anchored_log(runner, "native.log", raw)
            try:
                def scanned(*args, **kwargs):
                    os.ftruncate(child.log_anchor, 0)
                    os.lseek(child.log_anchor, 0, os.SEEK_SET)
                    os.write(child.log_anchor, b"changed after snapshot")
                    return subprocess.CompletedProcess([], 0)
                with mock.patch.object(target.subprocess, "run", side_effect=scanned) as scan:
                    combined, native = runner.scan(child)
                supplied = scan.call_args.kwargs["input"]
                self.assertIsInstance(supplied, bytes)
                self.assertEqual(hashlib.sha256(supplied).digest(), hashlib.sha256(raw).digest())
                self.assertEqual(combined, raw.decode(errors="replace"))
                self.assertEqual(native, combined)
            finally:
                runner.release_log_anchors()

    def test_log_snapshot_refuses_symlink_replacement_and_oversize_before_scan(self):
        for change in ("symlink", "replacement", "missing", "fifo", "alias", "oversize", "metadata"):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as directory:
                runner = target.Runner(Path(directory), "scanner")
                child = self.anchored_log(runner, "native.log", b"private")
                try:
                    if change == "symlink":
                        child.log.unlink()
                        other = Path(directory) / "other"
                        other.write_bytes(b"other")
                        child.log.symlink_to(other)
                    elif change == "replacement":
                        child.log.unlink()
                        child.log.write_bytes(b"replacement")
                    elif change == "missing":
                        child.log.unlink()
                    elif change == "fifo":
                        child.log.unlink()
                        os.mkfifo(child.log, 0o600)
                    elif change == "alias":
                        os.link(child.log, Path(directory) / "alias")
                    elif change == "oversize":
                        os.ftruncate(child.log_anchor, target.MAX_LOG_BYTES + 1)
                    else:
                        child.log.chmod(0o644)
                    with mock.patch.object(target.subprocess, "run") as scan, \
                            mock.patch.object(target.os, "read") as read, self.assertRaisesRegex(RuntimeError, "log_scan"):
                        runner.scan(child)
                    scan.assert_not_called()
                    read.assert_not_called()
                finally:
                    runner.release_log_anchors()

    def test_log_snapshot_refuses_change_during_read_and_bounded_tree_overflow(self):
        for change in ("during_read", "entries", "files", "depth"):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as directory:
                runner = target.Runner(Path(directory), "scanner")
                child = self.anchored_log(runner, "native.log", b"private")
                original_read = os.read
                def changed_read(fd, count):
                    data = original_read(fd, count)
                    if data:
                        os.ftruncate(child.log_anchor, 0)
                    return data
                try:
                    with contextlib.ExitStack() as stack:
                        if change == "during_read":
                            stack.enter_context(mock.patch.object(target.os, "read", side_effect=changed_read))
                        else:
                            stack.enter_context(mock.patch.object(target, {
                                "entries": "MAX_LOG_ENTRIES", "files": "MAX_LOG_FILES", "depth": "MAX_LOG_DEPTH",
                            }[change], 0))
                        scan = stack.enter_context(mock.patch.object(target.subprocess, "run"))
                        with self.assertRaisesRegex(RuntimeError, "log_scan"):
                            runner.scan(child)
                        scan.assert_not_called()
                finally:
                    runner.release_log_anchors()

    def test_combined_binary_log_limit_includes_record_separators(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = target.Runner(Path(directory), "scanner")
            first = self.anchored_log(runner, "first.log", b"aaaa")
            self.anchored_log(runner, "second.log", b"bbbb")
            try:
                with mock.patch.object(target, "MAX_LOG_BYTES", 8), \
                        mock.patch.object(target.subprocess, "run") as scan, self.assertRaisesRegex(RuntimeError, "log_scan"):
                    runner.scan(first)
                scan.assert_not_called()
            finally:
                runner.release_log_anchors()

    def test_native_artifact_must_be_one_named_test_inside_target(self):
        with tempfile.TemporaryDirectory() as folder:
            repo = Path(folder)
            binary = repo / "target/debug/deps/element_call_peer-hash"
            binary.parent.mkdir(parents=True)
            binary.touch()
            entry = {"reason": "compiler-artifact", "target": {"name": "element_call_peer"},
                     "profile": {"test": True}, "executable": str(binary)}
            with mock.patch.object(target, "REPO", repo):
                self.assertEqual(target.native_executable(json.dumps(entry)), binary.resolve())
                with self.assertRaises(RuntimeError):
                    target.native_executable(json.dumps(entry) + "\n" + json.dumps(entry))
                entry["profile"]["test"] = False
                with self.assertRaises(RuntimeError):
                    target.native_executable(json.dumps(entry))

    def test_fake_capture_files_have_distinct_known_video_and_nonzero_audio(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target.media_fixtures(root)
            data = (root / "camera.y4m").read_bytes()
            header, frames = data.split(b"\n", 1)
            self.assertEqual(header, b"YUV4MPEG2 W640 H360 F15:1 Ip A1:1 C420jpeg")
            self.assertEqual(len(frames), 30 * (6 + 640 * 360 * 3 // 2))
            self.assertEqual(frames[:6], b"FRAME\n")
            self.assertEqual(frames[6:326], bytes([82]) * 320)
            self.assertEqual(frames[326:646], bytes([145]) * 320)
            with wave.open(str(root / "microphone.wav")) as wav:
                self.assertEqual((wav.getnchannels(), wav.getframerate(), wav.getnframes()), (1, 48_000, 96_000))
                self.assertTrue(any(wav.readframes(480)))
            for file in root.iterdir():
                self.assertEqual(file.stat().st_mode & 0o777, 0o600)

    def test_diagnostic_refresh_is_rejected_before_starting_children(self):
        with tempfile.TemporaryDirectory() as directory:
            with mock.patch.object(target.sys, "platform", "linux"), mock.patch.dict(os.environ, {
                "GITHUB_ACTIONS": "true", "RUNNER_OS": "Linux", "MORPHEUS_ELEMENT_DIAG_REFRESH": "0",
            }), mock.patch.object(target, "Runner") as runner, self.assertRaises(RuntimeError):
                target.execute(Path(directory), Path(directory) / "fixture.json", "scanner")
            runner.assert_not_called()

    def test_scanner_failure_never_emits_log_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = target.Runner(Path(directory), "scanner")
            (runner.logs / "private.log").write_text("opaque-fixture-password")
            visible = io.StringIO()
            with mock.patch.object(target.subprocess, "run", return_value=subprocess.CompletedProcess([], 1)), contextlib.redirect_stdout(visible), self.assertRaisesRegex(RuntimeError, "log_scan"):
                runner.scan()
            self.assertNotIn("opaque", visible.getvalue())

    def test_cleanup_kills_group_before_reaping_even_after_leader_exit(self):
        child = target.Child.__new__(target.Child)
        child.closed = False
        child.process = mock.Mock(pid=12345, returncode=None)
        child.status = mock.Mock(return_value=7)
        events = []
        child.process.wait.side_effect = lambda **kwargs: events.append("reap")
        with mock.patch.object(target.os, "killpg", side_effect=lambda *_: events.append("kill")):
            child.close()
            child.close()
        self.assertEqual(events, ["kill", "reap"])

    def test_cleanup_refuses_to_signal_a_reaped_leader(self):
        child = target.Child.__new__(target.Child)
        child.closed = False
        child.process = mock.Mock(pid=12345, returncode=None)
        child.status = mock.Mock(side_effect=ChildProcessError())
        with mock.patch.object(target.os, "killpg") as kill, self.assertRaisesRegex(RuntimeError, "fixture_operation"):
            child.close()
        kill.assert_not_called()

    def test_cleanup_attempts_every_child_despite_an_ownership_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = target.Runner(Path(directory), "scanner")
            first, second = mock.Mock(), mock.Mock()
            second.close.side_effect = RuntimeError("fixture_operation")
            runner.children = [first, second]
            with self.assertRaises(RuntimeError):
                runner.close()
            first.close.assert_called_once()
            second.close.assert_called_once()

    def test_simultaneously_finished_peers_must_both_succeed(self):
        for native_status, browser_status, expected in ((0, 0, None), (0, 1, "browser_acceptance"),
                                                       (1, 0, "native_acceptance"), (1, 1, "native_acceptance")):
            native, browser = mock.Mock(), mock.Mock()
            native.status.return_value = native_status
            browser.status.return_value = browser_status
            with self.subTest(native=native_status, browser=browser_status):
                if expected:
                    with self.assertRaisesRegex(RuntimeError, expected):
                        target.wait_peers(native, browser)
                else:
                    target.wait_peers(native, browser)

    def test_peers_finishing_between_polls_still_check_terminal_status(self):
        native, browser = mock.Mock(started=time.monotonic()), mock.Mock()
        native.status.side_effect = [None, 0]
        browser.status.side_effect = [None, 1]
        with mock.patch.object(target.time, "sleep"), self.assertRaisesRegex(RuntimeError, "browser_acceptance"):
            target.wait_peers(native, browser)


@unittest.skipUnless(sys.platform == "linux", "Linux owned-group lifecycle")
class OwnedProcessTests(unittest.TestCase):
    def test_exited_leader_keeps_its_descendant_owned_until_cleanup(self):
        program = (
            "import subprocess,sys,time\n"
            "from pathlib import Path\n"
            "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)'])\n"
            "Path(sys.argv[1]).write_text(str(child.pid))\n"
            "raise SystemExit(7)\n"
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            ready = root / "child"
            child = target.Child([sys.executable, "-c", program, str(ready)], root / "raw.log", dict(os.environ), root)
            descendant = None
            descendant_start = None
            try:
                deadline = time.monotonic() + 5
                while child.status() is None:
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.01)
                descendant = int(ready.read_text())
                descendant_start = Path(f"/proc/{descendant}/stat").read_text().rsplit(")", 1)[1].split()[19]
                self.assertEqual(child.status(), 7)
                self.assertIsNone(child.process.returncode, "exit observation must not reap")
                self.assertTrue(Path(f"/proc/{child.process.pid}").exists())
                child.close()
                while Path(f"/proc/{descendant}/stat").exists():
                    state = Path(f"/proc/{descendant}/stat").read_text().rsplit(")", 1)[1].split()[0]
                    if state in ("Z", "X"):
                        break
                    self.assertLess(time.monotonic(), deadline, "descendant still running")
                    time.sleep(0.01)
                self.assertEqual(child.process.returncode, 7)
            finally:
                child.close()
                child.release_log_anchor()
                if descendant is not None:
                    with contextlib.suppress(ProcessLookupError, FileNotFoundError):
                        current = Path(f"/proc/{descendant}/stat").read_text().rsplit(")", 1)[1].split()[19]
                        if current == descendant_start:
                            os.kill(descendant, signal.SIGKILL)


class ModifiedPeerAssetsTests(unittest.TestCase):
    def make_assets(self, folder, *, variant="renewal44"):
        root = Path(folder) / "modified-peer"
        root.mkdir(mode=0o700)
        dist = root / "dist"
        dist.mkdir(mode=0o700)
        (dist / "index.html").write_bytes(b"synthetic app")
        (dist / "config.json").write_bytes(b"")
        receipt = {"schema": 1, "classification": "MODIFIED_PEER", "variant": variant,
                   "proposal_applied": variant == "renewal44", "element_revision": modified.ELEMENT_REVISION,
                   "sdk_revision": modified.SDK_REVISION, "proposal_sha256": modified.PROPOSAL_SHA256,
                   "element_lock_sha256": modified.ELEMENT_LOCK_SHA256, "builder_source_sha256": "0" * 64,
                   "assets": [{"path": name, "bytes": (dist / name).stat().st_size,
                               "sha256": hashlib.sha256((dist / name).read_bytes()).hexdigest()}
                              for name in ("index.html", "config.json")]}
        return root, receipt

    def seal(self, root, receipt):
        raw = json.dumps(receipt, sort_keys=True).encode()
        (root / "build-receipt.json").write_bytes(raw)
        return hashlib.sha256(raw).hexdigest()

    def test_exact_receipt_and_full_owned_tree_are_required(self):
        with tempfile.TemporaryDirectory() as folder:
            root, receipt = self.make_assets(folder)
            digest = self.seal(root, receipt)
            self.assertEqual(modified.verify_modified_assets(root, digest, variant="renewal44"), root / "dist")
            with self.assertRaises(RuntimeError):
                modified.verify_modified_assets(root, "f" * 64, variant="renewal44")
            (root / "dist/index.html").write_bytes(b"changed")
            with self.assertRaises(RuntimeError):
                modified.verify_modified_assets(root, digest, variant="renewal44")

    def test_wrong_proposal_and_path_escape_are_rejected_even_with_matching_receipt_hash(self):
        for field in ("proposal", "path"):
            with self.subTest(field=field), tempfile.TemporaryDirectory() as folder:
                root, receipt = self.make_assets(folder)
                if field == "proposal":
                    receipt["proposal_sha256"] = "f" * 64
                else:
                    receipt["assets"][0]["path"] = "../outside"
                digest = self.seal(root, receipt)
                with self.assertRaises(RuntimeError):
                    modified.verify_modified_assets(root, digest, variant="renewal44")

    def test_symlinks_hardlinks_and_unlisted_files_are_rejected(self):
        for kind in ("symlink", "hardlink", "unlisted"):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as folder:
                root, receipt = self.make_assets(folder)
                digest = self.seal(root, receipt)
                path = root / "dist/index.html"
                if kind == "unlisted":
                    (root / "dist/unlisted").write_bytes(b"extra")
                else:
                    outside = Path(folder) / "outside"
                    outside.write_bytes(path.read_bytes())
                    path.unlink()
                    if kind == "symlink":
                        path.symlink_to(outside)
                    else:
                        os.link(outside, path)
                with self.assertRaises(RuntimeError):
                    modified.verify_modified_assets(root, digest, variant="renewal44")

    def test_compressed_configuration_cannot_shadow_owned_loopback_configuration(self):
        with tempfile.TemporaryDirectory() as folder:
            root, receipt = self.make_assets(folder)
            (root / "dist/config.json.gz").write_bytes(b"not a configuration")
            receipt["assets"].append({"path": "config.json.gz", "bytes": 19,
                                      "sha256": hashlib.sha256(b"not a configuration").hexdigest()})
            digest = self.seal(root, receipt)
            with self.assertRaises(RuntimeError):
                modified.verify_modified_assets(root, digest, variant="renewal44")

    def test_modified_labels_cannot_satisfy_official_stock_evidence(self):
        output = "\n".join(f"MORPHEUS_MODIFIED_PEER_CHECK {name}=true" for name in target.CHECKS)
        self.assertEqual(target.evidence(output), {})
        with self.assertRaises(RuntimeError):
            target.evidence(output, complete=True)

    def test_both_closed_variants_require_their_own_matching_receipts(self):
        for variant in ("baseline44", "renewal44"):
            with self.subTest(variant=variant), tempfile.TemporaryDirectory() as folder:
                root, receipt = self.make_assets(folder, variant=variant)
                digest = self.seal(root, receipt)
                self.assertEqual(modified.verify_modified_assets(root, digest, variant=variant), root / "dist")
                wrong = "renewal44" if variant == "baseline44" else "baseline44"
                with self.assertRaises(RuntimeError):
                    modified.verify_modified_assets(root, digest, variant=wrong)

    def test_variant_and_applied_flag_cannot_be_forged_even_with_new_receipt_hash(self):
        cases = (("variant", "stock44"), ("variant", None), ("variant", "baseline44"),
                 ("proposal_applied", False), ("proposal_applied", 1),
                 ("proposal_applied", "true"), ("proposal_applied", None))
        for field, value in cases:
            with self.subTest(field=field, value=value), tempfile.TemporaryDirectory() as folder:
                root, receipt = self.make_assets(folder)
                receipt[field] = value
                digest = self.seal(root, receipt)
                with self.assertRaises(RuntimeError):
                    modified.verify_modified_assets(root, digest, variant="renewal44")
        with tempfile.TemporaryDirectory() as folder:
            root, receipt = self.make_assets(folder)
            digest = self.seal(root, receipt)
            with self.assertRaises(RuntimeError):
                modified.verify_modified_assets(root, digest, variant="stock44")

    def test_receipt_schema_is_exact_integer_and_duplicate_fields_are_rejected(self):
        for schema in (True, 1.0):
            with self.subTest(schema=schema), tempfile.TemporaryDirectory() as folder:
                root, receipt = self.make_assets(folder)
                receipt["schema"] = schema
                digest = self.seal(root, receipt)
                with self.assertRaises(RuntimeError):
                    modified.verify_modified_assets(root, digest, variant="renewal44")
        for field in ("variant", "proposal_sha256", "schema", "asset_path"):
            with self.subTest(field=field), tempfile.TemporaryDirectory() as folder:
                root, receipt = self.make_assets(folder)
                raw = json.dumps(receipt, sort_keys=True)
                if field == "asset_path":
                    needle = '"path": "index.html"'
                    replacement = '"path": "outside", "path": "index.html"'
                else:
                    needle = json.dumps(field) + ": " + json.dumps(receipt[field])
                    replacement = needle + ", " + needle
                self.assertEqual(raw.count(needle), 1)
                raw = raw.replace(needle, replacement).encode()
                (root / "build-receipt.json").write_bytes(raw)
                digest = hashlib.sha256(raw).hexdigest()
                with self.assertRaises(RuntimeError):
                    modified.verify_modified_assets(root, digest, variant="renewal44")

    def test_actual_default_stock_route_rejects_mixed_controlled_markers(self):
        acceptance = matrix.Acceptance.__new__(matrix.Acceptance)
        acceptance.suite = "element-interop"
        acceptance.modified_sender_manifest_sha256 = None
        acceptance.modified_sender_variant = None
        facts = "\n".join(f"MORPHEUS_ELEMENT_CHECK {name}=true" for name in sorted(target.CHECKS))
        def record(output):
            return acceptance.validated_record(matrix.ELEMENT_LABEL, output, test=False,
                                               ring_case=None, oauth_checks=None, element_checks=True)
        self.assertEqual(record(facts)["element_checks"], {name: True for name in target.CHECKS})
        for marker in ("MORPHEUS_MODIFIED_PEER_CHECK native_roster=true",
                       "MORPHEUS_MODIFIED_PEER_RESULT official_stock_acceptance=false beta_ready=false"):
            with self.subTest(marker=marker), self.assertRaises(RuntimeError):
                record(facts + "\n" + marker)
        self.assertEqual(record(facts)["element_checks"], {name: True for name in target.CHECKS})

    def test_actual_modified_route_requires_exact_label_result_and_all_facts_for_both_variants(self):
        result_marker = "MORPHEUS_MODIFIED_PEER_RESULT official_stock_acceptance=false beta_ready=false"
        names = sorted(target.CHECKS)
        self.assertEqual(len(names), 24)
        facts = [f"MORPHEUS_MODIFIED_PEER_CHECK {name}=true" for name in names]
        complete = "\n".join([*facts, result_marker])
        for variant in ("baseline44", "renewal44"):
            with self.subTest(variant=variant):
                acceptance = matrix.Acceptance.__new__(matrix.Acceptance)
                acceptance.suite = "element-interop"
                acceptance.modified_sender_manifest_sha256 = "f" * 64
                acceptance.modified_sender_variant = variant
                def record(output, label=matrix.MODIFIED_ELEMENT_LABEL):
                    return acceptance.validated_record(label, output, test=False, ring_case=None,
                                                       oauth_checks=None, element_checks=True)
                accepted = record(complete)
                self.assertEqual(accepted["classification"], "MODIFIED_PEER")
                self.assertEqual(accepted["modified_sender_variant"], variant)
                self.assertEqual(accepted["modified_sender_manifest_sha256"], "f" * 64)
                self.assertEqual(accepted["modified_peer_checks"], {name: True for name in names})
                self.assertIs(accepted["official_stock_acceptance"], False)
                self.assertIs(accepted["beta_ready"], False)
                self.assertNotIn("element_checks", accepted)
                rejected = (
                    ("wrong_label", complete, matrix.ELEMENT_LABEL),
                    ("mixed_stock", complete + "\nMORPHEUS_ELEMENT_CHECK native_roster=true", matrix.MODIFIED_ELEMENT_LABEL),
                    ("missing_result", "\n".join(facts), matrix.MODIFIED_ELEMENT_LABEL),
                    ("duplicate_result", complete + "\n" + result_marker, matrix.MODIFIED_ELEMENT_LABEL),
                    ("incomplete_facts", "\n".join([*facts[1:], result_marker]), matrix.MODIFIED_ELEMENT_LABEL),
                    ("false_fact", "\n".join([facts[0].removesuffix("true") + "false", *facts[1:], result_marker]), matrix.MODIFIED_ELEMENT_LABEL),
                )
                for case, output, label in rejected:
                    with self.subTest(variant=variant, case=case), self.assertRaises(RuntimeError):
                        record(output, label)
                self.assertIs(record(complete)["official_stock_acceptance"], False)
                self.assertIs(record(complete)["beta_ready"], False)

if __name__ == "__main__":
    unittest.main()
