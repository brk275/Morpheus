//! Compile-only actors for the real installed Windows transaction. These actors
//! never construct the app, configuration, vault, account stores or media workers.
//! Shipping builds do not compile this module, and installation activation stays false.
use super::super::{Config, PackageKind, Update, UpdateActivity, UpdateFormat, stage_bytes};
use super::*;
use std::io::{Seek, SeekFrom, Write};

const PLAN_ENV: &str = "MORPHEUS_WINDOWS_UPDATE_ACCEPTANCE_PLAN";
const PLAN_HASH_ENV: &str = "MORPHEUS_WINDOWS_UPDATE_ACCEPTANCE_PLAN_SHA256";
const NONCE: &str = env!("MORPHEUS_WINDOWS_UPDATE_ACCEPTANCE_NONCE");
const CASES: [&str; 8] = [
    "invalid-signature",
    "truncated",
    "discard",
    "staged-mutation",
    "call-exclusion",
    "failed-trial",
    "interrupted-marker",
    "upgrade",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    schema: u8,
    nonce: String,
    case: String,
    root: PathBuf,
    install: PathBuf,
    payload: PathBuf,
    payload_sha256: String,
    old: Identity,
    new: Identity,
    old_executable_sha256: String,
    new_executable_sha256: String,
    signature: String,
    wrong_signature: String,
    public_key: String,
}
fn require(value: bool) -> Result<(), UpdateError> {
    if value {
        Ok(())
    } else {
        Err(UpdateError::InvalidConfiguration)
    }
}
pub(super) fn valid_case(value: &str) -> bool {
    CASES.contains(&value)
}
fn env_eq(name: &str, value: &str) -> bool {
    std::env::var(name).ok().as_deref() == Some(value)
}
pub(super) fn event_false(value: Option<&serde_json::Value>) -> bool {
    value.is_some_and(|v| v.as_bool() == Some(false) || v.as_str() == Some("false"))
}
fn guard() -> Result<(), UpdateError> {
    require(NONCE.len() == 32 && NONCE.bytes().all(|b| b.is_ascii_hexdigit()))?;
    require(option_env!("MORPHEUS_PACKAGE_FORMAT") == Some("nsis-payload-v2"))?;
    for (name, value) in [
        ("GITHUB_ACTIONS", "true"),
        ("RUNNER_ENVIRONMENT", "github-hosted"),
        ("RUNNER_OS", "Windows"),
        ("GITHUB_REPOSITORY", "brk275/Morpheus"),
        ("GITHUB_REF", "refs/heads/main"),
        ("GITHUB_EVENT_NAME", "workflow_dispatch"),
        ("GITHUB_ACTOR", "brk275"),
        ("GITHUB_TRIGGERING_ACTOR", "brk275"),
    ] {
        require(env_eq(name, value))?;
    }
    require(
        std::env::var("ImageOS")
            .ok()
            .is_some_and(|v| matches!(v.as_str(), "win25" | "win25-vs2026")),
    )?;
    let event = std::env::var_os("GITHUB_EVENT_PATH").ok_or(UpdateError::InvalidConfiguration)?;
    let event: serde_json::Value =
        serde_json::from_slice(&native::read(Path::new(&event), 1024 * 1024)?)
            .map_err(|_| UpdateError::InvalidConfiguration)?;
    let owner = event
        .pointer("/repository/owner/id")
        .and_then(|v| v.as_u64());
    require(
        owner.is_some_and(|n| n > 0)
            && owner == event.pointer("/sender/id").and_then(|v| v.as_u64())
            && event.pointer("/repository/private") == Some(&serde_json::Value::Bool(false))
            && event
                .pointer("/inputs/platform")
                .and_then(|v| v.as_str())
                .is_some_and(|v| matches!(v, "windows" | "all"))
            && event_false(event.pointer("/inputs/source_bundle")),
    )
}
impl Plan {
    fn load() -> Result<Self, UpdateError> {
        // No installed path, registry key or child process is touched before
        // owner/standard-host, immutable-plan, nonce and private-root admission.
        guard()?;
        let path =
            PathBuf::from(std::env::var_os(PLAN_ENV).ok_or(UpdateError::InvalidConfiguration)?);
        let expected =
            std::env::var(PLAN_HASH_ENV).map_err(|_| UpdateError::InvalidConfiguration)?;
        require(valid_hash(&expected) && path.file_name().is_some_and(|v| v == "plan.json"))?;
        let temp = native::Directory::open(Path::new(
            &std::env::var_os("RUNNER_TEMP").ok_or(UpdateError::InvalidConfiguration)?,
        ))?;
        let run =
            native::Directory::open(&temp.path.join(format!("morpheus-windows-updateqa-{NONCE}")))?;
        let case_root =
            native::Directory::open(path.parent().ok_or(UpdateError::InvalidConfiguration)?)?;
        require(case_root.path.parent() == Some(run.path.as_path()))?;
        let bytes = native::read(&path, 16 * 1024)?;
        require(hash(&bytes) == expected)?;
        let plan: Self =
            serde_json::from_slice(&bytes).map_err(|_| UpdateError::InvalidConfiguration)?;
        require(
            plan.schema == 1
                && plan.nonce == NONCE
                && valid_case(&plan.case)
                && case_root.path.file_name().and_then(|v| v.to_str()) == Some(plan.case.as_str())
                && plan.root == case_root.path
                && plan.install == case_root.path.join("installed")
                && plan.payload == run.path.join("new-package/Morpheus.windows-payload-v2.bin")
                && plan.old.valid()
                && plan.new.compatible(&plan.old)
                && cargo_packager_updater::semver::Version::parse(&plan.new.version)
                    .ok()
                    .zip(cargo_packager_updater::semver::Version::parse(&plan.old.version).ok())
                    .is_some_and(|(new, old)| new.cmp_precedence(&old).is_gt())
                && valid_hash(&plan.payload_sha256)
                && valid_hash(&plan.old_executable_sha256)
                && valid_hash(&plan.new_executable_sha256)
                && [&plan.signature, &plan.wrong_signature, &plan.public_key]
                    .iter()
                    .all(|s| !s.is_empty() && s.len() <= 4096),
        )?;
        native::Directory::open(&plan.install)?;
        let identity = embedded_identity()?;
        require(identity == plan.old || identity == plan.new)?;
        let executable = std::env::current_exe().map_err(|_| UpdateError::UnsupportedPackage)?;
        let executable_hash = hash(&native::read(&executable, MAX_UPDATE_BYTES)?);
        require(
            executable_hash
                == if identity == plan.old {
                    plan.old_executable_sha256.clone()
                } else {
                    plan.new_executable_sha256.clone()
                },
        )?;
        Ok(plan)
    }
    fn installed_executable(&self) -> Result<(), UpdateError> {
        let path = std::env::current_exe().map_err(|_| UpdateError::UnsupportedPackage)?;
        require(
            path.file_name().is_some_and(|n| n == "morpheus-app.exe")
                && native::Directory::open(path.parent().ok_or(UpdateError::UnsupportedPackage)?)?
                    .path
                    == self.install,
        )
    }
    fn payload(&self) -> Result<Vec<u8>, UpdateError> {
        let bytes = native::read(&self.payload, MAX_UPDATE_BYTES)?;
        require(hash(&bytes) == self.payload_sha256)?;
        Ok(bytes)
    }
    fn update(&self, signature: &str) -> Result<Update, UpdateError> {
        Ok(Update {
            config: Config {
                endpoints: Vec::new(),
                pubkey: self.public_key.clone(),
                windows: None,
            },
            body: None,
            current_version: self.old.version.clone(),
            version: self.new.version.clone(),
            date: None,
            target: "windows-x86_64".into(),
            extract_path: self.root.clone(),
            // verify/stage only. No download/network method is called here.
            download_url: "https://example.invalid/windows-payload-v2.bin"
                .parse()
                .map_err(|_| UpdateError::InvalidConfiguration)?,
            signature: signature.into(),
            timeout: Some(Duration::from_secs(10)),
            headers: Default::default(),
            format: UpdateFormat::WindowsPayloadV2,
        })
    }
    fn fact(&self, name: &str, phase: &str, identity: &Identity) -> Result<(), UpdateError> {
        require(matches!(
            name,
            "stage.json"
                | "trial.json"
                | "restart.json"
                | "probe.json"
                | "checkpoint.json"
                | "recovery.json"
                | "cleanup.json"
        ))?;
        let bytes = serde_json::to_vec(&serde_json::json!({"schema":1, "nonce":self.nonce,
            "case":self.case, "phase":phase, "version":identity.version, "cohort":identity.persistence}))
            .map_err(|_| UpdateError::RecoveryRequired)?;
        native::write_new(&self.root.join(name), &bytes)
    }
}
fn session() -> Result<WindowsSession, UpdateError> {
    let (session, relaunched) = begin_startup_with_validation(true)?;
    require(!relaunched)?;
    session.ok_or(UpdateError::UnsupportedPackage)
}
fn stage(plan: &Plan) -> Result<(), UpdateError> {
    plan.installed_executable()?;
    require(embedded_identity()? == plan.old)?;
    let mut session = session()?;
    let bytes = plan.payload()?;
    let activity = UpdateActivity::default();
    match plan.case.as_str() {
        "invalid-signature" => require(matches!(
            stage_bytes(
                plan.update(&plan.wrong_signature)?,
                PackageKind::WindowsNsis,
                &bytes
            ),
            Err(UpdateError::CorruptStage)
        ))?,
        "truncated" => require(
            bytes.len() > 1
                && matches!(
                    stage_bytes(
                        plan.update(&plan.signature)?,
                        PackageKind::WindowsNsis,
                        &bytes[..bytes.len() - 1]
                    ),
                    Err(UpdateError::CorruptStage)
                ),
        )?,
        "call-exclusion" => {
            let call = activity.try_start_call()?;
            require(matches!(
                activity.try_install(),
                Err(UpdateError::CallActive)
            ))?;
            drop(call);
        }
        _ => {
            let mut staged = stage_bytes(
                plan.update(&plan.signature)?,
                PackageKind::WindowsNsis,
                &bytes,
            )?;
            if plan.case == "discard" {
                let path = staged.file.path().to_path_buf();
                drop(staged);
                require(!path.exists())?;
            } else if plan.case == "staged-mutation" {
                let file = staged.file.as_file_mut();
                file.seek(SeekFrom::Start(0))
                    .and_then(|_| file.write_all(&[0]))
                    .and_then(|_| file.sync_all())
                    .map_err(|_| UpdateError::StageFailed)?;
                require(matches!(staged.verify(), Err(UpdateError::CorruptStage)))?;
            } else {
                let lease = activity.try_install()?;
                let exact = staged.verified_bytes()?; // Same second signature check as production.
                let restart =
                    prepare_restart_admitted(&mut session, &exact, &plan.new.version, lease)?;
                restart.launch_supervisor()?;
                require(matches!(
                    activity.try_start_call(),
                    Err(UpdateError::UpdateActive)
                ))?;
                plan.fact("stage.json", "handed-off", &plan.old)?;
                return Ok(()); // Real parent process exits; helper waits its retained OS handle.
            }
        }
    }
    require(session.journal()?.is_none())?;
    session.current()?;
    plan.fact("stage.json", "unchanged", &plan.old)
}
fn trial(plan: &Plan) -> Result<(), UpdateError> {
    plan.installed_executable()?;
    require(embedded_identity()? == plan.new)?;
    let mut session = session()?;
    if plan.case == "failed-trial" {
        return plan.fact("trial.json", "rejected-before-store", &plan.new);
    }
    require(plan.case == "upgrade")?;
    session.acknowledge_before_store()?;
    plan.fact("trial.json", "committed-before-store", &plan.new)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if optional_private(&plan.root.join("release-trial"), 64)?.as_deref()
            == Some(NONCE.as_bytes())
        {
            return Ok(());
        }
        require(Instant::now() < deadline)?;
        std::thread::sleep(Duration::from_millis(25));
    }
}
fn observe(plan: &Plan, name: &str, cleanup: bool) -> Result<(), UpdateError> {
    plan.installed_executable()?;
    let (session, relaunched) = begin_startup_with_validation(true)?;
    if relaunched {
        return Ok(());
    } // Copied recovery owns the next launch.
    let session = session.ok_or(UpdateError::UnsupportedPackage)?;
    let expected = if cleanup {
        require(session.identity == plan.old || session.identity == plan.new)?;
        &session.identity
    } else if plan.case == "upgrade" {
        &plan.new
    } else {
        &plan.old
    };
    require(session.identity == *expected)?;
    session.current()?;
    let probe = matches!(name, "probe.json" | "cleanup.json");
    if probe {
        require(
            session.journal()?.is_none()
                && fs::read_dir(&session.state.path)
                    .map_err(|_| UpdateError::RecoveryRequired)?
                    .all(|e| e.ok().is_some_and(|e| e.file_name() == "lock")),
        )?;
    }
    plan.fact(
        name,
        if probe { "terminal-clean" } else { "restarted" },
        expected,
    )
}
/// The only fault point: actual durable new marker, before replacing old EXE.
/// The driver interrupts its whole retained Job, never a numeric PID.
pub(super) fn after_marker_replace(journal: &Journal) -> Result<(), UpdateError> {
    let plan = Plan::load()?;
    if plan.case != "interrupted-marker" {
        return Ok(());
    }
    require(
        journal.phase == Phase::ApplyIntent
            && journal.old.identity == plan.old
            && journal.new.identity == plan.new,
    )?;
    plan.fact("checkpoint.json", "marker-durable-before-exe", &plan.old)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(UpdateError::RestartFailed)
}
pub(crate) fn dispatch() -> Result<(), UpdateError> {
    let plan = Plan::load()?;
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    match args.first().and_then(|a| a.to_str()) {
        Some("--morpheus-windows-qa-stage") if args.len() == 1 => stage(&plan),
        Some("--morpheus-windows-qa-probe") if args.len() == 1 => {
            observe(&plan, "probe.json", false)
        }
        // Failure cleanup still enters the actual startup/recovery state machine.
        // No journal phase, payload identity or installed registry value is forced.
        Some("--morpheus-windows-qa-recover") if args.len() == 1 => {
            observe(&plan, "recovery.json", true)
        }
        Some("--morpheus-windows-qa-cleanup") if args.len() == 1 => {
            observe(&plan, "cleanup.json", true)
        }
        Some("--morpheus-windows-update-trial") if args.len() == 2 => trial(&plan),
        Some("--morpheus-windows-update-supervisor") if args.len() == 3 => {
            require(Path::new(&args[1]) == plan.install)?;
            supervisor_admitted(
                &plan.install,
                args[2].to_str().ok_or(UpdateError::InvalidConfiguration)?,
            )
        }
        Some("--morpheus-windows-update-recovery-supervisor") if args.len() == 4 => {
            require(Path::new(&args[1]) == plan.install)?;
            recovery_supervisor_admitted(
                &plan.install,
                args[2].to_str().ok_or(UpdateError::InvalidConfiguration)?,
                args[3].to_str().ok_or(UpdateError::InvalidConfiguration)?,
            )
        }
        None => observe(&plan, "restart.json", false),
        _ => Err(UpdateError::InvalidConfiguration),
    }
}
