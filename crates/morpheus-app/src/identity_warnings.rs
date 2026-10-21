//! Identity-change warnings for the open room. Read-only.
//!
//! One scope exists per account selection and open room. The core watch
//! (`morpheus_matrix::room_identity_warnings`) starts the SDK's stored-members
//! identity feed only while the room's member list is complete in the local
//! store and reports the room as not checked otherwise; this module shows a
//! row only for a checked room with warned members, or with warnings that
//! could not all be followed. Every published result checks the scope token,
//! the exact selection (A-B-A safe) and the visible account and room. A reset
//! clears the row and the details rows and closes the details synchronously
//! on the UI thread, so no member of a previous room or account survives on
//! another surface.
//!
//! Nothing here changes trust: no identity is pinned, verified, withdrawn or
//! acknowledged, nothing is dismissed and nothing is sent. View member routes
//! an exact listed user ID only from open details, and only while the details,
//! the security gate and the scope's account and room are current when it is
//! pressed; then through the member inspector's guarded entry
//! (`member_privacy::inspect_admitted`), which checks the inspector's own
//! admission synchronously. Focus returns to the row only after the person
//! closed the details or the inspector, never after a withdrawal by a
//! protected surface, another modal or a scope change. Nothing is logged.
//!
//! The exact user ID is only a row's routing key and the listed-ID check; it
//! is never shown or read aloud, since an accepted ID may hold line-break,
//! control or direction characters and has no length bound. Every shown or
//! spoken ID (the row's summary and a member's label without a usable display
//! name, the details' ID line, the list's and View member's labels) is its
//! bounded, visibly escaped form (`IdentityWarning::display_id`).

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use morpheus_matrix::{
    AccountSelection, IdentityWarning, IdentityWarningCancellation, IdentityWarningKind,
    IdentityWarningUpdate, IdentityWarnings as Warnings,
};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tokio::runtime::Handle;

use crate::{
    AppWindow, IdentityWarningItem, IdentityWarnings, live::LiveUi, security::SecurityGate,
};

const CHECK_WITH_THEM: &str = "Check with them before sharing anything sensitive.";

/// The details dialog is open (a modal: shortcuts and other surfaces wait).
pub(crate) fn is_open(ui: &AppWindow) -> bool {
    ui.global::<IdentityWarnings>().get_open()
}

/// A protected surface's admitted entry, before its flag changes. Retain
/// the room's read-only feed, but never revive old details or return focus.
/// Slint's IdentityWarnings.withdraw has the same presentation-only effect.
pub(crate) fn withdraw(ui: &AppWindow) {
    let view = ui.global::<IdentityWarnings>();
    view.set_return_focus(false);
    view.set_awaiting_inspector(false);
    view.set_open(false);
}

/// Synchronous: clear the row and details of the shown room or account now
/// (room change, account change, lock, detach); the controller rescopes once
/// the window shows what comes next.
pub(crate) fn reset(ui: &AppWindow) {
    ui.global::<IdentityWarnings>().invoke_reset();
}

struct Scope {
    id: u64,
    selection: AccountSelection,
    room: String,
    // Ends this scope's watch and every wait inside it.
    cancellation: IdentityWarningCancellation,
}

impl Scope {
    fn alive(&self) -> bool {
        !self.cancellation.is_cancelled() && self.selection.is_current()
    }
}

pub(crate) struct IdentityWarningsUi {
    live: Arc<LiveUi>,
    runtime: Handle,
    ui: slint::Weak<AppWindow>,
    next: AtomicU64,
    current: Mutex<Option<Arc<Scope>>>,
    // The exact user IDs of the rows shown, so View member routes only a
    // listed member. UI thread writes only.
    shown: Mutex<Vec<String>>,
}

