//! The installed font browser of Settings › Appearance › Fonts.
//!
//! It lists the families of the renderer's own font collection (the pinned
//! Slint/fontique `shared_collection`, the one text is drawn with). Nothing
//! else is read: no folder is walked and nothing is downloaded or
//! registered. Reading the families can be slow on some systems, so the
//! collection is cloned on the UI thread and read on a worker. Only one read
//! runs at a time: Refresh during a read is merged into it, and only the
//! newest read's result is kept. The catalog keeps
//! at most [`MAX_FAMILIES`] families, and a search shows at most
//! [`MAX_RESULTS`]; both say what they left out. Typing is searched once it
//! settles.
//!
//! The browser fills the existing font roles, through the same keys the
//! text fields edit:
//! - the interface (`appearance.font_family`);
//! - messages (`typography.message_font_family`);
//! - the composers (`typography.composer_font_family`);
//! - monospaced text (`typography.monospace_font_family`).
//! The generic names `sans-serif`, `serif` and `monospace` are offered
//! first, as the window resolves them. A choice is an ordinary Settings
//! edit: on the Settings page the theme studio drafts it (shown at once,
//! saved by Apply), and an invalid file keeps the last valid look. Fonts
//! are global; account and room levels hold theme and colors only.
//!
//! Each opened browser has a scope (`theme_studio::scope`). A choice is
//! written only while that scope is current, so an account switch (A -> B
//! -> A included), a detach or a new settings context while it is open
//! refuses the choice. Results of a catalog read that a newer read
//! superseded are dropped. A read that ends after the browser was closed
//! only fills the cache.
//!
//! This toolkit keeps a snapshot of operating-system fonts. Refresh reads
//! the families currently known to the renderer, including registered
//! application fonts; it cannot rescan installed system fonts. Restart
//! Morpheus after installing or removing system fonts. A family typed by
//! hand that is absent from the snapshot is kept; the renderer falls back.

use std::cell::RefCell;
use std::collections::HashSet;
use std::time::Duration;

use morpheus_config::{Config, ConfigStore};
use slint::fontique_011::fontique;
use slint::{ModelRc, VecModel};

use super::scope::{self, Scope};
use crate::{AppWindow, FontFamilyItem};

/// Most installed families the catalog keeps; more are counted.
pub(crate) const MAX_FAMILIES: usize = 4096;
/// Most families one search shows; more are counted.
pub(crate) const MAX_RESULTS: usize = 200;
/// Most distinct weights listed per family.
const MAX_WEIGHTS: usize = 12;
const MAX_QUERY_CHARS: usize = 64;
/// Keystrokes closer together than this are searched once.
const QUERY_SETTLE: Duration = Duration::from_millis(120);
/// The generic names the window resolves through the collection.
pub(crate) const GENERICS: [&str; 3] = ["sans-serif", "serif", "monospace"];
const SAMPLE: &str = "The quick brown fox jumps over the lazy dog · 0123456789 · Ag Il1 O0";

/// One installed family as the collection reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Family {
    pub(crate) name: String,
    /// The system lists it for the generic monospace family, or its name
    /// says "Mono". Neither is a measurement of glyph widths.
    pub(crate) monospace: bool,
    /// The weights its fonts have, sorted, at most [`MAX_WEIGHTS`].
    pub(crate) weights: Vec<u16>,
}

/// A generic name and the installed family it resolves to now, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Generic {
    pub(crate) name: &'static str,
    pub(crate) resolves_to: Option<String>,
}

/// The installed families, bounded and sorted by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Catalog {
    families: Vec<Family>,
    generics: Vec<Generic>,
    /// Families left out by [`MAX_FAMILIES`] or because the configuration
    /// could not store their name.
    omitted: usize,
}

/// One search: the families shown and how many matched in all.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Search<'a> {
    pub(crate) generics: Vec<&'a Generic>,
    pub(crate) families: Vec<&'a Family>,
    pub(crate) matched: usize,
}

