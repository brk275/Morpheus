"""Notice host hardening against synthetic trees and mocked hosts.

Real-filesystem cases run on Linux only, against a temporary tree each test
creates and owns, with the expected owner set to the test's own uid; no system
path is ever changed. Other owners, replaced entries and mounts are simulated
by patching os.stat or os.fstat. Only HostedNoticeTreeTests reads the real
notice trees, read-only and unprivileged, and only on a fresh GitHub-hosted
Ubuntu 24.04 runner.
"""

import ast
import contextlib
import errno
import io
import itertools
import json
import os
from pathlib import Path
import shutil
import stat
import sys
import tempfile
import types
import unittest
from unittest import mock

from scripts import harden_linux_notice_host as H


REPO = Path(__file__).resolve().parents[2]
SOURCE = REPO / "scripts/harden_linux_notice_host.py"
COMMAND = ("sudo --preserve-env=GITHUB_ACTIONS,RUNNER_ENVIRONMENT,RUNNER_OS,ImageOS "
           "/usr/bin/python3 -I -B scripts/harden_linux_notice_host.py")
HOSTED_ENVIRONMENT = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted",
                      "RUNNER_OS": "Linux", "ImageOS": "ubuntu24"}
SYSTEM = types.SimpleNamespace(sysname="Linux", machine="x86_64")
RELEASE = {"ID": "ubuntu", "VERSION_ID": "24.04"}

# A synthetic host below a temporary folder. / and /usr are safe; the notice
# trees and an unrelated /usr/share directory are group- or world-writable.
DIRECTORIES = {"usr/share": 0o777, "usr/share/doc": 0o777, "usr/share/doc/libc6": 0o777,
               "usr/share/doc/nested": 0o775, "usr/share/doc/nested/deeper": 0o777,
               "usr/share/common-licenses": 0o777, "usr/share/unrelated": 0o777}
FILES = {"usr/share/doc/libc6/copyright": 0o666, "usr/share/doc/nested/deeper/README": 0o777,
         "usr/share/common-licenses/GPL-2": 0o646, "usr/share/unrelated/data": 0o666}
HARD_LINK = ("usr/share/doc/libc6/copyright", "usr/share/doc/libc6/copyright-link")
LINKS = {"usr/share/doc/libc6-alias": "libc6", "usr/share/common-licenses/GPL": "GPL-2",
         "usr/share/doc/libc6/outside-dir": "../../unrelated",
         "usr/share/doc/libc6/outside-file": "../../unrelated/data"}
# Only group and world write bits go; execute bits and everything else stay.
HARDENED = {"usr/share": 0o755, "usr/share/doc": 0o755, "usr/share/doc/libc6": 0o755,
            "usr/share/doc/nested": 0o755, "usr/share/doc/nested/deeper": 0o755,
            "usr/share/common-licenses": 0o755, "usr/share/doc/libc6/copyright": 0o644,
            "usr/share/doc/libc6/copyright-link": 0o644,
            "usr/share/doc/nested/deeper/README": 0o755, "usr/share/common-licenses/GPL-2": 0o644}
STAT_FIELDS = ("st_mode", "st_ino", "st_dev", "st_nlink", "st_uid", "st_gid", "st_size",
               "st_atime", "st_mtime", "st_ctime")


def counts(**values):
    return dict(dict.fromkeys(H.COUNTS, 0), **values)


def build_host(folder):
    root = Path(folder) / "host"
    for name in DIRECTORIES:
        (root / name).mkdir(parents=True)
    for name in FILES:
        (root / name).write_bytes(("synthetic " + name + "\n").encode())
    os.link(root / HARD_LINK[0], root / HARD_LINK[1])
    for name, target in LINKS.items():
        os.symlink(target, root / name)
    for name, mode in {**DIRECTORIES, **FILES}.items():
        os.chmod(root / name, mode)
    os.chmod(root / "usr", 0o755)
    os.chmod(root, 0o755)
    return root


def snapshot(root):
    """Type, mode and content or link target of every path, without following links."""
    state = {"": ("dir", stat.S_IMODE(os.lstat(root).st_mode))}
    for current, directories, files in os.walk(root):
        for name in directories + files:
            path = Path(current) / name
            info = os.lstat(path)
            if stat.S_ISLNK(info.st_mode):
                value = ("link", os.readlink(path))
            elif stat.S_ISDIR(info.st_mode):
                value = ("dir", stat.S_IMODE(info.st_mode))
            elif stat.S_ISREG(info.st_mode):
                value = ("file", stat.S_IMODE(info.st_mode), path.read_bytes())
            else:
                value = ("other", stat.S_IMODE(info.st_mode))
            state[path.relative_to(root).as_posix()] = value
    return state


