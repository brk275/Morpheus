"""TEMPORARY owned native AX experiment; never merge or ship this directory."""
from pathlib import Path
import hashlib
import json
import os
import platform
import plistlib
import subprocess
import sys
import tempfile

EXPECTED = "2f824d40e30c323d5fa60c7f73870887b4c9b741a3e1673eb6385637b20802ab"
BUNDLE = "io.github.brk275.morpheus.ax-probe"
RESULTS = {"success", "unsupported", "no_value", "cannot_complete", "invalid", "other"}
ROLES = {"AXApplication", "AXWindow", "AXButton", "AXTextField", "AXStaticText",
         "AXGroup", "AXMenuBar", "AXMenuBarItem", "AXMenuItem", "AXMenu"}
SUBJECT = r'''
import AppKit
let app = NSApplication.shared
app.setActivationPolicy(.regular)
let menu = NSMenu()
let entry = NSMenuItem()
let submenu = NSMenu(title: "Probe")
submenu.addItem(NSMenuItem(title: "Quit", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q"))
entry.submenu = submenu
menu.addItem(entry)
app.mainMenu = menu
let window = NSWindow(contentRect: NSRect(x: 60, y: 60, width: 480, height: 260),
    styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
window.title = "Synthetic accessibility probe"
let first = NSTextField(frame: NSRect(x: 24, y: 180, width: 380, height: 28))
first.placeholderString = "Homeserver URL"
first.setAccessibilityLabel("Homeserver URL")
let second = NSTextField(frame: NSRect(x: 24, y: 130, width: 380, height: 28))
second.placeholderString = "Username"
second.setAccessibilityLabel("Username")
let password = NSSecureTextField(frame: NSRect(x: 24, y: 80, width: 380, height: 28))
password.setAccessibilityLabel("Password")
let button = NSButton(title: "Synthetic action", target: nil, action: nil)
button.frame = NSRect(x: 24, y: 24, width: 180, height: 30)
for control in [first, second, password, button] as [NSView] {
    window.contentView!.addSubview(control)
}
window.makeKeyAndOrderFront(nil)
app.activate(ignoringOtherApps: true)
app.run()
'''
CLASSIFY = r'''
let owned = AXUIElementCreateApplication(getpid())
let empty = NSArray()
let one = NSArray(object: owned)
let wrong = "not an array" as NSString
let checks: [String: Bool] = [
    "oversize_rejected": classifiedChildren(.success, NSArray(array: Array(repeating: owned, count: maxNodes + 1))) == nil,
    "no_value_empty": classifiedChildren(.noValue, nil)?.isEmpty == true,
    "no_value_payload_rejected": classifiedChildren(.noValue, empty) == nil,
    "success_empty": classifiedChildren(.success, empty)?.isEmpty == true,
    "success_nonempty": classifiedChildren(.success, one)?.count == 1,
    "success_nil_rejected": classifiedChildren(.success, nil) == nil,
    "success_wrong_type_rejected": classifiedChildren(.success, wrong) == nil,
    "success_wrong_element_rejected": classifiedChildren(.success, NSArray(object: wrong)) == nil,
    "success_mixed_elements_rejected": classifiedChildren(.success, NSArray(objects: owned, wrong)) == nil,
    "unsupported_rejected": classifiedChildren(.attributeUnsupported, nil) == nil,
    "cannot_complete_rejected": classifiedChildren(.cannotComplete, nil) == nil,
    "invalid_element_rejected": classifiedChildren(.invalidUIElement, nil) == nil,
    "illegal_argument_rejected": classifiedChildren(.illegalArgument, nil) == nil,
    "not_implemented_rejected": classifiedChildren(.notImplemented, nil) == nil
]
let data = try JSONSerialization.data(withJSONObject: checks, options: [.sortedKeys])
FileHandle.standardOutput.write(data)
'''
CLASSIFY_KEYS = {"oversize_rejected", "no_value_empty", "no_value_payload_rejected", "success_empty", "success_nonempty",
                 "success_nil_rejected", "success_wrong_type_rejected", "unsupported_rejected",
                 "success_wrong_element_rejected", "success_mixed_elements_rejected",
                 "cannot_complete_rejected", "invalid_element_rejected", "illegal_argument_rejected",
                 "not_implemented_rejected"}