/// Whether `name` is a family name the configuration accepts: what
/// `validate_font_family` checks (1 to 128 bytes, no control or direction
/// characters).
pub(crate) fn storable(name: &str) -> bool {
    !name.trim().is_empty()
        && name.len() <= 128
        && !name.chars().any(|character| {
            character.is_control()
                || matches!(character, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
}

fn is_generic(name: &str) -> bool {
    GENERICS
        .iter()
        .any(|generic| generic.eq_ignore_ascii_case(name.trim()))
}

impl Catalog {
    /// A catalog of `families` (in any order, possibly repeated) and the
    /// generic names' resolutions. Bounded; names the configuration could not
    /// store are left out and counted.
    pub(crate) fn from_families(
        families: impl IntoIterator<Item = Family>,
        generics: Vec<Generic>,
    ) -> Self {
        let mut list = Vec::new();
        let mut omitted = 0;
        for family in families {
            if !storable(&family.name) || list.len() >= MAX_FAMILIES {
                omitted += 1;
                continue;
            }
            list.push(family);
        }
        list.sort_by(|left: &Family, right: &Family| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.name.cmp(&right.name))
        });
        list.dedup_by(|later, earlier| later.name == earlier.name);
        Self {
            families: list,
            generics,
            omitted,
        }
    }

    /// Read the families of a clone of the renderer's collection. Runs on a
    /// worker; touches no file and nothing outside the collection.
    pub(crate) fn read(mut collection: fontique::Collection) -> Self {
        let monospace: HashSet<fontique::FamilyId> = collection
            .generic_families(fontique::GenericFamily::Monospace)
            .take(MAX_FAMILIES)
            .collect();
        let generics = GENERICS
            .iter()
            .map(|&name| {
                let generic = match name {
                    "serif" => fontique::GenericFamily::Serif,
                    "monospace" => fontique::GenericFamily::Monospace,
                    _ => fontique::GenericFamily::SansSerif,
                };
                let first = collection.generic_families(generic).next();
                let resolves_to = first
                    .and_then(|id| collection.family_name(id))
                    .map(str::to_owned);
                Generic { name, resolves_to }
            })
            .collect();
        // Names first (bounded), then each family's fonts.
        let mut names = Vec::new();
        let mut beyond = 0usize;
        for name in collection.family_names() {
            if names.len() < MAX_FAMILIES {
                names.push(name.to_owned());
            } else {
                beyond += 1;
            }
        }
        let families: Vec<Family> = names
            .into_iter()
            .map(|name| {
                let info = collection.family_by_name(&name);
                let listed_mono = info
                    .as_ref()
                    .is_some_and(|info| monospace.contains(&info.id()));
                let mut weights: Vec<u16> = info
                    .as_ref()
                    .map(|info| {
                        info.fonts()
                            .iter()
                            .map(|font| font.weight().value().round().clamp(1.0, 1000.0) as u16)
                            .collect()
                    })
                    .unwrap_or_default();
                weights.sort_unstable();
                weights.dedup();
                weights.truncate(MAX_WEIGHTS);
                let monospace = listed_mono || name.to_lowercase().contains("mono");
                Family {
                    name,
                    monospace,
                    weights,
                }
            })
            .collect();
        let mut catalog = Self::from_families(families, generics);
        catalog.omitted += beyond;
        catalog
    }

    pub(crate) fn len(&self) -> usize {
        self.families.len()
    }

    pub(crate) fn omitted(&self) -> usize {
        self.omitted
    }

    /// The installed family named `name`, ignoring case and surrounding
    /// space.
    pub(crate) fn find(&self, name: &str) -> Option<&Family> {
        let name = name.trim();
        self.families
            .iter()
            .find(|family| family.name.eq_ignore_ascii_case(name))
    }

    /// The generic names and families matching every word of `query`,
    /// ignoring case, optionally only monospaced ones. At most
    /// [`MAX_RESULTS`] families are returned; `matched` counts them all.
    pub(crate) fn search(&self, query: &str, monospace_only: bool) -> Search<'_> {
        let query = query.trim().to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        let matches = |text: &str| {
            let text = text.to_lowercase();
            words.iter().all(|word| text.contains(word))
        };
        let generics = self
            .generics
            .iter()
            .filter(|generic| !monospace_only || generic.name == "monospace")
            .filter(|generic| {
                matches(&format!(
                    "{} {}",
                    generic.name,
                    generic.resolves_to.as_deref().unwrap_or_default()
                ))
            })
            .collect();
        let mut families = Vec::new();
        let mut matched = 0;
        for family in &self.families {
            if (monospace_only && !family.monospace) || !matches(&family.name) {
                continue;
            }
            matched += 1;
            if families.len() < MAX_RESULTS {
                families.push(family);
            }
        }
        Search {
            generics,
            families,
            matched,
        }
    }
}

