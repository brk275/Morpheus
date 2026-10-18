// Accessibility and keyboard bridge for the installed macOS keyboard check.
//
// scripts/macos_package_ui.py owns the application process, the profile and
// every decision. This bridge only answers single-line JSON commands on
// standard input with one JSON line each: its own no-prompt permission state,
// attachment to one owned process, the count of on-screen windows that
// process owns, a bounded snapshot of that process's accessibility tree,
// focusing one snapshot element, the focused element's owner, real keyboard
// transitions posted directly to the owned process, and the window's close
// button. It never requests or changes a permission, never reads a text
// field's value, and never prints exception text, paths or anything but the
// reply objects below.
//
// The package smoke compiles this file with the runner's system Swift
// toolchain before the application starts; the binary stays on the ephemeral
// runner.

import AppKit
import ApplicationServices
import CoreGraphics
import Foundation

let maxNodes = 5000
let maxName = 256
let maxLine = 256
let keyCodes: [String: CGKeyCode] = ["Return": 0x24, "Tab": 0x30, "Space": 0x31, "Escape": 0x35]
let shiftCode: CGKeyCode = 0x38
let staticTextRole = "AXStaticText"
let runLoopTurn: TimeInterval = 0.05

struct Target {
    let pid: pid_t
    let executable: String
    let bundle: String
}

var target: Target? = nil
var elements: [AXUIElement] = []

// Fixed-token output only. A failed serialization is itself a fixed token.
func emit(_ text: String) {
    if let data = (text + "\n").data(using: .utf8) {
        FileHandle.standardOutput.write(data)
    }
}

func fail(_ code: String) {
    emit("{\"error\":\"\(code)\",\"ok\":false}")
}

func reply(_ object: [String: Any]) {
    var payload = object
    payload["ok"] = true
    guard JSONSerialization.isValidJSONObject(payload),
          let data = try? JSONSerialization.data(withJSONObject: payload, options: [.sortedKeys]),
          let text = String(data: data, encoding: .utf8) else {
        fail("bridge_error")
        return
    }
    emit(text)
}

// The same disposable hosted-runner conditions as the Python launcher. A
// mismatch exits without output; the launcher reports the fixed failure.
let environment = ProcessInfo.processInfo.environment
if environment["GITHUB_ACTIONS"] != "true" || environment["RUNNER_ENVIRONMENT"] != "github-hosted"
    || environment["RUNNER_OS"] != "macOS" || environment["ImageOS"] != "macos15" {
    exit(2)
}

func attribute(_ element: AXUIElement, _ name: String) -> CFTypeRef? {
    var value: CFTypeRef? = nil
    let result = AXUIElementCopyAttributeValue(element, name as CFString, &value)
    return result == .success ? value : nil
}

func string(_ element: AXUIElement, _ name: String) -> String? {
    return attribute(element, name) as? String
}

func owner(_ element: AXUIElement) -> pid_t {
    var pid: pid_t = 0
    return AXUIElementGetPid(element, &pid) == .success ? pid : -1
}

func bounded(_ name: String) -> (String, Bool) {
    if name.count > maxName {
        return (String(name.prefix(maxName)), true)
    }
    return (name, false)
}

// The accessible name is the title AccessKit publishes for every node. Only a
// static-text node without a title may fall back to its value; editable and
// protected fields are never asked for a value.
func accessibleName(_ element: AXUIElement, role: String) -> (String, Bool, String) {
    if let title = string(element, kAXTitleAttribute as String), !title.isEmpty {
        let (name, truncated) = bounded(title)
        return (name, truncated, "title")
    }
    if role == staticTextRole, let value = string(element, kAXValueAttribute as String), !value.isEmpty {
        let (name, truncated) = bounded(value)
        return (name, truncated, "value")
    }
    return ("", false, "none")
}

func frontmostOwned(_ pid: pid_t) -> Bool {
    guard let frontmost = NSWorkspace.shared.frontmostApplication else { return false }
    return frontmost.processIdentifier == pid
}

func permissions() {
    // Both checks are read-only; neither shows a system prompt.
    reply(["accessibility": AXIsProcessTrusted(), "post_event": CGPreflightPostEventAccess()])
}

// The driver sends the executable fully resolved, as libproc reports it.
// Foundation's URL standardization and symlink resolution strip a leading
// /private (documented for NSURL), so they cannot be compared with it;
// realpath resolves both sides the same way.
func canonicalPath(_ url: URL) -> String? {
    guard url.isFileURL else { return nil }
    return url.withUnsafeFileSystemRepresentation { representation -> String? in
        guard let representation = representation, let resolved = realpath(representation, nil) else { return nil }
        defer { free(resolved) }
        return String(cString: resolved)
    }
}

