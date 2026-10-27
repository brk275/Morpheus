param([string] $Mode = 'basic')

# Only fixed Windows system DLL exports are called. No Add-Type/compiler,
# external executable, batch file, SDK installer or shell command is invoked.
function New-HostReadDelegate([string] $Name, [Type] $Result, [Type[]] $Arguments) {
    $nameInfo = [Reflection.AssemblyName]::new('MorpheusHostRead_' + [Guid]::NewGuid().ToString('N'))
    $assembly = [Reflection.Emit.AssemblyBuilder]::DefineDynamicAssembly($nameInfo, [Reflection.Emit.AssemblyBuilderAccess]::Run)
    $module = $assembly.DefineDynamicModule('ReadOnly')
    $type = $module.DefineType('NativeCall', [Reflection.TypeAttributes]'Public,Sealed', [MulticastDelegate])
    $ctor = $type.DefineConstructor([Reflection.MethodAttributes]'Public,HideBySig,RTSpecialName',
        [Reflection.CallingConventions]::Standard, [Type[]]@([object], [IntPtr]))
    $ctor.SetImplementationFlags([Reflection.MethodImplAttributes]::Runtime)
    $invoke = $type.DefineMethod('Invoke', [Reflection.MethodAttributes]'Public,HideBySig,NewSlot,Virtual', $Result, $Arguments)
    $invoke.SetImplementationFlags([Reflection.MethodImplAttributes]::Runtime)
    $delegateType = $type.CreateTypeInfo().AsType()
    return [Runtime.InteropServices.Marshal]::GetDelegateForFunctionPointer(
        [Runtime.InteropServices.NativeLibrary]::GetExport($script:HostKernel, $Name), $delegateType)
}

function Initialize-HostReader {
    # One sequential reader owns its metadata scratch until Close-HostReader.
    # Refuse an overlapping initialization rather than lose live owned storage.
    $previous = Get-Variable -Name HostInfoMemory -Scope Script -ErrorAction SilentlyContinue
    $previousHandles = Get-Variable -Name HostHandles -Scope Script -ErrorAction SilentlyContinue
    if (($null -ne $previous -and $previous.Value -ne [IntPtr]::Zero) -or
        ($null -ne $previousHandles -and $previousHandles.Value.Count -ne 0)) { throw 'identity' }
    $script:HostPreflightBudgetCause = 'unknown'
    $script:HostInfoMemory = [IntPtr]::Zero
    $script:HostInfoScratch = $null
    $script:HostInfoBusy = $false
    $script:HostHandles = [Collections.Generic.List[IntPtr]]::new()
    $script:HostCanonicalPaths = $null
    $script:HostCanonicalUnits = 0
    # Only immutable lexical results are memoized. Count and UTF-16 code-unit
    # limits bound this reader-owned cache separately from receipt limits.
    $script:HostCanonicalEntryLimit = 8192
    $script:HostCanonicalUnitLimit = 1048576
    $script:HostCanonicalPaths = [Collections.Generic.Dictionary[string, string]]::new([StringComparer]::Ordinal)
    # These are fixed source limits, never manifest-selected overrides.
    $script:HostLimits = @{ Entries = 200000; Handles = 200256; FileBytes = 2147483648;
        TotalBytes = 68719476736; Depth = 64; JsonBytes = 33554432; MetadataBytes = 31457280; Seconds = 180 }
    $script:HostWatch = [Diagnostics.Stopwatch]::StartNew()
    $script:HostCount = 0; $script:HostBytes = [uint64]0; $script:HostMetadataBytes = 0
    $script:HostBuffer = [byte[]]::new(1048576)
    $script:HostFileRecords = [Collections.Generic.List[object]]::new()
    $script:HostDirectories = [Collections.Generic.Dictionary[string, object]]::new([StringComparer]::OrdinalIgnoreCase)
    $system = Get-HostCanonicalPath $env:SystemRoot
    if ($system -cnotmatch '^[A-Za-z]:\\Windows$') { throw 'host' }
    $script:HostKernel = [Runtime.InteropServices.NativeLibrary]::Load([IO.Path]::Combine($system, 'System32', 'kernel32.dll'))
    $script:HostOpen = New-HostReadDelegate 'CreateFileW' ([IntPtr]) ([Type[]]@([IntPtr], [uint32], [uint32], [IntPtr], [uint32], [uint32], [IntPtr]))
    $script:HostInfo = New-HostReadDelegate 'GetFileInformationByHandle' ([int]) ([Type[]]@([IntPtr], [IntPtr]))
    $script:HostType = New-HostReadDelegate 'GetFileType' ([uint32]) ([Type[]]@([IntPtr]))
    $script:HostClose = New-HostReadDelegate 'CloseHandle' ([int]) ([Type[]]@([IntPtr]))
    # Allocate last: no further initialization can fail after unmanaged ownership
    # is acquired. The 52-byte Win32 record and ABI are unchanged.
    $script:HostInfoScratch = [byte[]]::new(52)
    $script:HostInfoMemory = [Runtime.InteropServices.Marshal]::AllocHGlobal(52)
}