/// A font role the browser fills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Interface,
    Message,
    Composer,
    Monospace,
}

impl Role {
    pub(crate) const ALL: [Self; 4] = [
        Self::Interface,
        Self::Message,
        Self::Composer,
        Self::Monospace,
    ];

    fn from_index(index: i32) -> Option<Self> {
        usize::try_from(index)
            .ok()
            .and_then(|index| Self::ALL.get(index))
            .copied()
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Interface => "interface",
            Self::Message => "message",
            Self::Composer => "composer",
            Self::Monospace => "monospaced",
        }
    }

    /// The family `config` sets for this role itself, if any.
    pub(crate) fn own(self, config: &Config) -> Option<&str> {
        match self {
            Self::Interface => Some(&config.appearance.font_family),
            Self::Message => config.typography.message_font_family.as_deref(),
            Self::Composer => config.typography.composer_font_family.as_deref(),
            Self::Monospace => Some(&config.typography.monospace_font_family),
        }
    }

    /// The family the role draws with, inheritance included.
    pub(crate) fn effective(self, config: &Config) -> &str {
        match self {
            Self::Interface => &config.appearance.font_family,
            Self::Message => config.typography.message_family(&config.appearance),
            Self::Composer => config.typography.composer_family(&config.appearance),
            Self::Monospace => &config.typography.monospace_font_family,
        }
    }

    /// Set this role to `family`, or `None` to inherit (message and
    /// composer) or return to the built-in family (interface, monospace).
    pub(crate) fn set(self, config: &mut Config, family: Option<&str>) {
        let defaults = Config::default();
        let family = family.map(|family| family.trim().to_owned());
        match self {
            Self::Interface => {
                config.appearance.font_family = family.unwrap_or(defaults.appearance.font_family);
            }
            Self::Message => config.typography.message_font_family = family,
            Self::Composer => config.typography.composer_font_family = family,
            Self::Monospace => {
                config.typography.monospace_font_family =
                    family.unwrap_or(defaults.typography.monospace_font_family);
            }
        }
    }

    /// Where the role's family comes from, as fixed text.
    pub(crate) fn source(self, config: &Config) -> String {
        let effective = self.effective(config);
        match (self, self.own(config)) {
            (Self::Message, None) => format!("Follows the interface font ({effective})"),
            (Self::Composer, None) if config.typography.message_font_family.is_some() => {
                format!("Follows the message font ({effective})")
            }
            (Self::Composer, None) => {
                format!("Follows the message font, which follows the interface font ({effective})")
            }
            (_, Some(own)) if own == Role::default_for(self) => {
                format!("Built-in default ({own})")
            }
            (_, Some(own)) => format!("Your choice: {own}"),
            (_, None) => String::new(),
        }
    }

    fn default_for(role: Self) -> &'static str {
        match role {
            Self::Interface => "Inter",
            Self::Monospace => "monospace",
            Self::Message | Self::Composer => "",
        }
    }

    /// Sample unweighted interface and message text with the same body
    /// weight inherited from the window, and composer text with its explicit
    /// body weight. Explicit heading, sender and metadata weights remain
    /// independent; the monospaced sample stays at its regular weight.
    fn sample_metrics(self, config: &Config) -> (i32, i32) {
        let typography = &config.typography;
        let body = typography.body_weight.map_or(400, i32::from);
        match self {
            Self::Interface => (i32::from(typography.sidebar_size), body),
            Self::Message => (i32::from(typography.message_size), body),
            Self::Composer => (
                i32::from(typography.composer_size.unwrap_or(typography.message_size)),
                body,
            ),
            Self::Monospace => (
                i32::from(typography.timestamp_size.unwrap_or(10)).max(11),
                400,
            ),
        }
    }
}