impl IdentityWarningsUi {
    pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: &Handle) -> Arc<Self> {
        let owner = Arc::new(Self {
            live,
            runtime: runtime.clone(),
            ui: ui.as_weak(),
            next: AtomicU64::new(0),
            current: Mutex::new(None),
            shown: Mutex::new(Vec::new()),
        });
        let warnings = ui.global::<IdentityWarnings>();
        let this = owner.clone();
        let weak = ui.as_weak();
        warnings.on_context_changed(move || {
            if let Some(ui) = weak.upgrade() {
                this.context_changed(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        warnings.on_reset(move || {
            if let Some(ui) = weak.upgrade() {
                this.reset(&ui);
                this.defer_context(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        warnings.on_show_details(move || {
            if let Some(ui) = weak.upgrade() {
                this.show_details(&ui);
            }
        });
        let weak = ui.as_weak();
        warnings.on_close_details(move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<IdentityWarnings>().set_open(false);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        warnings.on_view_member(move |user| {
            if let Some(ui) = weak.upgrade() {
                this.view_member(&ui, user.as_str());
            }
        });
        owner
    }

    pub(crate) fn shutdown(&self, ui: &AppWindow) {
        self.reset(ui);
    }

    fn publishable(&self, scope: &Scope, ui: &AppWindow) -> bool {
        self.current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|current| current.id == scope.id)
            && scope.alive()
            && self.live.is_live.load(Ordering::Acquire)
            && ui.get_live_session()
            && ui.get_active_account_local_id().as_str() == scope.selection.account_id()
            && ui.get_active_room_id().as_str() == scope.room
    }

    fn current_scope(&self, ui: &AppWindow) -> Option<Arc<Scope>> {
        let scope = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        self.publishable(&scope, ui).then_some(scope)
    }

    /// Synchronous: no row, summary, listed member or open details of the
    /// previous scope survives this call.
    fn reset(&self, ui: &AppWindow) {
        let previous = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(scope) = previous {
            scope.cancellation.cancel();
        }
        clear(ui);
        self.shown.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    // A reset can come before the window shows the next room or account, or
    // for a switch that is then blocked. Recreate the scope for whatever is
    // visible once this event finishes.
    fn defer_context(self: &Arc<Self>, ui: &AppWindow) {
        let this = self.clone();
        let _ = ui
            .as_weak()
            .upgrade_in_event_loop(move |ui| this.context_changed(&ui));
    }

    fn context_changed(self: &Arc<Self>, ui: &AppWindow) {
        let account = ui.get_active_account_local_id().to_string();
        let room = ui.get_active_room_id().to_string();
        let live = ui.get_live_session() && self.live.is_live.load(Ordering::Acquire);
        if live
            && self
                .current
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|scope| {
                    scope.alive() && scope.selection.account_id() == account && scope.room == room
                })
        {
            return;
        }
        self.reset(ui);
        if !live || account.is_empty() || room.is_empty() {
            return;
        }
        let Some(selection) = self.live.manager.account_selection(&account) else {
            return;
        };
        let Ok(id) = self
            .next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_add(1))
        else {
            return;
        };
        let scope = Arc::new(Scope {
            id: id + 1,
            selection,
            room,
            cancellation: IdentityWarningCancellation::new(),
        });
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(scope.clone());
        self.runtime.spawn(self.clone().watch(scope));
    }

    async fn watch(self: Arc<Self>, scope: Arc<Scope>) {
        let Ok(mut watch) = self
            .live
            .manager
            .identity_warnings_selected(&scope.selection, &scope.room, &scope.cancellation)
            .await
        else {
            return;
        };
        while let Some(update) = watch.next().await {
            if !scope.alive() {
                return;
            }
            let this = self.clone();
            let target = scope.clone();
            // Slint values are built only inside the UI closure.
            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                if this.publishable(&target, &ui) {
                    this.apply(&ui, update);
                }
            });
        }
    }

    fn apply(&self, ui: &AppWindow, update: IdentityWarningUpdate) {
        let IdentityWarningUpdate::Checked(warnings) = update else {
            // Not checked: no warning holds any more, and none is claimed.
            clear(ui);
            self.shown.lock().unwrap_or_else(|e| e.into_inner()).clear();
            return;
        };
        let view = ui.global::<IdentityWarnings>();
        let rows: Vec<IdentityWarningItem> = warnings.members.iter().map(item).collect();
        *self.shown.lock().unwrap_or_else(|e| e.into_inner()) = listed(&warnings);
        view.set_summary(summary(&warnings).into());
        view.set_overflow(overflow(&warnings).into());
        view.set_rows(ModelRc::new(VecModel::from(rows)));
        if warnings.is_empty() {
            // Withdrawn, not dismissed: focus stays where it is.
            view.set_open(false);
        }
    }

    fn show_details(&self, ui: &AppWindow) {
        let view = ui.global::<IdentityWarnings>();
        if self.current_scope(ui).is_some()
            && !view.get_summary().is_empty()
            && ui.get_identity_warnings_allowed()
        {
            // Refusal above leaves every peer alone. Once admitted, a held
            // bar action must not survive these details, even after Close.
            crate::withdraw_call_bar(ui);
            // Nor a held unread jump (matrix_links::clear below ends it too).
            crate::unread_navigation::withdraw_jump(ui);
            // The details are another modal over the conversation: a shown
            // Matrix link, its read and join end first, synchronously, as at
            // every other modal's entry (matrix-link-blocked lists them).
            crate::matrix_links::clear(ui);
            // Visible Threads/Leave are refused by the shared admission
            // above; an admitted details dialog also retires latent owners.
            crate::thread_browser::withdraw(ui);
            view.set_return_focus(false);
            view.set_awaiting_inspector(false);
            view.set_open(true);
        }
    }

    fn view_member(&self, ui: &AppWindow, user: &str) {
        let listed = self
            .shown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|shown| shown == user);
        if !listed || self.current_scope(ui).is_none() {
            return;
        }
        route_member(ui, &self.live.security_gate, user);
    }
}

/// View member from the open details, after the caller checked that `user`
/// is listed for the current scope. Everything is read synchronously when it
/// is pressed: the details must still be open and allowed (no protected
/// surface, other modal, inspector or page change) and the security gate
/// idle, or nothing happens. The details then close (the inspector excludes
/// them) and the inspector's guarded entry decides; if it refuses, the
/// person's own action returns focus to the row. `true` when the inspector
/// opened.
pub(crate) fn route_member(ui: &AppWindow, gate: &SecurityGate, user: &str) -> bool {
    let view = ui.global::<IdentityWarnings>();
    if !view.get_open() || !ui.get_identity_warnings_allowed() || gate.blocked() {
        return false;
    }
    // Retire a held bar action before closing these details or opening the
    // inspector; its protected epoch is never adopted by an older action.
    crate::withdraw_call_bar(ui);
    view.set_return_focus(false);
    view.set_awaiting_inspector(false);
    view.set_open(false);
    if crate::member_privacy::inspect_admitted(ui, gate, user) {
        // The inspector's protected entry withdrew older focus intent.
        // Arm this return only after that synchronous admission succeeded.
        view.set_awaiting_inspector(true);
        return true;
    }
    view.set_awaiting_inspector(false);
    view.set_return_focus(true);
    false
}

fn clear(ui: &AppWindow) {
    let view = ui.global::<IdentityWarnings>();
    view.set_open(false);
    view.set_awaiting_inspector(false);
    view.set_return_focus(false);
    view.set_summary(SharedString::new());
    view.set_overflow(SharedString::new());
    view.set_rows(ModelRc::new(VecModel::<IdentityWarningItem>::default()));
}

/// The exact IDs View member may route: the listed rows', byte for byte.
fn listed(warnings: &Warnings) -> Vec<String> {
    warnings
        .members
        .iter()
        .map(|member| member.user_id.clone())
        .collect()
}

/// A member as shown or read aloud: their cleaned local display name, or else
/// their bounded, escaped ID; never the exact ID.
fn label(member: &IdentityWarning) -> String {
    member.name.clone().unwrap_or_else(|| member.display_id())
}

fn detail(kind: IdentityWarningKind) -> &'static str {
    match kind {
        IdentityWarningKind::VerifiedChanged => "Identity changed after you verified it",
        IdentityWarningKind::Changed => "Identity changed since it was first seen",
    }
}

/// A details row: `user_id` is the exact ID, which routes View member only;
/// `display_id` and `label` are what is shown and read aloud.
pub(crate) fn item(member: &IdentityWarning) -> IdentityWarningItem {
    IdentityWarningItem {
        user_id: member.user_id.as_str().into(),
        display_id: member.display_id().into(),
        label: label(member).into(),
        detail: detail(member.kind).into(),
    }
}

/// The row text, or empty when nobody is warned about. States what the SDK
/// reported and suggests checking with the person; never claims a takeover or
/// that anyone is safe. When every followed member cleared but some warnings
/// could not be followed, it says so without a count: those members may still
/// be affected.
pub(crate) fn summary(warnings: &Warnings) -> String {
    if warnings.is_empty() {
        return String::new();
    }
    let count = warnings.count();
    if count == 0 {
        return format!(
            "Some members' identities changed and could not all be followed. {CHECK_WITH_THEM}"
        );
    }
    if count == 1
        && !warnings.at_least
        && let Some(member) = warnings.members.first()
    {
        let who = label(member);
        return match member.kind {
            IdentityWarningKind::VerifiedChanged => {
                format!("{who}'s identity changed after you verified it. {CHECK_WITH_THEM}")
            }
            IdentityWarningKind::Changed => format!("{who}'s identity changed. {CHECK_WITH_THEM}"),
        };
    }
    let lead = if warnings.at_least {
        format!("At least {count} members' identities changed")
    } else {
        format!("{count} members' identities changed")
    };
    let verified = warnings
        .members
        .iter()
        .any(|member| member.kind == IdentityWarningKind::VerifiedChanged);
    if verified {
        format!("{lead}, including someone you verified. {CHECK_WITH_THEM}")
    } else {
        format!("{lead}. {CHECK_WITH_THEM}")
    }
}

/// The details' last line for members not listed, or empty.
pub(crate) fn overflow(warnings: &Warnings) -> String {
    match (warnings.omitted, warnings.at_least) {
        (0, false) => String::new(),
        (0, true) => "More members may be affected.".to_owned(),
        (1, false) => "And 1 more member.".to_owned(),
        (omitted, false) => format!("And {omitted} more members."),
        (omitted, true) => format!("And at least {omitted} more members."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(user: &str, name: Option<&str>, kind: IdentityWarningKind) -> IdentityWarning {
        IdentityWarning {
            user_id: user.into(),
            name: name.map(str::to_owned),
            kind,
        }
    }

    #[test]
    fn nobody_warned_about_shows_no_row() {
        assert_eq!(summary(&Warnings::default()), "");
        assert_eq!(overflow(&Warnings::default()), "");
    }

    #[test]
    fn one_member_is_named_with_the_reported_kind_and_a_suggestion() {
        let changed = Warnings {
            members: vec![member(
                "@pat:example.org",
                Some("Pat"),
                IdentityWarningKind::Changed,
            )],
            ..Warnings::default()
        };
        assert_eq!(
            summary(&changed),
            "Pat's identity changed. Check with them before sharing anything sensitive."
        );
        let verified = Warnings {
            members: vec![member(
                "@sam:example.org",
                None,
                IdentityWarningKind::VerifiedChanged,
            )],
            ..Warnings::default()
        };
        assert_eq!(
            summary(&verified),
            "@sam:example.org's identity changed after you verified it. Check with them before sharing anything sensitive."
        );
        // No claim of a takeover, and never that anyone is safe.
        for text in [summary(&changed), summary(&verified)] {
            assert!(
                !text.contains("hack") && !text.contains("compromis") && !text.contains("safe")
            );
        }
    }

    #[test]
    fn counts_are_exact_or_explicitly_a_lower_bound() {
        let mut warnings = Warnings {
            members: vec![
                member(
                    "@sam:example.org",
                    None,
                    IdentityWarningKind::VerifiedChanged,
                ),
                member("@pat:example.org", None, IdentityWarningKind::Changed),
            ],
            omitted: 3,
            at_least: false,
        };
        assert_eq!(
            summary(&warnings),
            "5 members' identities changed, including someone you verified. Check with them before sharing anything sensitive."
        );
        assert_eq!(overflow(&warnings), "And 3 more members.");
        warnings.at_least = true;
        assert!(summary(&warnings).starts_with("At least 5 members'"));
        assert_eq!(overflow(&warnings), "And at least 3 more members.");
        warnings.omitted = 1;
        warnings.at_least = false;
        assert_eq!(overflow(&warnings), "And 1 more member.");
    }

    #[test]
    fn warnings_that_could_not_be_followed_stay_visible_at_zero() {
        // Every followed member cleared, but some reported warnings were
        // never followed: a row stays, with no count and no all-clear.
        let unfollowed = Warnings {
            members: Vec::new(),
            omitted: 0,
            at_least: true,
        };
        let text = summary(&unfollowed);
        assert_eq!(
            text,
            "Some members' identities changed and could not all be followed. Check with them before sharing anything sensitive."
        );
        assert!(!text.chars().any(|character| character.is_ascii_digit()));
        assert!(!text.contains("safe") && !text.contains("No one"));
        assert_eq!(overflow(&unfollowed), "More members may be affected.");
        // Only a report with nothing followed and nothing unfollowed hides
        // the row.
        assert!(!unfollowed.is_empty());
        assert_eq!(summary(&Warnings::default()), "");
    }

    #[test]
    fn rows_carry_the_exact_id_for_routing_and_a_label_for_display() {
        let row = item(&member(
            "@pat:example.org",
            Some("Pat"),
            IdentityWarningKind::Changed,
        ));
        assert_eq!(row.user_id.as_str(), "@pat:example.org");
        assert_eq!(row.label.as_str(), "Pat");
        assert_eq!(
            row.detail.as_str(),
            "Identity changed since it was first seen"
        );
    }

    /// Only printable ASCII, spaces and the one shortening mark.
    fn safe(text: &str) -> bool {
        text.chars().all(|c| matches!(c, ' '..='~' | '…'))
    }

    /// IWCOMP01: an accepted ID with line breaks, tabs, direction characters,
    /// non-ASCII or more than 255 bytes is never shown or read aloud as it is.
    /// Without a usable display name (none, blank or unreadable: all `None`
    /// here), the label and the summary use the bounded, escaped ID, as the
    /// row's shown ID always does; a safe name is kept. The row's routing key
    /// and the listed IDs View member may route stay byte for byte.
    #[test]
    fn ids_are_shown_bounded_and_escaped_but_route_exactly() {
        use morpheus_matrix::MAX_IDENTITY_ID_DISPLAY_CHARS;
        let long = format!("@{}:example.org", "a".repeat(300));
        let wide = format!("@{}:example.org", "\u{540d}".repeat(200));
        let users = [
            "@pat\nadmin:example.org",
            "@pat\tx:example.org",
            "@a\u{202e}gro.x:evil.org",
            "@\u{fc}\u{2066}x:example.org",
            long.as_str(),
            wide.as_str(),
        ];
        for user in users {
            // Accepted exactly as the SDK parses a member's ID.
            let typed = matrix_sdk::ruma::UserId::parse(user).unwrap();
            assert_eq!(typed.as_str(), user);
            let unnamed = member(user, None, IdentityWarningKind::Changed);
            let shown = unnamed.display_id();
            assert!(safe(&shown) && shown != user, "{shown:?}");
            assert!(shown.chars().count() <= MAX_IDENTITY_ID_DISPLAY_CHARS);
            let row = item(&unnamed);
            assert_eq!(row.user_id.as_str(), user, "the routing key is exact");
            assert_eq!(row.display_id.as_str(), shown);
            assert_eq!(row.label.as_str(), shown, "no usable name: the shown ID");
            let one = Warnings {
                members: vec![unnamed.clone()],
                ..Warnings::default()
            };
            let text = summary(&one);
            let expected = format!("{shown}'s identity changed. {CHECK_WITH_THEM}");
            assert_eq!(text, expected);
            assert!(safe(&text));
            assert_eq!(listed(&one), [user], "View member may route the exact ID");
            let named = item(&member(user, Some("Pat"), IdentityWarningKind::Changed));
            assert_eq!(named.user_id.as_str(), user);
            assert_eq!(named.label.as_str(), "Pat");
            assert_eq!(named.display_id.as_str(), shown);
        }
        let first = member(users[0], None, IdentityWarningKind::Changed);
        assert_eq!(first.display_id(), r"@pat\nadmin:example.org");
        // In the window the exact ID only routes View member; every shown or
        // spoken ID is the escaped form.
        let window = include_str!("../ui/identity-warnings.slint");
        let code: Vec<&str> = window
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect();
        let raw: Vec<&str> = code
            .iter()
            .copied()
            .filter(|line| line.contains("user-id"))
            .collect();
        let route = "IdentityWarnings.view-member(IdentityWarnings.rows[root.selected].user-id);";
        assert_eq!(raw.len(), 2, "{raw:?}");
        assert_eq!(raw[0].trim(), "user-id: string,");
        assert_eq!(raw[1].trim(), route);
        for shown in [
            "+ IdentityWarnings.rows[root.selected].display-id + \", \"",
            "accessible-label: row.label + \", \" + row.display-id + \", \" + row.detail;",
            "text: row.display-id + \" · \" + row.detail;",
        ] {
            assert!(code.iter().any(|line| line.trim() == shown), "{shown}");
        }
    }
}
