"""Pure validators for a future ordinary installed-AppImage capture gate.

No loader overrides or subprocesses. The owner must authenticate the live child
PID, exact executable/library bytes and /proc inode mappings independently, then
pass complete bounded private LD_DEBUG=bindings output here. This validates only
the release executable's actual imports, never fabricates unused relocations.
"""

import re


MAX_TRACE = 8 * 1024 * 1024
MAX_INVENTORY = 1024 * 1024


class BindingFailure(RuntimeError):
    def __init__(self):
        super().__init__("Installed AppImage binding proof is incomplete or invalid")


def imported_pipewire_symbols(output, collisions):
    """Parse pinned GNU readelf --dyn-syms --wide output from shipped app ELF.

    Caller verifies readelf ownership/tool provenance and private output bounds.
    Strong/weak original PipeWire definitions in the app are forbidden. The
    isolated Morpheus-prefixed WebRTC stubs are not original pw_* symbols.
    """
    if (type(output) is not bytes or len(output) > MAX_INVENTORY or len(set(collisions)) != 33
            or any(not re.fullmatch(r"pw_[a-z0-9_]+", name) for name in collisions)):
        raise BindingFailure()
    try:
        lines = output.decode("ascii").splitlines()
    except UnicodeError:
        raise BindingFailure() from None
    imports = set()
    tables = 0
    entries = set()
    declared = 0
    for line in lines:
        header = re.fullmatch(r"Symbol table '.dynsym' contains (\d+) entries:", line)
        if header:
            tables += 1
            declared = int(header[1])
            if not 1 <= declared <= 32768:
                raise BindingFailure()
            continue
        match = re.fullmatch(r"\s*(\d+):\s+([0-9a-fA-F]+)\s+(\d+)\s+(\w+)\s+(\w+)\s+(\w+)\s+(\w+)\s*(.*?)\s*", line)
        if not match:
            if line.strip() not in ("", "Num:    Value          Size Type    Bind   Vis      Ndx Name"):
                raise BindingFailure()
            continue
        index = int(match[1])
        if index in entries:
            raise BindingFailure()
        entries.add(index)
        name = match[8].split(" ", 1)[0].split("@", 1)[0]
        if not name.startswith("pw_"):
            continue
        if (not re.fullmatch(r"pw_[a-z0-9_]+", name) or match[7] != "UND"
                or match[4] != "FUNC" or match[5] != "GLOBAL" or match[6] != "DEFAULT"):
            raise BindingFailure()
        imports.add(name)
    if tables != 1 or entries != set(range(declared)) or not imports or len(imports) > 256:
        raise BindingFailure()
    return {"imports": frozenset(imports), "collision_imports": frozenset(imports & set(collisions)),
            "not_imported_collisions": frozenset(set(collisions) - imports)}


def verify_binding_trace(trace, *, pid, executable, library, imports, complete, verified_library_aliases=()):
    """Require every actual app import to resolve solely to shipped client.

    Exact glibc 2.39 dl-lookup.c format; unknown providers/namespaces, conflicting
    repeated resolutions, truncation and absent records fail closed. Paths stay
    private; the return value contains only counts. Library-local calls cannot
    stand in for application-owned bindings.
    """
    if (complete is not True or type(trace) is not bytes or len(trace) > MAX_TRACE
            or type(pid) is not int or not 1 <= pid <= 2**31 - 1
            or not imports or len(imports) > 256
            or not all(re.fullmatch(r"pw_[a-z0-9_]+", name) for name in imports)
            or any(not isinstance(path, str) or not path.startswith("/")
                   or any(char in path for char in "\r\n\x00") for path in (executable, library))):
        raise BindingFailure()
    try:
        lines = trace.decode("utf-8", errors="strict").splitlines()
    except UnicodeError:
        raise BindingFailure() from None
    providers = {library, *verified_library_aliases}
    if len(providers) > 16 or any(not isinstance(path, str) or not path.startswith("/")
                                or len(path) > 4096 or any(c in path for c in "\r\n\x00") for path in providers):
        raise BindingFailure()
    found = set()
    prefix = re.compile(r"\s*" + str(pid) + r":\s+binding file ")
    form = re.compile(r"\s*(\d+):\s+binding file (.+) \[(\d+)\] to (.+) \[(\d+)\]: (normal|protected) symbol `([^']+)'(?: \[[^\]\r\n]+\])?\s*")
    for line in lines:
        if not prefix.match(line):
            continue
        match = form.fullmatch(line)
        if not match:
            raise BindingFailure()
        _, origin, namespace, provider, provider_namespace, visibility, symbol = match.groups()
        if origin != executable or not symbol.startswith("pw_"):
            continue
        if (namespace != "0" or provider_namespace != "0" or provider not in providers
                or visibility != "normal" or symbol not in imports):
            raise BindingFailure()
        found.add(symbol)
    if found != set(imports):
        raise BindingFailure()
    return {"app_pipewire_imports": len(imports), "app_pipewire_bindings_verified": len(found),
            "unexpected_providers": 0}


def provider_spellings(trace, *, pid, executable):
    """Collect bounded complete trace lines for live inode/hash attestation.

    This grants no trust. RuntimeIdentity must authenticate each exact spelling
    before passing it as a verified alias to the final complete-trace validator.
    A still-being-written final line is ignored here and checked after drain.
    """
    if type(trace) is not bytes or len(trace) > MAX_TRACE:
        raise BindingFailure()
    prefix = re.compile(r"\s*" + str(pid) + r":\s+binding file ")
    form = re.compile(r"\s*(\d+):\s+binding file (.+) \[(\d+)\] to (.+) \[(\d+)\]: (normal|protected) symbol `([^']+)'(?: \[[^\]\r\n]+\])?\s*")
    result = set()
    for raw in trace.splitlines(keepends=True):
        if not raw.endswith(b"\n"):
            continue
        try:
            line = raw.decode("utf-8", "strict")
        except UnicodeError:
            raise BindingFailure() from None
        if not prefix.match(line):
            continue
        match = form.fullmatch(line)
        if not match:
            raise BindingFailure()
        if match[2] != executable or not match[7].startswith("pw_"):
            continue
        if match[3] != "0" or match[5] != "0" or match[6] != "normal":
            raise BindingFailure()
        result.add(match[4])
        if len(result) > 16:
            raise BindingFailure()
    return result
