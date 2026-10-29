param([string] $ExpectedManifest, [string] $CandidateRoot)
# Reviewed standalone DATA probe. No checkout/import/Add-Type/tool execution.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$handles = [Collections.Generic.List[IntPtr]]::new()
$directories = [Collections.Generic.Dictionary[string, object]]::new([StringComparer]::OrdinalIgnoreCase)
$records = [Collections.Generic.List[object]]::new()
$clock = [Diagnostics.Stopwatch]::StartNew()
$total = [uint64]0
$scratch = [byte[]]::new(52)
$memory = [IntPtr]::Zero
$output = $null
$good = $false
$firstFailure = $null
$fileSubstage = 'open'
$fileOrdinal = 0
$admissionStage = 'metadata'

function Save-Failure([Management.Automation.ErrorRecord] $Failure) {
    if ($null -ne $script:firstFailure) { return }
    $kind = 'other'
    $problem = $Failure.Exception
    for ($depth = 0; $depth -lt 8 -and $null -ne $problem.InnerException; $depth++) {
        $problem = $problem.InnerException
    }
    if ($problem -is [Management.Automation.MethodException]) { $kind = 'binding' }
    elseif ($problem -is [BadImageFormatException]) { $kind = 'bad_image' }
    elseif ($problem -is [UnauthorizedAccessException]) { $kind = 'access' }
    elseif ($problem -is [IO.IOException]) { $kind = 'io' }
    elseif ($problem -is [ArgumentException]) { $kind = 'argument' }
    elseif ($problem -is [InvalidOperationException]) { $kind = 'operation' }
    elseif ($problem -is [NotSupportedException]) { $kind = 'unsupported' }
    elseif ($problem -is [Security.Cryptography.CryptographicException]) { $kind = 'crypto' }
    elseif ($problem -is [Management.Automation.RuntimeException]) { $kind = 'runtime' }
    $line = 0
    if ($null -ne $Failure.InvocationInfo) {
        $candidateLine = $Failure.InvocationInfo.ScriptLineNumber
        if ($candidateLine -is [int] -and $candidateLine -ge 1 -and $candidateLine -le 600) { $line = $candidateLine }
    }
    $script:firstFailure = [ordered]@{ compiler_metadata_failure = 'closed-diagnostic-v1';
        substage = $script:fileSubstage; ordinal = $script:fileOrdinal; category = $kind; line = $line }
}
function Emit-Failure {
    if ($null -ne $script:firstFailure) {
        Write-Output ($script:firstFailure | ConvertTo-Json -Compress)
    }
    Write-Output ('MORPHEUS_COMPILER_FIXTURE_ADMISSION failed stage=' + $script:admissionStage)
}