def mode(root, name):
    return stat.S_IMODE(os.lstat(Path(root) / name).st_mode)


def descriptors():
    return sorted(os.listdir("/proc/self/fd"))


def changed(result, **fields):
    """A stat result with some of its first ten fields replaced."""
    values = list(result[:10])
    for name, value in fields.items():
        values[STAT_FIELDS.index(name)] = value
    return os.stat_result(values)


def patched_stat(replacements):
    """os.stat whose result for some descriptor-relative names is replaced."""
    real = os.stat

    def fake(path, *args, **kwargs):
        value = real(path, *args, **kwargs)
        return replacements[path](value) if isinstance(path, str) and path in replacements else value
    return mock.patch("os.stat", side_effect=fake)


def patched_fstat(inode, change):
    real = os.fstat

    def fake(handle):
        value = real(handle)
        return change(value) if value.st_ino == inode else value
    return mock.patch("os.fstat", side_effect=fake)


def other_device(value):
    return changed(value, st_dev=value.st_dev + 1)


@contextlib.contextmanager
def recorded_opens():
    """Each os.open name and its flags."""
    real = os.open
    opens = []

    def fake(path, flags, *args, **kwargs):
        opens.append((path, flags))
        return real(path, flags, *args, **kwargs)
    with mock.patch("os.open", side_effect=fake):
        yield opens


