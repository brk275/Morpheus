// Diagnostic branch only. No application launch, AX tree read, input event,
// permission request or permission grant is performed by this probe.
import ApplicationServices
import CoreGraphics
import Darwin
import Foundation

#if arch(arm64)
let architecture = "arm64"
#elseif arch(x86_64)
let architecture = "x86_64"
#else
#error("The permission probe supports only the two native macOS targets.")
#endif

let environment = ProcessInfo.processInfo.environment
guard environment["GITHUB_ACTIONS"] == "true",
      environment["RUNNER_ENVIRONMENT"] == "github-hosted",
      environment["RUNNER_OS"] == "macOS",
      environment["GITHUB_EVENT_NAME"] == "workflow_dispatch",
      environment["GITHUB_REPOSITORY"] == "brk275/Morpheus",
      environment["GITHUB_REF"] == "refs/heads/codex/macos-ui-preflight",
      let sourceCommit = environment["GITHUB_SHA"], sourceCommit.utf8.count == 40,
      sourceCommit.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) })
else {
    fputs("Mac UI permission probe refused this environment.\n", stderr)
    exit(2)
}

struct ProbeResult: Encodable {
    let check = "macos_ui_permission_preflight"
    let source_commit: String
    let architecture: String
    let accessibility_trusted: Bool
    let keyboard_post_allowed: Bool
    let status: String
    let permission_requests = false
    let permission_grants = false
    let app_started = false
    let keyboard_events_sent = false
    let screen_reader_acceptance = false
}

// These are status queries. In particular, no prompting variant or request
// API is called when permission is absent.
let trusted = AXIsProcessTrusted()
let postAllowed = CGPreflightPostEventAccess()
let available = trusted && postAllowed
let result = ProbeResult(
    source_commit: sourceCommit, architecture: architecture,
    accessibility_trusted: trusted, keyboard_post_allowed: postAllowed,
    status: available ? "available" : "unavailable"
)
do {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys]
    FileHandle.standardOutput.write(try encoder.encode(result))
    FileHandle.standardOutput.write(Data([10]))
} catch {
    fputs("Mac UI permission probe could not encode its fixed result.\n", stderr)
    exit(2)
}
exit(available ? 0 : 1)