// One bounded turn of this bridge's otherwise idle main run loop. AppKit
// documents that NSRunningApplication's time-varying properties and
// NSWorkspace.runningApplications change only when the main run loop runs in
// a common mode (.default is one). The turn only lets pending updates arrive;
// it never waits for, retries or accepts an identity.
func refreshApplicationState() {
    _ = RunLoop.main.run(mode: .default, before: Date(timeIntervalSinceNow: runLoopTurn))
}

func attach(_ command: [String: Any]) {
    // A failed new attachment must never leave the previous target usable.
    target = nil
    elements = []
    guard let pidValue = command["pid"] as? Int, pidValue > 0, pidValue <= Int(Int32.max),
          let executable = command["executable"] as? String, !executable.isEmpty,
          let bundle = command["bundle"] as? String, !bundle.isEmpty else {
        fail("invalid_command")
        return
    }
    let pid = pid_t(pidValue)
    refreshApplicationState()
    // Absent metadata (*_missing) is reported apart from a present value that
    // differs (*_mismatch); only the driver decides whether to ask again.
    guard let running = NSRunningApplication(processIdentifier: pid) else {
        fail("attach_application_missing")
        return
    }
    guard let identifier = running.bundleIdentifier else {
        fail("attach_bundle_missing")
        return
    }
    guard identifier == bundle else {
        fail("attach_bundle_mismatch")
        return
    }
    guard let url = running.executableURL else {
        fail("attach_executable_missing")
        return
    }
    guard let path = canonicalPath(url) else {
        fail("attach_executable_unresolved")
        return
    }
    guard path == executable else {
        fail("attach_executable_mismatch")
        return
    }
    target = Target(pid: pid, executable: executable, bundle: bundle)
    elements = []
    reply([:])
}

func windows() {
    guard let target = target else { fail("not_attached"); return }
    guard let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
            as? [[String: Any]] else {
        fail("window_list_unavailable")
        return
    }
    var count = 0
    for info in list {
        guard let ownerPid = info[kCGWindowOwnerPID as String] as? Int, ownerPid == Int(target.pid) else { continue }
        guard let layer = info[kCGWindowLayer as String] as? Int, layer == 0 else { continue }
        count += 1
    }
    reply(["count": min(count, 16)])
}

// Breadth-first walk of the owned application's tree only. A node whose owner,
// role, name or children could not be read is reported with error=true and no
// children. Unowned nodes are reported without a name and never expanded.
// Every bound is enforced before the growth it limits: the recorded-plus-queued
// total never exceeds maxNodes, names are cut before they are kept, and a
// bounded walk is reported so the driver treats it as not-a-tree.
func snapshot() {
    guard let target = target else { fail("not_attached"); return }
    elements = []
    let root = AXUIElementCreateApplication(target.pid)
    guard owner(root) == target.pid else { fail("unowned_root"); return }
    var nodes: [[String: Any]] = []
    var queue: [(AXUIElement, Int)] = [(root, -1)]
    var index = 0
    var wasBounded = false
    while index < queue.count && !wasBounded {
        let (element, parent) = queue[index]
        index += 1
        let id = elements.count
        elements.append(element)
        var node: [String: Any] = ["id": id, "parent": parent, "pid": -1, "name": "", "role": "other",
                                   "subrole": "", "focused": false, "error": false, "truncated": false,
                                   "name_source": "none"]
        let pid = owner(element)
        node["pid"] = Int(pid)
        if pid == target.pid {
            if let role = string(element, kAXRoleAttribute as String) {
                node["role"] = role
                node["subrole"] = string(element, kAXSubroleAttribute as String) ?? ""
                let (name, truncated, source) = accessibleName(element, role: role)
                node["name"] = name
                node["truncated"] = truncated
                node["name_source"] = source
                node["focused"] = (attribute(element, kAXFocusedAttribute as String) as? Bool) ?? false
                if let children = attribute(element, kAXChildrenAttribute as String) as? [AXUIElement] {
                    for child in children {
                        if elements.count + (queue.count - index) >= maxNodes {
                            wasBounded = true
                            break
                        }
                        queue.append((child, id))
                    }
                } else {
                    node["error"] = true
                }
            } else {
                node["error"] = true
            }
        }
        nodes.append(node)
    }
    reply(["nodes": nodes, "bounded": wasBounded])
}

func focus(_ command: [String: Any]) {
    guard let target = target else { fail("not_attached"); return }
    guard let id = command["id"] as? Int, id >= 0, id < elements.count else { fail("invalid_id"); return }
    let element = elements[id]
    // Ownership is rechecked on the live element, not the snapshot copy.
    guard owner(element) == target.pid else { fail("unowned"); return }
    guard AXUIElementSetAttributeValue(element, kAXFocusedAttribute as CFString, kCFBooleanTrue) == .success else {
        fail("element_unavailable")
        return
    }
    reply([:])
}