@unittest.skipUnless(sys.platform == "linux", "descriptor-relative walks are checked on Linux")
class SyntheticTreeTests(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.folder = folder.name
        self.root = build_host(self.folder)
        self.uid = os.getuid()

    def host(self):
        return build_host(tempfile.mkdtemp(dir=self.folder))

    def walk(self, root=None, fix=True, **options):
        options.setdefault("owner", self.uid)
        before = descriptors()
        report = H.Walk(fix, root=str(self.root if root is None else root), **options).report()
        self.assertEqual(descriptors(), before, "a descriptor stayed open")
        self.assertTrue(H.public(report))
        return report

    def assert_failed(self, report, stage, reason):
        self.assertEqual((report["status"], report["stage"], report["reason"]), ("failed", stage, reason))

    def test_hardens_only_the_notice_trees_and_keeps_every_entry(self):
        before = snapshot(self.root)
        with recorded_opens() as opens:
            report = self.walk()
        self.assertEqual(report, H.result("hardened", "complete", None, counts(
            directories=6, files=4, symlinks=4, linked_files=2,
            group_writable=8, world_writable=8, tightened=9)))
        self.assertTrue(opens)
        self.assertTrue(all(flags & os.O_NOFOLLOW for _, flags in opens))
        self.assertFalse({name for name, _ in opens} & {Path(name).name for name in LINKS})
        after = snapshot(self.root)
        self.assertEqual(set(after), set(before))
        for name, value in before.items():
            expected = value if name not in HARDENED else (value[0], HARDENED[name], *value[2:])
            self.assertEqual(after[name], expected, name)
        # Symlinks in the trees, and the unrelated directory and file they
        # point to, are unchanged.
        self.assertEqual(mode(self.root, "usr/share/unrelated"), 0o777)
        self.assertEqual(mode(self.root, "usr/share/unrelated/data"), 0o666)
        text = json.dumps(report)
        for private in ("libc6", "copyright", "GPL", "unrelated", self.folder):
            self.assertNotIn(private, text)

    def test_second_pass_changes_nothing_and_a_survey_confirms_it(self):
        self.walk()
        hardened = snapshot(self.root)
        self.assertEqual(self.walk()["tightened"], 0)
        self.assertEqual(snapshot(self.root), hardened)
        self.assertEqual(self.walk(fix=False), H.result("surveyed", "complete", None, counts(
            directories=6, files=4, symlinks=4, linked_files=2)))

    def test_survey_is_read_only_and_opens_only_directories(self):
        before = snapshot(self.root)
        with recorded_opens() as opens:
            report = self.walk(fix=False)
        self.assertEqual(report, H.result("surveyed", "complete", None, counts(
            directories=6, files=4, symlinks=4, linked_files=2, group_writable=9, world_writable=9)))
        self.assertEqual(snapshot(self.root), before)
        self.assertTrue(opens)
        self.assertTrue(all(flags & os.O_DIRECTORY and flags & os.O_NOFOLLOW for _, flags in opens))

    def test_symlink_swapped_in_after_its_check_is_never_followed(self):
        # The check sees exactly the outside target, so only O_NOFOLLOW stops
        # the open from reaching it.
        for name, target in (("outside-dir", "usr/share/unrelated"),
                             ("outside-file", "usr/share/unrelated/data")):
            with self.subTest(name=name):
                root = self.host()
                outside = os.stat(root / target)
                with patched_stat({name: lambda _, outside=outside: outside}):
                    report = self.walk(root)
                self.assert_failed(report, "doc", "replaced")
                self.assertEqual(mode(root, "usr/share/unrelated"), 0o777)
                self.assertEqual(mode(root, "usr/share/unrelated/data"), 0o666)

    def test_entry_replaced_after_its_check_is_not_changed(self):
        other = os.stat(self.root / "usr/share/common-licenses/GPL-2")
        with patched_stat({"README": lambda _: other}):
            report = self.walk()
        self.assert_failed(report, "doc", "replaced")
        self.assertEqual(mode(self.root, "usr/share/doc/nested/deeper/README"), 0o777)
        self.assertEqual(mode(self.root, "usr/share/common-licenses/GPL-2"), 0o646)

    def test_fixed_paths_must_not_be_symlinks(self):
        for fixed, stage in (("usr", "ancestors"), ("usr/share", "share"),
                             ("usr/share/doc", "doc"), ("usr/share/common-licenses", "common_licenses")):
            with self.subTest(fixed=fixed):
                root = self.host()
                os.rename(root / fixed, root / (fixed + "-real"))
                os.symlink(Path(fixed).name + "-real", root / fixed)
                before = snapshot(root / (fixed + "-real"))
                report = self.walk(root)
                self.assert_failed(report, stage, "symlink")
                self.assertEqual(snapshot(root / (fixed + "-real")), before)

    def test_unsafe_ancestors_refuse_before_any_change(self):
        for name, value in (("", 0o775), ("", 0o757), ("usr", 0o775), ("usr", 0o757)):
            with self.subTest(name=name, mode=value):
                root = self.host()
                os.chmod(root / name, value)
                before = snapshot(root)
                self.assert_failed(self.walk(root), "ancestors", "mode")
                self.assertEqual(snapshot(root), before)
        before = snapshot(self.root)
        self.assert_failed(self.walk(owner=self.uid + 1), "ancestors", "owner")
        self.assertEqual(snapshot(self.root), before)

    def test_missing_or_mistyped_tree_fails_at_its_stage(self):
        shutil.rmtree(self.root / "usr/share/common-licenses")
        report = self.walk()
        self.assert_failed(report, "common_licenses", "missing")
        self.assertEqual(mode(self.root, "usr/share/doc/libc6/copyright"), 0o644)
        root = self.host()
        shutil.rmtree(root / "usr/share/doc")
        (root / "usr/share/doc").write_text("not a directory\n")
        self.assert_failed(self.walk(root), "doc", "type")

    def test_entry_owned_by_another_user_is_refused_unchanged(self):
        foreign = {"st_uid": self.uid + 1}
        for name, stage, kept in (("README", "doc", ("usr/share/doc/nested/deeper/README", 0o777)),
                                  ("nested", "doc", ("usr/share/doc/nested", 0o775)),
                                  ("GPL", "common_licenses", None)):
            with self.subTest(name=name):
                root = self.host()
                with patched_stat({name: lambda value: changed(value, **foreign)}):
                    report = self.walk(root)
                self.assert_failed(report, stage, "owner")
                if kept:
                    self.assertEqual(mode(root, kept[0]), kept[1])
        # Ownership reported by the opened descriptor is checked before fchmod.
        readme = os.stat(self.root / "usr/share/doc/nested/deeper/README").st_ino
        with patched_fstat(readme, lambda value: changed(value, **foreign)):
            report = self.walk()
        self.assert_failed(report, "doc", "owner")
        self.assertEqual(mode(self.root, "usr/share/doc/nested/deeper/README"), 0o777)

    def test_mount_crossing_is_refused(self):
        with patched_stat({"nested": other_device}):
            report = self.walk()
        self.assert_failed(report, "doc", "mount")
        self.assertEqual(mode(self.root, "usr/share/doc/nested"), 0o775)
        root = self.host()
        tree = os.stat(root / "usr/share/common-licenses").st_ino
        with patched_stat({"common-licenses": other_device}), patched_fstat(tree, other_device):
            report = self.walk(root)
        self.assert_failed(report, "common_licenses", "mount")
        self.assertEqual(mode(root, "usr/share/common-licenses"), 0o777)

    def test_other_file_types_are_refused_without_being_opened(self):
        os.mkfifo(self.root / "usr/share/doc/pipe")
        with recorded_opens() as opens:
            report = self.walk()
        self.assert_failed(report, "doc", "type")
        self.assertNotIn("pipe", {name for name, _ in opens})

    def test_depth_entry_and_time_bounds(self):
        self.assert_failed(self.walk(self.host(), depth=1), "doc", "depth")
        root = self.host()
        # The bound applies while listing, before any child changes.
        self.assert_failed(self.walk(root, entries=2), "doc", "count")
        self.assertEqual(mode(root, "usr/share/doc/libc6"), 0o777)
        report = self.walk(self.host(), seconds=5, clock=itertools.count().__next__)
        self.assertEqual((report["status"], report["reason"]), ("failed", "time"))

    def test_mode_change_is_confirmed(self):
        with mock.patch("os.fchmod"):
            report = self.walk()
        self.assert_failed(report, "share", "verify")
        self.assertEqual(mode(self.root, "usr/share"), 0o777)

    def test_errors_close_every_descriptor_and_keep_only_fixed_reasons(self):
        real_open = os.open

        def denied(path, *args, **kwargs):
            if path == "README":
                raise PermissionError(errno.EACCES, "private detail", "README")
            return real_open(path, *args, **kwargs)
        with mock.patch("os.open", side_effect=denied):
            report = self.walk()
        self.assert_failed(report, "doc", "access")
        root = self.host()
        readme = os.stat(root / "usr/share/doc/nested/deeper/README").st_ino
        real_fchmod = os.fchmod

        def broken(handle, value):
            if os.fstat(handle).st_ino == readme:
                raise RuntimeError("private detail")
            return real_fchmod(handle, value)
        with mock.patch("os.fchmod", side_effect=broken):
            report = self.walk(root)
        self.assert_failed(report, "doc", "internal")
        self.assertNotIn("private", json.dumps(report))


class GuardTests(unittest.TestCase):
    def reason(self, environment=None, system=SYSTEM, release=None, uids=(0, 0), platform="linux"):
        environment = HOSTED_ENVIRONMENT if environment is None else environment
        with mock.patch.object(H.sys, "platform", platform):
            return H.guard_reason(dict(environment), system, dict(RELEASE if release is None else release), uids)

    def test_only_the_exact_hosted_image_with_real_and_effective_root_passes(self):
        self.assertIsNone(self.reason())

    def test_local_other_hosts_images_and_users_refuse(self):
        cases = [(dict(platform="darwin"), "platform"),
                 (dict(system=types.SimpleNamespace(sysname="Darwin", machine="x86_64")), "platform"),
                 (dict(environment={}), "hosted"),
                 (dict(system=types.SimpleNamespace(sysname="Linux", machine="aarch64")), "arch"),
                 (dict(release={}), "release"),
                 (dict(release={"ID": "debian", "VERSION_ID": "24.04"}), "release"),
                 (dict(release={"ID": "ubuntu", "VERSION_ID": "22.04"}), "release"),
                 (dict(uids=(1001, 0)), "root"), (dict(uids=(0, 1001)), "root"),
                 (dict(uids=(1001, 1001)), "root")]
        for name in ("GITHUB_ACTIONS", "RUNNER_ENVIRONMENT", "RUNNER_OS"):
            cases.append((dict(environment={**HOSTED_ENVIRONMENT, name: "self-hosted"}), "hosted"))
            cases.append((dict(environment={key: value for key, value in HOSTED_ENVIRONMENT.items()
                                            if key != name}), "hosted"))
        for image in ("ubuntu22", "ubuntu24-arm", "win25", ""):
            cases.append((dict(environment={**HOSTED_ENVIRONMENT, "ImageOS": image}), "image"))
        for options, expected in cases:
            with self.subTest(options=options):
                self.assertEqual(self.reason(**options), expected)

    def test_release_fields_are_read_within_a_bound(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "os-release"
            path.write_text('NAME="Ubuntu"\nVERSION_ID="24.04"\nID=ubuntu\n')
            self.assertEqual(H.os_release(path), {"NAME": "Ubuntu", "VERSION_ID": "24.04", "ID": "ubuntu"})
            path.write_bytes(b"ID=ubuntu\n" + b"#" * H.OS_RELEASE_LIMIT)
            self.assertEqual(H.os_release(path), {})
            path.write_bytes(b"ID=\xff\n")
            self.assertEqual(H.os_release(path), {})
            self.assertEqual(H.os_release(Path(folder) / "missing"), {})


class MainTests(unittest.TestCase):
    def run_main(self, argv=()):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = H.main(list(argv))
        return code, json.loads(output.getvalue())

    def test_refusal_happens_before_any_walk(self):
        for reason in H.GUARD_REASONS[1:]:
            with self.subTest(reason=reason), \
                    mock.patch.object(H, "guard_reason", return_value=reason), \
                    mock.patch.object(H, "Walk") as walk:
                self.assertEqual(self.run_main(), (1, H.result("refused", "guard", reason)))
                walk.assert_not_called()

    def test_any_argument_refuses_before_the_guard(self):
        with mock.patch.object(H, "guard_reason") as guard, mock.patch.object(H, "Walk") as walk:
            self.assertEqual(self.run_main(["--apply"]), (1, H.result("refused", "guard", "arguments")))
        guard.assert_not_called()
        walk.assert_not_called()

    def test_guarded_run_uses_only_the_fixed_production_paths(self):
        hardened = H.result("hardened", "complete", None, counts(directories=3, tightened=3))
        with mock.patch.object(H, "guard_reason", return_value=None), \
                mock.patch.object(H, "Walk") as walk:
            walk.return_value.report.return_value = hardened
            self.assertEqual(self.run_main(), (0, hardened))
        walk.assert_called_once_with(True)

    def test_failed_or_malformed_reports_exit_nonzero_with_fixed_output(self):
        failed = H.result("failed", "doc", "owner")
        malformed = dict(H.result("hardened", "complete", None), path="/usr/share/doc/private")
        unknown = H.result("failed", "doc", "Permission denied: /usr/share/doc/private")
        negative = H.result("hardened", "complete", None, counts(files=-1))
        for walked, printed in ((failed, failed), (malformed, H.FALLBACK),
                                (unknown, H.FALLBACK), (negative, H.FALLBACK)):
            with self.subTest(walked=walked), \
                    mock.patch.object(H, "guard_reason", return_value=None), \
                    mock.patch.object(H, "Walk") as walk:
                walk.return_value.report.return_value = walked
                self.assertEqual(self.run_main(), (1, printed))

    def test_guard_exception_is_a_fixed_failure(self):
        with mock.patch.object(H, "guard_reason", side_effect=RuntimeError("private detail")), \
                mock.patch.object(H, "Walk") as walk:
            self.assertEqual(self.run_main(), (1, H.result("failed", "guard", "internal")))
        walk.assert_not_called()


class SourceContractTests(unittest.TestCase):
    tree = ast.parse(SOURCE.read_text(encoding="utf-8"))

    def calls(self, owner, name):
        return [node for node in ast.walk(self.tree) if isinstance(node, ast.Call)
                and isinstance(node.func, ast.Attribute) and node.func.attr == name
                and isinstance(node.func.value, ast.Name) and node.func.value.id == owner]

    def test_import_only_defines_and_uses_the_standard_library(self):
        imported = set()
        for node in self.tree.body:
            if isinstance(node, ast.Import):
                imported.update(alias.name for alias in node.names)
            elif isinstance(node, ast.ImportFrom):
                self.fail("unexpected from-import")
            elif isinstance(node, ast.Expr):
                self.assertIsInstance(node.value, ast.Constant)
            elif isinstance(node, ast.Assign):
                called = {ast.unparse(call.func) for call in ast.walk(node.value) if isinstance(call, ast.Call)}
                self.assertLessEqual(called, {"frozenset"})
            elif isinstance(node, ast.If):
                self.assertEqual(ast.unparse(node.test), "__name__ == '__main__'")
            else:
                self.assertIsInstance(node, (ast.FunctionDef, ast.ClassDef))
        self.assertEqual(imported, {"contextlib", "errno", "json", "os", "re", "stat", "sys", "time"})

    def test_only_fchmod_changes_anything_and_nothing_follows_a_symlink(self):
        attributes = {node.attr for node in ast.walk(self.tree) if isinstance(node, ast.Attribute)}
        self.assertFalse(attributes & {
            "chmod", "lchmod", "chown", "fchown", "lchown", "remove", "unlink", "rmdir", "removedirs",
            "rename", "renames", "replace", "symlink", "link", "mkdir", "makedirs", "walk", "fwalk",
            "listdir", "readlink", "realpath", "truncate", "ftruncate", "utime", "setxattr",
            "chdir", "fchdir", "chroot", "system", "popen", "fork", "execv", "execve", "spawnv"})
        self.assertEqual(len(self.calls("os", "fchmod")), 1)
        stats = self.calls("os", "stat")
        self.assertGreaterEqual(len(stats), 2)
        for node in stats:
            keywords = {keyword.arg: keyword.value for keyword in node.keywords}
            self.assertIn("dir_fd", keywords)
            self.assertIs(keywords["follow_symlinks"].value, False)
        opens = self.calls("os", "open")
        self.assertGreaterEqual(len(opens), 3)
        for node in opens:
            self.assertEqual(ast.unparse(node.args[1]).split("(")[0], "_flags")
        reads = [node for node in ast.walk(self.tree) if isinstance(node, ast.Call)
                 and isinstance(node.func, ast.Name) and node.func.id == "open"]
        self.assertEqual([ast.unparse(node.args[1]) for node in reads], ["'rb'"])

    @unittest.skipUnless(hasattr(os, "O_NOFOLLOW"), "POSIX open flags")
    def test_every_open_is_read_only_refuses_symlinks_and_never_blocks(self):
        writes = os.O_WRONLY | os.O_RDWR | os.O_CREAT | os.O_TRUNC | os.O_APPEND
        for directory in (True, False):
            flags = H._flags(directory)
            self.assertTrue(flags & os.O_NOFOLLOW and flags & os.O_CLOEXEC)
            self.assertFalse(flags & writes)
        self.assertTrue(H._flags(True) & os.O_DIRECTORY)
        self.assertTrue(H._flags(False) & os.O_NONBLOCK)


class WorkflowTests(unittest.TestCase):
    def job(self, workflow, name, following=None):
        text = (REPO / ".github/workflows" / workflow).read_text(encoding="utf-8")
        body = text.split("\n  " + name + ":\n", 1)[1]
        return text, body.split("\n  " + following + ":\n", 1)[0] if following else body

    def assert_hardener_is_the_first_python(self, text, job):
        self.assertEqual(text.count(COMMAND), 1)
        start = job.index(COMMAND)
        self.assertLess(job.index("actions/checkout@"), start)
        self.assertEqual(job.index("python3"), start + COMMAND.index("python3"))
        self.assertNotIn("persist-credentials: true", text)

    def test_ci_helpers_harden_before_any_helper_import(self):
        text, helpers = self.job("ci.yml", "helpers", "config")
        self.assert_hardener_is_the_first_python(text, helpers)
        self.assertIn("runs-on: ubuntu-24.04", helpers)
        self.assertIn("persist-credentials: false", helpers)
        self.assertLess(helpers.index(COMMAND), helpers.index("python3 -m unittest discover -s scripts/tests"))

    def test_linux_package_job_hardens_before_any_checkout_code(self):
        text, package = self.job("package-qa.yml", "package")
        self.assert_hardener_is_the_first_python(text, package)
        start = package.index(COMMAND)
        step = package[package.rindex("      - name:", 0, start):start]
        self.assertIn("        if: runner.os == 'Linux'\n", step)
        self.assertLess(start, package.index("Verify exact reviewed source checkout"))


@unittest.skipUnless(H.hosted_reason() is None, "the real notice trees are surveyed only on a fresh "
                     "GitHub-hosted Ubuntu 24.04 runner")
class HostedNoticeTreeTests(unittest.TestCase):
    def test_hosted_notice_trees_were_hardened_before_helper_tests(self):
        # Read-only and unprivileged: opens directories only and prints fixed
        # counts. The workflow's root step before these tests is the writer.
        report = H.survey()
        print("LINUX_NOTICE_HOST_SURVEY " + json.dumps(report, sort_keys=True), flush=True)
        self.assertTrue(H.public(report))
        self.assertEqual((report["status"], report["stage"], report["reason"]), ("surveyed", "complete", None))
        self.assertEqual((report["group_writable"], report["world_writable"]), (0, 0))
        self.assertGreater(report["files"], 0)


if __name__ == "__main__":
    unittest.main()