/// An open browser: the role it fills and the scope it was opened in.
struct Session {
    role: Role,
    scope: Scope,
}

#[derive(Default)]
struct Browser {
    store: Option<ConfigStore>,
    catalog: Option<Catalog>,
    /// The catalog read in flight; a newer read supersedes it.
    loading: Option<u64>,
    next_load: u64,
    open: Option<Session>,
    /// The family whose sample is shown.
    highlighted: String,
}

thread_local! {
    // Slint callbacks run on the UI thread only.
    static BROWSER: RefCell<Browser> = RefCell::new(Browser::default());
    static QUERY_TIMER: slint::Timer = slint::Timer::default();
}

fn with<R>(update: impl FnOnce(&mut Browser) -> R) -> R {
    BROWSER.with(|browser| update(&mut browser.borrow_mut()))
}

/// The accepted look's configuration, the one the fields show.
fn shown_config() -> Config {
    crate::appearance::remembered_theme_source()
        .map(|(config, _)| config)
        .unwrap_or_default()
}

pub(super) fn register(ui: &AppWindow, store: &ConfigStore) {
    with(|browser| browser.store = Some(store.clone()));
    let weak = ui.as_weak();
    ui.on_font_browser_show(move |role| {
        if let Some(ui) = weak.upgrade() {
            open(&ui, role);
        }
    });
    let weak = ui.as_weak();
    ui.on_font_browser_close(move || {
        if let Some(ui) = weak.upgrade() {
            close(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_font_browser_query_changed(move |text| {
        let Some(ui) = weak.upgrade() else { return };
        let bounded: String = text.chars().take(MAX_QUERY_CHARS).collect();
        if bounded.as_str() != text.as_str() {
            ui.set_font_browser_query(bounded.into());
        }
        // Typing is searched once it settles.
        let weak = ui.as_weak();
        QUERY_TIMER.with(|timer| {
            timer.start(slint::TimerMode::SingleShot, QUERY_SETTLE, move || {
                if let Some(ui) = weak.upgrade() {
                    project(&ui);
                }
            });
        });
    });
    let weak = ui.as_weak();
    ui.on_font_browser_monospace_toggled(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_font_browser_monospace_only(!ui.get_font_browser_monospace_only());
            project(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_font_browser_highlight(move |family| {
        if let Some(ui) = weak.upgrade() {
            with(|browser| browser.highlighted = family.to_string());
            project(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_font_browser_use(move |family| {
        if let Some(ui) = weak.upgrade() {
            choose(&ui, Some(family.as_str()));
        }
    });
    let weak = ui.as_weak();
    ui.on_font_browser_inherit(move || {
        if let Some(ui) = weak.upgrade() {
            choose(&ui, None);
        }
    });
    let weak = ui.as_weak();
    ui.on_font_browser_refresh(move || {
        if let Some(ui) = weak.upgrade() {
            load(&ui);
        }
    });
}

fn open(ui: &AppWindow, role: i32) {
    let Some(role) = Role::from_index(role) else {
        return;
    };
    let current = role.effective(&shown_config()).to_owned();
    let scope = scope::capture(ui);
    let needs_catalog = with(|browser| {
        browser.open = Some(Session { role, scope });
        browser.highlighted = current;
        browser.catalog.is_none() && browser.loading.is_none()
    });
    ui.set_font_browser_role(role as i32);
    ui.set_font_browser_open(true);
    ui.set_font_browser_notice("".into());
    if needs_catalog {
        load(ui);
    }
    project(ui);
}

fn close(ui: &AppWindow) {
    QUERY_TIMER.with(slint::Timer::stop);
    with(|browser| browser.open = None);
    ui.set_font_browser_open(false);
}

/// Read the installed families on a worker. The collection is the
/// renderer's, cloned here on the UI thread. A read already running is not
/// started again.
fn load(ui: &AppWindow) {
    let Some(id) = with(|browser| {
        if browser.loading.is_some() {
            return None;
        }
        browser.next_load = browser.next_load.wrapping_add(1);
        browser.loading = Some(browser.next_load);
        Some(browser.next_load)
    }) else {
        project(ui);
        return;
    };
    let collection = slint::fontique_011::shared_collection();
    let weak = ui.as_weak();
    let spawned = std::thread::Builder::new()
        .name("morpheus-font-catalog".into())
        .spawn(move || {
            let catalog = Catalog::read(collection);
            let _ = weak.upgrade_in_event_loop(move |ui| finish_load(&ui, id, catalog));
        });
    if spawned.is_err() {
        with(|browser| browser.loading = None);
        ui.set_font_browser_notice("The font list could not be read.".into());
    }
    project(ui);
}

/// A catalog read ended. Only the newest read is kept; one a newer read
/// superseded is dropped. A read that ends after the browser was closed
/// still fills the cache, and nothing is shown or chosen from it.
fn finish_load(ui: &AppWindow, id: u64, catalog: Catalog) {
    let current = with(|browser| {
        if browser.loading != Some(id) {
            return false;
        }
        browser.loading = None;
        browser.catalog = Some(catalog);
        true
    });
    if current && with(|browser| browser.open.is_some()) {
        project(ui);
    }
}

/// Write the open browser's role: `family`, or inherit with `None`. Only in
/// the scope the browser was opened in; otherwise the choice is refused and
/// the browser closes.
fn choose(ui: &AppWindow, family: Option<&str>) {
    let Some((role, opened)) = with(|browser| {
        browser
            .open
            .as_ref()
            .map(|session| (session.role, session.scope.clone()))
    }) else {
        return;
    };
    if !scope::current(ui, &opened) {
        close(ui);
        ui.set_font_browser_notice(
            "The account or settings changed while the font list was open; nothing was changed. Open it again to choose."
                .into(),
        );
        return;
    }
    if let Some(family) = family
        && !storable(family)
    {
        ui.set_font_browser_notice("That family name cannot be stored.".into());
        return;
    }
    let Some(store) = with(|browser| browser.store.clone()) else {
        return;
    };
    let saved = crate::save_config_edit(ui, &store, |config| {
        let before = config.clone();
        role.set(config, family);
        *config != before
    });
    let drafted = super::drafting();
    let name = family.unwrap_or("the inherited font");
    let text = match (saved, drafted) {
        (Some(_), true) => format!(
            "The {} font is now {name} in the preview; Apply in the bar saves it.",
            role.label()
        ),
        (Some(_), false) => format!("The {} font is now {name}.", role.label()),
        (None, _) => format!("The {} font was not changed.", role.label()),
    };
    ui.set_font_browser_notice(text.into());
    if let Some(family) = family {
        with(|browser| browser.highlighted = family.to_owned());
    }
    project(ui);
}

/// The family the window draws for `name`: a generic name resolved as the
/// window resolves it, else the name itself.
fn resolved(catalog: Option<&Catalog>, name: &str) -> String {
    catalog
        .and_then(|catalog| {
            catalog
                .generics
                .iter()
                .find(|generic| generic.name.eq_ignore_ascii_case(name.trim()))
                .and_then(|generic| generic.resolves_to.clone())
        })
        .unwrap_or_else(|| name.to_owned())
}

/// Publish the open browser: results, counts, the current family and its
/// source, and a sample of the highlighted family at the role's size and
/// weight.
fn project(ui: &AppWindow) {
    let config = shown_config();
    let query = ui.get_font_browser_query();
    let monospace_only = ui.get_font_browser_monospace_only();
    BROWSER.with(|browser| {
        let browser = browser.borrow();
        let Some(session) = browser.open.as_ref() else {
            return;
        };
        let role = session.role;
        let current = role.effective(&config);
        let catalog = browser.catalog.as_ref();
        let mut items = Vec::new();
        let status = match catalog {
            None if browser.loading.is_some() => "Reading the app's font list…".to_owned(),
            None => "The app's font list is not available.".to_owned(),
            Some(catalog) => {
                let search = catalog.search(query.as_str(), monospace_only);
                for generic in &search.generics {
                    items.push(FontFamilyItem {
                        name: generic.name.into(),
                        detail: match &generic.resolves_to {
                            Some(family) => format!("System default, now {family}"),
                            None => "System default; the renderer chooses".to_owned(),
                        }
                        .into(),
                        generic: true,
                        monospace: generic.name == "monospace",
                        current: generic.name.eq_ignore_ascii_case(current),
                    });
                }
                for family in &search.families {
                    let weights = family
                        .weights
                        .iter()
                        .map(u16::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    items.push(FontFamilyItem {
                        name: family.name.as_str().into(),
                        detail: match (family.monospace, weights.is_empty()) {
                            (true, false) => format!("Monospaced · weights {weights}"),
                            (true, true) => "Monospaced".to_owned(),
                            (false, false) => format!("Weights {weights}"),
                            (false, true) => String::new(),
                        }
                        .into(),
                        generic: false,
                        monospace: family.monospace,
                        current: family.name.eq_ignore_ascii_case(current),
                    });
                }
                let hidden = search.matched - search.families.len();
                let mut status = format!(
                    "{} of {} families known to this app match.",
                    search.matched,
                    catalog.len()
                );
                if hidden > 0 {
                    status.push_str(&format!(
                        " {hidden} more are not listed; type more of a name."
                    ));
                }
                if catalog.omitted() > 0 {
                    status.push_str(&format!(
                        " {} families were not read (the list holds {}).",
                        catalog.omitted(),
                        MAX_FAMILIES
                    ));
                }
                status
            }
        };
        ui.set_font_browser_results(ModelRc::new(VecModel::from(items)));
        ui.set_font_browser_status(status.into());
        ui.set_font_browser_role_label(format!("Choose the {} font", role.label()).into());
        ui.set_font_browser_current(role.source(&config).into());
        // A family typed by hand that is not installed is kept and said so.
        let missing =
            catalog.is_some_and(|catalog| !is_generic(current) && catalog.find(current).is_none());
        ui.set_font_browser_current_missing(missing);
        let highlighted = if browser.highlighted.is_empty() {
            current.to_owned()
        } else {
            browser.highlighted.clone()
        };
        ui.set_font_browser_previewed(highlighted.as_str().into());
        let (size, weight) = role.sample_metrics(&config);
        let drawn = resolved(catalog, &highlighted);
        let weights = catalog
            .and_then(|catalog| catalog.find(&drawn))
            .map(|family| family.weights.clone())
            .unwrap_or_default();
        let synthesized = !weights.is_empty() && !weights.contains(&(weight as u16));
        ui.set_font_browser_sample_family(drawn.as_str().into());
        ui.set_font_browser_sample_size(size);
        ui.set_font_browser_sample_weight(weight);
        ui.set_font_browser_sample_text(SAMPLE.into());
        ui.set_font_browser_sample_label(
            format!(
                "{highlighted} at {size} px, weight {weight}{}",
                if synthesized {
                    "; this family has no font of that weight, so the renderer uses the nearest one"
                } else {
                    ""
                }
            )
            .into(),
        );
    });
}

/// After the shown look or the active account changed: close a browser
/// whose scope ended (an account switch, A -> B -> A included, or a
/// detach), else show the new current values. Runs from
/// `appearance::apply_theme_mode`, after the accepted or previewed look is
/// remembered.
pub(super) fn refresh(ui: &AppWindow) {
    let Some(opened) = with(|browser| browser.open.as_ref().map(|session| session.scope.clone()))
    else {
        return;
    };
    if scope::current(ui, &opened) {
        project(ui);
    } else {
        close(ui);
        ui.set_font_browser_notice(
            "The font list closed because the account changed; nothing was changed.".into(),
        );
    }
}

/// The catalog as the tests built it, through the production search.
#[cfg(test)]
pub(crate) fn set_catalog_for_test(catalog: Catalog) {
    with(|browser| {
        browser.loading = None;
        browser.catalog = Some(catalog);
    });
}

#[cfg(test)]
pub(super) fn reset_for_test() {
    QUERY_TIMER.with(slint::Timer::stop);
    with(|browser| *browser = Browser::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn family(name: &str, monospace: bool, weights: &[u16]) -> Family {
        Family {
            name: name.to_owned(),
            monospace,
            weights: weights.to_vec(),
        }
    }

    fn catalog() -> Catalog {
        Catalog::from_families(
            [
                family("Inter", false, &[400, 700]),
                family("JetBrains Mono", true, &[400]),
                family("inter display", false, &[300]),
                family("Inter", false, &[400, 700]),
                family("Bad\u{202e}Name", false, &[]),
            ],
            vec![
                Generic {
                    name: "sans-serif",
                    resolves_to: Some("Inter".into()),
                },
                Generic {
                    name: "monospace",
                    resolves_to: Some("JetBrains Mono".into()),
                },
            ],
        )
    }

    #[test]
    fn the_catalog_is_sorted_deduplicated_and_refuses_unstorable_names() {
        let catalog = catalog();
        let names: Vec<_> = catalog
            .families
            .iter()
            .map(|family| family.name.as_str())
            .collect();
        assert_eq!(names, ["Inter", "inter display", "JetBrains Mono"]);
        assert_eq!(catalog.omitted(), 1);
        assert!(catalog.find(" INTER ").is_some());
        assert!(catalog.find("Not Installed").is_none());
    }

    #[test]
    fn searches_match_every_word_and_filter_monospace() {
        let catalog = catalog();
        let search = catalog.search("inter dis", false);
        assert_eq!(search.matched, 1);
        assert_eq!(search.families[0].name, "inter display");
        let mono = catalog.search("", true);
        assert_eq!(mono.families.len(), 1);
        assert_eq!(mono.generics.len(), 1);
        assert_eq!(mono.generics[0].name, "monospace");
        // Generic names match by their resolution too.
        assert_eq!(catalog.search("jetbrains", false).generics.len(), 1);
    }

    #[test]
    fn the_catalog_and_results_are_bounded() {
        let many =
            (0..MAX_FAMILIES + 10).map(|index| family(&format!("Family {index:05}"), false, &[]));
        let catalog = Catalog::from_families(many, Vec::new());
        assert_eq!(catalog.len(), MAX_FAMILIES);
        assert_eq!(catalog.omitted(), 10);
        let search = catalog.search("family", false);
        assert_eq!(search.families.len(), MAX_RESULTS);
        assert_eq!(search.matched, MAX_FAMILIES);
    }

    #[test]
    fn roles_read_inherit_and_reset_the_existing_keys() {
        let mut config = Config::default();
        assert_eq!(Role::Composer.effective(&config), "Inter");
        assert!(
            Role::Composer
                .source(&config)
                .starts_with("Follows the message font")
        );
        Role::Message.set(&mut config, Some(" serif "));
        assert_eq!(
            config.typography.message_font_family.as_deref(),
            Some("serif")
        );
        assert_eq!(Role::Composer.effective(&config), "serif");
        Role::Composer.set(&mut config, Some("JetBrains Mono"));
        assert_eq!(Role::Composer.effective(&config), "JetBrains Mono");
        assert_eq!(
            Role::Composer.source(&config),
            "Your choice: JetBrains Mono"
        );
        // Inherit removes only that role's own value.
        Role::Composer.set(&mut config, None);
        assert_eq!(config.typography.composer_font_family, None);
        assert_eq!(
            config.typography.message_font_family.as_deref(),
            Some("serif")
        );
        // Interface and monospace return to their built-in families.
        Role::Interface.set(&mut config, Some("Custom Font"));
        Role::Interface.set(&mut config, None);
        assert_eq!(config.appearance.font_family, "Inter");
        Role::Monospace.set(&mut config, None);
        assert_eq!(config.typography.monospace_font_family, "monospace");
        config.validate().unwrap();
        assert!(!storable("") && !storable("Bad\u{202e}Name") && storable("Noto Sans"));
        assert!(is_generic(" Serif "));
    }
}
