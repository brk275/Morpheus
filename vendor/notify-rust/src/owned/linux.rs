//! Existing async-io/zbus product route, driven on the actual owned std worker.
//! No per-connection driver thread is spawned. This module must be called on a
//! fresh worker with no entered Tokio runtime; it deliberately uses futures-lite
//! block_on, not zbus::block_on's optional process-global Tokio runtime.

use super::{Cancellation, Identity, Mode};
use crate::{xdg, Notification, NotificationResponse};
use futures_lite::{future, stream::StreamExt};
use std::time::Duration;
use zbus::{names::OwnedUniqueName, Connection, MatchRule, Message, MessageStream};

const CALL_BOUND: Duration = Duration::from_secs(5);
const MAX_QUEUED: usize = 4;

/// Fixed protocol error; daemon/transport errors and text are never forwarded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationFailure {
    /// Unsupported/over-limit input or session/daemon preparation failed.
    Preparation,
    /// Notify did not return a valid actual ID. Submission may already have
    /// reached the daemon; no retry or unchanged/no-delivery claim is made.
    Submission,
    /// A correlated action was malformed/unknown, or the signal stream ended.
    Callback,
    /// One exact known notification could not be withdrawn.
    Withdrawal,
    /// Match removal or socket-close request failed before local Rust teardown.
    Retirement,
}

/// Result of one awaited D-Bus or transport method, not a worker/join receipt.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProtocolRetirement {
    /// This exact method was never entered.
    #[default]
    NotEntered,
    /// This exact method returned success, with no OS-delivery/callback fence.
    Returned,
    /// This exact method returned failure, not successful removal/close.
    Failed,
}

/// Known protocol observations. On function return all private local connection,
/// stream and executor values have also been dropped on the same actual worker.
/// No count, closed flag or Stream Drop is an OS-thread/TLS join receipt.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Retirement {
    /// Exact registered rule removal method outcome.
    pub rule: ProtocolRetirement,
    /// Exact known notification-ID withdrawal method outcome.
    pub notification: ProtocolRetirement,
    /// Socket close method outcome, separately from final local Rust drop.
    pub transport: ProtocolRetirement,
}

/// The real operation outcome, after private connection/executor teardown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// Original submission admission refused before Notify entry.
    Refused,
    /// Notify returned its actual nonzero ID, with no daemon-delivery fence.
    Submitted,
    /// Cancelled after actual submission.
    Cancelled,
    /// Exact daemon/current-ID/advertised-action response for fresh app checks.
    Responded(NotificationResponse),
}

/// Actual operation outcome plus separate protocol retirement observations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Report {
    /// Actual operation outcome.
    pub outcome: Outcome,
    /// Separate method outcomes, never a worker/TLS/supervisor join.
    pub retirement: Retirement,
}

impl Report {
    /// Linux has no foreign callback retaining this UUID after this worker's
    /// private connection/executor teardown. The caller must still really join
    /// its worker/TLS and supervisor, and retain actual incomplete/panic owners.
    pub fn requires_identity_reservation(&self) -> bool {
        false
    }
}

/// Fixed operation failure plus separate method retirement observations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Failure {
    /// Earliest actual operation error; later cleanup failures stay in facts.
    pub operation: OperationFailure,
    /// Separate method outcomes, including all attempted cleanup.
    pub retirement: Retirement,
}

impl Failure {
    /// No daemon callback holds Rust/SDK/UI/text state after private teardown.
    /// This does not release a caller's unjoined actual worker/supervisor owner.
    pub fn requires_identity_reservation(self) -> bool {
        false
    }
}

// Cancellation prevents the next stage. Entered connect/preparation/native
// operations are retained and awaited to actual return, including offloaded
// Unix socket, TCP resolution and nonce work. Only the signal wait is raced.
fn direct_transport(address: &zbus::Address) -> bool {
    // Refuse command-producing and unknown transports BEFORE Builder/connection
    // admission. Never execute ibus/unixexec or substitute a different address.
    matches!(
        address.transport(),
        zbus::address::Transport::Unix(_) | zbus::address::Transport::Tcp(_)
    )
}

