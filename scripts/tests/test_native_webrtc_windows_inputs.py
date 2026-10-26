#!/usr/bin/env python3
"""Existing unittest route: Windows producer admission DATA boundaries only."""
import hashlib
import copy
import importlib.util
import io
import json
import os
import stat
import subprocess
from pathlib import Path
import tempfile
import unittest
import zipfile
from unittest import mock

PATH = Path(__file__).resolve().parents[1] / "run_native_webrtc_windows.py"
SPEC = importlib.util.spec_from_file_location("native_windows_producer", PATH)
TARGET = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(TARGET)


class WindowsNativeInputsTests(unittest.TestCase):
    def test_windows_original_cipd_instance_path_and_both_hashes_are_required(self):
        helper_path = PATH.parent / "native_webrtc/python_stager.py"
        self.assertEqual(hashlib.sha256(helper_path.read_bytes()).hexdigest(), TARGET.HELPERS["python_stager.py"])
        spec = importlib.util.spec_from_file_location("windows_cipd_actual_transport", helper_path)
        stager = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(stager)
        profile = json.loads((PATH.parents[1] / "native/webrtc/windows/runtime-data.json").read_bytes())
        actual = [item["tool"] for item in profile["materials"] if item["tool"]["name"] in TARGET.WINDOWS_CIPD_SHA1]
        self.assertEqual(len(actual), 3)
        for tool in actual:
            self.assertEqual(TARGET.windows_cipd_instance(tool)["hashAlgo"], "SHA1")
            for changed in (dict(tool, sha256="0" * 64), dict(tool, bytes=tool["bytes"] + 1),
                            dict(tool, source=dict(tool["source"], package="foreign/wheel"))):
                with self.assertRaises(TARGET.Refused):
                    TARGET.windows_cipd_instance(changed)
        payload = b"opaque synthetic archive DATA"
        sha1 = hashlib.sha1(payload).hexdigest()
        tool = dict(actual[0], bytes=len(payload), sha256=hashlib.sha256(payload).hexdigest())
        # A controlled fixture pin exercises the actual request and hashing
        # functions. Production pin identities are checked against real profile
        # rows above; no network/archive/tool execution or fake hash is used.
        pin = {key: tool[key] for key in ("name", "format", "bytes", "sha256", "source")}
        pin["instance_sha1"] = sha1
        signed = "https://storage.googleapis.com/chrome-infra-packages/store/SHA1/" + sha1 + "?fixture=1"
        for variant in ("valid", "corrupt", "surplus", "wrong_algorithm", "foreign_host", "duplicate", "length"):
            with self.subTest(variant=variant), tempfile.TemporaryDirectory() as temporary:
                lookup_connection, archive_connection = mock.Mock(), mock.Mock()
                lookup, response = mock.Mock(), mock.Mock()
                url = signed.replace("/SHA1/", "/SHA256/") if variant == "wrong_algorithm" else signed
                if variant == "foreign_host":
                    url = url.replace("storage.googleapis.com", "foreign.invalid")
                metadata = b")]}'\n" + json.dumps({"signedUrl": url}).encode()
                if variant == "duplicate":
                    metadata = b")]}'\n{\"signedUrl\":\"" + signed.encode() + b"\",\"signedUrl\":\"" + signed.encode() + b"\"}"
                lookup.read.return_value = metadata
                content = b"x" * len(payload) if variant == "corrupt" else payload + (b"x" if variant == "surplus" else b"")
                response.read.side_effect = io.BytesIO(content).read
                response.getheader.side_effect = lambda name: (str(len(payload) + (1 if variant == "length" else 0))
                                                               if name == "Content-Length" else None)
                owner = mock.Mock()
                owner.create.side_effect = lambda destination: destination.open("xb")
                destination = Path(temporary) / "original.zip"
                with mock.patch.dict(TARGET.WINDOWS_CIPD_SHA1, {tool["name"]: pin}), \
                        mock.patch.object(stager, "https", side_effect=[(lookup_connection, lookup),
                                                                     (archive_connection, response)]) as transport:
                    if variant == "valid":
                        TARGET.windows_cipd_download(stager, owner, tool, destination)
                        self.assertEqual(destination.read_bytes(), payload)
                    else:
                        with self.assertRaises((TARGET.Refused, stager.Refusal)):
                            TARGET.windows_cipd_download(stager, owner, tool, destination)
                first = transport.call_args_list[0]
                self.assertEqual(first.args[0], "https://chrome-infra-packages.appspot.com/prpc/cipd.Repository/GetInstanceURL")
                self.assertEqual(json.loads(first.kwargs["body"]), {"package": tool["source"]["package"],
                    "instance": {"hashAlgo": "SHA1", "hexDigest": sha1}})
                lookup_connection.close.assert_called_once_with()
                if variant in ("wrong_algorithm", "foreign_host", "duplicate"):
                    self.assertEqual(transport.call_count, 1)
                    self.assertFalse(destination.exists())
                else:
                    self.assertEqual(transport.call_count, 2)
                    self.assertEqual(transport.call_args_list[1].args[0], signed)
                    archive_connection.close.assert_called_once_with()

    def test_complete_bound_windows_runtime_profile_reaches_actual_35_material_consumer(self):
        plan_path = PATH.parents[1] / "native/webrtc/windows/pipeline-plan.json"
        plan = json.loads(plan_path.read_bytes())
        record = plan["runtime_profile"]
        raw = (PATH.parents[1] / record["path"]).read_bytes()
        self.assertEqual({"bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()},
                         {key: record[key] for key in ("bytes", "sha256")})
        profile = json.loads(raw)
        self.assertEqual(len(profile["materials"]), 35)
        self.assertIsNone(plan["source_profile"])
        self.assertIsNone(plan["build_inputs"])
        self.assertIs(plan["cache_handoff_authorized"], False)
        helper_path = PATH.parent / "native_webrtc/python_stager.py"
        self.assertEqual(hashlib.sha256(helper_path.read_bytes()).hexdigest(), TARGET.HELPERS["python_stager.py"])
        spec = importlib.util.spec_from_file_location("windows_35_actual_stager", helper_path)
        stager = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(stager)
        control = mock.Mock(root=Path("synthetic-windows-runtime"))
        # Run original admission, table and source checks; no network, archive
        # extraction, CIPD/vpython environment, executable tool or build runs.
        with mock.patch.object(TARGET, "bound_asset", return_value=(None, profile)), \
                mock.patch.object(stager, "download") as downloads, \
                mock.patch.object(TARGET, "windows_cipd_download") as original_instances, \
                mock.patch.object(stager, "extract") as ordinary, \
                mock.patch.object(TARGET, "windows_python_zip_extract") as python_zip, \
                mock.patch.object(TARGET, "windows_rust_extract") as rust, \
                mock.patch.object(TARGET, "write_json") as receipt:
            TARGET.runtime_stage(stager, Path("synthetic-workspace"), plan, control)
        self.assertEqual(downloads.call_count, 32)
        self.assertEqual(original_instances.call_count, 3)
        paths = {call.args[2] for call in downloads.call_args_list}
        paths.update(call.args[3] for call in original_instances.call_args_list)
        self.assertEqual(len(paths), 35)
        for call in original_instances.call_args_list:
            instance = TARGET.windows_cipd_instance(call.args[2])
            self.assertEqual(instance["hashAlgo"], "SHA1")
            self.assertRegex(instance["hexDigest"], r"^[0-9a-f]{40}$")
        self.assertEqual(ordinary.call_count, 32)
        self.assertEqual(python_zip.call_count, 2)
        self.assertEqual(rust.call_count, 1)
        self.assertEqual({call.args[2]["name"] for call in python_zip.call_args_list}, {"cpython", "vpython"})
        self.assertIs(receipt.call_args.args[2]["build_ready"], False)
        self.assertIs(receipt.call_args.args[2]["genuine_vpython_realized"], False)
        self.assertEqual(receipt.call_args.args[2]["tools_executed"], [])
        for item in profile["materials"]:
            if item["role"] not in TARGET.WINDOWS_PYTHON_ZIPS:
                continue
            tool = item["tool"]
            table = TARGET.windows_python_zip_members(stager, tool)
            self.assertEqual(len(table), len(tool["members"]))
            with self.assertRaises(stager.Refusal):
                stager.check_members(tool)
            for changed in (dict(tool, sha256="0" * 64),
                            dict(tool, source=dict(tool["source"], package="foreign/python"))):
                with self.assertRaises(TARGET.Refused):
                    TARGET.windows_python_zip_members(stager, changed)
            for name in ("CON .txt", "../escape name", " parent/file", "parent/file ", "bad\tname"):
                changed = copy.deepcopy(tool)
                changed["members"][0]["path"] = name
                with self.subTest(name=name), self.assertRaises((TARGET.Refused, stager.Refusal)):
                    TARGET.windows_python_zip_members(stager, changed)
            changed = copy.deepcopy(tool)
            changed["members"][0]["sha256"] = "0" * 64
            with self.assertRaises(TARGET.Refused):
                TARGET.windows_python_zip_members(stager, changed)

    def test_windows_python_zip_preserves_all_seven_original_spaced_spellings(self):
        plan = json.loads((PATH.parents[1] / "native/webrtc/windows/pipeline-plan.json").read_bytes())
        profile = json.loads((PATH.parents[1] / plan["runtime_profile"]["path"]).read_bytes())
        names = [row["path"] for item in profile["materials"]
                 if item["role"] in TARGET.WINDOWS_PYTHON_ZIPS
                 for row in item["tool"]["members"] if " " in row["path"]]
        self.assertEqual(len(names), 7)
        helper_path = PATH.parent / "native_webrtc/python_stager.py"
        self.assertEqual(hashlib.sha256(helper_path.read_bytes()).hexdigest(), TARGET.HELPERS["python_stager.py"])
        spec = importlib.util.spec_from_file_location("windows_python_zip_actual_stager", helper_path)
        stager = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(stager)
        for bad_mode in (False, True):
            with self.subTest(bad_mode=bad_mode), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                root.chmod(0o700)
                archive, output = root / "synthetic.zip", root / "output"
                table, payloads = {}, {}
                with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_STORED) as target:
                    for index, name in enumerate(names):
                        data = ("synthetic original path DATA " + str(index)).encode("ascii")
                        entry = zipfile.ZipInfo(name)
                        entry.create_system = 3
                        entry.external_attr = (stat.S_IFREG | (0o600 if bad_mode else 0o400)) << 16
                        target.writestr(entry, data)
                        payloads[name] = data
                        table[name] = {"path": name, "kind": "regular", "bytes": len(data), "mode": 0o400,
                                       "output_mode": 0o600, "sha256": hashlib.sha256(data).hexdigest()}
                stage = object.__new__(stager.Stage)
                stage.root, stage.files, stage.dirs = output, [], []
                stage.windows = stager.WindowsDirectories() if os.name == "nt" else None
                stage.held, stage.identities = [], {}
                stage.mkdir(output)
                try:
                    # Full immutable archive/table admission is exercised above.
                    # This synthetic ZIP isolates the real header/write/parent
                    # boundary with original spellings and synthetic payloads.
                    with mock.patch.object(TARGET, "windows_python_zip_members", return_value=table):
                        if bad_mode:
                            with self.assertRaises(TARGET.Refused):
                                TARGET.windows_python_zip_extract(stager, stage,
                                    {"bytes": archive.stat().st_size}, archive, output)
                        else:
                            TARGET.windows_python_zip_extract(stager, stage,
                                {"bytes": archive.stat().st_size}, archive, output)
                            self.assertEqual({p.relative_to(output).as_posix() for p in output.rglob("*") if p.is_file()},
                                             set(names))
                            for name, data in payloads.items():
                                self.assertEqual(stager.snapshot(output / name, stager.MAX_FILE), data)
                finally:
                    stage.finish(False)

    def test_windows_rust_oversize_exception_is_exact_and_download_bucket_is_fixed(self):
        helper_path = PATH.parent / "native_webrtc/python_stager.py"
        self.assertEqual(hashlib.sha256(helper_path.read_bytes()).hexdigest(),
                         TARGET.HELPERS["python_stager.py"])
        spec = importlib.util.spec_from_file_location("windows_rust_actual_stager", helper_path)
        stager = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(stager)
        tool = copy.deepcopy(TARGET.WINDOWS_RUST) | {
            "alias_copies": [], "omitted_aliases": [], "omission_evidence": None,
            "members": [copy.deepcopy(TARGET.WINDOWS_RUST_PDB)]}
        pdb = copy.deepcopy(TARGET.WINDOWS_RUST_PDB)
        self.assertEqual(TARGET.windows_rust_member_limit(stager, tool, pdb), 556744704)
        ordinary = dict(pdb, path="bin/ordinary.pdb", raw_path="bin/ordinary.pdb", bytes=512 * 1024 ** 2)
        self.assertEqual(TARGET.windows_rust_member_limit(stager, tool, ordinary), stager.MAX_FILE)
        for field, value in (("path", "bin/other.pdb"), ("raw_path", "./" + pdb["path"]),
                             ("bytes", pdb["bytes"] + 1), ("mode", 0o777),
                             ("output_mode", 0o700), ("sha256", "0" * 64), ("kind", "symlink")):
            with self.subTest(member_field=field), self.assertRaises(TARGET.Refused):
                TARGET.windows_rust_member_limit(stager, tool, dict(pdb, **{field: value}))
        for field, value in (("name", "clang"), ("format", "zip"), ("bytes", tool["bytes"] - 1),
                             ("sha256", "0" * 64), ("source", dict(tool["source"], generation=1)),
                             ("source", dict(tool["source"], object="Linux_x64/other.tar.xz")),
                             ("source", dict(tool["source"], bucket="foreign"))):
            with self.subTest(archive_field=field), self.assertRaises(TARGET.Refused):
                TARGET.windows_rust_member_limit(stager, dict(tool, **{field: value}), pdb)
        # The ordinary stager remains capped and a partial/altered inventory
        # cannot gain the special extraction path even with the original PDB.
        with self.assertRaises(stager.Refusal):
            stager.check_members(tool)
        with self.assertRaises(TARGET.Refused):
            TARGET.windows_rust_members(stager, tool)
        original_source = dict(tool["source"])
        with mock.patch.object(stager, "download") as download:
            TARGET.windows_rust_download(stager, "owner", tool, Path("owned-rust.tar.xz"))
        submitted = download.call_args.args[1]
        self.assertEqual(submitted["source"], original_source | {"bucket": "chromium-browser-clang"})
        self.assertEqual(tool["source"], original_source)
        self.assertEqual({key: value for key, value in submitted.items() if key != "source"},
                         {key: value for key, value in tool.items() if key != "source"})

    def test_windows_rust_member_uses_bounded_chunks_and_streamed_stable_rehash(self):
        helper_path = PATH.parent / "native_webrtc/python_stager.py"
        self.assertEqual(hashlib.sha256(helper_path.read_bytes()).hexdigest(),
                         TARGET.HELPERS["python_stager.py"])
        spec = importlib.util.spec_from_file_location("windows_rust_stream_actual_stager", helper_path)
        stager = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(stager)
        tool = copy.deepcopy(TARGET.WINDOWS_RUST) | {
            "alias_copies": [], "omitted_aliases": [], "omission_evidence": None}
        payload = b"synthetic Rust DATA\0" * 65537
        row = {"path": "bin/synthetic.pdb", "raw_path": "bin/synthetic.pdb", "kind": "regular",
               "bytes": len(payload), "mode": 0o666, "output_mode": 0o600,
               "sha256": hashlib.sha256(payload).hexdigest()}
        class Fragmented(io.BytesIO):
            def __init__(self, data):
                super().__init__(data)
                self.requests = []
            def read(self, size=-1):
                self.requests.append(size)
                self.assert_bounded(size)
                return super().read(min(size, 65536))
            @staticmethod
            def assert_bounded(size):
                if not 0 < size <= 1024 * 1024:
                    raise AssertionError("unbounded member read")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            owner = mock.Mock()
            owner.create.side_effect = lambda destination: destination.open("xb")
            # Test the real stable streaming fact() on POSIX too. Only its
            # independent Windows drive-path gate is replaced for this fixture.
            with mock.patch.object(TARGET, "path", side_effect=lambda value: Path(value)), \
                    mock.patch.object(stager, "snapshot", side_effect=AssertionError("full-member allocation")):
                stream = Fragmented(payload)
                destination = root / "original.pdb"
                TARGET.windows_rust_write_member(stager, owner, destination, stream, tool, row)
                self.assertEqual(TARGET.fact(destination), {key: row[key] for key in ("bytes", "sha256")})
                self.assertGreater(len(stream.requests), 2)
                for label, data, selected in (("truncated", payload[:-1], row),
                                               ("surplus", payload + b"x", row),
                                               ("hash", payload, dict(row, sha256="0" * 64))):
                    with self.subTest(label=label), self.assertRaises(TARGET.Refused):
                        TARGET.windows_rust_write_member(stager, owner, root / (label + ".pdb"),
                                                         Fragmented(data), tool, selected)
                oversized = dict(row, bytes=stager.MAX_FILE + 1)
                with self.assertRaises(TARGET.Refused):
                    TARGET.windows_rust_write_member(stager, owner, root / "foreign.pdb",
                                                     Fragmented(b""), tool, oversized)
                self.assertFalse((root / "foreign.pdb").exists())

    def test_actual_two_version_wheels_and_virtualenv_keep_distinct_runtime_consumer_roots(self):
        # Exact original universal six versions and required virtualenv CIPD ZIP inventories.
        # Only the production DATA consumer/check_members run; no download, extraction or tool execution.
        materials = [{'role': 'wheels',
          'source_mount': None,
          'tool': {'name': 'wheels-321750c49ce19b7c',
                   'format': 'zip',
                   'bytes': 10762,
                   'sha256': '4a23290e689d8df1695cc54758e9c3c7e969a820c25396043ce6b893816ebb33',
                   'source': {'kind': 'cipd',
                              'package': 'infra/python/wheels/six-py2_py3',
                              'version': 'version:1.10.0'},
                   'members': [{'path': 'six-1.10.0-py2.py3-none-any.whl',
                                'kind': 'regular',
                                'bytes': 10341,
                                'mode': 384,
                                'output_mode': 384,
                                'sha256': '0ff78c403d9bccf5a425a6d31a12aa6b47f1c21ca4dc2573a7e2f32a97335eb1'},
                               {'path': '.cipdpkg/manifest.json',
                                'kind': 'regular',
                                'bytes': 109,
                                'mode': 384,
                                'output_mode': 384,
                                'sha256': '7f552b6f0052d1bcea939509cdcc29cea173dad509996611be4d3b84d9e21f5d'}],
                   'alias_copies': [],
                   'omitted_aliases': [],
                   'omission_evidence': None}},
         {'role': 'wheels',
          'source_mount': None,
          'tool': {'name': 'wheels-0f0494d6bafc1c0d',
                   'format': 'zip',
                   'bytes': 11476,
                   'sha256': 'c664d8cde41f44c5a24d8eaa8e1fc1416068cdf8e59ac9a89bd59d0958bad7e5',
                   'source': {'kind': 'cipd',
                              'package': 'infra/python/wheels/six-py2_py3',
                              'version': 'version:1.16.0'},
                   'members': [{'path': 'six-1.16.0-py2.py3-none-any.whl',
                                'kind': 'regular',
                                'bytes': 11053,
                                'mode': 256,
                                'output_mode': 384,
                                'sha256': '8abb2f1d86890a2dfb989f9a77cfcfd3e47c2a354b01111771326f8aa26e0254'},
                               {'path': '.cipdpkg/manifest.json',
                                'kind': 'regular',
                                'bytes': 111,
                                'mode': 256,
                                'output_mode': 384,
                                'sha256': 'f652ac35a9dc7b73f52c8bdd3f947beb27ff79c37d6a77ed877aaf93f3f225bf'}],
                   'alias_copies': [],
                   'omitted_aliases': [],
                   'omission_evidence': None}},
         {'role': 'wheels',
          'source_mount': None,
          'tool': {'name': 'wheels-74479c1acae88219',
                   'format': 'zip',
                   'bytes': 4406690,
                   'sha256': '2529d5c7e428d2d943fe9c22447f4cf8afef8bec39e735de30970417d4e48e24',
                   'source': {'kind': 'cipd',
                              'package': 'infra/3pp/tools/virtualenv',
                              'version': 'version:2@20.25.1.chromium.8'},
                   'members': [{'path': 'virtualenv-20.25.1/virtualenv.pyz',
                                'kind': 'regular',
                                'bytes': 5705128,
                                'mode': 256,
                                'sha256': '03eee6023553aa233b93990704694588de21631a9604ffa1eb8c19f879352f9b',
                                'output_mode': 384},
                               {'path': '.cipdpkg/manifest.json',
                                'kind': 'regular',
                                'bytes': 103,
                                'mode': 256,
                                'sha256': '863ee7fdc5ca628bb75cd0a15b746f05c5b6d677b42642c76d2e88ba65d6163b',
                                'output_mode': 384}],
                   'alias_copies': [],
                   'omitted_aliases': [],
                   'omission_evidence': None}}]
        profile = {"schema": "morpheus-windows-runtime-data-v1", "target": "win-x64", "materials": materials}
        helper_path = PATH.parent / "native_webrtc/python_stager.py"
        self.assertEqual(hashlib.sha256(helper_path.read_bytes()).hexdigest(),
                         TARGET.HELPERS["python_stager.py"])
        spec = importlib.util.spec_from_file_location("windows_runtime_actual_data_stager", helper_path)
        stager = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(stager)
        control = mock.Mock(root=Path("synthetic-runtime-data"))
        plan = {"runtime_profile": {"sha256": "a" * 64}}
        def consume(selected):
            with mock.patch.object(TARGET, "bound_asset", return_value=(None, selected)), \
                    mock.patch.object(stager, "download") as download, \
                    mock.patch.object(TARGET, "windows_cipd_download") as original_instances, \
                    mock.patch.object(stager, "extract") as extract, \
                    mock.patch.object(TARGET, "write_json"):
                TARGET.runtime_stage(stager, Path("synthetic-workspace"), plan, control)
                return download.call_args_list, original_instances.call_args_list, extract.call_args_list
        downloads, original_instances, extractions = consume(profile)
        self.assertEqual(len(downloads), 2)
        self.assertEqual(len(original_instances), 1)
        paths = {call.args[2] for call in downloads}
        paths.update(call.args[3] for call in original_instances)
        self.assertEqual(len(paths), 3)
        self.assertEqual(original_instances[0].args[2], materials[0]["tool"])
        self.assertEqual(TARGET.windows_cipd_instance(original_instances[0].args[2]),
                         {"hashAlgo": "SHA1", "hexDigest": "e00dc04ad5029579cffc146324c365377f8bcd1d"})
        self.assertEqual({call.args[3].name for call in extractions},
                         {item["tool"]["name"] for item in materials})
        for item, call in zip(materials, extractions):
            self.assertEqual(call.args[1], item["tool"])
            self.assertEqual(call.args[3], control.root / "runtime" / item["tool"]["name"])
        # Duplicate identities, forged derived keys and foreign-platform origins cannot become new cohorts.
        for change in ("duplicate", "key", "platform", "origin"):
            invalid = copy.deepcopy(profile)
            if change == "duplicate":
                invalid["materials"].append(copy.deepcopy(materials[0]))
            elif change == "key":
                invalid["materials"][1]["tool"]["name"] = materials[0]["tool"]["name"]
            elif change == "platform":
                invalid["materials"][1]["tool"]["source"]["package"] = "infra/python/wheels/six/linux-amd64_cp311_cp311"
            else:
                invalid["materials"][1]["tool"]["source"]["package"] = "foreign/virtualenv"
            with self.subTest(change=change), self.assertRaises(TARGET.Refused):
                consume(invalid)
        oversized = copy.deepcopy(profile)
        oversized["materials"] = [materials[0]] * 41
        with self.assertRaises(TARGET.Refused):
            consume(oversized)

    def test_original_owner_main_public_host_contract_is_required(self):
        env = dict(GITHUB_ACTIONS="true", RUNNER_ENVIRONMENT="github-hosted", RUNNER_OS="Windows",
            RUNNER_ARCH="X64", GITHUB_REPOSITORY="brk275/Morpheus", GITHUB_EVENT_NAME="workflow_dispatch",
            GITHUB_REF="refs/heads/main", GITHUB_ACTOR="brk275", GITHUB_TRIGGERING_ACTOR="brk275")
        self.assertTrue(TARGET.environment_admitted("nt", env))
        for key, value in (("GITHUB_TRIGGERING_ACTOR", "foreign"), ("GITHUB_ACTOR", "foreign"),
                           ("GITHUB_REF", "refs/pull/44/merge"), ("GITHUB_EVENT_NAME", "push"),
                           ("RUNNER_ENVIRONMENT", "self-hosted"), ("RUNNER_OS", "Linux"),
                           ("RUNNER_ARCH", "ARM64"), ("GITHUB_REPOSITORY", "foreign/Morpheus")):
            with self.subTest(key=key):
                self.assertFalse(TARGET.environment_admitted("nt", env | {key: value}))
        for key in env:
            changed = dict(env)
            changed.pop(key)
            self.assertFalse(TARGET.environment_admitted("nt", changed))
        self.assertFalse(TARGET.environment_admitted("posix", env))

    def test_duplicate_json_and_windows_path_aliases_fail_closed(self):
        with self.assertRaises(TARGET.Refused):
            TARGET.unique([("gates", "unknown"), ("gates", "approved")])
        for name in ("../secret", "a/./b", "a//b", "/absolute", "C:/drive", "a\\b", "a:stream",
                     ".git/config", "con.txt", "a/trailing.", "a/trailing ", "a/line\nfeed", "a/\x00"):
            with self.subTest(name=name), self.assertRaises(TARGET.Refused):
                TARGET.relative(name)
        self.assertEqual(TARGET.relative("third_party/rust-toolchain/bin/rustc.exe"),
                         "third_party/rust-toolchain/bin/rustc.exe")

    def test_actual_projection_rows_match_existing_builder_typed_inventory(self):
        builder_path = PATH.with_name("build_native_webrtc_windows.py")
        spec = importlib.util.spec_from_file_location("native_windows_builder_contract", builder_path)
        builder = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(builder)
        owner = mock.Mock()
        owner.root = Path("private")
        source, core, runtime = Path("private/source"), Path("core"), Path("private")
        ordinary = {"path": "gn.exe", "kind": "regular", "bytes": 4,
                    "sha256": "a" * 64, "output_mode": 0o600}
        executable = {"path": "bin/rustc.exe", "kind": "regular", "bytes": 8,
                      "sha256": "b" * 64, "output_mode": 0o700}
        core_manifest = {"targets": {"Windowsx64": [{"name": "gn", "members": [ordinary],
            "alias_copies": [], "omitted_aliases": []}]}}
        runtime_profile = {"materials": [{"role": "rust", "source_mount": "third_party/rust-toolchain",
            "tool": {"members": [executable]}}]}
        emitted = []
        normalizer = mock.Mock()
        normalizer.verify_source_aliases.return_value = []
        with mock.patch.object(TARGET, "tree_files", return_value=[]), \
                mock.patch.object(TARGET, "load_unbound", return_value=[]), \
                mock.patch.object(TARGET, "fixed_module", return_value=normalizer) as module, \
                mock.patch.object(TARGET, "register_parent"), mock.patch.object(TARGET, "copy_file") as copy, \
                mock.patch.object(TARGET, "write_json", side_effect=lambda owner, path, rows: emitted.extend(rows)):
            TARGET.project(owner, source, core, core_manifest, runtime, runtime_profile)
        module.assert_called_once_with("git_source_stager.py", TARGET.HELPERS["git_source_stager.py"])
        self.assertEqual(normalizer.verify_source_aliases.call_args_list, [mock.call(source), mock.call(source)])
        self.assertEqual(copy.call_count, 2)
        self.assertEqual([row["mode"] for row in emitted], ["100644", "100755"])
        self.assertEqual(builder.typed_files(emitted), [
            {"path": "buildtools/win/gn.exe", "bytes": 4, "sha256": "a" * 64},
            {"path": "third_party/rust-toolchain/bin/rustc.exe", "bytes": 8, "sha256": "b" * 64}])
        with self.assertRaises(builder.RecipeError):
            builder.typed_files([{k: v for k, v in emitted[0].items() if k != "mode"}])


    def test_package_headers_retain_original_git_api_not_projected_toolchains(self):
        spec = importlib.util.spec_from_file_location("native_windows_header_contract",
                                                    PATH.with_name("build_native_webrtc_windows.py"))
        builder = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(builder)
        original = {"api/native.h": {"bytes": 4, "sha256": "a" * 64},
            "sdk/wrapper.inc": {"bytes": 8, "sha256": "b" * 64},
            "third_party/abseil-cpp/absl/base/config.h": {"bytes": 12, "sha256": "c" * 64},
            "LICENSE": {"bytes": 16, "sha256": "d" * 64}}
        complete = original | {
            "third_party/llvm-build/Release+Asserts/lib/clang/include/stddef.h":
                {"bytes": 20, "sha256": "e" * 64},
            "third_party/rust-toolchain/lib/rustlib/include/rust.h":
                {"bytes": 24, "sha256": "f" * 64}}
        saved = dict(complete)
        self.assertEqual(builder.package_source_headers(original, complete),
            ["api/native.h", "sdk/wrapper.inc", "third_party/abseil-cpp/absl/base/config.h"])
        self.assertEqual(complete, saved)
        with self.assertRaises(builder.RecipeError):
            builder.package_source_headers(original, complete | {
                "api/native.h": {"bytes": 99, "sha256": "a" * 64}})

    def test_source_stage_copies_exact_consumer_inputs_inside_owned_temp(self):
        # Actual source_stage + bound_asset/copy_file/write_json file IO. Only
        # Windows host path admission and the final exporter call are replaced;
        # this test neither admits a host nor executes Git/source provision.
        class Control:
            def __init__(self, root):
                self.root = root
            def mkdir(self, path):
                path.mkdir()
            def create(self, path):
                return path.open("xb")
            def bytes(self, path, raw):
                with self.create(path) as stream:
                    stream.write(raw)
                    stream.flush()
                    os.fsync(stream.fileno())

        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            workspace, runner = root / "workspace", root / "runner"
            workspace.mkdir()
            runner.mkdir()
            control = Control(runner / "control")
            control.root.mkdir()
            def asset(name, value):
                path = workspace / "native/webrtc/windows" / name
                path.parent.mkdir(parents=True, exist_ok=True)
                raw = value if isinstance(value, bytes) else json.dumps(value).encode()
                path.write_bytes(raw)
                return {"path": path.relative_to(workspace).as_posix(), "bytes": len(raw),
                        "sha256": hashlib.sha256(raw).hexdigest()}
            security = asset("security.json", {"baseline_commit": TARGET.NATIVE,
                                               "baseline_tree": TARGET.TREE})
            patches = [asset("patches/%02d.patch" % n, ("synthetic patch %d\n" % n).encode())
                       for n in range(13)]
            manifest = asset("patch-manifest.json", {"schema": "morpheus-native-source-patches-v1",
                "patches": [{"file": row["path"], "bytes": row["bytes"], "sha256": row["sha256"]}
                            for row in patches]})
            source_plan = asset("source-plan.json", {"schema": "morpheus-native-webrtc-sources-v3",
                "target": "win-x64", "git": {"path": "GIT_UNBOUND"}})
            git = workspace / "git.exe"
            git.write_bytes(b"synthetic tool DATA only")
            git_fact = {"path": str(git), "bytes": git.stat().st_size,
                        "sha256": hashlib.sha256(git.read_bytes()).hexdigest(), "version": "synthetic"}
            profile = asset("profile.json", {"schema": "morpheus-windows-source-stage-v1",
                "target": "win-x64", "source_plan": source_plan, "patch_manifest": manifest,
                "security_plan": security, "patch_files": patches, "host_git": git_fact})
            original_files = {p: p.read_bytes() for p in workspace.rglob("*") if p.is_file()}
            observed = []
            def exporter(module, args):
                selected_file = Path(args[args.index("--plan") + 1])
                self.assertTrue(selected_file.is_relative_to(control.root))
                selected = json.loads(selected_file.read_bytes())
                self.assertEqual(hashlib.sha256(selected_file.read_bytes()).hexdigest(),
                                 args[args.index("--plan-sha256") + 1])
                output = Path(args[args.index("--output-root") + 1])
                security_file = Path(selected["security_plan"]["path"])
                self.assertTrue(security_file.is_relative_to(runner))
                self.assertFalse(security_file.is_relative_to(output))
                self.assertEqual(security_file.read_bytes(),
                                 (workspace / security["path"]).read_bytes())
                self.assertEqual(selected["security_plan"]["sha256"], security["sha256"])
                patch_file = Path(selected["patch_manifest"]["path"])
                self.assertTrue(patch_file.is_relative_to(runner))
                self.assertEqual(hashlib.sha256(patch_file.read_bytes()).hexdigest(),
                                 selected["patch_manifest"]["sha256"])
                relocated = json.loads(patch_file.read_bytes())
                for row, original in zip(relocated["patches"], patches, strict=True):
                    copied = Path(row["file"])
                    self.assertTrue(copied.is_relative_to(runner))
                    self.assertFalse(copied.is_relative_to(output))
                    self.assertNotEqual(copied, workspace / original["path"])
                    self.assertEqual(copied.read_bytes(), (workspace / original["path"]).read_bytes())
                    self.assertEqual({k: row[k] for k in ("bytes", "sha256")},
                                     {k: original[k] for k in ("bytes", "sha256")})
                observed.append(True)
            with mock.patch.object(TARGET, "path", side_effect=Path), \
                    mock.patch.object(TARGET, "SECURITY", security["sha256"]), \
                    mock.patch.object(TARGET, "fixed_module", return_value=object()), \
                    mock.patch.object(TARGET, "invoke", side_effect=exporter):
                output = TARGET.source_stage(None, runner, workspace, {"source_profile": profile}, control)
            self.assertEqual(observed, [True])
            self.assertEqual(output, control.root / "source-provision")
            self.assertEqual(original_files,
                             {p: p.read_bytes() for p in workspace.rglob("*") if p.is_file()})

    def test_workflow_supplies_fixed_absolute_plan_and_entry_keeps_path_policy(self):
        workflow = PATH.parents[1] / ".github/workflows/native-webrtc-windows-source.yml"
        source = workflow.read_text()
        self.assertIn("$plan = Join-Path -Path $env:GITHUB_WORKSPACE -ChildPath "
                      "'native/webrtc/windows/pipeline-plan.json'", source)
        self.assertIn("--plan $plan --plan-sha256", source)
        self.assertNotIn("--plan native/webrtc/windows/pipeline-plan.json", source)
        with self.assertRaises(TARGET.Refused):
            TARGET.path("native/webrtc/windows/pipeline-plan.json")

    def test_guard_failure_does_not_touch_repository_api_or_load_tools(self):
        with mock.patch.object(TARGET, "environment_admitted", return_value=False), \
                mock.patch.object(TARGET, "repository_public") as api, \
                mock.patch.object(TARGET, "fixed_module") as module, self.assertRaises(TARGET.Refused):
            TARGET.runner()
        api.assert_not_called()
        module.assert_not_called()

    if os.name == "nt":
        def test_actual_entry_wrapped_file_and_raw_directory_ancestry(self):
            # This regression requires the fresh affected Windows host. It
            # invokes only reviewed entry ancestry on generated temporary data.
            self.assertEqual(os.environ.get("GITHUB_ACTIONS"), "true")
            self.assertEqual(os.environ.get("RUNNER_ENVIRONMENT"), "github-hosted")
            workflow = (PATH.parents[1] / ".github/workflows/native-webrtc-windows-source.yml").read_text()
            start = workflow.index("            while ($null -ne $parent) {")
            end = workflow.index("            $entryFailure = 'python_identity'", start)
            walk = workflow[start:end]
            self.assertEqual(walk.count("while ($null -ne $parent) {"), 1)
            walk = walk.replace("while ($null -ne $parent) {",
                                "while ($null -ne $parent) {\n              $visited.Add($parent.FullName)", 1)
            script = r"""
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
try {
    $root = $env:MORPHEUS_ANCESTRY_ROOT
    $directory = Join-Path $root 'actual\nested'
    [IO.Directory]::CreateDirectory($directory) | Out-Null
    $file = Join-Path $directory 'synthetic.bin'
    [IO.File]::WriteAllBytes($file, [byte[]]@(1,2,3,4))
    $item = Get-Item -LiteralPath $file -Force
    if ($item -isnot [IO.FileInfo] -or $item.PSIsContainer) { throw 'wrapped' }
    $raw = $item.Directory
    if ($raw -isnot [IO.DirectoryInfo] -or $null -ne $raw.PSObject.Properties['PSIsContainer']) { throw 'raw' }
    $visited = [Collections.Generic.List[string]]::new()
    $parent = $item
    $walk = [ScriptBlock]::Create($env:MORPHEUS_ENTRY_WALK)
    . $walk
    if ($null -ne $parent -or $visited.Count -lt 3 -or $visited[0] -cne $item.FullName -or
        $visited[1] -cne $raw.FullName -or $visited[$visited.Count - 1] -cne $raw.Root.FullName) { throw 'ancestry' }
    # A genuine temporary junction must still trigger the unchanged explicit
    # reparse refusal, rather than an absent-property/other incidental error.
    $alias = Join-Path $root 'alias'
    New-Item -ItemType Junction -Path $alias -Target $directory | Out-Null
    $item = Get-Item -LiteralPath (Join-Path $alias 'synthetic.bin') -Force
    $parent = $item
    $visited.Clear()
    $refused = $false
    try { . $walk } catch { if ($_.Exception.Message -ceq 'python') { $refused = $true } else { throw 'unexpected' } }
    if (-not $refused) { throw 'reparse' }
    Write-Output 'MORPHEUS_WINDOWS_ANCESTRY_OK'
} catch { Write-Output 'MORPHEUS_WINDOWS_ANCESTRY_FAILED'; exit 1 }
"""
            pwsh = Path(os.environ["ProgramFiles"]) / "PowerShell/7/pwsh.exe"
            with tempfile.TemporaryDirectory() as temporary:
                env = os.environ.copy()
                env["MORPHEUS_ANCESTRY_ROOT"] = temporary
                env["MORPHEUS_ENTRY_WALK"] = walk
                result = subprocess.run([str(pwsh), "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", script],
                    text=True, encoding="utf-8", capture_output=True, timeout=45, env=env)
            self.assertEqual(result.returncode, 0, "fixed entry ancestry regression failed")
            self.assertEqual(result.stdout.strip(), "MORPHEUS_WINDOWS_ANCESTRY_OK")


if __name__ == "__main__":
    unittest.main()
