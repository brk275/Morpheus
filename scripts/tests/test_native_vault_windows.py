"""Portable pure/mocked Windows root contracts; no DLL or vault calls."""
import ctypes
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

from scripts import native_vault_windows as W

USER = "S-1-5-21-11-22-33-1001"


class RootTests(unittest.TestCase):
    def test_descriptor_requires_exact_owner_and_two_protected_inheritable_aces(self):
        descriptor = W.expected_descriptor(USER)
        self.assertTrue(W.descriptor_is_private(descriptor, USER))
        reordered = f"O:{USER}D:PAI(A;CIOI;0x1f01ff;;;S-1-5-18)(A;OICI;FA;;;{USER})"
        self.assertTrue(W.descriptor_is_private(reordered, USER))
        for invalid in (descriptor.replace("D:P", "D:"), descriptor.replace("D:P", "D:AR"),
                        descriptor.replace("O:" + USER, "O:SY"),
                        descriptor.replace(";;;SY", ";;;BA"),
                        descriptor.replace(";OICI;", ";OICIID;"),
                        descriptor.replace(";OICI;", ";OICIIO;"),
                        descriptor.replace(";FA;", ";FR;"),
                        descriptor + "(A;OICI;FA;;;WD)", descriptor + "\0",
                        descriptor.replace("(A;", "(D;", 1),
                        descriptor.replace(";;;SY", ";;;" + USER),
                        descriptor.replace("D:P", "D:NO_ACCESS_CONTROL")):
            with self.subTest(invalid=invalid):
                self.assertFalse(W.descriptor_is_private(invalid, USER))

    def test_sid_decoder_requires_bounded_current_user_shape(self):
        raw = bytes([1, 5]) + (5).to_bytes(6, "big")
        raw += b"".join(value.to_bytes(4, "little") for value in (21, 11, 22, 33, 1001))
        self.assertEqual(W.sid_from_bytes(raw), USER)
        for invalid in (b"", raw[:-1], raw + b"\0", bytes([2]) + raw[1:],
                        bytes([1, 16]) + raw[2:]):
            with self.assertRaises(W.NativeVaultWindowsError):
                W.sid_from_bytes(invalid)
        for invalid in ("S-1-5-18", "S-1-5-21-11-22-33-01001", "S-1-5-21-11-22-33-4294967296",
                        USER + ")(A;;FA;;;WD)", "private"):
            self.assertFalse(W.valid_sid(invalid))

    def test_errors_never_include_foreign_error_text(self):
        error = W.NativeVaultWindowsError("private token", "private", created=1)
        self.assertEqual(str(error), "internal")
        self.assertIsNone(error.status)
        self.assertFalse(error.created)

    def test_creation_is_before_verification_and_unknown_existing_root_is_untouched(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "new"
            events = []
            api = SimpleNamespace(user_sid=lambda: USER,
                create=lambda path, sid: (events.append("create"), path.mkdir()))
            with patch.object(W, "guard_root", return_value=root), patch.object(W, "_Api", return_value=api), \
                    patch.object(W, "_verify", side_effect=lambda *args: events.append("verify")):
                W.prepare_private_root(root)
                self.assertEqual(events, ["create", "verify"])
                with self.assertRaises(W.NativeVaultWindowsError) as caught:
                    W.prepare_private_root(root)
                self.assertFalse(caught.exception.created)
                self.assertEqual(events, ["create", "verify"])

    def test_partial_creation_failure_retains_only_new_empty_root_ownership(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "new"
            api = SimpleNamespace(user_sid=lambda: USER, create=lambda path, sid: path.mkdir())
            with patch.object(W, "guard_root", return_value=root), patch.object(W, "_Api", return_value=api), \
                    patch.object(W, "_verify", side_effect=W.NativeVaultWindowsError("verify", 5)):
                with self.assertRaises(W.NativeVaultWindowsError) as caught:
                    W.prepare_private_root(root)
                self.assertTrue(caught.exception.created)
                self.assertEqual(caught.exception.code, "verify")
                self.assertEqual(list(root.iterdir()), [])

    def test_creation_error_never_grants_cleanup_ownership(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "new"
            api = SimpleNamespace(user_sid=lambda: USER, create=Mock(side_effect=W.NativeVaultWindowsError("create", 183)))
            with patch.object(W, "guard_root", return_value=root), patch.object(W, "_Api", return_value=api), \
                    patch.object(W, "_verify") as verify:
                with self.assertRaises(W.NativeVaultWindowsError) as caught:
                    W.prepare_private_root(root)
                self.assertFalse(caught.exception.created)
                verify.assert_not_called()

    def test_read_only_verify_has_no_create_or_acl_mutation(self):
        api = SimpleNamespace(user_sid=lambda: USER, create=Mock())
        root = Path("unused")
        with patch.object(W, "guard_root", return_value=root), patch.object(W, "_Api", return_value=api), \
                patch.object(W, "_verify") as verify:
            W.verify_private_root(root)
            verify.assert_called_once_with(root, api, USER)
            api.create.assert_not_called()

    def test_identity_or_acl_change_fails_verification(self):
        first = SimpleNamespace(st_dev=1, st_ino=2)
        other = SimpleNamespace(st_dev=1, st_ino=3)
        for acl, second in ((W.expected_descriptor(USER), other), ("O:SYD:P", first)):
            api = SimpleNamespace(descriptor=lambda path: acl)
            with patch.object(W, "root_metadata", side_effect=[first, second]):
                with self.assertRaises(W.NativeVaultWindowsError):
                    W._verify(Path("unused"), api, USER)

    def test_guard_checks_host_nonce_exact_child_and_parent(self):
        with tempfile.TemporaryDirectory() as directory:
            temp = Path(directory).resolve()
            nonce = "a" * 64
            env = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted",
                   "RUNNER_OS": "Windows", "RUNNER_TEMP": str(temp), "MORPHEUS_NATIVE_VAULT_ISOLATED": nonce}
            root = temp / ("morpheus-native-vault-" + nonce)
            with patch.object(W.sys, "platform", "win32"), patch.dict(W.os.environ, env, clear=True):
                self.assertEqual(W.guard_root(root), root)
                for bad in (temp / "wrong", temp.parent / root.name, Path("relative")):
                    with self.assertRaises(W.NativeVaultWindowsError):
                        W.guard_root(bad)
                for name in env:
                    with patch.dict(W.os.environ, {name: ""}):
                        with self.assertRaises(W.NativeVaultWindowsError):
                            W.guard_root(root)

    def test_security_attributes_have_explicit_noninheritable_pointer_layout(self):
        self.assertEqual(ctypes.sizeof(W.SecurityAttributes), 24)
        descriptor = W.PTR(1234)
        value = W.SecurityAttributes(24, descriptor, 0)
        self.assertEqual((value.length, value.descriptor, value.inherit), (24, 1234, 0))



class ApiTests(unittest.TestCase):
    """Execute only Python callbacks against ctypes-owned buffers, never DLLs."""
    def api(self):
        api = W._Api.__new__(W._Api)
        api.kernel = SimpleNamespace(GetCurrentProcess=lambda: -1,
                                     CloseHandle=Mock(return_value=1), LocalFree=Mock(return_value=None))
        api.advapi = SimpleNamespace()
        return api

    def test_token_sid_is_copied_from_bounded_owned_buffer_and_handle_closed(self):
        raw = bytes([1, 5]) + (5).to_bytes(6, "big")
        raw += b"".join(value.to_bytes(4, "little") for value in (21, 11, 22, 33, 1001))
        api = self.api()
        def open_token(process, access, output):
            self.assertEqual((process, access), (-1, 8))
            output._obj.value = 123
            return 1
        def token_info(token, kind, buffer, capacity, output):
            self.assertEqual((token.value, kind), (123, 1))
            output._obj.value = 16 + len(raw)
            if buffer is None:
                self.assertEqual(capacity, 0)
                return 0
            self.assertEqual(capacity, 16 + len(raw))
            W.PTR.from_buffer(buffer).value = ctypes.addressof(buffer) + 16
            ctypes.memmove(ctypes.addressof(buffer) + 16, raw, len(raw))
            return 1
        api.advapi.OpenProcessToken = open_token
        api.advapi.GetTokenInformation = token_info
        with patch.object(ctypes, "get_last_error", return_value=122, create=True):
            self.assertEqual(api.user_sid(), USER)
        self.assertEqual(api.kernel.CloseHandle.call_args.args[0].value, 123)

    def test_bad_token_size_pointer_or_second_query_closes_token_before_failure(self):
        for fault in ("huge", "pointer", "second"):
            with self.subTest(fault=fault):
                api = self.api()
                def open_token(process, access, output):
                    output._obj.value = 123
                    return 1
                def token_info(token, kind, buffer, capacity, output):
                    output._obj.value = 4097 if fault == "huge" else 64
                    if buffer is None:
                        return 0
                    if fault == "second":
                        return 0
                    W.PTR.from_buffer(buffer).value = ctypes.addressof(buffer) - 1
                    return 1
                api.advapi.OpenProcessToken = open_token
                api.advapi.GetTokenInformation = token_info
                with patch.object(ctypes, "get_last_error", return_value=122, create=True):
                    with self.assertRaises(W.NativeVaultWindowsError):
                        api.user_sid()
                self.assertEqual(api.kernel.CloseHandle.call_count, 1)

    def test_create_supplies_exact_protected_descriptor_and_always_frees_it(self):
        for success in (True, False):
            api = self.api()
            def convert(value, revision, output, length):
                self.assertEqual((value, revision), (W.expected_descriptor(USER), 1))
                output._obj.value = 567
                length._obj.value = 100
                return 1
            def create(path, attributes):
                self.assertEqual(path, "owned-root")
                self.assertEqual((attributes._obj.length, attributes._obj.descriptor, attributes._obj.inherit), (24, 567, 0))
                return success
            api.advapi.ConvertStringSecurityDescriptorToSecurityDescriptorW = convert
            api.kernel.CreateDirectoryW = create
            with patch.object(ctypes, "get_last_error", return_value=183, create=True):
                if success:
                    api.create(Path("owned-root"), USER)
                else:
                    with self.assertRaises(W.NativeVaultWindowsError) as caught:
                        api.create(Path("owned-root"), USER)
                    self.assertEqual((caught.exception.code, caught.exception.status), ("create", 183))
            self.assertEqual(api.kernel.LocalFree.call_args.args[0].value, 567)

    def test_descriptor_query_uses_only_owner_and_dacl_and_frees_both_allocations(self):
        api = self.api()
        value = W.expected_descriptor(USER)
        memory = ctypes.create_unicode_buffer(value)
        def query(path, kind, information, owner, group, dacl, sacl, output):
            self.assertEqual((kind, information, owner, group, dacl, sacl), (1, 5, None, None, None, None))
            output._obj.value = 567
            return 0
        def convert(descriptor, revision, information, output, length):
            self.assertEqual((descriptor.value, revision, information), (567, 1, 5))
            output._obj.value = ctypes.addressof(memory)
            length._obj.value = len(memory)
            return 1
        api.advapi.GetNamedSecurityInfoW = query
        api.advapi.ConvertSecurityDescriptorToStringSecurityDescriptorW = convert
        self.assertEqual(api.descriptor(Path("owned-root")), value)
        self.assertEqual([call.args[0].value for call in api.kernel.LocalFree.call_args_list], [ctypes.addressof(memory), 567])

    def test_descriptor_conversion_failure_and_oversize_do_not_read_or_leak(self):
        for fault in ("failure", "oversize"):
            api = self.api()
            def query(*args):
                args[-1]._obj.value = 567
                return 0
            def convert(descriptor, revision, information, output, length):
                if fault == "failure":
                    return 0
                output._obj.value = 789
                length._obj.value = 4097
                return 1
            api.advapi.GetNamedSecurityInfoW = query
            api.advapi.ConvertSecurityDescriptorToStringSecurityDescriptorW = convert
            with patch.object(ctypes, "get_last_error", return_value=5, create=True), \
                    patch.object(ctypes, "wstring_at", side_effect=AssertionError("invalid length must not be read")):
                with self.assertRaises(W.NativeVaultWindowsError):
                    api.descriptor(Path("owned-root"))
            self.assertEqual([call.args[0].value for call in api.kernel.LocalFree.call_args_list], [567] if fault == "failure" else [789, 567])

if __name__ == "__main__":
    unittest.main()