def main():
    os.umask(0o077)
    for key, value in {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted",
                       "RUNNER_OS": "macOS", "ImageOS": "macos15"}.items():
        assert os.environ.get(key) == value
    source = Path("scripts/macos_package_ui.swift").read_text()
    assert hashlib.sha256(source.encode()).hexdigest() == EXPECTED
    # Instrument only AXChildren result counting and the private reply. Do not
    # change traversal, naming, ownership or error handling in the real bridge.
    source = source.replace("func attribute(_ element:", "var childResults: [String: Int] = [:]\n\nfunc attribute(_ element:", 1)
    old = "    return classifiedChildren(result, value)\n"
    assert source.count(old) == 1
    source = source.replace(old, '''    do {
        let key: String
        switch result {
        case .success: key = "success"
        case .attributeUnsupported: key = "unsupported"
        case .noValue: key = "no_value"
        case .cannotComplete: key = "cannot_complete"
        case .invalidUIElement: key = "invalid"
        default: key = "other"
        }
        childResults[key] = min(maxNodes, (childResults[key] ?? 0) + 1)
    }
''' + old)
    old = '    reply(["nodes": nodes, "bounded": wasBounded])'
    assert source.count(old) == 1
    source = source.replace(old, '    reply(["nodes": nodes, "bounded": wasBounded, "child_results": childResults])')
    sys.path.insert(0, str(Path("scripts").resolve(strict=True)))
    import macos_package_ui as ui  # Exact reviewed 9a5affc helper; hosted only.
    environment = {key: os.environ[key] for key in
                   ("PATH", "HOME", "LANG", "TMPDIR", "DEVELOPER_DIR", "GITHUB_ACTIONS",
                    "RUNNER_ENVIRONMENT", "RUNNER_OS", "ImageOS") if key in os.environ}
    # Keep private files on this ephemeral worker, including if process
    # cleanup cannot be proved. There is no recursive deletion or upload.
    root = Path(tempfile.mkdtemp(prefix="morpheus-native-ax-", dir=os.environ["RUNNER_TEMP"])).resolve(strict=True)
    root.chmod(0o700)
    contents = root / "Subject.app" / "Contents"
    executable = contents / "MacOS" / "MorpheusAxSubject"
    executable.parent.mkdir(parents=True)
    (contents / "Info.plist").write_bytes(plistlib.dumps({
        "CFBundleIdentifier": BUNDLE, "CFBundleName": "MorpheusAxSubject",
        "CFBundleExecutable": executable.name, "CFBundlePackageType": "APPL",
        "CFBundleVersion": "1", "NSHighResolutionCapable": True,
    }))
    subject_source = root / "subject.swift"
    subject_source.write_text(SUBJECT)
    bridge_source = root / "bridge.swift"
    bridge_source.write_text(source)
    binary = root / "bridge"
    app = bridge = None
    snapshot = None
    with (root / "private.log").open("wb") as log:
        def run(command):
            result = subprocess.run(command, stdin=subprocess.DEVNULL, stdout=log,
                                    stderr=subprocess.STDOUT, timeout=180, env=environment)
            assert result.returncode == 0
        run(["xcrun", "swiftc", "-O", "-o", str(executable), str(subject_source)])
        run(["xcrun", "swiftc", "-O", "-o", str(binary), str(bridge_source)])
        function = "func classifiedChildren(" + source.split("func classifiedChildren(", 1)[1].split("\nfunc childElements(", 1)[0]
        bound = [line for line in source.splitlines() if line.startswith("let maxNodes = ")]
        assert bound == ["let maxNodes = 5000"]
        harness = root / "classification.swift"
        harness.write_text("import AppKit\nimport ApplicationServices\nimport Darwin\nimport Foundation\n"
                           + bound[0] + "\n" + function + CLASSIFY)
        classifier = root / "classification"
        run(["xcrun", "swiftc", "-O", "-o", str(classifier), str(harness)])
        classification = subprocess.run([str(classifier)], stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=log, timeout=10, env=environment)
        assert classification.returncode == 0 and len(classification.stdout) <= 4096
        classified = json.loads(classification.stdout)
        assert type(classified) is dict and set(classified) == CLASSIFY_KEYS
        assert all(type(value) is bool for value in classified.values())
        if not all(classified.values()):
            print(json.dumps({"check": "native_ax_child_classification", "status": "failed",
                              "checks": classified}, sort_keys=True), flush=True)
            raise RuntimeError("Classifier boundary check failed")
        run(["codesign", "--force", "--sign", "-", str(contents.parent)])
        try:
            diagnostic = ui.Diagnostics({})
            bridge = ui.Bridge([str(binary)], diagnostic, environment=environment, log=log)
            permission = bridge.call({"op": "permissions"})  # Read only; no prompt.
            assert permission.get("accessibility") is True
            app = subprocess.Popen([str(executable)], stdin=subprocess.DEVNULL,
                                   stdout=log, stderr=subprocess.STDOUT,
                                   env=environment, start_new_session=True)
            ui.attach_owned(bridge, app, executable, BUNDLE, diagnostic)
            ui.owned_window(bridge, app, diagnostic)
            ui.require_anchored(app, executable, diagnostic)
            snapshot = bridge.call({"op": "snapshot"})
            ui.require_anchored(app, executable, diagnostic)
            raw = json.dumps(snapshot, sort_keys=True).encode()
            assert len(raw) <= 2 * 1024 * 1024
            scan = subprocess.run([os.environ["AX_PROBE_SCANNER"], "stdin", "--redact=100",
                "--ignore-gitleaks-allow", "--no-banner", "--config", os.environ["AX_PROBE_CONFIG"],
                "--gitleaks-ignore-path", os.environ["AX_PROBE_IGNORE"]], input=raw,
                stdout=log, stderr=subprocess.STDOUT, cwd=root, env=environment, timeout=30)
            assert scan.returncode == 0
        finally:
            try:
                if bridge is not None:
                    bridge.close()
            finally:
                if app is not None:
                    ui.stop_group(app)
    assert type(snapshot) is dict and snapshot.get("ok") is True
    assert snapshot.get("bounded") is False
    children = snapshot.get("child_results")
    assert type(children) is dict and set(children) <= RESULTS
    assert all(type(v) is int and 0 <= v <= 5000 for v in children.values())
    nodes = snapshot.get("nodes")
    assert type(nodes) is list and 0 < len(nodes) <= 5000
    facts = {}
    assert nodes[0].get("pid") == app.pid and not nodes[0].get("error")
    for raw_node in nodes:
        node = ui.Node(raw_node)
        role = (node.role if node.role in ROLES else "other") if node.pid == app.pid else "unowned"
        group = facts.setdefault(role, {"nodes": 0, "incomplete": 0})
        group["nodes"] += 1
        group["incomplete"] += int(node.error)
    assert children.get("no_value", 0) >= 1
    assert "unowned" not in facts
    assert all(group["incomplete"] == 0 for group in facts.values())
    print(json.dumps({"check": "native_macos_ax_children", "status": "passed",
        "architecture": platform.machine(), "original_bridge_sha256": EXPECTED,
        "instrumented_bridge_sha256": hashlib.sha256(source.encode()).hexdigest(),
        "classifier_source_sha256": hashlib.sha256(function.encode()).hexdigest(),
        "classifier_checks": classified,
        "children_results": children, "role_counts": facts,
        "owned_app": True, "process_cleanup": True, "input_sent": False,
        "permission_requested": False, "accounts_used": False, "raw_output_public": False,
        "limits": "synthetic_appkit_subject_not_installed_morpheus"}, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except BaseException:
        print("MACOS_AX_CHILDREN_PROBE_FAILED raw_logs_withheld", flush=True)
        raise SystemExit(1) from None