func focused() {
    guard let target = target else { fail("not_attached"); return }
    let application = AXUIElementCreateApplication(target.pid)
    guard let value = attribute(application, kAXFocusedUIElementAttribute as String) else {
        reply(["present": false, "owned": false, "name": "", "truncated": false])
        return
    }
    guard CFGetTypeID(value) == AXUIElementGetTypeID() else {
        fail("element_unavailable")
        return
    }
    let element = value as! AXUIElement
    let pid = owner(element)
    guard pid == target.pid else {
        reply(["present": true, "owned": false, "name": "", "truncated": false])
        return
    }
    let role = string(element, kAXRoleAttribute as String) ?? "other"
    let (name, truncated, _) = accessibleName(element, role: role)
    reply(["present": true, "owned": true, "name": name, "truncated": truncated])
}

struct Transition {
    let code: CGKeyCode
    let down: Bool
    let shifted: Bool
}

// Keyboard input is posted directly to the owned process, never to the
// session, and the owned process must be the frontmost application
// immediately before EVERY native post, including releases. A failed proof
// ends the chord at once; nothing further is posted toward whatever took the
// foreground. The driver fails the run either way.
func sendKey(_ command: [String: Any]) {
    guard let target = target else { fail("not_attached"); return }
    guard var name = command["key"] as? String else { fail("invalid_key"); return }
    var shift = false
    if name == "ShiftTab" {
        name = "Tab"
        shift = true
    }
    guard let code = keyCodes[name] else { fail("invalid_key"); return }
    var recovered = false
    if !frontmostOwned(target.pid) {
        guard let running = NSRunningApplication(processIdentifier: target.pid) else { fail("window_gone"); return }
        _ = running.activate(options: [.activateIgnoringOtherApps])
        Thread.sleep(forTimeInterval: 0.15)
        guard frontmostOwned(target.pid) else { fail("foreground_unowned"); return }
        recovered = true
    }
    var transitions: [Transition] = []
    if shift { transitions.append(Transition(code: shiftCode, down: true, shifted: false)) }
    transitions.append(Transition(code: code, down: true, shifted: shift))
    transitions.append(Transition(code: code, down: false, shifted: shift))
    if shift { transitions.append(Transition(code: shiftCode, down: false, shifted: false)) }
    var posted = 0
    for transition in transitions {
        guard frontmostOwned(target.pid) else {
            fail(posted == 0 ? "foreground_unowned" : "input_rejected")
            return
        }
        guard let event = CGEvent(keyboardEventSource: nil, virtualKey: transition.code, keyDown: transition.down) else {
            fail("input_rejected")
            return
        }
        event.flags = transition.shifted ? [.maskShift] : []
        event.postToPid(target.pid)
        posted += 1
    }
    reply(["recovered": recovered, "posted": posted])
}

// The window's own close button, pressed through its accessibility action
// after an ownership recheck: the macOS counterpart of WM_DELETE_WINDOW.
func close() {
    guard let target = target else { fail("not_attached"); return }
    let application = AXUIElementCreateApplication(target.pid)
    guard let list = attribute(application, kAXWindowsAttribute as String) as? [AXUIElement] else {
        fail("close_unavailable")
        return
    }
    guard list.count == 1, owner(list[0]) == target.pid else { fail("window_count"); return }
    guard let value = attribute(list[0], kAXCloseButtonAttribute as String),
          CFGetTypeID(value) == AXUIElementGetTypeID() else {
        fail("close_unavailable")
        return
    }
    let button = value as! AXUIElement
    guard owner(button) == target.pid else { fail("unowned"); return }
    guard AXUIElementPerformAction(button, kAXPressAction as CFString) == .success else {
        fail("close_unavailable")
        return
    }
    reply([:])
}

reply(["ready": true])
while let line = readLine(strippingNewline: true) {
    if line.count > maxLine {
        fail("invalid_command")
        continue
    }
    guard let data = line.data(using: .utf8),
          let parsed = try? JSONSerialization.jsonObject(with: data),
          let command = parsed as? [String: Any],
          let op = command["op"] as? String else {
        fail("invalid_command")
        continue
    }
    switch op {
    case "permissions": permissions()
    case "attach": attach(command)
    case "windows": windows()
    case "snapshot": snapshot()
    case "focus": focus(command)
    case "focused": focused()
    case "key": sendKey(command)
    case "close": close()
    case "exit": exit(0)
    default: fail("invalid_command")
    }
}
exit(0)
