# Build and verification policy

This is the canonical build-spending policy for this repository. All coding agents must follow it. The root CLAUDE.md points here; do not maintain a second, conflicting copy of these rules.

Where older documentation describes a blanket CI or package command, this policy governs when and for which components to run it. Existing documentation or workflow YAML is not authorization to start paid builds. The underlying toolchain, signed-device, provider, security and installed-package acceptance requirements still apply.

## Select checks by impact

- Choose checks from the changed components, their dependencies, shared contracts and affected platforms, not the number of changed lines.
- Documentation and copy-only changes must not trigger native packaging or a full platform matrix. Run relevant documentation, UI or targeted checks when needed.
- During development, use targeted checks and batch related fixes before requesting expensive hosted verification.
- Inspect workflow triggers before pushing or opening a pull request. Do not trigger unrelated paid jobs just to validate a small change. If existing workflows cannot select the required scope, report that limitation and prepare the filtering fix instead of repeatedly triggering the broad workflow.

## Avoid duplicate work

- Check active runs and existing results before dispatching a build. Never start overlapping builds for the same platform and candidate, or duplicate push, pull-request and manual runs for the same validation.
- Cancel only obsolete verification runs belonging to the current task. Do not cancel publication, deployment, migrations or unrelated work.
- Allow an in-flight build to populate its cache before starting another equivalent build. Do not knowingly launch multiple cold builds that compete for the same cache.
- After a failure, diagnose it and rerun failed or affected jobs. Do not restart successful, unaffected jobs merely because another job failed.
- Reuse verified artifacts only when source, dependency locks, build configuration, toolchain and relevant verification inputs are unchanged. Record the producing run, revision and immutable artifact identity. Never present stale evidence as verification of changed inputs.

## Preserve release quality

- Cost control does not waive compatibility, security, installation, migration or other applicable release checks.
- Complete the full required platform and integration checks on a stable release candidate. Relevant source or input changes invalidate the affected evidence.
- Keep ordinary development checks separate from full release verification. Do not require both complete local and hosted suites for unchanged inputs when valid equivalent evidence is available.
- Workflow filtering and concurrency must enforce these rules separately; these instructions alone do not change automatic workflow triggers.

## Spending boundaries

- Do not increase billing caps, enable paid-runner upgrades, add paid services or make repeated paid-build attempts without owner authorization covering that work.
- Stop on an exhausted budget or an unchanged failure. Report the cause instead of retrying blindly or finding another billed account.
- No paid Morpheus builds are authorized. Budgets or approvals for other projects do not apply here. A repository budget change is not implied by this file. Use standard hosted runners only after the repository is public, unless the owner explicitly authorizes a different arrangement.
- Report actual billed usage separately from estimates. Sum active job execution time across runners; elapsed workflow time is not billed runner time. Use each runner's current rate, and exclude queued time and copied successful-job records when analyzing retries.

## Morpheus requirements

- A Linux-only packaging change must build and test Linux packaging only; it must not rebuild Mac or Windows packages.
- Use the complete supported native matrix for shared native changes and release candidates: Apple Silicon, Intel Mac, Windows and Linux. A platform-specific check is not evidence for the other platforms.
- Preserve the applicable gates in [verification](docs/VERIFICATION.md), [packaging](docs/PACKAGING.md) and the [release checklist](docs/RELEASE_CHECKLIST.md), including installation, real accounts/homeservers, device behavior, signing, updates, rollback and licensing.
