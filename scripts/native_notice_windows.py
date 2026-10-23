"""Windows file/ACL primitives for the source-only notice acquisition route.

Never used on Linux. No shell, impersonation, registry or profile access. Private
roots are born with a protected inheritable current-user + SYSTEM DACL. Reads
reject remote volumes/reparse points and deny concurrent write/delete sharing.
"""
from __future__ import annotations
import ctypes
from ctypes import wintypes as W
import os
from pathlib import Path
import secrets
import stat


def require(value):
    if not value:
        raise ValueError("Native notice Windows ownership rejected")


class Security:
    def __init__(self):
        require(os.name == "nt")
        self.a = ctypes.WinDLL("advapi32", use_last_error=True)
        self.k = ctypes.WinDLL("kernel32", use_last_error=True)
        declarations = (
            (self.k, "GetCurrentProcess", [], W.HANDLE),
            (self.k, "CloseHandle", [W.HANDLE], W.BOOL),
            (self.k, "LocalFree", [W.HLOCAL], W.HLOCAL),
            (self.k, "GetDriveTypeW", [W.LPCWSTR], W.UINT),
            (self.k, "CreateDirectoryW", [W.LPCWSTR, ctypes.c_void_p], W.BOOL),
            (self.k, "GetFileInformationByHandle", [W.HANDLE, ctypes.c_void_p], W.BOOL),
            (self.k, "CreateFileW", [W.LPCWSTR, W.DWORD, W.DWORD, ctypes.c_void_p, W.DWORD, W.DWORD, W.HANDLE], W.HANDLE),
            (self.a, "OpenProcessToken", [W.HANDLE, W.DWORD, ctypes.POINTER(W.HANDLE)], W.BOOL),
            (self.a, "GetTokenInformation", [W.HANDLE, ctypes.c_int, ctypes.c_void_p, W.DWORD, ctypes.POINTER(W.DWORD)], W.BOOL),
            (self.a, "ConvertSidToStringSidW", [ctypes.c_void_p, ctypes.POINTER(W.LPWSTR)], W.BOOL),
            (self.a, "ConvertStringSecurityDescriptorToSecurityDescriptorW", [W.LPCWSTR, W.DWORD, ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(W.DWORD)], W.BOOL),
            (self.a, "GetNamedSecurityInfoW", [W.LPWSTR, ctypes.c_int, W.DWORD, ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p)], W.DWORD),
            (self.a, "GetSecurityInfo", [W.HANDLE, ctypes.c_int, W.DWORD, ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p)], W.DWORD),
            (self.a, "GetAclInformation", [ctypes.c_void_p, ctypes.c_void_p, W.DWORD, ctypes.c_int], W.BOOL),
            (self.a, "GetAce", [ctypes.c_void_p, W.DWORD, ctypes.POINTER(ctypes.c_void_p)], W.BOOL),
        )
        for library, name, args, result in declarations:
            function = getattr(library, name); function.argtypes = args; function.restype = result
        token = W.HANDLE()
        require(self.a.OpenProcessToken(self.k.GetCurrentProcess(), 0x8, ctypes.byref(token)))
        try:
            values = []
            # Copy both SIDs from the same exact current-process token. A
            # newly created child's owner need not equal TOKEN_USER; TOKEN_OWNER
            # is the OS default, not an inferred Administrators category.
            for information_class in (1, 4):  # TokenUser, TokenOwner
                size = W.DWORD()
                require(not self.a.GetTokenInformation(token, information_class, None, 0, ctypes.byref(size))
                        and ctypes.get_last_error() == 122
                        and ctypes.sizeof(ctypes.c_void_p) <= size.value <= 65536)
                capacity = size.value
                storage = ctypes.create_string_buffer(capacity)
                require(self.a.GetTokenInformation(token, information_class, storage, capacity, ctypes.byref(size))
                        and ctypes.sizeof(ctypes.c_void_p) <= size.value <= capacity)
                values.append(self.sid_string(ctypes.c_void_p.from_buffer(storage).value))
            self.sid, self.default_owner = values
        finally:
            require(self.k.CloseHandle(token))

    def sid_string(self, sid):
        value = W.LPWSTR()
        require(self.a.ConvertSidToStringSidW(sid, ctypes.byref(value)))
        try:
            text = value.value
            require(text is not None and len(text) < 256)
            return text
        finally:
            require(self.k.LocalFree(ctypes.cast(value, ctypes.c_void_p)) is None)

    def acl(self, value, *, handle=False, private=False, ancestor=False):
        owner, dacl, descriptor = ctypes.c_void_p(), ctypes.c_void_p(), ctypes.c_void_p()
        arguments = (value, 1, 0x5, ctypes.byref(owner), None, ctypes.byref(dacl), None, ctypes.byref(descriptor))
        status = self.a.GetSecurityInfo(*arguments) if handle else self.a.GetNamedSecurityInfoW(*arguments)
        require(status == 0 and descriptor.value and owner.value and dacl.value)
        try:
            allowed = {self.sid, "S-1-5-18"} | (set() if private else {"S-1-5-32-544", "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"})
            owner_allowed = allowed | ({self.default_owner} if private else set())
            require(self.sid_string(owner) in owner_allowed)
            # Private write ACEs remain user/System only. The exact TokenOwner
            # exception is ownership, never permission for a group writer.

            class Size(ctypes.Structure):
                _fields_ = [("count", W.DWORD), ("used", W.DWORD), ("free", W.DWORD)]
            size = Size()
            require(self.a.GetAclInformation(dacl, ctypes.byref(size), ctypes.sizeof(size), 2)
                    and size.count <= 256 and size.used <= 65536)
            for index in range(size.count):
                ace = ctypes.c_void_p()
                require(self.a.GetAce(dacl, index, ctypes.byref(ace)))
                header = (ctypes.c_ubyte * 4).from_address(ace.value)
                kind, flags, length = header[0], header[1], header[2] | header[3] << 8
                require(length >= 8)
                # Only conventional allowed/denied ACEs have this fixed SID
                # offset. Unknown/object/callback ACEs fail closed, not skipped.
                require(kind in (0, 1))
                if flags & 0x8:  # INHERIT_ONLY cannot authorize this object.
                    continue
                mask = W.DWORD.from_address(ace.value + 4).value
                # Ancestor-only ADD_SUBDIRECTORY cannot replace the existing
                # protected child. All actual objects retain the full write mask.
                write_mask = 0x500D0156 & (~0x4 if ancestor else 0xFFFFFFFF)
                if kind == 0 and mask & write_mask:
                    require(length >= 12 and self.sid_string(ace.value + 8) in allowed)
        finally:
            require(self.k.LocalFree(descriptor) is None)

    def path(self, path, *, directory=False, private=False):
        path = Path(path)
        require(path.is_absolute() and not str(path).startswith(("\\\\", "//"))
                and path.resolve(strict=True) == path
                and self.k.GetDriveTypeW(path.anchor) == 3)  # DRIVE_FIXED only
        for item in (path, *path.parents):
            info = item.lstat()
            require(not stat.S_ISLNK(info.st_mode) and not getattr(info, "st_file_attributes", 0) & 0x400)
            self.acl(str(item), private=private if item == path else False, ancestor=item != path)
        info = path.lstat()
        require(stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode) and info.st_nlink == 1)
        return path

    def create_root(self, parent):
        parent = self.path(parent, directory=True)
        class Attributes(ctypes.Structure):
            _fields_ = [("size", W.DWORD), ("descriptor", ctypes.c_void_p), ("inherit", W.BOOL)]
        descriptor = ctypes.c_void_p()
        require(self.a.ConvertStringSecurityDescriptorToSecurityDescriptorW(
            "O:" + self.sid + "D:P(A;OICI;FA;;;" + self.sid + ")(A;OICI;FA;;;SY)",
            1, ctypes.byref(descriptor), None))
        try:
            attributes = Attributes(ctypes.sizeof(Attributes), descriptor, False)
            for _ in range(16):
                path = parent / ("morpheus-native-source-" + secrets.token_hex(16))
                if self.k.CreateDirectoryW(str(path), ctypes.byref(attributes)):
                    return self.path(path, directory=True, private=True)
                require(ctypes.get_last_error() == 183)  # Only collision retries.
            raise ValueError("Native source private root collision")
        finally:
            require(self.k.LocalFree(descriptor) is None)

    def open_read(self, path, *, private=False):
        import msvcrt
        path = self.path(path, private=private)
        handle = self.k.CreateFileW(str(path), 0x80000000 | 0x20000, 1, None, 3, 0x00200000 | 0x08000000, None)
        require(handle not in (None, ctypes.c_void_p(-1).value))
        try:
            self.acl(handle, handle=True, private=private)
            class FileInfo(ctypes.Structure):
                _fields_ = [("attributes", W.DWORD), ("creation", W.FILETIME), ("access", W.FILETIME),
                    ("write", W.FILETIME), ("volume", W.DWORD), ("size_high", W.DWORD), ("size_low", W.DWORD),
                    ("links", W.DWORD), ("index_high", W.DWORD), ("index_low", W.DWORD)]
            info = FileInfo()
            require(self.k.GetFileInformationByHandle(handle, ctypes.byref(info))
                    and not info.attributes & (0x400 | 0x10) and info.links == 1)
            # fd conversion transfers this exact noninheritable handle.
            descriptor = msvcrt.open_osfhandle(handle, os.O_RDONLY | os.O_BINARY | os.O_NOINHERIT)
            handle = None
            return descriptor
        finally:
            if handle is not None:
                require(self.k.CloseHandle(handle))


def security():
    return Security()
