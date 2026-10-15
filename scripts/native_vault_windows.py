"""Fresh hosted Windows fixture root, with no credential-store operations.

Create only an absent nonce-named direct RUNNER_TEMP child. Its protected DACL
is present at creation, before any profile/log exists. Never repair an existing
root or widen another object's permissions. Call verify again before phases.
"""
from __future__ import annotations

import ctypes
import os
from pathlib import Path
import re
import stat
import sys

DWORD = ctypes.c_uint32
BOOL = ctypes.c_int32
PTR = ctypes.c_void_p
CODES = {"guard", "root", "token", "descriptor", "create", "verify", "internal"}
SID = re.compile(r"S-1-5-21-(?:[0-9]{1,10}-){3}[0-9]{1,10}\Z")


class NativeVaultWindowsError(RuntimeError):
    def __init__(self, code, status=None, *, created=False):
        self.code = code if code in CODES else "internal"
        self.status = status if type(status) is int and 0 <= status <= 0xFFFFFFFF else None
        # Only prepare sets this, immediately after successful CreateDirectoryW.
        # On failure the caller may rmdir that still-empty owned root only.
        self.created = created is True
        super().__init__(self.code)


def require(value, code):
    if not value:
        raise NativeVaultWindowsError(code)


def valid_sid(value):
    return (type(value) is str and SID.fullmatch(value) is not None
            and all(str(int(part)) == part and int(part) <= 0xFFFFFFFF
                    for part in value.split("-")[4:]))


def sid_from_bytes(raw):
    """Decode only a bounded copied TokenUser SID, never dereference a caller SID."""
    require(type(raw) is bytes and 8 <= len(raw) <= 68, "token")
    require(raw[0] == 1 and 1 <= raw[1] <= 15 and len(raw) == 8 + 4 * raw[1], "token")
    value = "S-1-" + str(int.from_bytes(raw[2:8], "big"))
    value += "".join("-" + str(int.from_bytes(raw[n:n + 4], "little")) for n in range(8, len(raw), 4))
    require(valid_sid(value), "token")
    return value


def expected_descriptor(sid):
    require(valid_sid(sid), "token")
    return f"O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)"


def descriptor_is_private(value, sid):
    """Closed SDDL subset: owner plus two explicit inheritable full-access ACEs."""
    if not valid_sid(sid) or type(value) is not str or len(value) > 4096:
        return False
    prefix = f"O:{sid}D:"
    if not value.startswith(prefix):
        return False
    remainder = value[len(prefix):]
    index = remainder.find("(")
    if index < 0 or remainder[:index] not in {"P", "PAI", "AIP"}:
        return False
    aces = re.findall(r"\(A;(?:OICI|CIOI);(?:FA|0x1[fF]01[fF][fF]);;;([^;()]+)\)", remainder[index:])
    exact = re.fullmatch(r"(?:\(A;(?:OICI|CIOI);(?:FA|0x1[fF]01[fF][fF]);;;[^;()]+\)){2}", remainder[index:])
    normalized = ["S-1-5-18" if value == "SY" else value for value in aces]
    return exact is not None and len(normalized) == 2 and set(normalized) == {sid, "S-1-5-18"}


def guard_root(root):
    require(sys.platform == "win32" and ctypes.sizeof(PTR) == 8
            and os.environ.get("GITHUB_ACTIONS") == "true"
            and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
            and os.environ.get("RUNNER_OS") == "Windows", "guard")
    nonce = os.environ.get("MORPHEUS_NATIVE_VAULT_ISOLATED", "")
    require(re.fullmatch(r"[0-9a-f]{64}", nonce) is not None, "guard")
    temporary = os.environ.get("RUNNER_TEMP", "")
    require(temporary and Path(temporary).is_absolute(), "root")
    parent = Path(temporary).resolve(strict=True)
    root = Path(root)
    require(root.is_absolute() and root.name == "morpheus-native-vault-" + nonce
            and root.parent.resolve(strict=True) == parent, "root")
    # Actual operations use the trusted canonical parent and exact nonce leaf.
    return parent / root.name


def root_metadata(root):
    value = root.lstat()
    require(stat.S_ISDIR(value.st_mode) and not stat.S_ISLNK(value.st_mode)
            and not value.st_file_attributes & 0x400, "root")
    return value


class SecurityAttributes(ctypes.Structure):
    _fields_ = [("length", DWORD), ("descriptor", PTR), ("inherit", BOOL)]


