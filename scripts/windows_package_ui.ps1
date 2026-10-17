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
    static void Transition(INPUT[] inputs, int index, ushort virtualKey, bool release) {
        inputs[index].type = 1;
        inputs[index].u.ki.wVk = virtualKey;
        inputs[index].u.ki.dwFlags = release ? 2u : 0u;
    }
    // The complete key chord in ONE native SendInput call. Windows inserts the
    // array serially, without interleaving other input, so the proof of the
    // owned foreground window taken immediately before this call covers every
    // transition. Returns the number of transitions the system accepted.
    public static uint Chord(ushort virtualKey, bool shift) {
        int count = shift ? 4 : 2;
        INPUT[] inputs = new INPUT[count];
        int index = 0;
        if (shift) { Transition(inputs, index++, 0x10, false); }
        Transition(inputs, index++, virtualKey, false);
        Transition(inputs, index++, virtualKey, true);
        if (shift) { Transition(inputs, index++, 0x10, true); }
        return SendInput((uint)count, inputs, Marshal.SizeOf(typeof(INPUT)));
    }
    // Every release a partially accepted chord still needs, in ONE native call.
    public static uint ReleaseAll(ushort[] virtualKeys) {
        INPUT[] inputs = new INPUT[virtualKeys.Length];
        for (int index = 0; index < virtualKeys.Length; index++) { Transition(inputs, index, virtualKeys[index], true); }
        return SendInput((uint)virtualKeys.Length, inputs, Marshal.SizeOf(typeof(INPUT)));
    }
    public static uint WindowProcess(IntPtr window) { uint owner = 0; GetWindowThreadProcessId(window, out owner); return owner; }
}
"@

$utf8 = New-Object System.Text.UTF8Encoding($false)
[Console]::OutputEncoding = $utf8
[Console]::InputEncoding = $utf8
$script:elements = New-Object 'System.Collections.Generic.List[System.Windows.Automation.AutomationElement]'
$MaxNodes = 5000
$MaxName = 256
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
#
# Every bound is enforced before the growth it limits: the total of recorded
# and queued elements never exceeds $MaxNodes, so sibling enumeration stops
# before a wide or endless sibling chain can grow the queue; a name longer than
# $MaxName is cut before it is kept; a bounded or incomplete walk is reported
# and the driver treats it as not-a-tree.
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
    while ($queue.Count -gt 0 -and -not $bounded) {
        $pair = $queue.Dequeue()
        $element = $pair[0]
        $id = $script:elements.Count
        $script:elements.Add($element)
        $node = [ordered]@{ id = $id; parent = [int]$pair[1]; pid = -1; name = ''; type = 'other'
                            password = $false; focused = $false; error = $false; truncated = $false }
        try {
            $node.pid = [int]$element.Current.ProcessId
            if ($node.pid -eq $ProcessId) {
                $name = [string]$element.Current.Name
                if ($name.Length -gt $MaxName) { $name = $name.Substring(0, $MaxName); $node.truncated = $true }
                $node.name = $name
                $node.type = TypeName $element
                $node.password = [bool]$element.Current.IsPassword
                $node.focused = [bool]$element.Current.HasKeyboardFocus
                $child = $walker.GetFirstChild($element)
                while ($child -ne $null) {
                    if ($script:elements.Count + $queue.Count -ge $MaxNodes) { $bounded = $true; break }
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
        $truncated = $false
        if ($owned) {
            $name = [string]$element.Current.Name
            if ($name.Length -gt $MaxName) { $name = $name.Substring(0, $MaxName); $truncated = $true }
        }
        Reply @{ ok = $true; present = $true; owned = $owned; name = $name; truncated = $truncated }
    } catch { Fail 'element_unavailable' }
}

function ForegroundOwned {
    $handle = [MorpheusInput]::GetForegroundWindow()
    if ($handle.ToInt64() -ne $WindowHandle) { return $false }
    return ([MorpheusInput]::WindowProcess($handle) -eq [uint32]$ProcessId)
}

# Keyboard input is desktop-wide by nature. The exact owned window handle and
# process must be the foreground window immediately before the single native
# SendInput call that inserts the whole chord; there is no second call whose
# target could have changed in between.
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
    $expected = [uint32]2
    if ($shift) { $expected = [uint32]4 }
    $sent = [MorpheusInput]::Chord($Keys[$name], $shift)
    if ($sent -ne $expected) {
        # The system accepted only a prefix of the chord. Every key that prefix
        # left pressed is released in ONE native call, immediately after one
        # fresh proof of the owned foreground window; a changed foreground gets
        # nothing. The run fails either way.
        $held = New-Object 'System.Collections.Generic.List[uint16]'
        if ($shift) {
            if ($sent -eq 2) { $held.Add($Keys[$name]) }
            if ($sent -ge 1 -and $sent -le 3) { $held.Add($Shift) }
        } elseif ($sent -eq 1) { $held.Add($Keys[$name]) }
        if ($held.Count -gt 0 -and (ForegroundOwned)) { [void][MorpheusInput]::ReleaseAll($held.ToArray()) }
        Fail 'input_rejected'
        return
    }
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
