# UI Automation bridge for the installed Windows keyboard check (Windows PowerShell 5.1).
#
# scripts/windows_package_ui.py owns the application process and all decisions.
# This bridge only answers single-line JSON commands on standard input with one
# JSON line each: a bounded snapshot of the owned window's UI Automation tree,
# focusing one snapshot element, the focused element's owner, and real keyboard
# input. Every reply carries fixed tokens or data the Python driver keeps in
# memory; it never prints exception text, values of inputs or anything else.
param([Parameter(Mandatory=$true)][int]$ProcessId, [Parameter(Mandatory=$true)][long]$WindowHandle)
$ErrorActionPreference = 'Stop'
# The same disposable hosted-runner conditions as the Python launcher. A
# mismatch exits without output; the launcher reports the fixed driver failure.
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or
    $env:RUNNER_OS -ne 'Windows' -or $env:ImageOS -ne 'win25') { exit 2 }
if (-not [Environment]::UserInteractive) { exit 3 }
if ($ProcessId -le 0 -or $WindowHandle -le 0) { exit 4 }

Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class MorpheusInput {
    [StructLayout(LayoutKind.Sequential)]
    public struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Sequential)]
    public struct MOUSEINPUT { public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Explicit)]
    public struct INPUTUNION { [FieldOffset(0)] public MOUSEINPUT mi; [FieldOffset(0)] public KEYBDINPUT ki; }
    [StructLayout(LayoutKind.Sequential)]
    public struct INPUT { public uint type; public INPUTUNION u; }
    [DllImport("user32.dll", SetLastError = true)] public static extern uint SendInput(uint count, INPUT[] inputs, int size);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr window);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
    // One real keyboard transition through the normal input path (no window messages).
    public static uint Key(ushort virtualKey, bool release) {
        INPUT[] inputs = new INPUT[1];
        inputs[0].type = 1;
        inputs[0].u.ki.wVk = virtualKey;
        inputs[0].u.ki.dwFlags = release ? 2u : 0u;
        return SendInput(1, inputs, Marshal.SizeOf(typeof(INPUT)));
    }
    public static uint WindowProcess(IntPtr window) { uint owner = 0; GetWindowThreadProcessId(window, out owner); return owner; }
}
"@

$utf8 = New-Object System.Text.UTF8Encoding($false)
[Console]::OutputEncoding = $utf8
[Console]::InputEncoding = $utf8
$script:elements = New-Object 'System.Collections.Generic.List[System.Windows.Automation.AutomationElement]'
$MaxNodes = 5000
$Keys = @{ Return = [uint16]0x0D; Space = [uint16]0x20; Tab = [uint16]0x09; Escape = [uint16]0x1B }
$Shift = [uint16]0x10

function Reply($object) {
    [Console]::Out.WriteLine((ConvertTo-Json -InputObject $object -Compress -Depth 6))
    [Console]::Out.Flush()
}

function Fail([string]$code) { Reply @{ ok = $false; error = $code } }

function Owned($element) {
    try { return ([int]$element.Current.ProcessId -eq $ProcessId) } catch { return $false }
}

function TypeName($element) {
    $name = [string]$element.Current.ControlType.ProgrammaticName
    if ($name.StartsWith('ControlType.')) { $name = $name.Substring(12) }
    return $name.ToLowerInvariant()
}

