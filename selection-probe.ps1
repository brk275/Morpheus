param([string] $CandidateSource)
# TEMPORARY selection observation only. Never merge or run an inventory.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$stage = 'source_parse'
try {
    $tokens = $null; $errors = $null
    $ast = [Management.Automation.Language.Parser]::ParseFile($CandidateSource, [ref]$tokens, [ref]$errors)
    if ($errors.Count -ne 0) { throw 'source_parse' }
    $stage = 'source_functions'
    $definitions = $ast.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] }, $true)
    $needle = '        $owned = New-HostOwnedTraversal' + "`n"
    $replacement = $needle + "        if (`$null -ne `$owned) { return 'owned' }`n        return 'fallback'`n"
    $changed = 0
    $pieces = foreach ($definition in $definitions) {
        $text = $definition.Extent.Text
        if ($definition.Name -ceq 'Get-HostToolchainInventory') {
            if (($text.Length - $text.Replace($needle, '').Length) -ne $needle.Length) { throw 'source_functions' }
            $text = $text.Replace($needle, $replacement)
            $changed++
        }
        $text
    }
    if ($changed -ne 1) { throw 'source_functions' }
    # The exact production functions share this script scope. Initialize captures
    # the modified toolchain function itself; no owner/provenance slot is spoofed.
    . ([ScriptBlock]::Create(($pieces -join "`n")))
    $stage = 'bootstrap'
    $script:HostPreflightStage = 'bootstrap'
    $script:HostPreflightBudgetCause = 'unknown'
    $facts = @{}
    foreach ($name in @('python.exe', 'git.exe')) {
        # Capture the first genuine application selected by the fresh hosted
        # image, with no alias/function or fallback executable invocation.
        $commands = @(Get-Command -Name $name -CommandType Application -All -ErrorAction SilentlyContinue)
        if ($commands.Count -gt 0) { $facts[$name] = Get-BoundedFileIdentity $commands[0].Source }
        else { $facts[$name] = $null }
    }
    $stage = 'metadata'
        $metadata = Get-HostReceiptMetadata @{ run_id = $env:GITHUB_RUN_ID; run_attempt = $env:GITHUB_RUN_ATTEMPT;
            source_commit = $env:GITHUB_SHA; image_os = $env:ImageOS; image_version = $env:ImageVersion;
            powershell_version = $PSVersionTable.PSVersion.ToString() }
    $stage = 'selection'
    $selected = @(Get-HostToolchainInventory)
    if ($selected.Count -ne 1 -or $selected[0] -isnot [string] -or
        ($selected[0] -cne 'owned' -and $selected[0] -cne 'fallback')) { throw 'selection' }
    $stage = 'cleanup'
    if ($script:HostInfoMemory -ne [IntPtr]::Zero -or $script:HostHandles.Count -ne 0 -or
        $script:HostCount -ne 0 -or $script:HostBytes -ne [uint64]0) { throw 'cleanup' }
    Write-Output ('MORPHEUS_WINDOWS_SELECTION ' + $selected[0])
    exit 0
} catch {
    # No error text, paths, type names, account values or arbitrary payloads.
    $closed = 'unknown'
    foreach ($known in @('source_parse', 'source_functions', 'bootstrap', 'metadata', 'selection', 'cleanup')) {
        if ([string]::Equals($stage, $known, [StringComparison]::Ordinal)) { $closed = $known; break }
    }
    Write-Output ('MORPHEUS_WINDOWS_SELECTION failed stage=' + $closed)
    exit 1
}
