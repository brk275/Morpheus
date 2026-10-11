# Windows synthetic capture acceptance

This opt-in fixture exercises Morpheus's existing Windows `ScreenCapture`
adapter on the standard free GitHub-hosted `windows-2025` runner. It has not
passed until the exact candidate's hosted result reports `passed: true`.

## Scope

The ignored Rust test `windows_selected_sources_change_and_stop` launches an
owned Windows PowerShell / WinForms child with an unpredictable nonce title.
It requires an interactive desktop and exactly one monitor, and checks that
the owned child PID and exact title identify one window. That borderless,
topmost window must cover the selected monitor before any display capture.
The fixture paints red and blue halves, then exchanges their colors.

The test uses the production opaque window key and selected display ID to
check three successive I420 frames from each source, a changed image from
each source, cancellation before capture, drop/reopen, rejection of a retitled
source and its stale key, explicit reselection, and rejection after closure.
Pixel buffers stay in memory. The fixture never saves screenshots or uploads
raw logs, window titles, user paths, monitor metadata, or fixture identifiers.

The pinned xcap 0.9.8 default Windows path uses GDI (`PrintWindow` / DWM window
fallback and display `BitBlt`). This is synthetic adapter acceptance on a
disposable VM. It does not establish Windows Graphics Capture support,
physical GPU or multimonitor/DPI compatibility, secure/protected desktop
behavior, the app's source picker, real RTC delivery, camera or microphone
behavior, installation, or signed-package acceptance. A missing interactive
desktop, missing source, mismatched pixels, or zero tests fails the check.

## Invocation and ownership

CI explicitly sets `MORPHEUS_WINDOWS_CAPTURE_ACCEPTANCE=1` and invokes
`python scripts/run_windows_capture_acceptance.py`. The helper, Rust test,
and WinForms fixture also require `GITHUB_ACTIONS=true`,
`RUNNER_ENVIRONMENT=github-hosted`, and `RUNNER_OS=Windows`. The helper rejects
other operating systems. These guards limit accidental invocation; they are
not a security boundary against someone deliberately forging an environment.

The helper creates an unnamed, noninheritable Windows Job with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and no breakaway permission. It starts
every direct child suspended, assigns it to that Job, then resumes exactly
one suspend count. Assignment failure terminates the still-suspended child
through a retained process handle. Process and thread handles are closed on
all paths. Cleanup attempts every retained child even after another failure;
it never signals a numeric PID. Temporary inheritable standard-I/O handles
are duplicates, so the helper's originals stay noninheritable.

Termination completion is proved with a bounded wait on each retained
process handle. Windows documents that `TerminateProcess` is asynchronous
and can return `ERROR_ACCESS_DENIED` for a process that has terminated;
a failed termination request is accepted only if the subsequent wait observes
the handle signaled. Timeout or wait failure still fails cleanup. The source
previously retried that observation with a zero-time wait, which could reject
pending completion after Job termination. The first hosted attempt at
`db25e42` stopped before capture with cleanup status 5; that result is
consistent with this defect, but its old diagnostics cannot establish whether
an earlier startup failure was also present. See Microsoft's
[TerminateProcess completion semantics](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-terminateprocess).

A mandatory hosted preflight starts a disposable child plus a detached
grandchild, observes both in the Job, then terminates the Job and requires
zero active processes. This checks descendant cleanup on the actual runner;
it does not simulate power loss. Closing the last Job handle is an additional
kernel teardown safeguard. [Microsoft Job Object documentation](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)
describes descendant assignment and kill-on-close behavior;
[CreateProcessW](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw)
and [AssignProcessToJobObject](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-assignprocesstojobobject)
define the suspended-child boundary.

The helper builds only the dedicated integration target, reusing the native
job's cache (600-second build deadline), then invokes its one exact ignored
test (120-second process deadline). A GDI call can block within that process;
the outer Job deadline still owns termination. The workflow step has a
15-minute limit. Failure does not skip independent security checks, but the
final outcome gate rejects the candidate.

## Evidence and privacy

Temporary files live outside the checkout in the runner user's normal
temporary directory and are removed before public summary output. On Windows,
privacy relies on that disposable runner account's inherited filesystem ACL;
Python's POSIX-style `chmod` is not represented as a Windows ACL guarantee.
No real accounts, credentials, or Matrix services are used.

Public evidence contains only a pass boolean, fixed phase/failure/operation
enums, and optional unsigned 32-bit process/Win32 statuses. If cleanup also
fails, bounded prior-failure facts preserve the earlier operation without
formatting exception messages. Native error bodies and
libtest panic payloads stay private. Passing requires exit code zero, the
exact named test's success line, a one-passed/zero-failed/zero-ignored summary,
the terminal fixture phase, and successful owned-process cleanup.

Portable Python mocks check quoting, suspended assignment ordering, failure
cleanup, deadlines, and evidence filtering. They do not prove Win32 structure
layout or native capture; the hosted preflight and exact native test supply
that platform evidence. Changes to dedicated fixture files select only
Windows. Shared workflow/scope or native changes retain the full supported
platform matrix under `AGENTS.md`.