class _Api:
    def __init__(self):
        require(sys.platform == "win32" and ctypes.sizeof(SecurityAttributes) == 24, "guard")
        self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        self.advapi = ctypes.WinDLL("advapi32", use_last_error=True)
        signatures = (
            (self.kernel, "GetCurrentProcess", [], PTR),
            (self.kernel, "CloseHandle", [PTR], BOOL),
            (self.kernel, "LocalFree", [PTR], PTR),
            (self.kernel, "CreateDirectoryW", [ctypes.c_wchar_p, ctypes.POINTER(SecurityAttributes)], BOOL),
            (self.advapi, "OpenProcessToken", [PTR, DWORD, ctypes.POINTER(PTR)], BOOL),
            (self.advapi, "GetTokenInformation", [PTR, ctypes.c_int, PTR, DWORD, ctypes.POINTER(DWORD)], BOOL),
            (self.advapi, "ConvertStringSecurityDescriptorToSecurityDescriptorW", [ctypes.c_wchar_p, DWORD, ctypes.POINTER(PTR), ctypes.POINTER(DWORD)], BOOL),
            (self.advapi, "GetNamedSecurityInfoW", [ctypes.c_wchar_p, ctypes.c_int, DWORD, PTR, PTR, PTR, PTR, ctypes.POINTER(PTR)], DWORD),
            (self.advapi, "ConvertSecurityDescriptorToStringSecurityDescriptorW", [PTR, DWORD, DWORD, ctypes.POINTER(PTR), ctypes.POINTER(DWORD)], BOOL),
        )
        for library, name, arguments, result in signatures:
            function = getattr(library, name)
            function.argtypes, function.restype = arguments, result

    def user_sid(self):
        token = PTR()
        if not self.advapi.OpenProcessToken(self.kernel.GetCurrentProcess(), 0x8, ctypes.byref(token)):
            raise NativeVaultWindowsError("token", ctypes.get_last_error())
        try:
            size = DWORD()
            first = self.advapi.GetTokenInformation(token, 1, None, 0, ctypes.byref(size))
            require(not first and ctypes.get_last_error() == 122 and 16 <= size.value <= 4096, "token")
            buffer = ctypes.create_string_buffer(size.value)
            capacity = len(buffer)
            if not self.advapi.GetTokenInformation(token, 1, buffer, capacity, ctypes.byref(size)):
                raise NativeVaultWindowsError("token", ctypes.get_last_error())
            require(16 <= size.value <= capacity, "token")
            address = PTR.from_buffer(buffer).value
            start = ctypes.addressof(buffer)
            require(address is not None and start <= address <= start + size.value - 8, "token")
            header = ctypes.string_at(address, 8)
            length = 8 + 4 * header[1]
            require(header[0] == 1 and 1 <= header[1] <= 15 and address + length <= start + size.value, "token")
            return sid_from_bytes(ctypes.string_at(address, length))
        finally:
            self.kernel.CloseHandle(token)

    def create(self, root, sid):
        descriptor, size = PTR(), DWORD()
        if not self.advapi.ConvertStringSecurityDescriptorToSecurityDescriptorW(expected_descriptor(sid), 1, ctypes.byref(descriptor), ctypes.byref(size)):
            raise NativeVaultWindowsError("descriptor", ctypes.get_last_error())
        try:
            require(descriptor.value is not None and 20 <= size.value <= 4096, "descriptor")
            attributes = SecurityAttributes(ctypes.sizeof(SecurityAttributes), descriptor, 0)
            if not self.kernel.CreateDirectoryW(str(root), ctypes.byref(attributes)):
                raise NativeVaultWindowsError("create", ctypes.get_last_error())
        finally:
            self.kernel.LocalFree(descriptor)

    def descriptor(self, root):
        descriptor, output, length = PTR(), PTR(), DWORD()
        status = self.advapi.GetNamedSecurityInfoW(str(root), 1, 0x5, None, None, None, None, ctypes.byref(descriptor))
        if status != 0:
            raise NativeVaultWindowsError("verify", status)
        try:
            require(descriptor.value is not None, "verify")
            if not self.advapi.ConvertSecurityDescriptorToStringSecurityDescriptorW(descriptor, 1, 0x5, ctypes.byref(output), ctypes.byref(length)):
                raise NativeVaultWindowsError("verify", ctypes.get_last_error())
            require(output.value is not None and 1 <= length.value <= 4096, "verify")
            raw = ctypes.wstring_at(output, length.value)
            text, separator, padding = raw.partition("\0")
            require(separator and not padding.strip("\0"), "verify")
            return text
        finally:
            if output.value:
                self.kernel.LocalFree(output)
            self.kernel.LocalFree(descriptor)


def _verify(root, api, sid):
    before = root_metadata(root)
    require(descriptor_is_private(api.descriptor(root), sid), "verify")
    after = root_metadata(root)
    require((before.st_dev, before.st_ino) == (after.st_dev, after.st_ino), "root")


def prepare_private_root(root):
    """Atomic create+verify. On error, created permits empty rmdir only."""
    created = False
    try:
        root = guard_root(root)
        try:
            root.lstat()
        except FileNotFoundError:
            pass
        else:
            raise NativeVaultWindowsError("root")
        api = _Api()
        sid = api.user_sid()
        api.create(root, sid)
        created = True
        _verify(root, api, sid)
        require(not any(root.iterdir()), "root")
    except NativeVaultWindowsError as error:
        error.created = created
        raise
    except BaseException:
        raise NativeVaultWindowsError("internal", created=created) from None


def verify_private_root(root):
    try:
        root = guard_root(root)
        api = _Api()
        _verify(root, api, api.user_sid())
    except NativeVaultWindowsError:
        raise
    except BaseException:
        raise NativeVaultWindowsError("internal") from None