function Assert-HostBudget {
    # Evaluate each original predicate once, in the same short-circuit order.
    # Retain only the first refused budget, including through reader cleanup.
    $cause = if ($script:HostWatch.Elapsed.TotalSeconds -gt $script:HostLimits.Seconds) {
        'deadline'
    } elseif ($script:HostCount -gt $script:HostLimits.Entries) {
        'entries'
    } elseif ($script:HostBytes -gt $script:HostLimits.TotalBytes) {
        'bytes'
    } elseif ($script:HostHandles.Count -gt $script:HostLimits.Handles) {
        'handles'
    } elseif ($script:HostMetadataBytes -gt $script:HostLimits.MetadataBytes) {
        'metadata'
    } else { return }
    if ($script:HostPreflightBudgetCause -ceq 'unknown') { $script:HostPreflightBudgetCause = $cause }
    throw 'limit'
}

function Get-CanonicalHostPath([string] $Value) {
    if ([string]::IsNullOrEmpty($Value) -or $Value.Length -gt 1024 -or
        $Value -notmatch '^[A-Za-z]:\\' -or $Value.Substring(2).Contains(':') -or
        $Value.Contains('/') -or $Value -match '[\x00-\x1f*?"<>|]') {
        # Classify only an already-refused immutable string, in the exact
        # original short-circuit order. These labels grant no path allowance.
        if ([string]::IsNullOrEmpty($Value)) { throw 'path_empty' }
        if ($Value.Length -gt 1024) { throw 'path_length' }
        if ($Value -notmatch '^[A-Za-z]:\\') { throw 'path_drive' }
        if ($Value.Substring(2).Contains(':')) { throw 'path_colon' }
        if ($Value.Contains('/')) { throw 'path_slash' }
        if ($Value -match '[\x00-\x1f*?"<>|]') { throw 'path_chars' }
        throw 'path_input'
    }
    $full = [IO.Path]::GetFullPath($Value)
    if ($full.TrimEnd('\') -cne $Value.TrimEnd('\')) { throw 'path_normalized' }
    foreach ($part in $Value.Substring(3).Split('\')) {
        if ($part -eq '' -and $Value.Length -eq 3) { continue }
        if ($part -eq '' -or $part -in @('.', '..') -or $part.EndsWith('.') -or $part.EndsWith(' ') -or
            $part -match '^(?i:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)' -or $part -ieq '.git') {
            if ($part -eq '') { throw 'path_part_empty' }
            if ($part -in @('.', '..')) { throw 'path_part_dot' }
            if ($part.EndsWith('.')) { throw 'path_part_trailingdot' }
            if ($part.EndsWith(' ')) { throw 'path_part_trailingspace' }
            if ($part -match '^(?i:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)') { throw 'path_part_device' }
            if ($part -ieq '.git') { throw 'path_part_git' }
            throw 'path_component'
        }
    }
    return $full
}

# Internal reader callers initialize the owner first. The original lexical
# helper above remains available before initialization. After Close, a null
# owner falls back to full validation without retaining any result.
function Get-HostCanonicalPath([string] $Value) {
    $memo = $script:HostCanonicalPaths
    [string] $full = ''
    if ($null -ne $memo -and -not [string]::IsNullOrEmpty($Value) -and
        $memo.TryGetValue($Value, [ref]$full)) { return $full }
    $full = Get-CanonicalHostPath $Value
    if ($null -ne $memo) {
        $units = $Value.Length + $full.Length
        if ($memo.Count -lt $script:HostCanonicalEntryLimit -and
            $units -le $script:HostCanonicalUnitLimit - $script:HostCanonicalUnits) {
            $memo.Add($Value, $full)
            $script:HostCanonicalUnits += $units
        }
    }
    return $full
}

function Get-HostHandleInfo([IntPtr] $Handle, $Expected, [bool] $Content = $false) {
    # These synchronous calls have no callbacks. Never overlap use or return a
    # view of shared scratch: the result below contains only copied values.
    if ($script:HostInfoBusy -or $script:HostInfoMemory -eq [IntPtr]::Zero) { throw 'identity' }
    $script:HostInfoBusy = $true
    try {
        if ($script:HostType.Invoke($Handle) -ne 1) { throw 'type' }
        $memory = $script:HostInfoMemory
        if ($script:HostInfo.Invoke($Handle, $memory) -eq 0) { throw 'identity' }
        $data = $script:HostInfoScratch; [Runtime.InteropServices.Marshal]::Copy($memory, $data, 0, 52)
        $attributes = [BitConverter]::ToUInt32($data, 0)
        if (($attributes -band 0x400) -ne 0) { throw 'reparse' }
        if ($PSBoundParameters.ContainsKey('Expected')) {
            if ($Expected -isnot [byte[]] -or $Expected.Length -ne 52) { throw 'identity' }
            # Equality of these fixed-width spans is exactly the old copied
            # attributes/volume/file-index and optional size/write/link check.
            # No hashtable or hex strings are needed for a repeated comparison.
            if ($attributes -ne [BitConverter]::ToUInt32($Expected, 0) -or
                [BitConverter]::ToUInt32($data, 28) -ne [BitConverter]::ToUInt32($Expected, 28) -or
                [BitConverter]::ToUInt64($data, 44) -ne [BitConverter]::ToUInt64($Expected, 44) -or
                ($Content -and (
                    [BitConverter]::ToUInt64($data, 32) -ne [BitConverter]::ToUInt64($Expected, 32) -or
                    [BitConverter]::ToUInt64($data, 20) -ne [BitConverter]::ToUInt64($Expected, 20) -or
                    [BitConverter]::ToUInt32($data, 40) -ne [BitConverter]::ToUInt32($Expected, 40)))) { throw 'changed' }
            return
        }
        return @{ attributes = $attributes; directory = (($attributes -band 0x10) -ne 0);
            bytes = ([uint64][BitConverter]::ToUInt32($data, 32) * 4294967296 + [uint64][BitConverter]::ToUInt32($data, 36));
            write = [Convert]::ToHexString($data, 20, 8); links = [BitConverter]::ToUInt32($data, 40);
            identity = ([Convert]::ToHexString($data, 28, 4) + [Convert]::ToHexString($data, 44, 8)) }
    } finally { $script:HostInfoBusy = $false }
}

function Open-HostReadHandle([string] $LiteralPath, [bool] $Directory) {
    Assert-HostBudget
    $full = Get-HostCanonicalPath $LiteralPath
    $wide = [Runtime.InteropServices.Marshal]::StringToHGlobalUni('\\?\' + $full)
    try {
        # OPEN_REPARSE_POINT always; BACKUP_SEMANTICS only enables directory
        # opens. Share READ only prevents original opened files being written,
        # renamed or deleted until the complete snapshot has been checked.
        $access = if ($Directory) { [uint32]128 } else { [uint32]2147483648 }
        $flags = if ($Directory) { [uint32]0x02200000 } else { [uint32]0x00200000 }
        $handle = $script:HostOpen.Invoke($wide, $access, [uint32]1, [IntPtr]::Zero, [uint32]3, $flags, [IntPtr]::Zero)
    } finally { [Runtime.InteropServices.Marshal]::FreeHGlobal($wide) }
    if ($handle -eq [IntPtr]::new(-1)) { throw 'open' }
    $script:HostHandles.Add($handle); Assert-HostBudget
    $info = Get-HostHandleInfo $handle
    if ($info.directory -ne $Directory) { throw 'type' }
    # Private record storage, never a receipt field or a shared-scratch view.
    # At most the existing 200256 retained handles own one 52-byte copy each.
    $comparison = [byte[]]$script:HostInfoScratch.Clone()
    return @{ path = $full; handle = $handle; initial = $info; comparison = $comparison }
}

function Assert-HostIdentity($Record, [bool] $Content) {
    Get-HostHandleInfo $Record.handle $Record.comparison $Content
}

function Open-HostDirectoryChain([string] $LiteralPath) {
    $full = Get-HostCanonicalPath $LiteralPath
    $current = $full.Substring(0, 3)
    $parts = @('') + @($full.Substring(3).Split('\') | Where-Object { $_ -ne '' })
    foreach ($part in $parts) {
        if ($part -ne '') { $current = [IO.Path]::Combine($current, $part) }
        if (-not $script:HostDirectories.ContainsKey($current)) {
            $record = Open-HostReadHandle $current $true
            $script:HostDirectories.Add($current, $record)
        } else { Assert-HostIdentity $script:HostDirectories[$current] $false }
    }
    return $script:HostDirectories[$full]
}

function Get-HostFileFact([string] $LiteralPath, [int] $TextLimit = 0, [uint64] $RemainingBytes = [uint64]::MaxValue) {
    $full = Get-HostCanonicalPath $LiteralPath
    $null = Open-HostDirectoryChain ([IO.Path]::GetDirectoryName($full))
    $record = Open-HostReadHandle $full $false
    $script:HostFileRecords.Add($record)
    if ($record.initial.bytes -gt $script:HostLimits.FileBytes) {
        if ($script:HostPreflightBudgetCause -ceq 'unknown') { $script:HostPreflightBudgetCause = 'bytes' }
        throw 'file_size'
    }
    if ($TextLimit -gt 0 -and $record.initial.bytes -gt $TextLimit) {
        if ($script:HostPreflightBudgetCause -ceq 'unknown') { $script:HostPreflightBudgetCause = 'bytes' }
        throw 'text_size'
    }
    # The retained no-follow handle supplies the size. Refuse before creating
    # a stream or digest so the next payload cannot exceed the root budget.
    if ($record.initial.bytes -gt $RemainingBytes) {
        if ($script:HostPreflightBudgetCause -ceq 'unknown') { $script:HostPreflightBudgetCause = 'bytes' }
        throw 'limit'
    }
    $safe = [Microsoft.Win32.SafeHandles.SafeFileHandle]::new($record.handle, $false)
    $stream = [IO.FileStream]::new($safe, [IO.FileAccess]::Read, 1048576, $false)
    $digest = [Security.Cryptography.SHA256]::Create(); $buffer = $script:HostBuffer
    $text = if ($TextLimit -gt 0) { [IO.MemoryStream]::new() } else { $null }
    try {
        $size = [uint64]0
        while (($count = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
            Assert-HostBudget; $size += $count
            if ($size -gt $record.initial.bytes) { throw 'changed' }
            $null = $digest.TransformBlock($buffer, 0, $count, $null, 0)
            if ($null -ne $text) { $text.Write($buffer, 0, $count) }
        }
        $null = $digest.TransformFinalBlock([byte[]]::new(0), 0, 0)
        if ($size -ne $record.initial.bytes) { throw 'changed' }
        Assert-HostIdentity $record $true
        $fact = @{ bytes = $size; sha256 = [Convert]::ToHexString($digest.Hash).ToLowerInvariant() }
        if ($null -ne $text) { $fact.text = [Text.UTF8Encoding]::new($false, $true).GetString($text.ToArray()) }
        return $fact
    } finally {
        $stream.Dispose(); $safe.Dispose(); $digest.Dispose()
        if ($null -ne $text) { $text.Dispose() }
    }
}

function Get-HostDirectoryNames([string] $Directory) {
    $names = [Collections.Generic.List[string]]::new()
    $folded = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($path in [IO.Directory]::EnumerateFileSystemEntries($Directory)) {
        Assert-HostBudget
        $name = [IO.Path]::GetFileName($path)
        $null = Get-HostCanonicalPath ([IO.Path]::Combine($Directory, $name))
        if (-not $folded.Add($name)) { throw 'alias' }
        $names.Add($name)
        if ($names.Count -gt $script:HostLimits.Entries) {
            if ($script:HostPreflightBudgetCause -ceq 'unknown') { $script:HostPreflightBudgetCause = 'entries' }
            throw 'entries'
        }
    }
    $result = $names.ToArray(); [Array]::Sort($result, [StringComparer]::Ordinal)
    return ,$result
}

function Get-HostRootInventory([string] $LiteralRoot) {
    $root = Get-HostCanonicalPath $LiteralRoot
    $null = Open-HostDirectoryChain $root
    $pending = [Collections.Generic.Stack[object]]::new(); $pending.Push(@{ path = $root; depth = 0 })
    $snapshots = [Collections.Generic.List[object]]::new(); $files = [Collections.Generic.List[object]]::new()
    while ($pending.Count -gt 0) {
        Assert-HostBudget; $next = $pending.Pop()
        if ($next.depth -gt $script:HostLimits.Depth) { throw 'depth' }
        $directory = Open-HostDirectoryChain $next.path
        $names = Get-HostDirectoryNames $next.path
        $snapshots.Add(@{ directory = $directory; names = $names })
        foreach ($name in $names) {
            $script:HostCount++; Assert-HostBudget
            $path = [IO.Path]::Combine($next.path, $name)
            # Attributes only select the read-open kind. The final no-follow
            # handle supplies the authoritative type before any payload read.
            $attributes = [IO.File]::GetAttributes($path)
            if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'reparse' }
            if (($attributes -band [IO.FileAttributes]::Directory) -ne 0) {
                $null = Open-HostDirectoryChain $path
                $pending.Push(@{ path = $path; depth = $next.depth + 1 })
            } else {
                $fact = Get-HostFileFact $path 0 ([uint64]($script:HostLimits.TotalBytes - $script:HostBytes))
                $script:HostBytes += $fact.bytes; Assert-HostBudget
                $relative = [IO.Path]::GetRelativePath($root, $path).Replace('\', '/')
                # JSON upper bound before adding a row, including worst-case
                # escaping. Leave 2MiB for fixed roots/roles and top-level data.
                $script:HostMetadataBytes += 160 + 6 * $relative.Length
                Assert-HostBudget
                $files.Add(@{ path = $relative; bytes = $fact.bytes; sha256 = $fact.sha256 })
            }
        }
    }
    foreach ($snapshot in $snapshots) {
        Assert-HostBudget; Assert-HostIdentity $snapshot.directory $true
        $again = Get-HostDirectoryNames $snapshot.directory.path
        if ($again.Count -ne $snapshot.names.Count) { throw 'changed' }
        for ($i = 0; $i -lt $again.Count; $i++) { if ($again[$i] -cne $snapshot.names[$i]) { throw 'changed' } }
    }
    return @{ root = $root; files = @($files.ToArray()) }
}

function Close-HostReader {
    if ($script:HostInfoBusy) { throw 'identity' }
    $good = $true
    try {
        if ($null -ne $script:HostHandles) {
            for ($i = $script:HostHandles.Count - 1; $i -ge 0; $i--) {
                $handle = $script:HostHandles[$i]
                # An uncertain close is never retried on a potentially reused handle.
                $script:HostHandles[$i] = [IntPtr]::Zero
                if ($handle -ne [IntPtr]::Zero -and $script:HostClose.Invoke($handle) -eq 0) { $good = $false }
            }
            $script:HostHandles.Clear()
        }
    } finally {
        try {
            # Owned scratch is freed even when closing a native file handle fails.
            $memory = $script:HostInfoMemory
            $script:HostInfoMemory = [IntPtr]::Zero
            $script:HostInfoScratch = $null
            $script:HostInfoBusy = $false
            if ($memory -ne [IntPtr]::Zero) { [Runtime.InteropServices.Marshal]::FreeHGlobal($memory) }
        } finally {
            # Retire lexical ownership even if native cleanup throws. No result
            # survives Close or is shared with the next initialized reader.
            $memo = $script:HostCanonicalPaths
            $script:HostCanonicalPaths = $null
            $script:HostCanonicalUnits = 0
            if ($null -ne $memo) { $memo.Clear() }
        }
    }
    if (-not $good) { throw 'handle_cleanup' }
}

function Get-HostReceiptMetadata([hashtable] $Values) {
    $required = @('run_id', 'run_attempt', 'source_commit', 'image_os', 'image_version', 'powershell_version')
    if ($Values.Count -ne $required.Count) { throw 'metadata' }
    foreach ($key in $required) {
        if (-not $Values.ContainsKey($key) -or $Values[$key] -isnot [string]) { throw 'metadata' }
    }
    if ($Values.run_id -cnotmatch '^[0-9]{1,20}$' -or $Values.run_attempt -cnotmatch '^[0-9]{1,6}$' -or
        $Values.source_commit -cnotmatch '^[0-9a-f]{40}$') { throw 'metadata' }
    foreach ($key in @('image_os', 'image_version', 'powershell_version')) {
        if ($Values[$key] -cnotmatch '^[A-Za-z0-9._-]{1,64}$') { throw 'metadata' }
    }
    return $Values
}

function Get-HostPreflightFailureMessage($Stage, $BudgetCause) {
    # Keep the original one-argument result byte-for-byte. The entry opts into
    # a separate closed budget label; never stringify an untrusted value.
    $budgetSuffix = ''
    if ($PSBoundParameters.ContainsKey('BudgetCause')) {
        $budgetSuffix = ' budget=unknown'
        if ($BudgetCause -is [string]) {
            foreach ($budget in @('deadline', 'entries', 'bytes', 'handles', 'metadata')) {
                if ([string]::Equals($BudgetCause, $budget, [StringComparison]::Ordinal)) {
                    $budgetSuffix = ' budget=' + $budget
                    break
                }
            }
        }
    }
    # Only source-fixed phases can reach the log. Never inspect the exception,
    # a path, environment value or another object's string representation.
    if ($Stage -is [string]) {
        foreach ($phase in @('initialization', 'bootstrap', 'vs_inventory',
            'sdk_inventory', 'role_binding', 'final_identity', 'serialization')) {
            if ([string]::Equals($Stage, $phase, [StringComparison]::Ordinal)) {
                return 'MORPHEUS_NATIVE_WINDOWS_PREFLIGHT failed stage=' + $phase + $budgetSuffix
            }
        }
    }
    return 'MORPHEUS_NATIVE_WINDOWS_PREFLIGHT failed stage=unknown' + $budgetSuffix
}

function Get-HostToolchainInventory {
    $script:HostPreflightStage = 'initialization'
    Initialize-HostReader
    try {
        $script:HostPreflightStage = 'role_binding'
        $drive = $env:SystemRoot.Substring(0, 2)
        $vsBase = $drive + '\Program Files\Microsoft Visual Studio\2022'
        $candidates = @('Enterprise', 'Professional', 'Community', 'Preview', 'BuildTools') |
            ForEach-Object { [IO.Path]::Combine($vsBase, $_) } | Where-Object { [IO.Directory]::Exists($_) }
        if (@($candidates).Count -ne 1) { throw 'visual_studio_selection' }
        $vs = Get-HostCanonicalPath (@($candidates)[0])
        $sdk = $drive + '\Program Files (x86)\Windows Kits\10'
        $versionFile = [IO.Path]::Combine($vs, 'VC', 'Auxiliary', 'Build', 'Microsoft.VCToolsVersion.default.txt')
        $versionFact = Get-HostFileFact $versionFile 256
        $version = $versionFact.text.Trim()
        if ($version -notmatch '^14\.[0-9]{1,3}\.[0-9]{1,6}$') { throw 'msvc_version' }
        $msvc = [IO.Path]::Combine($vs, 'VC', 'Tools', 'MSVC', $version)
        $vcvars = [IO.Path]::Combine($vs, 'VC', 'vcvarsall.bat')
        if (-not [IO.File]::Exists($vcvars)) { $vcvars = [IO.Path]::Combine($vs, 'VC', 'Auxiliary', 'Build', 'vcvarsall.bat') }
        $roles = [ordered]@{ vcvarsall = $vcvars; msvc_default_version = $versionFile;
            cl = [IO.Path]::Combine($msvc, 'bin', 'Hostx64', 'x64', 'cl.exe');
            link = [IO.Path]::Combine($msvc, 'bin', 'Hostx64', 'x64', 'link.exe');
            msvc_include = [IO.Path]::Combine($msvc, 'include', 'vcruntime.h');
            msvc_lib = [IO.Path]::Combine($msvc, 'lib', 'x64', 'vcruntime.lib');
            sdk_rc = [IO.Path]::Combine($sdk, 'bin', '10.0.26100.0', 'x64', 'rc.exe');
            sdk_mt = [IO.Path]::Combine($sdk, 'bin', '10.0.26100.0', 'x64', 'mt.exe');
            sdk_um = [IO.Path]::Combine($sdk, 'Include', '10.0.26100.0', 'um', 'Windows.h');
            sdk_shared = [IO.Path]::Combine($sdk, 'Include', '10.0.26100.0', 'shared', 'sdkddkver.h');
            sdk_ucrt = [IO.Path]::Combine($sdk, 'Include', '10.0.26100.0', 'ucrt', 'stdlib.h');
            sdk_kernel_lib = [IO.Path]::Combine($sdk, 'Lib', '10.0.26100.0', 'um', 'x64', 'kernel32.lib');
            sdk_ucrt_lib = [IO.Path]::Combine($sdk, 'Lib', '10.0.26100.0', 'ucrt', 'x64', 'ucrt.lib') }
        $roleFacts = [ordered]@{}
        foreach ($name in $roles.Keys) {
            $fact = Get-HostFileFact $roles[$name]
            if ($fact.bytes -le 0) { throw 'role' }
            $roleFacts[$name] = @{ path = $roles[$name]; fact = $fact }
        }
        $script:HostPreflightStage = 'vs_inventory'
        $vsInventory = Get-HostRootInventory $vs
        $script:HostPreflightStage = 'sdk_inventory'
        $sdkInventory = Get-HostRootInventory $sdk
        $roots = @($vsInventory, $sdkInventory)
        $script:HostPreflightStage = 'final_identity'
        foreach ($record in $script:HostDirectories.Values) { Assert-HostIdentity $record $false }
        foreach ($record in $script:HostFileRecords) { Assert-HostBudget; Assert-HostIdentity $record $true }
        Assert-HostBudget
        return [ordered]@{ schema = 'morpheus-native-windows-host-toolchain-inventory-v1';
            visual_studio_root = $vs; visual_studio_year = '2022'; msvc_toolset_version = $version;
            windows_sdk_version = '10.0.26100.0'; roots = $roots; roles = $roleFacts;
            upstream_build_revision = 'be1a8f6dcd7df7e46320192c5e2f364e50d79bbf';
            upstream_vs_toolchain_sha256 = 'd13a7b17b81c2084de5a76e907c458ca997a4ddcdef96d3b74c8fcb3aca5f463';
            upstream_setup_toolchain_sha256 = 'f6ae24c99ae0bfc72b093da415615119652f25415155dffb4db22f49b78ca8cf';
            entries = $script:HostCount; total_bytes = $script:HostBytes; limits = $script:HostLimits;
            environment_realized = $false; vcvars_or_compilers_executed = $false;
            host_approval_granted = $false; build_ready = $false; distribution_ready = $false }
    } finally { Close-HostReader }
}

# Read-only fresh-host identity collection. No Python/Git/tool is executed.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$script:HostPreflightStage = 'bootstrap'
$script:HostPreflightBudgetCause = 'unknown'
try {
    if ($Mode -cnotin @('basic', 'host-toolchain')) { throw 'mode' }
    if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or
        $env:RUNNER_OS -ne 'Windows' -or $env:RUNNER_ARCH -ne 'X64' -or
        $env:GITHUB_REPOSITORY -ne 'brk275/Morpheus' -or $env:GITHUB_REF -ne 'refs/heads/main' -or
        $env:GITHUB_EVENT_NAME -ne 'workflow_dispatch' -or $env:GITHUB_ACTOR -ne 'brk275' -or
        $env:GITHUB_TRIGGERING_ACTOR -ne 'brk275') { throw 'guard' }
    function Get-BoundedFileIdentity([string] $LiteralPath) {
        $full = [IO.Path]::GetFullPath($LiteralPath)
        if ($full -notmatch '^[A-Za-z]:\\' -or $full.StartsWith('\\') -or
            $full.Substring(2).Contains(':')) { throw 'path' }
        $item = Get-Item -LiteralPath $full -Force
        if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
            $item.Length -gt 268435456 -or $item.Length -le 0) { throw 'file' }
        $parent = $item.Directory
        while ($null -ne $parent) {
            if ($parent.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'parent' }
            $parent = $parent.Parent
        }
        # Share READ only: no concurrent writer/deletion through this open.
        $stream = [IO.File]::Open($full, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
        try {
            if ($stream.Length -ne $item.Length) { throw 'changed' }
            $digest = [Security.Cryptography.SHA256]::Create()
            try { $sha = [Convert]::ToHexString($digest.ComputeHash($stream)).ToLowerInvariant() }
            finally { $digest.Dispose() }
            if ($stream.Length -ne $item.Length) { throw 'changed' }
        } finally { $stream.Dispose() }
        $current = Get-Item -LiteralPath $full -Force
        if ($current.Length -ne $item.Length -or $current.LastWriteTimeUtc -ne $item.LastWriteTimeUtc -or
            ($current.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'changed' }
        return @{ path = $full; bytes = $item.Length; sha256 = $sha }
    }
    $facts = @{}
    foreach ($name in @('python.exe', 'git.exe')) {
        # Capture the first genuine application selected by the fresh hosted
        # image, with no alias/function or fallback executable invocation.
        $commands = @(Get-Command -Name $name -CommandType Application -All -ErrorAction SilentlyContinue)
        if ($commands.Count -gt 0) { $facts[$name] = Get-BoundedFileIdentity $commands[0].Source }
        else { $facts[$name] = $null }
    }
    if ($Mode -eq 'host-toolchain') {
        $script:HostPreflightStage = 'initialization'
        $metadata = Get-HostReceiptMetadata @{ run_id = $env:GITHUB_RUN_ID; run_attempt = $env:GITHUB_RUN_ATTEMPT;
            source_commit = $env:GITHUB_SHA; image_os = $env:ImageOS; image_version = $env:ImageVersion;
            powershell_version = $PSVersionTable.PSVersion.ToString() }
        $inventory = Get-HostToolchainInventory
        $script:HostPreflightStage = 'serialization'
        foreach ($key in $metadata.Keys) { $inventory[$key] = $metadata[$key] }
        $inventory.bootstrap_tools = $facts
        # Only filenames, sizes and hashes of fixed host roots leave the reader.
        # Never print SDK/tool bytes, ambient environment or native error text.
        $json = $inventory | ConvertTo-Json -Depth 10 -Compress
        if ([Text.Encoding]::UTF8.GetByteCount($json) -gt 33554432) {
            if ($script:HostPreflightBudgetCause -ceq 'unknown') { $script:HostPreflightBudgetCause = 'metadata' }
            throw 'json_size'
        }
        Write-Output $json
        return
    }
    $script:HostPreflightStage = 'serialization'
    [ordered]@{
        schema = 'morpheus-native-windows-host-preflight-v1'
        run_id = $env:GITHUB_RUN_ID
        run_attempt = $env:GITHUB_RUN_ATTEMPT
        source_commit = $env:GITHUB_SHA
        image_os = $env:ImageOS
        image_version = $env:ImageVersion
        powershell_version = $PSVersionTable.PSVersion.ToString()
        tools = $facts
        git_python_versions_not_executed = $true
        msvc_sdk_rust_vpython_cipd_admission = 'UNBOUND'
        native_sources_complete = $false
        build_ready = $false
        distribution_ready = $false
        cache_write = $false
        build_admission_granted = $false
    } | ConvertTo-Json -Depth 5 -Compress | Write-Output
} catch {
    Write-Output (Get-HostPreflightFailureMessage $script:HostPreflightStage $script:HostPreflightBudgetCause)
    exit 1
}
