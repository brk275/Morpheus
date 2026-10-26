# Read-only fresh-host identity collection. No Python/Git/tool is executed.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try {
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
    Write-Output 'MORPHEUS_NATIVE_WINDOWS_PREFLIGHT failed'
    exit 1
}