const DRIVER: &str = "org.freedesktop.DBus";
const DRIVER_PATH: &str = "/org/freedesktop/DBus";
const NO_OWNER: &str = "org.freedesktop.DBus.Error.NameHasNoOwner";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplyFailure {
    NoOwner,
    Refused,
}
/// zbus correlates reply serials; this additional boundary requires the actual
/// expected responder for BOTH success and MethodError. Raw error text is dropped.
fn checked_reply(result: zbus::Result<Message>, expected: &str) -> Result<Message, ReplyFailure> {
    match result {
        Ok(reply)
            if reply.header().message_type() == zbus::message::Type::MethodReturn
                && reply.header().sender().map(|name| name.as_str()) == Some(expected) =>
        {
            Ok(reply)
        }
        Err(zbus::Error::MethodError(name, _, reply))
            if reply.header().message_type() == zbus::message::Type::Error
                && reply.header().sender().map(|name| name.as_str()) == Some(expected)
                && name.as_str() == NO_OWNER
                && reply.header().error_name().map(|name| name.as_str()) == Some(NO_OWNER) =>
        {
            Err(ReplyFailure::NoOwner)
        }
        _ => Err(ReplyFailure::Refused),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OwnerMethod {
    Resolve,
    Activate,
}
trait DriverMethods {
    async fn request(&mut self, method: OwnerMethod, name: &str) -> zbus::Result<Message>;
}
struct DriverConnection<'a>(&'a Connection);
impl DriverMethods for DriverConnection<'_> {
    async fn request(&mut self, method: OwnerMethod, name: &str) -> zbus::Result<Message> {
        match method {
            OwnerMethod::Resolve => {
                self.0
                    .call_method(
                        Some(DRIVER),
                        DRIVER_PATH,
                        Some(DRIVER),
                        "GetNameOwner",
                        &(name,),
                    )
                    .await
            }
            OwnerMethod::Activate => {
                self.0
                    .call_method(
                        Some(DRIVER),
                        DRIVER_PATH,
                        Some(DRIVER),
                        "StartServiceByName",
                        &(name, 0u32),
                    )
                    .await
            }
        }
    }
}
/// Preserve normal D-Bus service activation without sending Notify twice or
/// accepting an arbitrary owner. The exact already-validated well-known name
/// is the only activation target. Each entered original RPC is fully awaited.
async fn resolve_owner(
    driver: &mut impl DriverMethods,
    name: zbus::names::WellKnownName<'_>,
    cancellation: &Cancellation,
) -> Result<Option<OwnedUniqueName>, OperationFailure> {
    if cancellation.is_cancelled() {
        return Ok(None);
    }
    let first = checked_reply(
        driver.request(OwnerMethod::Resolve, name.as_str()).await,
        DRIVER,
    );
    if cancellation.is_cancelled() {
        return Ok(None);
    }
    let reply = match first {
        Ok(reply) => reply,
        Err(ReplyFailure::NoOwner) => {
            let started = checked_reply(
                driver.request(OwnerMethod::Activate, name.as_str()).await,
                DRIVER,
            )
            .map_err(|_| OperationFailure::Preparation)?;
            let status: u32 = started
                .body()
                .deserialize()
                .map_err(|_| OperationFailure::Preparation)?;
            zbus::fdo::StartServiceReply::try_from(status)
                .map_err(|_| OperationFailure::Preparation)?;
            if cancellation.is_cancelled() {
                return Ok(None);
            }
            let resolved = checked_reply(
                driver.request(OwnerMethod::Resolve, name.as_str()).await,
                DRIVER,
            )
            .map_err(|_| OperationFailure::Preparation)?;
            if cancellation.is_cancelled() {
                return Ok(None);
            }
            resolved
        }
        Err(ReplyFailure::Refused) => return Err(OperationFailure::Preparation),
    };
    let body = reply.body();
    let raw: &str = body
        .deserialize()
        .map_err(|_| OperationFailure::Preparation)?;
    if raw.len() > 255 {
        return Err(OperationFailure::Preparation);
    }
    OwnedUniqueName::try_from(raw)
        .map(Some)
        .map_err(|_| OperationFailure::Preparation)
}

