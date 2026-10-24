//! Compile-gated synthetic installed-bundle transaction actors. Shipping builds
//! contain no entry point or activation override. These actors never construct
//! UI, runtimes, profiles, stores or network clients. The old actor really
//! exits after its helper has an acknowledged kernel observer. This remains
//! separate from normal AppWindow/account/media shutdown acceptance.

use super::*;
use std::cell::RefCell;

const PLAN_ENV: &str = "MORPHEUS_MAC_UPDATE_ACCEPTANCE_PLAN";
const LIMIT: Duration = Duration::from_secs(150);
const FORCED_AFTER_APPLY_EXIT: i32 = 86;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Case {
    Upgrade,
    InvalidSignature,
    TamperedStage,
    PreparedRestart,
    AfterApplyRecovery,
    TrialFailureRollback,
}

impl Case {
    fn token(self) -> &'static str {
        match self {
            Self::Upgrade => "upgrade",
            Self::InvalidSignature => "invalid_signature",
            Self::TamperedStage => "tampered_stage",
            Self::PreparedRestart => "prepared_restart",
            Self::AfterApplyRecovery => "after_apply_recovery",
            Self::TrialFailureRollback => "trial_failure_rollback",
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    schema: u8,
    nonce: String,
    case: Case,
    old_version: String,
    new_version: String,
    old_tree_sha256: String,
    new_tree_sha256: String,
    archive_sha256: String,
    public_key: String,
    signature: String,
}

#[derive(Clone)]
struct Run {
    plan: Plan,
    root: PathBuf,
    installed: PathBuf,
    state: PathBuf,
}

thread_local! {
    static CURRENT: RefCell<Option<Run>> = const { RefCell::new(None) };
    static RETAINED: RefCell<Vec<OwnedChild>> = const { RefCell::new(Vec::new()) };
}

fn require(condition: bool) -> Result<(), UpdateError> {
    condition
        .then_some(())
        .ok_or(UpdateError::InvalidConfiguration)
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hash_text(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Plan {
    fn validate(&self, nonce: &str) -> Result<(), UpdateError> {
        let old =
            Version::parse(&self.old_version).map_err(|_| UpdateError::InvalidConfiguration)?;
        let new =
            Version::parse(&self.new_version).map_err(|_| UpdateError::InvalidConfiguration)?;
        require(
            self.schema == 1
                && self.nonce == nonce
                && nonce.len() == 32
                && nonce.bytes().all(|b| b.is_ascii_hexdigit())
                && self.old_version.len() <= 128
                && self.new_version.len() <= 128
                && new.cmp_precedence(&old) == std::cmp::Ordering::Greater
                && hash_text(&self.old_tree_sha256)
                && hash_text(&self.new_tree_sha256)
                && hash_text(&self.archive_sha256)
                && !self.public_key.is_empty()
                && self.public_key.len() <= 4096
                && !self.signature.is_empty()
                && self.signature.len() <= 4096,
        )
    }
}

fn hosted() -> Result<(), UpdateError> {
    for (key, value) in [
        ("GITHUB_ACTIONS", "true"),
        ("RUNNER_ENVIRONMENT", "github-hosted"),
        ("RUNNER_OS", "macOS"),
        ("ImageOS", "macos15"),
        ("GITHUB_REPOSITORY", "brk275/Morpheus"),
        ("GITHUB_EVENT_NAME", "workflow_dispatch"),
        ("GITHUB_ACTOR", "brk275"),
        ("GITHUB_TRIGGERING_ACTOR", "brk275"),
    ] {
        require(std::env::var(key).as_deref() == Ok(value))?;
    }
    require(owner() != 0 && unsafe { libc::getuid() } == owner())?;
    let event_path =
        std::env::var_os("GITHUB_EVENT_PATH").ok_or(UpdateError::InvalidConfiguration)?;
    let event: serde_json::Value =
        serde_json::from_slice(&bounded_read(Path::new(&event_path), 1024 * 1024)?)
            .map_err(|_| UpdateError::InvalidConfiguration)?;
    let id = event["repository"]["owner"]["id"]
        .as_u64()
        .ok_or(UpdateError::InvalidConfiguration)?;
    let actor_id = id.to_string();
    require(
        id > 0
            && event["repository"]["private"] == false
            && event["repository"]["full_name"] == "brk275/Morpheus"
            && event["repository"]["owner"]["login"] == "brk275"
            && event["sender"]["login"] == "brk275"
            && event["sender"]["id"].as_u64() == Some(id)
            && std::env::var("GITHUB_ACTOR_ID").ok().as_deref() == Some(actor_id.as_str()),
    )
}

fn private_existing(path: &Path) -> Result<(), UpdateError> {
    let metadata = directory(path)?;
    require(
        path.is_absolute()
            && path
                .canonicalize()
                .map_err(|_| UpdateError::InvalidConfiguration)?
                == path
            && metadata.mode() & 0o077 == 0,
    )
}

impl Run {
    fn load() -> Result<Self, UpdateError> {
        hosted()?;
        require(!INSTALLATION_VALIDATED)?;
        let nonce = option_env!("MORPHEUS_MAC_UPDATE_ACCEPTANCE_NONCE")
            .ok_or(UpdateError::InvalidConfiguration)?;
        let path =
            PathBuf::from(std::env::var_os(PLAN_ENV).ok_or(UpdateError::InvalidConfiguration)?);
        require(path.is_absolute() && path.file_name().is_some_and(|p| p == "run.json"))?;
        let metadata =
            fs::symlink_metadata(&path).map_err(|_| UpdateError::InvalidConfiguration)?;
        require(metadata.mode() & 0o077 == 0 && metadata.nlink() == 1)?;
        let plan: Plan = serde_json::from_slice(&bounded_read(&path, 32 * 1024)?)
            .map_err(|_| UpdateError::InvalidConfiguration)?;
        plan.validate(nonce)?;
        let temporary = PathBuf::from(
            std::env::var_os("RUNNER_TEMP").ok_or(UpdateError::InvalidConfiguration)?,
        );
        let temporary = temporary
            .canonicalize()
            .map_err(|_| UpdateError::InvalidConfiguration)?;
        let owner_root = temporary.join(format!("morpheus-mac-update-qa-{nonce}"));
        let root = owner_root.join(plan.case.token());
        require(path == root.join("run.json"))?;
        private_existing(&owner_root)?;
        private_existing(&root)?;
        private_existing(&root.join("events"))?;
        private_existing(&owner_root.join("payload"))?;
        private_existing(&owner_root.join("tmp"))?;
        require(std::env::var_os("TMPDIR").map(PathBuf::from) == Some(owner_root.join("tmp")))?;
        for name in [
            "CARGO_PACKAGER_SIGN_PRIVATE_KEY",
            "CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD",
            "MORPHEUS_UPDATE_FEED",
            "MORPHEUS_UPDATE_PUBLIC_KEY",
        ] {
            require(std::env::var_os(name).is_none())?;
        }
        let installed = root.join("Morpheus.app");
        let (installed, state) = MacAppSession::paths(&installed)?;
        let running = std::env::current_exe().map_err(|_| UpdateError::UnsupportedPackage)?;
        let running = running
            .canonicalize()
            .map_err(|_| UpdateError::UnsupportedPackage)?;
        let installed_executable = installed
            .join(EXECUTABLE)
            .canonicalize()
            .map_err(|_| UpdateError::UnsupportedPackage)?;
        let previous_executable = state
            .join("previous.app")
            .join(EXECUTABLE)
            .canonicalize()
            .ok();
        require(running == installed_executable || previous_executable.as_ref() == Some(&running))?;
        let embedded = PackageMetadata::embedded()?;
        require(embedded.version == plan.old_version || embedded.version == plan.new_version)?;
        Ok(Self {
            plan,
            root,
            installed,
            state,
        })
    }

    fn mark(&self, token: &str) -> Result<(), UpdateError> {
        require(token.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') && token.len() <= 48)?;
        let path = self.root.join("events").join(token);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|_| UpdateError::RecoveryRequired)?;
        file.write_all(b"{\"passed\":true}\n")
            .and_then(|_| file.sync_all())
            .map_err(|_| UpdateError::RecoveryRequired)
    }

    fn seen(&self, token: &str) -> bool {
        bounded_read(&self.root.join("events").join(token), 64)
            .is_ok_and(|b| b == b"{\"passed\":true}\n")
    }

    fn wait(&self, token: &str) -> Result<(), UpdateError> {
        let deadline = Instant::now() + LIMIT;
        while !self.seen(token) {
            if Instant::now() >= deadline {
                return Err(UpdateError::RestartFailed);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Ok(())
    }

    fn offer_helper_observer(&self, helper: &OwnedChild) -> Result<(), UpdateError> {
        #[derive(Serialize)]
        struct Ticket<'a> {
            schema: u8,
            nonce: &'a str,
            transaction: String,
            parent: u32,
            helper: u32,
        }
        let journal =
            MacAppSession::read_journal(&self.state)?.ok_or(UpdateError::RecoveryRequired)?;
        require(journal.phase == Phase::Prepared && !helper.exited()?)?;
        let ticket = Ticket {
            schema: 1,
            nonce: &self.plan.nonce,
            transaction: journal.id,
            parent: std::process::id(),
            helper: helper
                .child
                .as_ref()
                .ok_or(UpdateError::RecoveryRequired)?
                .id(),
        };
        let bytes = serde_json::to_vec(&ticket).map_err(|_| UpdateError::RecoveryRequired)?;
        require(bytes.len() <= 4096)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.root.join("helper-observer.json"))
            .map_err(|_| UpdateError::RecoveryRequired)?;
        output
            .write_all(&bytes)
            .and_then(|_| output.sync_all())
            .map_err(|_| UpdateError::RecoveryRequired)?;
        // The actual Child remains retained and unreaped through the external
        // observer's registration acknowledgment. No PID lookup may replace it.
        self.mark("helper_ticket_ready")?;
        self.wait("helper_observer_armed")?;
        require(!helper.exited()?)
    }

    fn assert_old(&self) -> Result<(), UpdateError> {
        require(
            tree_hash(&self.installed)? == self.plan.old_tree_sha256
                && MacAppSession::read_journal(&self.state)?.is_none(),
        )
    }

    fn signed_stage(&self, corrupt: bool) -> Result<super::super::StagedUpdate, UpdateError> {
        use super::super::{
            Config, PackageKind, Update, UpdateFormat, stage_bytes, validate_candidate,
        };
        let archive = self
            .root
            .parent()
            .unwrap()
            .join("payload/Morpheus.app.tar.gz");
        let mut bytes = bounded_read(&archive, MAX_UPDATE_BYTES)?;
        require(digest(&bytes) == self.plan.archive_sha256)?;
        if corrupt {
            let byte = bytes.last_mut().ok_or(UpdateError::CorruptStage)?;
            *byte ^= 1;
        }
        let update = Update {
            config: Config {
                pubkey: self.plan.public_key.clone(),
                ..Default::default()
            },
            body: None,
            current_version: self.plan.old_version.clone(),
            version: self.plan.new_version.clone(),
            date: None,
            target: format!("macos-{}", std::env::consts::ARCH),
            extract_path: self.root.join("unused"),
            download_url: "https://127.0.0.1/unused-synthetic-input"
                .parse()
                .map_err(|_| UpdateError::InvalidConfiguration)?,
            signature: self.plan.signature.clone(),
            timeout: Some(Duration::from_secs(1)),
            headers: Default::default(),
            format: UpdateFormat::App,
        };
        validate_candidate(
            &update,
            &Version::parse(&self.plan.old_version).unwrap(),
            PackageKind::MacApp,
        )?;
        // No downloader/client is invoked. The same official verifier and exact
        // staged-file re-verification used by production remain mandatory.
        stage_bytes(update, PackageKind::MacApp, &bytes)
    }
}

pub(super) fn retain_child(child: Child) {
    RETAINED.with(|children| {
        children
            .borrow_mut()
            .push(OwnedChild { child: Some(child) })
    });
}

struct Children;
impl Drop for Children {
    fn drop(&mut self) {
        while let Some(mut child) = RETAINED.with(|children| children.borrow_mut().pop()) {
            let _ = child.stop();
        }
        CURRENT.with(|run| {
            run.borrow_mut().take();
        });
    }
}

// Observe with WNOWAIT and retain the direct-child anchor until the only
// reap. Live interruption/timeout uses OwnedChild::stop, never a cached PID.
fn finish_children() -> Result<Vec<bool>, UpdateError> {
    let mut results = Vec::new();
    while let Some(mut child) = RETAINED.with(|children| children.borrow_mut().pop()) {
        let deadline = Instant::now() + LIMIT;
        while !child.exited()? {
            if Instant::now() >= deadline {
                child.stop()?;
                return Err(UpdateError::RestartFailed);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        // These synthetic actors never create a same-group descendant: the
        // shared app_command/OwnedChild paths give every nested actor a new
        // group. The direct exited actor is still unreaped, so wait owns its
        // exact identity; no signal is needed for successful completion.
        let status = child
            .child
            .take()
            .unwrap()
            .wait()
            .map_err(|_| UpdateError::RecoveryRequired)?;
        results.push(status.success());
    }
    Ok(results)
}

pub(super) fn after_apply_checkpoint() -> Result<(), UpdateError> {
    let run = CURRENT
        .with(|run| run.borrow().clone())
        .ok_or(UpdateError::InvalidConfiguration)?;
    if run.plan.case == Case::AfterApplyRecovery {
        run.mark("after_apply_before_trial")?;
        run.wait("force_exit_after_apply")?;
        let journal =
            MacAppSession::read_journal(&run.state)?.ok_or(UpdateError::RecoveryRequired)?;
        require(
            run.seen("helper_observer_armed")
                && run.seen("helper_parent_exit_observed")
                && journal.phase == Phase::AwaitingHealth
                && tree_hash(&run.installed)? == run.plan.new_tree_sha256
                && RETAINED.with(|children| children.borrow().is_empty()),
        )?;
        // SAFETY: this function is called only in the compile-gated actor
        // after Run::load authenticated the compiled nonce/public fresh Mac
        // fixture and the external observer acknowledged exact ownership.
        // No trial child exists. _exit deliberately skips Rust cleanup to
        // preserve real helper crash/interruption coverage, never shipping UI.
        unsafe { libc::_exit(FORCED_AFTER_APPLY_EXIT) };
    }
    Ok(())
}

pub(super) fn parent_exit_checkpoint() -> Result<(), UpdateError> {
    let run = CURRENT
        .with(|run| run.borrow().clone())
        .ok_or(UpdateError::InvalidConfiguration)?;
    require(run.seen("helper_observer_armed"))?;
    // Called only after the unchanged production monitor returns NOTE_EXIT.
    run.mark("helper_parent_exit_observed")
}

fn startup(run: &Run, trial: bool) -> Result<(), UpdateError> {
    let (mut session, restarted) = begin_startup_admitted(true)?;
    if restarted {
        require(finish_children()? == [true])?;
        run.mark("recovery_relaunched_old")?;
        return Ok(());
    }
    let session = session.as_mut().ok_or(UpdateError::RecoveryRequired)?;
    if trial && run.plan.case == Case::TrialFailureRollback {
        run.mark("trial_failed_before_health")?;
        return Err(UpdateError::RestartFailed);
    }
    session.acknowledge_before_store()?;
    if trial {
        require(tree_hash(&run.installed)? == run.plan.new_tree_sha256)?;
        run.mark("trial_committed_before_store")?;
    } else if PackageMetadata::embedded()?.version == run.plan.new_version {
        require(
            tree_hash(&run.installed)? == run.plan.new_tree_sha256
                && MacAppSession::read_journal(&run.state)?.is_none(),
        )?;
        run.mark("committed_restart_before_store")?;
    } else {
        run.assert_old()?;
        run.mark("old_restart_before_store")?;
    }
    // Deliberately stop here. No real profile/store or UI is ever opened.
    Ok(())
}

fn prepare(run: &Run) -> Result<(), UpdateError> {
    use super::super::UpdateActivity;
    require(PackageMetadata::embedded()?.version == run.plan.old_version)?;
    run.assert_old()?;
    if run.plan.case == Case::InvalidSignature {
        require(matches!(
            run.signed_stage(true),
            Err(UpdateError::CorruptStage)
        ))?;
        run.assert_old()?;
        return run.mark("invalid_signature_rejected");
    }
    let mut staged = run.signed_stage(false)?;
    let activity = UpdateActivity::default();
    let call = activity.try_start_call()?;
    require(matches!(
        activity.try_install(),
        Err(UpdateError::CallActive)
    ))?;
    drop(call);
    let mut session = MacAppSession::lock(&run.installed, Duration::ZERO)?;
    let lease = activity.try_install()?;
    let bytes = staged.verified_bytes()?;
    let restart = prepare_restart_admitted(&mut session, &bytes, staged.version(), lease)?;
    require(matches!(
        activity.try_start_call(),
        Err(UpdateError::UpdateActive)
    ))?;
    run.mark("prepared")?;
    if run.plan.case == Case::PreparedRestart {
        // The external driver owns this actor and kills it before any helper
        // is spawned. Normal startup recovery then reads the durable journal.
        run.wait("never_continue_prepared")?;
    }
    if run.plan.case == Case::TamperedStage {
        run.wait("continue_tampered_stage")?;
    }
    restart.launch_supervisor()?;
    require(matches!(
        activity.try_start_call(),
        Err(UpdateError::UpdateActive)
    ))?;
    run.mark("helper_ready_lease_held")?;
    let mut helper = RETAINED
        .with(|children| children.borrow_mut().pop())
        .ok_or(UpdateError::RecoveryRequired)?;
    require(RETAINED.with(|children| children.borrow().is_empty()))?;
    run.offer_helper_observer(&helper)?;
    // The external driver now owns a kernel registration for this exact
    // anchored child. The old actor must actually return/exit so production
    // NOTE_EXIT can fire; staying alive as its helper's monitor is invalid.
    drop(session);
    require(activity.try_start_call().is_ok())?;
    run.mark("synthetic_shutdown")?;
    // Transfer observation only after the kernel observer is armed. Dropping
    // this actual Child does not reap or signal it; no numeric identity is
    // later reused by this exiting parent. The driver's exact exit event is
    // mandatory before any scenario or fixture cleanup can pass.
    drop(helper.child.take().ok_or(UpdateError::RecoveryRequired)?);
    Ok(())
}

pub(crate) fn dispatch() -> Result<(), UpdateError> {
    let _children = Children;
    let run = Run::load()?;
    CURRENT.with(|current| *current.borrow_mut() = Some(run.clone()));
    let args: Vec<_> = std::env::args_os().collect();
    let result = match args.get(1).and_then(|arg| arg.to_str()) {
        Some("--morpheus-mac-qa-prepare") if args.len() == 2 => prepare(&run),
        Some("--morpheus-mac-update-supervise") if args.len() == 5 => {
            require(Path::new(&args[2]) == run.installed)?;
            supervise(
                &run.installed,
                args[3].to_str().ok_or(UpdateError::RecoveryRequired)?,
                parse_parent_pid(&args[4])?,
            )
        }
        Some("--morpheus-mac-update-trial") if args.len() == 3 => startup(&run, true),
        None if args.len() == 1 => startup(&run, false),
        _ => Err(UpdateError::InvalidConfiguration),
    };
    let children = finish_children()?;
    require(children.iter().all(|passed| *passed))?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_plan_is_closed_and_requires_distinct_version_and_exact_build_nonce() {
        let json = r#"{"schema":1,"nonce":"0123456789abcdef0123456789abcdef","case":"upgrade","old_version":"0.1.0","new_version":"0.1.1","old_tree_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","new_tree_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","archive_sha256":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","public_key":"generated-public-fixture-key","signature":"generated-fixture-signature"}"#;
        let plan: Plan = serde_json::from_str(json).unwrap();
        plan.validate("0123456789abcdef0123456789abcdef").unwrap();
        assert!(plan.validate("ffffffffffffffffffffffffffffffff").is_err());
        for source in [
            json.replace("0.1.1", "0.1.0"),
            json.replace("\"upgrade\"", "\"foreign\""),
            json.replace("{", "{\"arbitrary_path\":\"/foreign\","),
            json.replacen("{", "{\"schema\":1,", 1),
        ] {
            assert!(
                serde_json::from_str::<Plan>(&source).is_err()
                    || serde_json::from_str::<Plan>(&source)
                        .unwrap()
                        .validate(&plan.nonce)
                        .is_err()
            );
        }
    }
}