function Need($Value) { if (-not $Value) { throw 'refused' } }
function Budget {
    Need ($clock.Elapsed.TotalSeconds -le 120 -and $handles.Count -le 1100 -and
        $records.Count -le 1040 -and $script:total -le 1073741824)
}
function Native-Delegate([string] $Name, [Type] $ResultType, [Type[]] $ArgumentTypes) {
    # Same fixed ABI-only delegate declaration as the reviewed bootstrap; no
    # C# source, Add-Type, SDK compiler, supplied method or library is executed.
    $assembly = [Reflection.Emit.AssemblyBuilder]::DefineDynamicAssembly(
        [Reflection.AssemblyName]::new('MorpheusCompilerMetadata_' + [Guid]::NewGuid().ToString('N')),
        [Reflection.Emit.AssemblyBuilderAccess]::Run)
    $module = $assembly.DefineDynamicModule('ReadOnly')
    $type = $module.DefineType('Call', [Reflection.TypeAttributes]'Public,Sealed', [MulticastDelegate])
    $ctor = $type.DefineConstructor([Reflection.MethodAttributes]'Public,HideBySig,RTSpecialName',
        [Reflection.CallingConventions]::Standard, [Type[]]@([object], [IntPtr]))
    $ctor.SetImplementationFlags([Reflection.MethodImplAttributes]::Runtime)
    $invoke = $type.DefineMethod('Invoke', [Reflection.MethodAttributes]'Public,HideBySig,NewSlot,Virtual', $ResultType, $ArgumentTypes)
    $invoke.SetImplementationFlags([Reflection.MethodImplAttributes]::Runtime)
    return [Runtime.InteropServices.Marshal]::GetDelegateForFunctionPointer(
        [Runtime.InteropServices.NativeLibrary]::GetExport($kernel, $Name), $type.CreateTypeInfo().AsType())
}
function Read-Identity([IntPtr] $Handle) {
    Need ($fileType.Invoke($Handle) -eq 1 -and $fileInfo.Invoke($Handle, $memory) -ne 0)
    [Runtime.InteropServices.Marshal]::Copy($memory, $scratch, 0, 52)
    Need (([BitConverter]::ToUInt32($scratch, 0) -band 0x400) -eq 0)
    return ,([byte[]]$scratch.Clone())
}
function Same-Identity([IntPtr] $Handle, [byte[]] $Expected) {
    $now = Read-Identity $Handle
    Need ([BitConverter]::ToUInt32($now, 0) -eq [BitConverter]::ToUInt32($Expected, 0) -and
        [BitConverter]::ToUInt32($now, 28) -eq [BitConverter]::ToUInt32($Expected, 28) -and
        [BitConverter]::ToUInt64($now, 44) -eq [BitConverter]::ToUInt64($Expected, 44) -and
        [BitConverter]::ToUInt64($now, 32) -eq [BitConverter]::ToUInt64($Expected, 32) -and
        [BitConverter]::ToUInt64($now, 20) -eq [BitConverter]::ToUInt64($Expected, 20) -and
        [BitConverter]::ToUInt32($now, 40) -eq [BitConverter]::ToUInt32($Expected, 40))
}
function Open-Read([string] $Path, [bool] $Directory) {
    Budget
    $wide = [Runtime.InteropServices.Marshal]::StringToHGlobalUni('\\?\' + $Path)
    try {
        $access = if ($Directory) { [uint32]128 } else { [uint32]2147483648 }
        $flags = if ($Directory) { [uint32]0x02200000 } else { [uint32]0x00200000 }
        $handle = $open.Invoke($wide, $access, [uint32]1, [IntPtr]::Zero, [uint32]3, $flags, [IntPtr]::Zero)
    } finally { [Runtime.InteropServices.Marshal]::FreeHGlobal($wide) }
    Need ($handle -ne [IntPtr]::new(-1))
    $handles.Add($handle); Budget
    $identity = Read-Identity $handle
    Need ((([BitConverter]::ToUInt32($identity, 0) -band 0x10) -ne 0) -eq $Directory)
    return @{ handle = $handle; identity = $identity }
}
function Chain([string] $Path) {
    $current = $Path.Substring(0, 3)
    foreach ($part in (@('') + @($Path.Substring(3).Split('\') | Where-Object { $_ -ne '' }))) {
        if ($part -ne '') { $current = [IO.Path]::Combine($current, $part) }
        if ($directories.ContainsKey($current)) {
            $entry = $directories[$current]; Same-Identity $entry.handle $entry.identity
        } else { $directories.Add($current, (Open-Read $current $true)) }
    }
}
function Dll-Names([string] $Directory) {
    Chain $Directory
    $names = [Collections.Generic.List[string]]::new()
    $folded = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($entry in [IO.Directory]::EnumerateFileSystemEntries($Directory)) {
        Budget
        $name = [IO.Path]::GetFileName($entry)
        if (-not $name.EndsWith('.dll', [StringComparison]::OrdinalIgnoreCase)) { continue }
        Need ($name -cmatch '^[A-Za-z0-9._-]{1,128}\.[dD][lL][lL]$' -and $folded.Add($name))
        $names.Add($name); Need ($names.Count -le 512)
    }
    $result = $names.ToArray(); [Array]::Sort($result, [StringComparer]::Ordinal)
    return ,$result
}
function Optional-Version([string] $Value) {
    if ([string]::IsNullOrEmpty($Value)) { return $null }
    Need ($Value.Length -le 256 -and $Value -cmatch '^[\x20-\x7e]+$')
    return $Value
}
function File-Fact([string] $Relative) {
    $script:fileSubstage = 'open'
    if ($script:fileOrdinal -lt 1040) { $script:fileOrdinal++ }
    Need ($Relative -cmatch '^[A-Za-z0-9._/-]{1,200}$' -and -not $Relative.Contains('..'))
    $path = [IO.Path]::GetFullPath([IO.Path]::Combine($root, $Relative.Replace('/', '\')))
    Need ($path.StartsWith($root + '\', [StringComparison]::OrdinalIgnoreCase))
    Chain ([IO.Path]::GetDirectoryName($path))
    $record = Open-Read $path $false
    $before = $record.identity
    $size = [uint64][BitConverter]::ToUInt32($before, 32) * 4294967296 + [uint64][BitConverter]::ToUInt32($before, 36)
    Need ($size -gt 0 -and $size -le 268435456 -and $size -le 1073741824 - $script:total)
    $records.Add($record)
    $script:fileSubstage = 'hash'
    $safe = [Microsoft.Win32.SafeHandles.SafeFileHandle]::new($record.handle, $false)
    $stream = [IO.FileStream]::new($safe, [IO.FileAccess]::Read, 1048576, $false)
    $hash = [Security.Cryptography.SHA256]::Create()
    $read = [uint64]0
    try {
        while (($count = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
            Budget; $read += $count; Need ($read -le $size)
            $null = $hash.TransformBlock($buffer, 0, $count, $null, 0)
        }
        $null = $hash.TransformFinalBlock([byte[]]::new(0), 0, 0)
        Need ($read -eq $size)
        $digest = [Convert]::ToHexString($hash.Hash).ToLowerInvariant()
    } catch { Save-Failure $_; throw }
    finally { $stream.Dispose(); $safe.Dispose(); $hash.Dispose() }
    $script:fileSubstage = 'identity'
    Same-Identity $record.handle $before
    $script:total += $size
    $script:fileSubstage = 'assembly_metadata'
    $assembly = $null
    if ($Relative.EndsWith('.dll', [StringComparison]::OrdinalIgnoreCase)) {
        try {
            # GetAssemblyName reads PE metadata without loading the assembly.
            $name = [Reflection.AssemblyName]::GetAssemblyName($path)
            Need ($name.Name -cmatch '^[A-Za-z0-9._-]{1,128}$')
            $token = $name.GetPublicKeyToken()
            $tokenHex = if ($null -eq $token) { '' } else { [Convert]::ToHexString($token).ToLowerInvariant() }
            Need ($tokenHex -cmatch '^(?:[0-9a-f]{16})?$')
            $assembly = [ordered]@{ name = $name.Name; version = $name.Version.ToString(); public_key_token = $tokenHex }
        } catch {
            $problem = $_.Exception
            # Preserve the documented bad-image category even when .NET wraps
            # its native/no-metadata InvalidOperationException as the inner cause.
            for ($depth = 0; $depth -lt 8 -and $problem -isnot [BadImageFormatException] -and
                $null -ne $problem.InnerException; $depth++) {
                $problem = $problem.InnerException
            }
            if ($problem -isnot [BadImageFormatException]) {
                Save-Failure $_; throw 'metadata'
            }
        }
    }
    $script:fileSubstage = 'file_version'
    $version = [Diagnostics.FileVersionInfo]::GetVersionInfo($path)
    $script:fileSubstage = 'identity'
    Same-Identity $record.handle $before
    $script:fileSubstage = 'file_version'
    return [ordered]@{ path = $Relative; bytes = $size; sha256 = $digest; managed_assembly = $assembly;
        file_version = (Optional-Version $version.FileVersion); product_version = (Optional-Version $version.ProductVersion) }
}

# Temporary admission controls. These do not enter the production inventory.
function Hash-AdmissionFile([string] $Path, [uint64] $ExpectedBytes, [string] $ExpectedSha) {
    Budget
    $full = [IO.Path]::GetFullPath($Path)
    Need ($full -cmatch '^[A-Za-z]:\\' -and -not $full.StartsWith('\\') -and -not $full.Substring(2).Contains(':'))
    Chain ([IO.Path]::GetDirectoryName($full))
    $record = Open-Read $full $false
    $before = $record.identity
    $size = [uint64][BitConverter]::ToUInt32($before, 32) * 4294967296 + [uint64][BitConverter]::ToUInt32($before, 36)
    Need ($size -gt 0 -and $size -le 268435456 -and $size -le 1073741824 - $script:total)
    if ($ExpectedBytes -ne 0) { Need ($size -eq $ExpectedBytes) }
    $records.Add($record)
    $safe = [Microsoft.Win32.SafeHandles.SafeFileHandle]::new($record.handle, $false)
    $stream = [IO.FileStream]::new($safe, [IO.FileAccess]::Read, 1048576, $false)
    $hash = [Security.Cryptography.SHA256]::Create()
    $read = [uint64]0
    try {
        while (($count = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
            Budget; $read += $count; Need ($read -le $size)
            $null = $hash.TransformBlock($buffer, 0, $count, $null, 0)
        }
        $null = $hash.TransformFinalBlock([byte[]]::new(0), 0, 0)
        Need ($read -eq $size)
        $digest = [Convert]::ToHexString($hash.Hash).ToLowerInvariant()
    } catch { Save-Failure $_; throw }
    finally { $stream.Dispose(); $safe.Dispose(); $hash.Dispose() }
    Same-Identity $record.handle $before
    if ($ExpectedSha -ne '') { Need ($digest -ceq $ExpectedSha) }
    $script:total += $size
    return $digest
}
function Get-FixedFixtureFailure([string] $Text) {
    # Literal vocabularies from the exact pinned existing fixture, never raw
    # exception text, traceback, filenames, tokens or arbitrary regex captures.
    $stages = @('source_parse', 'source_functions', 'failure_context', 'metadata', 'path_refusals',
        'synthetic_setup', 'reader_initialize', 'inventory', 'digest_assertion', 'write_exclusion',
        'limit_bytes', 'limit_entries', 'limit_deadline', 'limit_handles', 'limit_metadata',
        'aggregate_setup', 'aggregate_before_read', 'aggregate_exact_fit', 'junction_setup',
        'junction_refusal', 'unknown')
    $checks = @('source_parse', 'metadata', 'accepted', 'path', 'path_input', 'path_normalized',
        'path_component', 'path_empty', 'path_length', 'path_drive', 'path_colon', 'path_slash',
        'path_chars', 'path_part_empty', 'path_part_dot', 'path_part_trailingdot',
        'path_part_trailingspace', 'path_part_device', 'path_part_git', 'host', 'type', 'identity',
        'reparse', 'open', 'changed', 'limit', 'file_size', 'text_size', 'depth', 'entries', 'alias',
        'handle_cleanup', 'inventory', 'hash', 'aggregate_before_read', 'aggregate_exact_fit', 'failure_context')
    $categories = @('unexpected', 'access', 'io', 'argument', 'cast', 'member', 'invocation', 'powershell_runtime') +
        @($checks | ForEach-Object { 'check_' + $_ })
    $kinds = @('binding', 'invocation', 'runtime', 'cast', 'argument', 'argument_null',
        'invalid_operation', 'type_load', 'ambiguous', 'ps_operation', 'ps_argument',
        'index', 'null_reference', 'invalid_program', 'unsupported', 'format', 'disposed', 'unknown')
    $pattern = '(?m)^AssertionError: (?:-?[0-9]{1,5} != 0 : )?fixed Windows host reader boundary case failed stage=(' +
        ($stages -join '|') + ') category=(' + ($categories -join '|') +
        ')(?: line=([0-9]{1,4}) kind=(' + ($kinds -join '|') +
        ')(?: site=(ordinary|managed|unknown) origin=(fn|case|unknown))?)?\r?$'
    $regex = [regex]::new($pattern, [Text.RegularExpressions.RegexOptions]::CultureInvariant, [TimeSpan]::FromMilliseconds(100))
    $matches = $regex.Matches($Text)
    $result = [ordered]@{ fixture_failure = 'fixed-existing-case-v1'; stage = 'unknown'; category = 'unexpected' }
    if ($matches.Count -ne 1) { return $result }
    $match = $matches[0]
    # Store only the matching source literal, rather than an arbitrary capture.
    foreach ($known in $stages) { if ($match.Groups[1].Value -ceq $known) { $result.stage = $known; break } }
    foreach ($known in $categories) { if ($match.Groups[2].Value -ceq $known) { $result.category = $known; break } }
    if ($match.Groups[3].Success) {
        if ($result.stage -cnotin @('reader_initialize', 'inventory') -or
            ($result.stage -ceq 'reader_initialize' -and $match.Groups[5].Success) -or
            ($result.stage -ceq 'inventory' -and -not $match.Groups[5].Success)) {
            return [ordered]@{ fixture_failure = 'fixed-existing-case-v1'; stage = 'unknown'; category = 'unexpected' }
        }
        $line = [int]::Parse($match.Groups[3].Value, [Globalization.CultureInfo]::InvariantCulture)
        if ($line -gt 2895) {
            return [ordered]@{ fixture_failure = 'fixed-existing-case-v1'; stage = 'unknown'; category = 'unexpected' }
        }
        $result['line'] = $line
        foreach ($known in $kinds) { if ($match.Groups[4].Value -ceq $known) { $result['kind'] = $known; break } }
        if ($result.stage -ceq 'inventory') {
            foreach ($known in @('ordinary', 'managed', 'unknown')) {
                if ($match.Groups[5].Value -ceq $known) { $result['site'] = $known; break }
            }
            foreach ($known in @('fn', 'case', 'unknown')) {
                if ($match.Groups[6].Value -ceq $known) { $result['origin'] = $known; break }
            }
        }
    }
    return $result
}
function Run-ExistingFixture([string] $Candidate) {
    $commands = @(Get-Command python.exe -CommandType Application -All -ErrorAction Stop)
    Need ($commands.Count -gt 0)
    $python = [IO.Path]::GetFullPath($commands[0].Source)
    # The same genuine application selection as the existing reviewed fixture
    # workflow, now also held read-only for the complete child lifetime.
    $null = Hash-AdmissionFile $python 0 ''
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $python
    $start.WorkingDirectory = $Candidate
    $start.UseShellExecute = $false
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.CreateNoWindow = $true
    foreach ($argument in @('-B', '-m', 'unittest',
        'scripts.tests.test_native_webrtc_windows_inputs.WindowsNativeInputsTests.test_host_inventory_actual_read_handles_paths_and_limits', '-v')) {
        $start.ArgumentList.Add($argument)
    }
    $start.Environment['PYTHONDONTWRITEBYTECODE'] = '1'
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $start
    $started = $false
    $childClock = [Diagnostics.Stopwatch]::new()
    $outBuffer = [byte[]]::new(8192); $errBuffer = [byte[]]::new(8192)
    $outBytes = 0; $errBytes = 0
    $outCapture = [IO.MemoryStream]::new(); $errCapture = [IO.MemoryStream]::new()
    Budget
    $clock.Stop()
    try {
        $started = $process.Start(); Need $started
        $childClock.Start()
        $outStream = $process.StandardOutput.BaseStream
        $errStream = $process.StandardError.BaseStream
        $outTask = $outStream.ReadAsync($outBuffer, 0, $outBuffer.Length)
        $errTask = $errStream.ReadAsync($errBuffer, 0, $errBuffer.Length)
        while ($null -ne $outTask -or $null -ne $errTask -or -not $process.HasExited) {
            Need ($childClock.Elapsed.TotalSeconds -le 60)
            if ($null -ne $outTask -and $outTask.IsCompleted) {
                $count = $outTask.GetAwaiter().GetResult()
                $outBytes += $count; Need ($outBytes -le 65536)
                $outCapture.Write($outBuffer, 0, $count)
                $outTask = if ($count -eq 0) { $null } else { $outStream.ReadAsync($outBuffer, 0, $outBuffer.Length) }
            }
            if ($null -ne $errTask -and $errTask.IsCompleted) {
                $count = $errTask.GetAwaiter().GetResult()
                $errBytes += $count; Need ($errBytes -le 65536)
                $errCapture.Write($errBuffer, 0, $count)
                $errTask = if ($count -eq 0) { $null } else { $errStream.ReadAsync($errBuffer, 0, $errBuffer.Length) }
            }
            # Retain only bounded private bytes. Unknown content is never
            # forwarded; only the finite source-fixed result below may leave.
            if (-not $process.HasExited) { $null = $process.WaitForExit(10) }
        }
        $utf8 = [Text.UTF8Encoding]::new($false, $true)
        $stderr = $utf8.GetString($errCapture.ToArray())
        if ($process.ExitCode -ne 0) {
            Write-Output ((Get-FixedFixtureFailure $stderr) | ConvertTo-Json -Compress)
            throw 'fixture'
        }
        $method = 'test_host_inventory_actual_read_handles_paths_and_limits'
        $qualified = 'scripts.tests.test_native_webrtc_windows_inputs.WindowsNativeInputsTests.' + $method
        $successPattern = '\A' + [regex]::Escape($method + ' (' + $qualified + ') ... ok') +
            '\r?\n\r?\n-{70}\r?\nRan 1 test in [0-9]{1,2}\.[0-9]{1,6}s\r?\n\r?\nOK\r?\n\z'
        $successRegex = [regex]::new($successPattern, [Text.RegularExpressions.RegexOptions]::CultureInvariant, [TimeSpan]::FromMilliseconds(100))
        Need ($outBytes -eq 0 -and $successRegex.IsMatch($stderr))
    } catch { Save-Failure $_; throw }
    finally {
        try {
            if ($started -and -not $process.HasExited) {
                $process.Kill($true)
                Need ($process.WaitForExit(5000))
            }
        } finally {
            try { $process.Dispose() }
            finally {
                try { $outCapture.Dispose(); $errCapture.Dispose() } finally { $clock.Start() }
            }
        }
    }
}

try {
    Write-Output '{"compiler_metadata_phase":"guards","status":"started"}'
    Need ($env:GITHUB_ACTIONS -ceq 'true' -and $env:RUNNER_ENVIRONMENT -ceq 'github-hosted' -and
        $env:GITHUB_REPOSITORY -ceq 'brk275/Morpheus' -and $env:GITHUB_EVENT_NAME -ceq 'workflow_dispatch' -and
        $env:GITHUB_REF -ceq 'refs/heads/codex/windows-owned-two-range-fixture-20261029' -and
        $env:GITHUB_ACTOR -ceq 'brk275' -and $env:GITHUB_TRIGGERING_ACTOR -ceq 'brk275' -and
        $env:RUNNER_OS -ceq 'Windows' -and $env:RUNNER_ARCH -ceq 'X64')
    Write-Output '{"compiler_metadata_phase":"hooks","status":"started"}'
    # Read only fixed hook names; never enumerate/print ambient environment.
    foreach ($hook in @('DOTNET_STARTUP_HOOKS', 'DOTNET_ADDITIONAL_DEPS', 'DOTNET_SHARED_STORE', 'CORECLR_PROFILER', 'COR_PROFILER')) {
        Need ([string]::IsNullOrEmpty([Environment]::GetEnvironmentVariable($hook)))
    }
    foreach ($hook in @('CORECLR_ENABLE_PROFILING', 'COR_ENABLE_PROFILING')) {
        $value = [Environment]::GetEnvironmentVariable($hook)
        Need ([string]::IsNullOrEmpty($value) -or $value -ceq '0')
    }
    Write-Output '{"compiler_metadata_phase":"paths","status":"started"}'
    Need ([Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture -eq [Runtime.InteropServices.Architecture]::X64)
    $root = [IO.Path]::GetFullPath($PSHOME)
    Need ([string]::Equals($root, 'C:\Program Files\PowerShell\7', [StringComparison]::OrdinalIgnoreCase))
    $system = [IO.Path]::GetFullPath($env:SystemRoot)
    Need ($system -cmatch '^[A-Za-z]:\\Windows$')
    Write-Output '{"compiler_metadata_phase":"native","status":"started"}'
    $kernel = [Runtime.InteropServices.NativeLibrary]::Load([IO.Path]::Combine($system, 'System32', 'kernel32.dll'))
    $open = Native-Delegate 'CreateFileW' ([IntPtr]) ([Type[]]@([IntPtr], [uint32], [uint32], [IntPtr], [uint32], [uint32], [IntPtr]))
    $fileType = Native-Delegate 'GetFileType' ([uint32]) ([Type[]]@([IntPtr]))
    $fileInfo = Native-Delegate 'GetFileInformationByHandle' ([int]) ([Type[]]@([IntPtr], [IntPtr]))
    $close = Native-Delegate 'CloseHandle' ([int]) ([Type[]]@([IntPtr]))
    $buffer = [byte[]]::new(1048576)
    $memory = [Runtime.InteropServices.Marshal]::AllocHGlobal(52)
    Write-Output '{"compiler_metadata_phase":"dll_inventory","status":"started"}'
    $rootNames = Dll-Names $root; $refRoot = [IO.Path]::Combine($root, 'ref'); $refNames = Dll-Names $refRoot
    Need ($rootNames.Count -gt 0 -and $refNames.Count -gt 0)
    foreach ($required in @('System.Management.Automation.dll', 'Microsoft.PowerShell.Commands.Utility.dll',
        'Microsoft.CodeAnalysis.dll', 'Microsoft.CodeAnalysis.CSharp.dll', 'System.Private.CoreLib.dll',
        'System.Collections.Immutable.dll', 'System.Reflection.Metadata.dll')) { Need ($rootNames -ccontains $required) }
    $files = [Collections.Generic.List[object]]::new()
    Write-Output '{"compiler_metadata_phase":"version_checks","status":"started"}'
    Need ([string]::Equals([Environment]::ProcessPath, [IO.Path]::Combine($root, 'pwsh.exe'), [StringComparison]::OrdinalIgnoreCase))
    Need ([string]::Equals([Reflection.Assembly]::GetEntryAssembly().Location,
        [IO.Path]::Combine($root, 'pwsh.dll'), [StringComparison]::OrdinalIgnoreCase))
    Need ([string]::Equals([PSObject].Assembly.Location,
        [IO.Path]::Combine($root, 'System.Management.Automation.dll'), [StringComparison]::OrdinalIgnoreCase))
    $coreLocation = [object].Assembly.Location
    $coreInRoot = $coreLocation.StartsWith($root + '\', [StringComparison]::OrdinalIgnoreCase)
    $psVersion = $PSVersionTable.PSVersion.ToString()
    Need ($psVersion -cmatch '^[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]{1,64})?$' -and $PSVersionTable.PSEdition -ceq 'Core')
    $framework = [Runtime.InteropServices.RuntimeInformation]::FrameworkDescription
    Need ($framework.Length -le 128 -and $framework -cmatch '^[A-Za-z0-9 .+_-]+$')
    # Identity header is explicitly incomplete: no bundle/ref verification or admission.
    Write-Output (([ordered]@{ schema = 'morpheus-powershell-compiler-metadata-unverified-header-v1';
        verification = 'UNVERIFIED'; powershell_version = $psVersion; powershell_edition = 'Core';
        architecture = 'X64'; framework = $framework; core_runtime_in_fixed_root = $coreInRoot;
        process_and_sma_fixed_root_checks = $true; compiler_admission_granted = $false;
        build_ready = $false }) | ConvertTo-Json -Compress)
    Write-Output '{"compiler_metadata_phase":"bundle_hashes","status":"started"}'
    foreach ($name in $rootNames) { $files.Add((File-Fact $name)) }
    Write-Output '{"compiler_metadata_phase":"ref_hashes","status":"started"}'
    foreach ($name in $refNames) { $files.Add((File-Fact ('ref/' + $name))) }
    Write-Output '{"compiler_metadata_phase":"fixed_files","status":"started"}'
    foreach ($name in @('pwsh.exe', 'pwsh.deps.json', 'pwsh.runtimeconfig.json',
        'Modules/Microsoft.PowerShell.Utility/Microsoft.PowerShell.Utility.psd1')) { $files.Add((File-Fact $name)) }
    $licenses = [Collections.Generic.List[object]]::new()
    foreach ($name in @('LICENSE.txt', 'ThirdPartyNotices.txt')) {
        if ([IO.File]::Exists([IO.Path]::Combine($root, $name))) { $licenses.Add((File-Fact $name)) }
        else { $licenses.Add([ordered]@{ path = $name; present = $false }) }
    }
    Write-Output '{"compiler_metadata_phase":"snapshot_and_close","status":"started"}'
    $againRoot = Dll-Names $root; $againRef = Dll-Names $refRoot
    Need ($againRoot.Count -eq $rootNames.Count -and $againRef.Count -eq $refNames.Count)
    for ($i = 0; $i -lt $rootNames.Count; $i++) { Need ($rootNames[$i] -ceq $againRoot[$i]) }
    for ($i = 0; $i -lt $refNames.Count; $i++) { Need ($refNames[$i] -ceq $againRef[$i]) }
    foreach ($record in $records) { Budget; Same-Identity $record.handle $record.identity }
    foreach ($record in $directories.Values) { Budget; Same-Identity $record.handle $record.identity }
    $output = [ordered]@{ schema = 'morpheus-powershell-compiler-metadata-v1'; powershell_version = $psVersion;
        powershell_edition = 'Core'; architecture = 'X64'; framework = $framework;
        core_runtime_in_fixed_root = $coreInRoot; root_dll_names = $rootNames; default_reference_names = $refNames;
        files = @($files.ToArray()); licenses = @($licenses.ToArray()); total_bytes = $script:total;
        add_type_invoked = $false; roslyn_compilation_invoked = $false; candidate_code_imported = $false;
        sdk_tool_executed = $false; compiler_admission_granted = $false; build_ready = $false }
    $script:admissionStage = 'compiler_inputs'
    $workspace = [IO.Path]::GetFullPath($env:GITHUB_WORKSPACE)
    $expectedPath = [IO.Path]::GetFullPath($ExpectedManifest)
    $candidate = [IO.Path]::GetFullPath($CandidateRoot)
    Need ([string]::Equals($expectedPath, [IO.Path]::Combine($workspace, 'guard', 'compiler-host-inputs.json'), [StringComparison]::OrdinalIgnoreCase))
    Need ([string]::Equals($candidate, [IO.Path]::Combine($workspace, 'candidate'), [StringComparison]::OrdinalIgnoreCase))
    $manifestSha = '771fcbae07497dc9b1b9c7ee3efd8a8582988c163e730d2664798383000fb617'
    $null = Hash-AdmissionFile $expectedPath 96520 $manifestSha
    $expected = [Text.Encoding]::UTF8.GetString([IO.File]::ReadAllBytes($expectedPath)) | ConvertFrom-Json -AsHashtable
    Need ($expected.schema -ceq 'morpheus-owned-compiler-host-inputs-v1' -and
        $psVersion -ceq $expected.powershell_version -and $framework -ceq $expected.framework -and $coreInRoot)
    Need ($rootNames.Count -eq 301 -and $refNames.Count -eq 167 -and $expected.files.Count -eq 474)
    for ($i = 0; $i -lt $rootNames.Count; $i++) { Need ($rootNames[$i] -ceq $expected.root_dll_names[$i]) }
    for ($i = 0; $i -lt $refNames.Count; $i++) { Need ($refNames[$i] -ceq $expected.default_reference_names[$i]) }
    $actualFiles = [Collections.Generic.Dictionary[string, object]]::new([StringComparer]::Ordinal)
    foreach ($fact in (@($files.ToArray()) + @($licenses.ToArray()))) { $actualFiles.Add($fact.path, $fact) }
    Need ($actualFiles.Count -eq 474)
    foreach ($fact in $expected.files) {
        Need ($actualFiles.ContainsKey($fact.path))
        $actual = $actualFiles[$fact.path]
        Need ($actual.bytes -eq $fact.bytes -and $actual.sha256 -ceq $fact.sha256)
    }
    $script:admissionStage = 'candidate_source'
    $null = Hash-AdmissionFile ([IO.Path]::Combine($candidate, 'scripts', 'native_webrtc', 'windows_preflight.ps1')) 168702 'f578c3a486091dbc74225d325e428412653a63bbe0a4e038adad675563b843b1'
    $null = Hash-AdmissionFile ([IO.Path]::Combine($candidate, 'scripts', 'tests', 'test_native_webrtc_windows_inputs.py')) 188912 '65cc87ed705cea88d885ded2453c0fe053c43c34a60d4a827a2588aeb3df754d'
    $script:admissionStage = 'fixture'
    # Only metadata work uses this clock. The separately bounded existing
    # fixture child retains its original45-second PowerShell-case timeout.
    # The unchanged five-minute workflow bounds the whole lock lifetime.
    Budget
    Run-ExistingFixture $candidate
    $script:admissionStage = 'recheck'
    $againRoot = Dll-Names $root; $againRef = Dll-Names $refRoot
    Need ($againRoot.Count -eq $rootNames.Count -and $againRef.Count -eq $refNames.Count)
    for ($i = 0; $i -lt $rootNames.Count; $i++) { Need ($rootNames[$i] -ceq $againRoot[$i]) }
    for ($i = 0; $i -lt $refNames.Count; $i++) { Need ($refNames[$i] -ceq $againRef[$i]) }
    foreach ($record in $records) { Budget; Same-Identity $record.handle $record.identity }
    foreach ($record in $directories.Values) { Budget; Same-Identity $record.handle $record.identity }
    $output = [ordered]@{ schema = 'morpheus-reviewed-windows-compiler-fixture-admission-v1';
        candidate_source = '2504b9ed104fbd0cdf892893eb7f534d5d337f11';
        compiler_input_manifest_sha256 = $manifestSha; powershell_version = $psVersion;
        framework = $framework; compiler_tool_files = 474; fixture_passed = $true;
        fixture_compiler_invoked = $true; wrapper_compilation_invoked = $false;
        external_sdk_tool_executed = $false; full_inventory_tested = $false;
        main_entry_tested = $false; native_build_admission_granted = $false; build_ready = $false }
    $good = $true
} catch { Save-Failure $_; $good = $false }
finally {
    try {
        for ($i = $handles.Count - 1; $i -ge 0; $i--) {
            $handle = $handles[$i]; $handles[$i] = [IntPtr]::Zero
            try {
                if ($handle -ne [IntPtr]::Zero -and $close.Invoke($handle) -eq 0) { $good = $false }
            } catch { Save-Failure $_; $good = $false }
        }
    } finally {
        try {
            if ($memory -ne [IntPtr]::Zero) { [Runtime.InteropServices.Marshal]::FreeHGlobal($memory); $memory = [IntPtr]::Zero }
        } catch { Save-Failure $_; $good = $false }
    }
}
if (-not $good) { Emit-Failure; exit 1 }
try {
    $json = $output | ConvertTo-Json -Depth 7 -Compress
    Need ([Text.Encoding]::UTF8.GetByteCount($json) -le 2097152)
    Write-Output $json
    exit 0
} catch { Save-Failure $_; Emit-Failure; exit 1 }