fn decode(
    message: &Message,
    server: &str,
    id: u32,
    actions: &[String],
) -> Result<Option<NotificationResponse>, OperationFailure> {
    let header = message.header();
    if header.message_type() != zbus::message::Type::Signal
        || header.sender().map(|name| name.as_str()) != Some(server)
        || header.path().map(|path| path.as_str()) != Some(xdg::NOTIFICATION_OBJECTPATH)
        || header.interface().map(|interface| interface.as_str())
            != Some(xdg::NOTIFICATION_INTERFACE)
    {
        return Ok(None);
    }
    match header.member().map(|member| member.as_str()) {
        Some("ActionInvoked") => {
            // Borrow the wire string. Allocate only a checked, bounded original
            // advertised key; never copy arbitrary daemon-provided text.
            let body = message.body();
            let (received_id, action) = body
                .deserialize::<(u32, &str)>()
                .map_err(|_| OperationFailure::Callback)?;
            if received_id != id {
                return Ok(None);
            }
            let key = actions
                .iter()
                .find(|key| key.as_str() == action)
                .ok_or(OperationFailure::Callback)?;
            Ok(Some(if key == "default" {
                NotificationResponse::Default
            } else {
                NotificationResponse::Action(key.clone())
            }))
        }
        Some("NotificationClosed") => {
            let (received_id, reason) = message
                .body()
                .deserialize::<(u32, u32)>()
                .map_err(|_| OperationFailure::Callback)?;
            Ok((received_id == id).then(|| NotificationResponse::Closed(reason.into())))
        }
        _ => Ok(None),
    }
}

async fn drive(executor: &zbus::Executor<'static>) -> Result<Report, Failure> {
    loop {
        executor.tick().await;
    }
}

impl Notification {
    /// Run one notification using the existing default zbus/async-io backend.
    ///
    /// Call only on the actual fresh owned std worker, outside any entered
    /// Tokio runtime. The private executor is driven on this same thread; no
    /// detached connection driver, SDK/UI/cache owner or foreign callback exists.
    /// The caller still retains and actually joins worker/TLS and supervisor.
    ///
    /// Original app/account submission authority is consumed immediately before
    /// Notify. A submitted message click is a NEW protected intent, never old
    /// passive submission/Answer/capture authority. Early daemon signals are
    /// bounded in the private queue until the actual Notify ID is known.
    pub fn show_owned_with_response(
        self,
        _identity: Identity,
        cancellation: Cancellation,
        mode: Mode,
        handover: impl FnOnce() -> bool,
        submitted: impl FnOnce(),
        response: impl FnOnce(NotificationResponse),
    ) -> Result<Report, Failure> {
        // Match current product's finite plain action set and text budget; never
        // forward unbounded action keys/labels into callback state.
        if self.summary.len() > 4096
            || self.body.len() > 65536
            || self
                .subtitle
                .as_ref()
                .is_some_and(|subtitle| subtitle.len() > 4096)
            || self.appname.len() > 4096
            || self.icon.len() > 4096
            || self.actions.len() % 2 != 0
            || self.actions.len() > 16
            || self.actions.chunks_exact(2).any(|pair| {
                pair[0].is_empty()
                    || pair[0].encode_utf16().take(65).count() > 64
                    || pair[1].len() > 4096
            })
            || self.id.is_some()
            || !self.hints_unique.is_empty()
            || self.hints.len() > 2
            || self.hints.iter().any(|hint| {
                !matches!(
                    hint,
                    crate::Hint::SuppressSound(_) | crate::Hint::SoundName(_)
                )
            })
            || self.hints.iter().any(|hint| {
                matches!(hint,
                crate::Hint::SoundName(name) if name.len() > 256)
            })
        {
            return Err(Failure {
                operation: OperationFailure::Preparation,
                retirement: Retirement::default(),
            });
        }
        future::block_on(async move {
            if cancellation.is_cancelled() {
                return Ok(Report {
                    outcome: Outcome::Refused,
                    retirement: Retirement::default(),
                });
            }
            let refused = || Failure {
                operation: OperationFailure::Preparation,
                retirement: Retirement::default(),
            };
            let address = zbus::Address::session().map_err(|_| refused())?;
            if !direct_transport(&address) {
                return Err(refused());
            }
            let builder = zbus::connection::Builder::address(address)
                .map_err(|_| refused())?
                .internal_executor(false)
                .max_queued(MAX_QUEUED)
                .method_timeout(CALL_BOUND);
            if cancellation.is_cancelled() {
                return Ok(Report {
                    outcome: Outcome::Refused,
                    retirement: Retirement::default(),
                });
            }
            // Do NOT race/drop this connect/authentication future. It can own
            // actual offloaded socket/DNS/nonce work. After it returns a real
            // connection, cancellation follows the same connected cleanup.
            let connection = builder.build().await.map_err(|_| refused())?;
            let executor = connection.executor().clone();
            let result = future::race(
                connected(
                    self,
                    connection,
                    cancellation,
                    mode,
                    handover,
                    submitted,
                    response,
                ),
                drive(&executor),
            )
            .await;
            // All private connection references are gone. No other thread ran
            // this executor; its exact Drop drains cancelled runnable owners.
            // This is local Rust teardown, not a task-count or native-TLS claim.
            drop(executor);
            result
        })
    }
}