# Breadth-first raw-view walk of the owned window only. A node whose owner,
# name, role or children could not be read is reported with error=true and no
# children, so the driver treats that pass as incomplete instead of as a tree
# that happens to lack the node. Unowned nodes are reported without a name and
# never expanded. Element indexes stay valid until the next snapshot.
function Snapshot {
    $script:elements.Clear()
    try {
        $root = [System.Windows.Automation.AutomationElement]::FromHandle([IntPtr]$WindowHandle)
    } catch { Fail 'root_unavailable'; return }
    if (-not (Owned $root) -or [long]$root.Current.NativeWindowHandle -ne $WindowHandle) { Fail 'unowned_root'; return }
    $walker = [System.Windows.Automation.TreeWalker]::RawViewWalker
    $nodes = New-Object System.Collections.Generic.List[object]
    $queue = New-Object 'System.Collections.Generic.Queue[object]'
    $queue.Enqueue(@($root, -1))
    $bounded = $false
    while ($queue.Count -gt 0) {
        $pair = $queue.Dequeue()
        $element = $pair[0]
        if ($script:elements.Count -ge $MaxNodes) { $bounded = $true; break }
        $id = $script:elements.Count
        $script:elements.Add($element)
        $node = [ordered]@{ id = $id; parent = [int]$pair[1]; pid = -1; name = ''; type = 'other'
                            password = $false; focused = $false; error = $false }
        try {
            $node.pid = [int]$element.Current.ProcessId
            if ($node.pid -eq $ProcessId) {
                $node.name = [string]$element.Current.Name
                $node.type = TypeName $element
                $node.password = [bool]$element.Current.IsPassword
                $node.focused = [bool]$element.Current.HasKeyboardFocus
                $child = $walker.GetFirstChild($element)
                while ($child -ne $null) {
                    $queue.Enqueue(@($child, $id))
                    $child = $walker.GetNextSibling($child)
                }
            }
        } catch { $node.error = $true }
        $nodes.Add([pscustomobject]$node)
    }
    Reply @{ ok = $true; nodes = $nodes.ToArray(); bounded = $bounded }
}

function FocusElement([int]$id) {
    if ($id -lt 0 -or $id -ge $script:elements.Count) { Fail 'invalid_id'; return }
    $element = $script:elements[$id]
    try {
        # Ownership is rechecked on the live element, not the snapshot copy.
        if (-not (Owned $element)) { Fail 'unowned'; return }
        $element.SetFocus()
        Reply @{ ok = $true }
    } catch { Fail 'element_unavailable' }
}

function Focused {
    try {
        $element = [System.Windows.Automation.AutomationElement]::FocusedElement
        if ($element -eq $null) { Reply @{ ok = $true; present = $false; owned = $false; name = '' }; return }
        $owned = Owned $element
        $name = ''
        if ($owned) { $name = [string]$element.Current.Name }
        Reply @{ ok = $true; present = $true; owned = $owned; name = $name }
    } catch { Fail 'element_unavailable' }
}

function ForegroundOwned {
    $handle = [MorpheusInput]::GetForegroundWindow()
    if ($handle.ToInt64() -ne $WindowHandle) { return $false }
    return ([MorpheusInput]::WindowProcess($handle) -eq [uint32]$ProcessId)
}

# Keyboard input is desktop-wide by nature, so the exact owned window handle
# and process must be the foreground window immediately before every SendInput.
function SendKey([string]$name) {
    $shift = $false
    if ($name -eq 'ShiftTab') { $name = 'Tab'; $shift = $true }
    if (-not $Keys.ContainsKey($name)) { Fail 'invalid_key'; return }
    $recovered = $false
    if (-not (ForegroundOwned)) {
        if (-not [MorpheusInput]::IsWindow([IntPtr]$WindowHandle)) { Fail 'window_gone'; return }
        [void][MorpheusInput]::SetForegroundWindow([IntPtr]$WindowHandle)
        Start-Sleep -Milliseconds 150
        if (-not (ForegroundOwned)) { Fail 'foreground_unowned'; return }
        $recovered = $true
    }
    $sent = [uint32]0
    $expected = [uint32]2
    if ($shift) { $expected = 4; $sent += [MorpheusInput]::Key($Shift, $false) }
    $sent += [MorpheusInput]::Key($Keys[$name], $false)
    $sent += [MorpheusInput]::Key($Keys[$name], $true)
    if ($shift) { $sent += [MorpheusInput]::Key($Shift, $true) }
    if ($sent -ne $expected) { Fail 'input_rejected'; return }
    Reply @{ ok = $true; recovered = $recovered }
}

try {
    Reply @{ ok = $true; ready = $true }
    while ($true) {
        $line = [Console]::In.ReadLine()
        if ($line -eq $null) { break }
        if ($line.Length -gt 256) { Fail 'invalid_command'; continue }
        try { $command = ConvertFrom-Json -InputObject $line } catch { Fail 'invalid_command'; continue }
        switch ([string]$command.op) {
            'snapshot' { Snapshot }
            'focus' { FocusElement ([int]$command.id) }
            'focused' { Focused }
            'key' { SendKey ([string]$command.key) }
            'exit' { exit 0 }
            default { Fail 'invalid_command' }
        }
    }
    exit 0
} catch {
    # Never echo the exception: it can carry element names or system text.
    try { Fail 'bridge_error' } catch { }
    exit 1
}
