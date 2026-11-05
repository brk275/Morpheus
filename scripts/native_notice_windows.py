"""Windows file/ACL primitives for the source-only notice acquisition route.

Never used on Linux. No shell, impersonation, registry or owner-profile access. Private
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
import math
import re
import time


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
            (self.k, "GetFileInformationByHandleEx", [W.HANDLE, ctypes.c_int, ctypes.c_void_p, W.DWORD], W.BOOL),
            (self.k, "SetFileInformationByHandle", [W.HANDLE, ctypes.c_int, ctypes.c_void_p, W.DWORD], W.BOOL),
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

    def system_git(self):
        # The provider's exact Git launcher can have protected hardlink aliases.
        # No caller supplies a path or a hardlink-policy flag. Keep the original
        # non-private ownership/DACL and canonical local-path checks unchanged.
        require(os.name == "nt")
        path = Path(os.environ["PROGRAMFILES"]) / "Git/cmd/git.exe"
        require(path.is_absolute() and not str(path).startswith(("\\\\", "//"))
                and path.resolve(strict=True) == path
                and self.k.GetDriveTypeW(path.anchor) == 3)
        for item in (path, *path.parents):
            info = item.lstat()
            require(not stat.S_ISLNK(info.st_mode) and not getattr(info, "st_file_attributes", 0) & 0x400)
            self.acl(str(item), private=False, ancestor=item != path)
        info = path.lstat()
        require(stat.S_ISREG(info.st_mode) and info.st_nlink >= 1)
        return path

    def private_directory(self, path, *, parents=False, exist_ok=False):
        """Create only inside an already private parent with explicit user/SYSTEM ACL.

        Python's Windows mkdir(0700) grants Administrators access and overrides
        the reserved root's protected inheritance. Never repair an existing ACL
        or relax private readers; every existing parent and final child must
        satisfy the unchanged private owner/DACL/reparse/fixed-drive policy.
        """
        require(os.name == "nt" and type(parents) is bool and type(exist_ok) is bool)
        path = Path(path)
        require(path.is_absolute() and not str(path).startswith(("\\\\", "//"))
                and len(path.parts) <= 128)
        for part in path.parts[1:]:
            require(part not in {"", ".", ".."} and not part.endswith((" ", "."))
                    and not any(character in part for character in '<>:"/\\|?*\0'))
        require(path.name not in {"", ".", ".."})
        if parents and not path.parent.exists():
            self.private_directory(path.parent, parents=True, exist_ok=True)
        parent = self.path(path.parent, directory=True, private=True)
        require(path.parent == parent)
        if path.exists():
            require(exist_ok)
            return self.path(path, directory=True, private=True)
        class Attributes(ctypes.Structure):
            _fields_ = [("size", W.DWORD), ("descriptor", ctypes.c_void_p), ("inherit", W.BOOL)]
        descriptor = ctypes.c_void_p()
        require(self.a.ConvertStringSecurityDescriptorToSecurityDescriptorW(
            "O:" + self.sid + "D:P(A;OICI;FA;;;" + self.sid + ")(A;OICI;FA;;;SY)",
            1, ctypes.byref(descriptor), None))
        try:
            attributes = Attributes(ctypes.sizeof(Attributes), descriptor, False)
            created = self.k.CreateDirectoryW(str(path), ctypes.byref(attributes))
            require(created or exist_ok and ctypes.get_last_error() == 183)
            return self.path(path, directory=True, private=True)
        finally:
            require(self.k.LocalFree(descriptor) is None)

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

    def unlink_private_profile_junction(self, workspace, workspace_identity, root, root_identity,
                                        path, expected_leaf, *, deadline):
        """Delete only a captured private runtime-profile junction, never its target.

        Callers have stopped every owned Job and completely scanned its logs.
        The generic path/read policy remains unchanged and rejects all reparse points.
        """
        workspace, root, path = Path(workspace), Path(root), Path(path)
        profile = root / "tools-home" / "home"
        require(workspace.is_absolute() and workspace.parent == Path.home()
                and re.fullmatch(r"morpheus-native-source-[0-9a-f]{32}", workspace.name)
                and root.parent == workspace
                and re.fullmatch(r"morpheus-windows-updateqa-[0-9a-f]{32}", root.name)
                and path.is_absolute() and path != profile and path.is_relative_to(profile)
                and not str(path).startswith(("\\\\", "//")) and len(path.parts) <= 128)
        for part in path.parts[1:]:
            require(part not in {"", ".", ".."} and not part.endswith((" ", "."))
                    and not any(character in part for character in '<>:"/\\|?*\0'))
            device = part.partition(".")[0].rstrip(" ").casefold()
            require(device not in {"con", "prn", "aux", "nul", "clock$", "conin$", "conout$"}
                    and re.fullmatch(r"(?:com|lpt)[1-9¹²³]", device) is None)
        require(type(deadline) in (int, float) and math.isfinite(deadline)
                and 0 < deadline - time.monotonic() <= 6900)
        for value in (workspace_identity, root_identity):
            require(type(value) is tuple and len(value) == 2
                    and all(type(number) is int and number >= 0 for number in value) and value[1] > 0)
        require(type(expected_leaf) is tuple and len(expected_leaf) == 8
                and all(type(number) is int for number in expected_leaf)
                and expected_leaf[0] >= 0 and expected_leaf[1] > 0 and expected_leaf[2] > 0
                and expected_leaf[3] >= 0 and 0 <= expected_leaf[4] <= 0xffffffff
                and expected_leaf[4] & (0x400 | 0x10) == 0x410
                and expected_leaf[5] == 0xa0000003 and expected_leaf[6] == 1 and expected_leaf[7] == 0)
        require(self.k.GetDriveTypeW(path.anchor) == 3)

        class IdInfo(ctypes.Structure):
            _fields_ = [("volume", ctypes.c_uint64), ("identifier", ctypes.c_ubyte * 16)]
        class BasicInfo(ctypes.Structure):
            _fields_ = [("created", ctypes.c_int64), ("accessed", ctypes.c_int64),
                        ("written", ctypes.c_int64), ("changed", ctypes.c_int64),
                        ("attributes", ctypes.c_uint32)]
        class TagInfo(ctypes.Structure):
            _fields_ = [("attributes", ctypes.c_uint32), ("tag", ctypes.c_uint32)]
        class StandardInfo(ctypes.Structure):
            _fields_ = [("allocation", ctypes.c_int64), ("size", ctypes.c_int64),
                        ("links", ctypes.c_uint32), ("pending", ctypes.c_ubyte), ("directory", ctypes.c_ubyte)]
        class Disposition(ctypes.Structure):
            _fields_ = [("delete", ctypes.c_ubyte)]  # Windows BOOLEAN, not BOOL.
        require(tuple(map(ctypes.sizeof, (IdInfo, BasicInfo, TagInfo, StandardInfo, Disposition)))
                == (24, 40, 8, 24, 1))
        def check_time():
            require(time.monotonic() < deadline)
        def inspect(handle):
            check_time()
            identity, basic, tag, standard = IdInfo(), BasicInfo(), TagInfo(), StandardInfo()
            for kind, value in ((18, identity), (0, basic), (9, tag), (1, standard)):
                check_time()
                require(self.k.GetFileInformationByHandleEx(handle, kind, ctypes.byref(value), ctypes.sizeof(value)))
            require(not standard.pending and standard.directory and standard.links == 1
                    and basic.attributes == tag.attributes and tag.attributes & 0x10)
            born = (basic.created - 116444736000000000) * 100
            written = (basic.written - 116444736000000000) * 100
            require(basic.created >= 0 and basic.written >= 0)
            # Trusted drive/system ancestors can predate the Unix epoch; exact
            # IDs/timestamps still bind them. Fresh private objects are checked below.
            # Matches CPython's FileIdInfo/lstat mapping without conflating
            # Windows CreationTime and ChangeTime.
            value = (identity.volume, int.from_bytes(bytes(identity.identifier), "little"),
                     born, written, tag.attributes, tag.tag, standard.links, standard.size)
            return value, basic.changed
        def named_identity(selected):
            check_time()
            value = selected.lstat()  # Every ancestor is held no-delete/no-reparse.
            return (value.st_dev, value.st_ino, value.st_birthtime_ns, value.st_mtime_ns,
                    value.st_file_attributes, value.st_reparse_tag, value.st_nlink, value.st_size)
        handles, parents = [], []
        failure = None
        leaf = None
        try:
            # Lock from the fixed drive down: a checked ancestor cannot be
            # renamed into a junction between validation and opening its child.
            current = Path(path.anchor)
            chain = [current]
            for part in path.parts[1:-1]:
                current = current / part
                chain.append(current)
            for current in chain:
                check_time()
                handle = self.k.CreateFileW(str(current), 0x20000 | 0x80, 3, None, 3, 0x02200000, None)
                require(handle not in (None, ctypes.c_void_p(-1).value))
                handles.append(handle)
                value = inspect(handle)
                require(not value[0][4] & 0x400 and value[0][5] == 0
                        and value[0] == named_identity(current))
                private = current == workspace or current.is_relative_to(workspace)
                if private: require(value[0][2] > 0 and value[0][3] >= 0)
                check_time()
                self.acl(handle, handle=True, private=private, ancestor=not private)
                if current == workspace: require(value[0][:2] == workspace_identity)
                if current == root: require(value[0][:2] == root_identity)
                parents.append((current, handle, value, private))
            require(any(current == workspace for current, *_ in parents)
                    and any(current == root for current, *_ in parents)
                    and parents[-1][0] == path.parent)
            check_time()
            require(named_identity(path) == expected_leaf)
            leaf = self.k.CreateFileW(str(path), 0x10000 | 0x20000 | 0x80, 1, None, 3, 0x02200000, None)
            require(leaf not in (None, ctypes.c_void_p(-1).value))
            handles.append(leaf)
            observed = inspect(leaf)
            require(observed[0] == expected_leaf)
            check_time()
            self.acl(leaf, handle=True, private=True)
            # All authority remains handle-bound until the one deletion.
            for current, handle, previous, private in parents:
                require(inspect(handle) == previous and named_identity(current) == previous[0])
                check_time()
                self.acl(handle, handle=True, private=private, ancestor=not private)
            require(inspect(leaf) == observed)
            check_time()
            self.acl(leaf, handle=True, private=True)
            check_time()
            disposition = Disposition(1)
            require(self.k.SetFileInformationByHandle(leaf, 4, ctypes.byref(disposition), ctypes.sizeof(disposition)))
            handles.pop()
            require(self.k.CloseHandle(leaf))
            leaf = None
            check_time()
            try:
                path.lstat()
            except FileNotFoundError:
                pass
            else:
                raise ValueError("Native profile junction name was replaced")
            for current, handle, previous, private in parents:
                checked_now = inspect(handle)
                now = checked_now[0]
                if current == path.parent:
                    # This one directory's write/change times may advance for
                    # our authorized unlink; its object/type/ACL cannot change.
                    require(now[:3] == previous[0][:3] and now[4:] == previous[0][4:])
                else:
                    require(checked_now == previous)
                require(named_identity(current) == now)
                check_time()
                self.acl(handle, handle=True, private=private, ancestor=not private)
        except BaseException as error:
            failure = error
        finally:
            for handle in reversed(handles):
                try:
                    require(self.k.CloseHandle(handle))
                except BaseException as error:
                    if failure is None: failure = error
        if failure is not None:
            if isinstance(failure, (KeyboardInterrupt, SystemExit)) or type(failure) is ValueError:
                raise failure
            raise ValueError("Native notice Windows ownership rejected") from None

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