async fn connected(
    notification: Notification,
    connection: Connection,
    cancellation: Cancellation,
    mode: Mode,
    handover: impl FnOnce() -> bool,
    submitted: impl FnOnce(),
    response: impl FnOnce(NotificationResponse),
) -> Result<Report, Failure> {
    let mut notification = Some(notification);
    let mut rule = None::<String>;
    let mut stream = None::<MessageStream>;
    let mut known = None::<(String, u32)>;
    let mut retirement = Retirement::default();
    let operation = async {
        if cancellation.is_cancelled() {
            return Ok(Outcome::Refused);
        }
        let bus = notification.as_ref().unwrap().bus.clone().into_name();
        // The supported dual-backend configuration returns Cow<str>, while the
        // zbus-only configuration returns WellKnownName. Borrow the exact name
        // as str and validate it before either resolver can enter an RPC.
        let bus_name: &str = &bus;
        let bus_name = zbus::names::WellKnownName::try_from(bus_name)
            .map_err(|_| OperationFailure::Preparation)?;
        let Some(server) =
            resolve_owner(&mut DriverConnection(&connection), bus_name, &cancellation).await?
        else {
            return Ok(Outcome::Refused);
        };
        let match_rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(server.as_str())
            .map_err(|_| OperationFailure::Preparation)?
            .path(xdg::NOTIFICATION_OBJECTPATH)
            .map_err(|_| OperationFailure::Preparation)?
            .interface(xdg::NOTIFICATION_INTERFACE)
            .map_err(|_| OperationFailure::Preparation)?
            .build()
            .to_string();
        // Unfiltered local stream owns no automatic rule/Drop task. We register
        // and explicitly remove only our exact rule with fixed errors below.
        stream = Some(MessageStream::from(&connection));
        if cancellation.is_cancelled() {
            return Ok(Outcome::Refused);
        }
        rule = Some(match_rule);
        let register_arguments = (rule.as_ref().unwrap().as_str(),);
        // Record actual entry before this original request. Never cancel/drop
        // its in-flight future; on any error remove only this exact rule later.
        let registered = checked_reply(
            connection
                .call_method(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    Some("org.freedesktop.DBus"),
                    "AddMatch",
                    &register_arguments,
                )
                .await,
            DRIVER,
        )
        .map_err(|_| OperationFailure::Preparation)?;
        registered
            .body()
            .deserialize::<()>()
            .map_err(|_| OperationFailure::Preparation)?;
        if cancellation.is_cancelled() || !handover() {
            return Ok(Outcome::Refused);
        }
        let current = notification.as_ref().unwrap();
        // Never select-drop this already-admitted operation. Its actual return
        // (or fixed method error) is observed before notification data is freed.
        let reply = checked_reply(
            connection
                .call_method(
                    Some(server.as_str()),
                    xdg::NOTIFICATION_OBJECTPATH,
                    Some(xdg::NOTIFICATION_INTERFACE),
                    "Notify",
                    &(
                        &current.appname,
                        0u32,
                        &current.icon,
                        &current.summary,
                        &current.body,
                        &current.actions,
                        crate::hints::hints_to_map(current),
                        i32::from(current.timeout),
                    ),
                )
                .await,
            server.as_str(),
        )
        .map_err(|_| OperationFailure::Submission)?;
        let id: u32 = reply
            .body()
            .deserialize()
            .map_err(|_| OperationFailure::Submission)?;
        if id == 0 {
            return Err(OperationFailure::Submission);
        }
        known = Some((server.as_str().to_owned(), id));
        let actions: Vec<String> = current
            .actions
            .chunks_exact(2)
            .map(|pair| pair[0].clone())
            .collect();
        // No Rust notification/body/cache owner survives through signal wait.
        drop(notification.take());
        submitted();
        if mode == Mode::SubmitOnly {
            return Ok(Outcome::Submitted);
        }
        loop {
            let receive = async {
                let stream = stream.as_mut().unwrap();
                match stream.try_next().await {
                    Ok(Some(message)) => decode(&message, server.as_str(), id, &actions),
                    _ => Err(OperationFailure::Callback),
                }
            };
            let next = future::race(async { receive.await.map(Some) }, async {
                cancellation.cancelled().await;
                Ok(None)
            })
            .await?;
            match next {
                None => return Ok(Outcome::Cancelled),
                Some(Some(event)) => return Ok(Outcome::Responded(event)),
                Some(None) => (),
            }
        }
    }
    .await;
    drop(notification.take());
    // All terminal branches attempt the independent mandatory cleanup steps.
    // Close only our exact known ID; unknown Notify errors never provoke retry.
    let withdraw =
        matches!(operation, Ok(Outcome::Cancelled | Outcome::Responded(_))) || operation.is_err();
    if let Some((server, id)) = known.as_ref().filter(|_| withdraw) {
        retirement.notification = if checked_reply(
            connection
                .call_method(
                    Some(server.as_str()),
                    xdg::NOTIFICATION_OBJECTPATH,
                    Some(xdg::NOTIFICATION_INTERFACE),
                    "CloseNotification",
                    id,
                )
                .await,
            server.as_str(),
        )
        .and_then(|reply| {
            reply
                .body()
                .deserialize::<()>()
                .map_err(|_| ReplyFailure::Refused)
        })
        .is_ok()
        {
            ProtocolRetirement::Returned
        } else {
            ProtocolRetirement::Failed
        };
    }
    drop(stream.take()); // no implicit removal task: this stream has no match_rule.
    if let Some(rule) = rule.take() {
        retirement.rule = if checked_reply(
            connection
                .call_method(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    Some("org.freedesktop.DBus"),
                    "RemoveMatch",
                    &(rule,),
                )
                .await,
            DRIVER,
        )
        .and_then(|reply| {
            reply
                .body()
                .deserialize::<()>()
                .map_err(|_| ReplyFailure::Refused)
        })
        .is_ok()
        {
            ProtocolRetirement::Returned
        } else {
            ProtocolRetirement::Failed
        };
    }
    retirement.transport = if connection.clone().close().await.is_ok() {
        ProtocolRetirement::Returned
    } else {
        ProtocolRetirement::Failed
    };
    // Private connection only: after all local streams/method futures are gone,
    // await its actual drop event, then outer same-thread executor teardown.
    connection.graceful_shutdown().await;
    if let Ok(Outcome::Responded(ref value)) = operation {
        response(value.clone());
    }
    let outcome = operation.map_err(|operation| Failure {
        operation,
        retirement,
    })?;
    let failed = if retirement.notification == ProtocolRetirement::Failed {
        Some(OperationFailure::Withdrawal)
    } else if retirement.rule == ProtocolRetirement::Failed
        || retirement.transport == ProtocolRetirement::Failed
    {
        Some(OperationFailure::Retirement)
    } else {
        None
    };
    match failed {
        Some(operation) => Err(Failure {
            operation,
            retirement,
        }),
        None => Ok(Report {
            outcome,
            retirement,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(sender: &str, path: &str, interface: &str, id: u32, action: &str) -> Message {
        Message::signal(path, interface, "ActionInvoked")
            .unwrap()
            .sender(sender)
            .unwrap()
            .build(&(id, action))
            .unwrap()
    }

    #[test]
    fn only_direct_parsed_session_transports_are_admitted_before_connection() {
        for address in [
            "unix:path=/synthetic/morpheus-bus",
            "tcp:host=127.0.0.1,port=12345",
        ] {
            assert!(direct_transport(&address.parse().unwrap()));
        }
        for address in ["unixexec:path=/synthetic/never-run", "ibus:"] {
            assert!(!direct_transport(&address.parse().unwrap()));
        }
        assert!("foreign:path=/synthetic/never-run"
            .parse::<zbus::Address>()
            .is_err());
    }

    #[test]
    fn signal_requires_exact_daemon_path_interface_id_and_advertised_action() {
        let actions = vec!["default".to_owned(), "bounded".to_owned()];
        let valid = signal(
            ":1.42",
            xdg::NOTIFICATION_OBJECTPATH,
            xdg::NOTIFICATION_INTERFACE,
            7,
            "default",
        );
        assert_eq!(
            decode(&valid, ":1.42", 7, &actions),
            Ok(Some(NotificationResponse::Default))
        );
        for other in [
            signal(
                ":1.43",
                xdg::NOTIFICATION_OBJECTPATH,
                xdg::NOTIFICATION_INTERFACE,
                7,
                "default",
            ),
            signal(":1.42", "/other", xdg::NOTIFICATION_INTERFACE, 7, "default"),
            signal(
                ":1.42",
                xdg::NOTIFICATION_OBJECTPATH,
                "org.example.Other",
                7,
                "default",
            ),
            signal(
                ":1.42",
                xdg::NOTIFICATION_OBJECTPATH,
                xdg::NOTIFICATION_INTERFACE,
                8,
                "default",
            ),
        ] {
            assert_eq!(decode(&other, ":1.42", 7, &actions), Ok(None));
        }
        let unknown = signal(
            ":1.42",
            xdg::NOTIFICATION_OBJECTPATH,
            xdg::NOTIFICATION_INTERFACE,
            7,
            "foreign",
        );
        assert_eq!(
            decode(&unknown, ":1.42", 7, &actions),
            Err(OperationFailure::Callback)
        );
        let known = signal(
            ":1.42",
            xdg::NOTIFICATION_OBJECTPATH,
            xdg::NOTIFICATION_INTERFACE,
            7,
            "bounded",
        );
        assert_eq!(
            decode(&known, ":1.42", 7, &actions),
            Ok(Some(NotificationResponse::Action("bounded".to_owned())))
        );
    }

    fn request() -> Message {
        Message::method_call(DRIVER_PATH, "SyntheticFixture")
            .unwrap()
            .sender(":1.8")
            .unwrap()
            .destination(DRIVER)
            .unwrap()
            .build(&())
            .unwrap()
    }
    fn owner_reply(sender: &str, owner: &str) -> Message {
        Message::method_return(&request().header())
            .unwrap()
            .sender(sender)
            .unwrap()
            .build(&owner)
            .unwrap()
    }
    fn no_owner(sender: &str) -> zbus::Error {
        let reply = Message::error(&request().header(), NO_OWNER)
            .unwrap()
            .sender(sender)
            .unwrap()
            .build(&())
            .unwrap();
        zbus::Error::MethodError(NO_OWNER.try_into().unwrap(), None, reply)
    }
    struct FixtureDriver {
        replies: std::collections::VecDeque<zbus::Result<Message>>,
        calls: Vec<(OwnerMethod, String)>,
        cancellation: Option<Cancellation>,
    }
    impl DriverMethods for FixtureDriver {
        async fn request(&mut self, method: OwnerMethod, name: &str) -> zbus::Result<Message> {
            self.calls.push((method, name.to_owned()));
            if method == OwnerMethod::Activate {
                if let Some(cancel) = self.cancellation.take() {
                    cancel.cancel();
                }
            }
            self.replies
                .pop_front()
                .unwrap_or(Err(zbus::Error::InvalidReply))
        }
    }
    fn fixture(replies: Vec<zbus::Result<Message>>) -> FixtureDriver {
        FixtureDriver {
            replies: replies.into(),
            calls: Vec::new(),
            cancellation: None,
        }
    }
    fn activation_reply(sender: &str, status: u32) -> Message {
        Message::method_return(&request().header())
            .unwrap()
            .sender(sender)
            .unwrap()
            .build(&status)
            .unwrap()
    }

    #[test]
    fn absent_owner_activates_only_the_exact_service_then_resolves_without_notify_replay() {
        for status in [1, 2] {
            let mut driver = fixture(vec![
                Err(no_owner(DRIVER)),
                Ok(activation_reply(DRIVER, status)),
                Ok(owner_reply(DRIVER, ":1.42")),
            ]);
            let name =
                zbus::names::WellKnownName::try_from("org.freedesktop.Notifications").unwrap();
            let outcome = future::block_on(resolve_owner(&mut driver, name, &Cancellation::new()));
            assert_eq!(outcome.unwrap().unwrap().as_str(), ":1.42");
            assert_eq!(
                driver.calls,
                vec![
                    (OwnerMethod::Resolve, "org.freedesktop.Notifications".into()),
                    (
                        OwnerMethod::Activate,
                        "org.freedesktop.Notifications".into()
                    ),
                    (OwnerMethod::Resolve, "org.freedesktop.Notifications".into())
                ]
            );
            assert!(driver.replies.is_empty());
        }
        let mut existing = fixture(vec![Ok(owner_reply(DRIVER, ":1.43"))]);
        let outcome = future::block_on(resolve_owner(
            &mut existing,
            "org.freedesktop.Notifications".try_into().unwrap(),
            &Cancellation::new(),
        ));
        assert_eq!(outcome.unwrap().unwrap().as_str(), ":1.43");
        assert_eq!(existing.calls.len(), 1);
    }

    #[test]
    fn foreign_receipts_invalid_activation_and_cancelled_owner_resolution_never_authorize_send() {
        for replies in [
            vec![Err(no_owner(":1.99"))],
            vec![Ok(owner_reply(":1.99", ":1.42"))],
            vec![Err(no_owner(DRIVER)), Ok(activation_reply(":1.99", 1))],
            vec![Err(no_owner(DRIVER)), Ok(activation_reply(DRIVER, 3))],
            vec![
                Err(no_owner(DRIVER)),
                Ok(activation_reply(DRIVER, 1)),
                Ok(owner_reply(":1.99", ":1.42")),
            ],
        ] {
            let mut driver = fixture(replies);
            let outcome = future::block_on(resolve_owner(
                &mut driver,
                "org.freedesktop.Notifications".try_into().unwrap(),
                &Cancellation::new(),
            ));
            assert_eq!(outcome, Err(OperationFailure::Preparation));
        }
        let cancellation = Cancellation::new();
        let mut cancelled = fixture(vec![Err(no_owner(DRIVER)), Ok(activation_reply(DRIVER, 1))]);
        cancelled.cancellation = Some(cancellation.clone());
        let outcome = future::block_on(resolve_owner(
            &mut cancelled,
            "org.freedesktop.Notifications".try_into().unwrap(),
            &cancellation,
        ));
        assert_eq!(outcome, Ok(None));
        assert_eq!(cancelled.calls.len(), 2);
        // The same checked-reply boundary is used by Notify/CloseNotification
        // and driver AddMatch/RemoveMatch; identical body cannot grant identity.
        assert!(checked_reply(Ok(owner_reply(":1.42", ":1.42")), ":1.42").is_ok());
        assert!(checked_reply(Ok(owner_reply(":1.99", ":1.42")), ":1.42").is_err());
        assert!(checked_reply(Err(no_owner(":1.99")), DRIVER).is_err());
        assert!(checked_reply(Ok(request()), DRIVER).is_err());
        let missing_sender = Message::method_return(&request().header())
            .unwrap()
            .build(&":1.42")
            .unwrap();
        assert!(checked_reply(Ok(missing_sender), DRIVER).is_err());
        let mismatched_error = Message::error(
            &request().header(),
            "org.freedesktop.DBus.Error.AccessDenied",
        )
        .unwrap()
        .sender(DRIVER)
        .unwrap()
        .build(&())
        .unwrap();
        assert!(matches!(
            checked_reply(
                Err(zbus::Error::MethodError(
                    NO_OWNER.try_into().unwrap(),
                    None,
                    mismatched_error
                )),
                DRIVER
            ),
            Err(ReplyFailure::Refused)
        ));
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let mut untouched = fixture(vec![Ok(owner_reply(DRIVER, ":1.42"))]);
        assert_eq!(
            future::block_on(resolve_owner(
                &mut untouched,
                "org.freedesktop.Notifications".try_into().unwrap(),
                &cancellation
            )),
            Ok(None)
        );
        assert!(untouched.calls.is_empty());
    }
}
