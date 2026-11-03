//! Mods page, Browse and My library tabs: search mkdsrust.com, sign in (device code), and install / update / remove
//! site mods. All network work runs on background threads and reports through [`BrowseEvent`]s.
use super::*;
use crate::site::{self, Account, Client, ListPage, ModDetail, ModSummary, Query, State};
use mkds_builder::modding;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    time::Instant,
};

#[derive(Clone, Copy, Default, PartialEq)]
pub enum Tab {
    #[default]
    Installed,
    Browse,
    Library,
}

pub enum BrowseEvent {
    Listed(u64, Result<ListPage, String>),
    Detail(String, Result<ModDetail, site::ApiError>),
    /// A detail fetched only to learn the newest version of an installed mod.
    Latest(String, ModDetail),
    Library(Result<Vec<ModSummary>, site::ApiError>),
    Saved(String, Result<bool, site::ApiError>),
    DeviceStarted(Result<site::DeviceStart, String>),
    SignedIn(Result<Account, String>),
    /// The site rejected the stored token (revoked).
    TokenRejected,
}

pub struct Device {
    code: String,
    url: String,
    cancel: Arc<AtomicBool>,
}

pub struct BrowseState {
    tx: Sender<BrowseEvent>,
    rx: Receiver<BrowseEvent>,
    pub query: Query,
    /// What was last requested (a change of `query` refetches).
    sent: Option<Query>,
    typed: Option<Instant>,
    generation: u64,
    loading: bool,
    pub list: Option<ListPage>,
    error: String,
    /// The open detail page (slug) and the fetched details (also the newest-version cache for update checks).
    pub open: Option<String>,
    details: BTreeMap<String, ModDetail>,
    detail_error: String,
    checked: BTreeSet<String>,
    account_loaded: bool,
    pub account: Option<Account>,
    device: Option<Device>,
    signing_in: bool,
    library: Option<Vec<ModSummary>>,
    library_loading: bool,
    library_error: String,
    saved: BTreeSet<String>,
}
impl Default for BrowseState {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            query: Query::default(),
            sent: None,
            typed: None,
            generation: 0,
            loading: false,
            list: None,
            error: String::new(),
            open: None,
            details: BTreeMap::new(),
            detail_error: String::new(),
            checked: BTreeSet::new(),
            account_loaded: false,
            account: None,
            device: None,
            signing_in: false,
            library: None,
            library_loading: false,
            library_error: String::new(),
            saved: BTreeSet::new(),
        }
    }
}
impl BrowseState {
    fn client(&self) -> Client {
        Client::new(self.account.as_ref().map(|a| a.token.as_str()))
    }
    fn spawn(&self, job: impl FnOnce(&Sender<BrowseEvent>) + Send + 'static) {
        let tx = self.tx.clone();
        std::thread::spawn(move || job(&tx));
    }
    pub fn search(&mut self) {
        self.generation += 1;
        self.loading = true;
        self.error.clear();
        self.typed = None;
        let (generation, query, client) = (self.generation, self.query.clone(), self.client());
        self.sent = Some(self.query.clone());
        self.spawn(move |tx| {
            let _ = tx.send(BrowseEvent::Listed(generation, client.list(&query).map_err(|e| e.message)));
        });
    }
    fn fetch_detail(&mut self, slug: &str) {
        self.detail_error.clear();
        let (slug, client) = (slug.to_string(), self.client());
        self.spawn(move |tx| {
            let r = client.detail(&slug);
            let _ = tx.send(BrowseEvent::Detail(slug, r));
        });
    }
    /// Learn the newest version of installed site mods once per session (for "Update available").
    fn check_updates(&mut self, installed: &[site::Installed]) {
        let slugs: Vec<String> =
            site::sourced_slugs(installed).into_keys().filter(|s| !self.checked.contains(s) && !self.details.contains_key(s)).collect();
        if slugs.is_empty() {
            return;
        }
        self.checked.extend(slugs.iter().cloned());
        let client = self.client();
        self.spawn(move |tx| {
            for slug in slugs.into_iter().take(50) {
                if let Ok(d) = client.detail(&slug) {
                    if tx.send(BrowseEvent::Latest(slug, d)).is_err() {
                        return;
                    }
                }
            }
        });
    }
    fn load_library(&mut self) {
        if self.account.is_none() {
            return;
        }
        self.library_loading = true;
        self.library_error.clear();
        let client = self.client();
        self.spawn(move |tx| {
            let _ = tx.send(BrowseEvent::Library(client.library()));
        });
    }
    fn set_saved(&mut self, slug: &str, saved: bool) {
        let (slug, client) = (slug.to_string(), self.client());
        self.spawn(move |tx| {
            let r = client.set_saved(&slug, saved);
            let _ = tx.send(BrowseEvent::Saved(slug, r));
        });
    }
    fn sign_in(&mut self) {
        if self.signing_in {
            return;
        }
        self.signing_in = true;
        let client = Client::new(None);
        self.spawn(move |tx| {
            let _ = tx.send(BrowseEvent::DeviceStarted(client.device_start(&site::client_name()).map_err(|e| e.message)));
        });
    }
    fn poll_device(&mut self, start: site::DeviceStart) {
        let cancel = Arc::new(AtomicBool::new(false));
        self.device = Some(Device { code: start.code.clone(), url: start.verify_url.clone(), cancel: cancel.clone() });
        let client = Client::new(None);
        self.spawn(move |tx| {
            let deadline = Instant::now() + Duration::from_secs(start.expires_in.clamp(60, 1800));
            let mut interval = start.interval.clamp(5, 60);
            let result = loop {
                // sleep in small steps so Cancel is quick
                for _ in 0..interval * 4 {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                if Instant::now() > deadline {
                    break Err("The sign-in code expired. Start again.".to_string());
                }
                match client.device_poll(&start.device_code) {
                    Ok(site::Poll::Pending) => {}
                    Ok(site::Poll::SlowDown(n)) => interval = n.clamp(interval + 1, 60),
                    Ok(site::Poll::Approved(mut account)) => {
                        if account.username.is_empty() {
                            account.username = Client::new(Some(&account.token)).me().map(|u| u.username).unwrap_or_default();
                        }
                        break Ok(account);
                    }
                    // a dropped connection is retried until the code expires
                    Err(e) if e.code == "network" => {}
                    Err(e) => break Err(e.message),
                }
            };
            if !cancel.load(Ordering::Relaxed) {
                let _ = tx.send(BrowseEvent::SignedIn(result));
            }
        });
    }
    fn cancel_sign_in(&mut self) {
        if let Some(d) = self.device.take() {
            d.cancel.store(true, Ordering::Relaxed);
        }
        self.signing_in = false;
    }
}

const GRAY: Color32 = Color32::GRAY;
const GOLD: Color32 = Color32::from_rgb(245, 194, 74);
const GREEN: Color32 = Color32::from_rgb(110, 200, 120);

pub(crate) fn badge(ui: &mut egui::Ui, text: &str, c: Color32) {
    egui::Frame::none()
        .fill(c.gamma_multiply(0.18))
        .stroke(egui::Stroke::new(1., c.gamma_multiply(0.6)))
        .rounding(10.)
        .inner_margin(egui::Margin::symmetric(8., 1.))
        .show(ui, |ui| ui.label(RichText::new(text).small().color(c)));
}
fn ds_badge(ui: &mut egui::Ui, ds_status: &str) {
    match ds_status {
        "ds_verified" => badge(ui, "Verified on DS", GREEN),
        "ds_compatible" => badge(ui, "Works on DS", TEAL),
        _ => {}
    }
}
fn category_label(category: &str) -> &str {
    site::CATEGORIES.iter().find(|(c, _)| *c == category).map_or("Other", |(_, l)| l)
}
fn rating_text(rating: Option<f64>, ratings: u64) -> String {
    match rating {
        Some(r) if ratings > 0 => format!("★ {r:.1} ({ratings})"),
        _ => "No ratings".into(),
    }
}
fn date(s: &str) -> &str {
    s.get(..10).unwrap_or(s)
}
fn size_text(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.0} KB", b as f64 / 1024.),
        b => format!("{b} B"),
    }
}
fn row_frame() -> egui::Frame {
    egui::Frame::none().fill(Color32::from_rgb(30, 41, 49)).rounding(8.).inner_margin(egui::Margin::symmetric(12., 8.))
}

/// What a row or detail button asks the launcher to do.
enum Action {
    Open(String),
    Install(String, Option<i64>),
    Uninstall(String),
    Save(String, bool),
    SignIn,
}

impl Launcher {
    fn account(&mut self) -> Option<&Account> {
        let b = &mut self.mods.browse;
        if !b.account_loaded {
            b.account_loaded = true;
            b.account = site::load_account(&self.dir);
        }
        b.account.as_ref()
    }

    /// Browse events (called every frame from [`Launcher::poll`]).
    pub(crate) fn poll_browse(&mut self, ctx: Option<&egui::Context>) {
        let events: Vec<_> = self.mods.browse.rx.try_iter().collect();
        for event in events {
            let b = &mut self.mods.browse;
            match event {
                BrowseEvent::Listed(generation, r) => {
                    if generation != b.generation {
                        continue;
                    }
                    b.loading = false;
                    match r {
                        Ok(page) => b.list = Some(page),
                        Err(e) => b.error = e,
                    }
                }
                BrowseEvent::Detail(slug, r) => match r {
                    Ok(d) => {
                        b.details.insert(slug, d);
                    }
                    Err(e) => {
                        if b.open.as_deref() == Some(slug.as_str()) {
                            b.detail_error = e.message;
                        }
                    }
                },
                BrowseEvent::Latest(slug, d) => {
                    b.details.insert(slug, d);
                }
                BrowseEvent::Library(r) => {
                    b.library_loading = false;
                    match r {
                        Ok(mods) => {
                            b.saved = mods.iter().map(|m| m.slug.clone()).collect();
                            b.library = Some(mods);
                        }
                        Err(e) if e.signed_out() => self.sign_out("Your mkdsrust.com sign-in is no longer valid. Sign in again."),
                        Err(e) => b.library_error = e.message,
                    }
                }
                BrowseEvent::Saved(slug, r) => match r {
                    Ok(saved) => {
                        if saved {
                            b.saved.insert(slug.clone());
                        } else {
                            b.saved.remove(&slug);
                            if let Some(lib) = &mut b.library {
                                lib.retain(|m| m.slug != slug);
                            }
                        }
                        self.message = if saved { "Saved to your library on mkdsrust.com." } else { "Removed from your library." }.into();
                        if saved {
                            b.library = None; // refetch on the next library view (newest first)
                        }
                    }
                    Err(e) if e.signed_out() => self.sign_out(&e.message),
                    Err(e) => self.message = e.message,
                },
                BrowseEvent::DeviceStarted(r) => match r {
                    Ok(start) => {
                        if let Some(ctx) = ctx {
                            ctx.open_url(egui::OpenUrl::new_tab(&start.verify_url));
                        }
                        b.poll_device(start);
                    }
                    Err(e) => {
                        b.signing_in = false;
                        self.message = format!("Could not start sign-in: {e}");
                    }
                },
                BrowseEvent::SignedIn(r) => {
                    b.device = None;
                    b.signing_in = false;
                    match r {
                        Ok(account) => match site::save_account(&self.dir, &account) {
                            Ok(()) => {
                                self.message = format!("Signed in to mkdsrust.com as {}.", account.username);
                                b.account = Some(account);
                                b.account_loaded = true;
                                b.library = None;
                            }
                            Err(e) => self.message = e,
                        },
                        Err(e) => self.message = e,
                    }
                }
                BrowseEvent::TokenRejected => self.sign_out("Your mkdsrust.com sign-in is no longer valid. Sign in again."),
            }
        }
    }

    fn sign_out(&mut self, message: &str) {
        let b = &mut self.mods.browse;
        b.cancel_sign_in();
        b.account = None;
        b.account_loaded = true;
        b.library = None;
        b.saved.clear();
        self.message = match site::delete_account(&self.dir) {
            Ok(()) => message.into(),
            Err(e) => e,
        };
    }

    /// Download and install a site mod with the same screening as a local zip.
    fn install_from_site(&mut self, slug: String, version: Option<i64>) {
        if self.busy {
            return;
        }
        let Some(account) = self.account().cloned() else {
            self.message = "Sign in to mkdsrust.com to download mods.".into();
            return;
        };
        let (game, rom, data) = (self.prefs.output.clone(), self.prefs.rom.clone(), self.dir.clone());
        let browse_tx = self.mods.browse.tx.clone();
        let tx = self.start("Downloading the mod from mkdsrust.com…");
        std::thread::spawn(move || {
            let client = Client::new(Some(&account.token));
            let r = site::install(&client, &slug, version, &data, &game, &rom);
            if r.as_ref().is_err_and(|e| e.signed_out()) {
                let _ = browse_tx.send(BrowseEvent::TokenRejected);
            }
            let _ = tx.send(Event::ModInstalled(r.map_err(|e| if e.code == "screening" { e.message } else { format!("Mod not installed: {e}") })));
        });
    }

    fn browse_action(&mut self, action: Action) {
        match action {
            Action::Open(slug) => {
                if !self.mods.browse.details.contains_key(&slug) {
                    self.mods.browse.fetch_detail(&slug);
                }
                self.mods.browse.detail_error.clear();
                self.mods.browse.open = Some(slug);
            }
            Action::Install(slug, version) => self.install_from_site(slug, version),
            Action::Uninstall(id) => match modding::remove_installed(&self.prefs.output, &id) {
                Ok(()) => self.message = format!("Removed {id}."),
                Err(e) => self.message = e.to_string(),
            },
            Action::Save(slug, saved) => self.mods.browse.set_saved(&slug, saved),
            Action::SignIn => self.mods.browse.sign_in(),
        }
    }

    /// Tabs and the account corner at the top of the Mods page. Returns the active tab.
    pub(crate) fn mods_tabs(&mut self, ui: &mut egui::Ui) -> Tab {
        self.account();
        let mut tab = self.mods.tab;
        ui.horizontal(|ui| {
            for (t, label) in [(Tab::Installed, "Installed"), (Tab::Browse, "Browse"), (Tab::Library, "My library")] {
                let text = RichText::new(label).size(17.);
                if ui.selectable_label(tab == t, if tab == t { text.strong().color(Color32::WHITE) } else { text }).clicked() {
                    tab = t;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let b = &mut self.mods.browse;
                if let Some(a) = &b.account {
                    let mut out = false;
                    if ui.small_button("Sign out").clicked() {
                        out = true;
                    }
                    ui.label(RichText::new(format!("Signed in as {}", a.username)).color(TEAL));
                    if out {
                        self.sign_out("Signed out. (Revoke old launcher sign-ins on mkdsrust.com/me.)");
                    }
                } else if b.signing_in {
                    ui.spinner();
                    ui.label("Signing in…");
                } else if ui.button("Sign in to mkdsrust.com").on_hover_text("Needed to download mods and use your library").clicked() {
                    b.sign_in();
                }
            });
        });
        if tab != self.mods.tab {
            self.mods.tab = tab;
            self.mods.browse.open = None;
        }
        ui.separator();
        tab
    }

    pub(crate) fn browse_page(&mut self, ui: &mut egui::Ui, tab: Tab) {
        if !self.game_ready() && self.mods.browse.open.is_none() {
            ui.colored_label(Color32::YELLOW, "Build your game first (Setup) to install mods. Browsing works now.");
        }
        let installed = site::installed(&self.prefs.output);
        let b = &mut self.mods.browse;
        b.check_updates(&installed);
        let mut action = None;
        if let Some(slug) = b.open.clone() {
            if ui.button("◀  Back to list").clicked() {
                b.open = None;
            } else {
                self.detail_view(ui, &slug, &installed, &mut action);
            }
        } else if tab == Tab::Library {
            self.library_view(ui, &installed, &mut action);
        } else {
            self.search_view(ui, &installed, &mut action);
        }
        if let Some(a) = action {
            self.browse_action(a);
        }
        self.sign_in_window(ui.ctx());
    }

    fn search_view(&mut self, ui: &mut egui::Ui, installed: &[site::Installed], action: &mut Option<Action>) {
        let b = &mut self.mods.browse;
        let mut now = false;
        let row = egui::vec2(ui.available_width(), 40.);
        ui.allocate_ui_with_layout(row, egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.spacing_mut().button_padding = egui::vec2(14., 8.);
            ui.spacing_mut().interact_size.y = 38.;
            let search = ui.add(
                egui::TextEdit::singleline(&mut b.query.q).hint_text("Search mods").desired_width(260.).margin(egui::vec2(10., 9.)),
            );
            if search.changed() {
                b.typed = Some(Instant::now());
            }
            if search.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                now = true;
            }
            let before = b.query.clone();
            egui::ComboBox::from_id_salt("browse-category")
                .selected_text(if b.query.category.is_empty() { "All categories" } else { category_label(&b.query.category) })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut b.query.category, String::new(), "All categories");
                    for (c, label) in site::CATEGORIES {
                        ui.selectable_value(&mut b.query.category, c.to_string(), label);
                    }
                });
            egui::ComboBox::from_id_salt("browse-sort")
                .selected_text(site::SORTS.iter().find(|(s, _)| *s == b.query.sort).map_or("Newest", |(_, l)| l))
                .show_ui(ui, |ui| {
                    for (s, label) in site::SORTS {
                        ui.selectable_value(&mut b.query.sort, s.to_string(), label);
                    }
                });
            ui.checkbox(&mut b.query.ds_only, "Works on DS");
            if b.query.category != before.category || b.query.sort != before.sort || b.query.ds_only != before.ds_only {
                b.query.page = 1;
                now = true;
            }
            if ui.add_enabled(!b.loading, egui::Button::new("Refresh")).clicked() {
                now = true;
            }
        });
        // search 0.4 s after typing stops; first view loads at once
        if b.typed.is_some_and(|t| t.elapsed() > Duration::from_millis(400)) {
            b.query.page = 1;
            now = true;
        }
        if now || b.sent.is_none() {
            b.search();
        }
        if b.loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading mods from mkdsrust.com…");
            });
        }
        if !b.error.is_empty() {
            ui.colored_label(Color32::from_rgb(240, 128, 96), &b.error);
        }
        let Some(list) = b.list.clone() else { return };
        let filtered = !b.query.q.trim().is_empty() || !b.query.category.is_empty() || b.query.ds_only;
        if list.mods.is_empty() {
            ui.add_space(20.);
            ui.vertical_centered(|ui| {
                if filtered {
                    ui.label(RichText::new("No mods match your search.").size(18.));
                    ui.label(RichText::new("Try other words, another category, or turn off Works on DS.").color(GRAY));
                } else {
                    ui.label(RichText::new("No mods on mkdsrust.com yet.").size(18.));
                    ui.label(RichText::new("Be the first: make one with Create mod on the Installed tab, then upload it on the site.").color(GRAY));
                }
            });
            return;
        }
        ui.label(RichText::new(format!("{} mod{}", list.total, if list.total == 1 { "" } else { "s" })).color(GRAY));
        for m in &list.mods {
            let latest = self.mods.browse.details.get(&m.slug).and_then(|d| d.versions.first()).map(site::Latest::from);
            self.mod_row(ui, m, site::state(&m.slug, latest, installed), action);
        }
        let b = &mut self.mods.browse;
        if list.page_count > 1 {
            ui.horizontal(|ui| {
                if ui.add_enabled(list.page > 1 && !b.loading, egui::Button::new("◀ Previous")).clicked() {
                    b.query.page = list.page - 1;
                    b.search();
                }
                ui.label(format!("Page {} of {}", list.page, list.page_count));
                if ui.add_enabled(list.page < list.page_count && !b.loading, egui::Button::new("Next ▶")).clicked() {
                    b.query.page = list.page + 1;
                    b.search();
                }
            });
        }
    }

    fn library_view(&mut self, ui: &mut egui::Ui, installed: &[site::Installed], action: &mut Option<Action>) {
        let b = &mut self.mods.browse;
        if b.account.is_none() {
            ui.add_space(20.);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Your library lives on mkdsrust.com.").size(18.));
                ui.label(RichText::new("Sign in to see the mods you saved and install them with one click.").color(GRAY));
                if ui.add_enabled(!b.signing_in, egui::Button::new("Sign in to mkdsrust.com")).clicked() {
                    *action = Some(Action::SignIn);
                }
            });
            return;
        }
        if b.library.is_none() && !b.library_loading && b.library_error.is_empty() {
            b.load_library();
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("Mods you saved on mkdsrust.com, newest first.").color(GRAY));
            if ui.add_enabled(!b.library_loading, egui::Button::new("Refresh")).clicked() {
                b.load_library();
            }
        });
        if b.library_loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading your library…");
            });
        }
        if !b.library_error.is_empty() {
            ui.colored_label(Color32::from_rgb(240, 128, 96), &b.library_error);
        }
        let Some(lib) = b.library.clone() else { return };
        if lib.is_empty() {
            ui.add_space(20.);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Nothing saved yet.").size(18.));
                ui.label(RichText::new("Use Save to library on a mod (here or on the site) to keep it handy.").color(GRAY));
            });
            return;
        }
        for m in &lib {
            let latest = self
                .mods
                .browse
                .details
                .get(&m.slug)
                .and_then(|d| d.versions.first())
                .map(site::Latest::from)
                .or_else(|| m.latest.as_ref().map(site::Latest::from));
            self.mod_row(ui, m, site::state(&m.slug, latest, installed), action);
        }
    }

    fn mod_row(&self, ui: &mut egui::Ui, m: &ModSummary, state: State, action: &mut Option<Action>) {
        let can_install = !self.busy && self.game_ready();
        let why = if self.game_ready() { "Wait for the current task to finish" } else { "Build your game first (Setup)" };
        let r = row_frame().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&m.title).strong().size(17.));
                        mods_ui::chip(ui, site::kind_for_category(&m.category));
                        ds_badge(ui, &m.ds_status);
                        match &state {
                            State::Installed { version, .. } => badge(ui, &format!("Installed v{version}"), TEAL),
                            State::Update { latest, .. } => badge(ui, &format!("Update: v{latest}"), GOLD),
                            State::NotInstalled => {}
                        }
                    });
                    let mut line = format!("by {}", m.author);
                    if !m.tags.is_empty() {
                        line += &format!("  ·  {}", m.tags.join(", "));
                    }
                    ui.label(RichText::new(line).small().color(GRAY));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match &state {
                        State::NotInstalled => {
                            let hint = if self.mods.browse.account.is_some() { "Download and install" } else { "Sign in to mkdsrust.com to download" };
                            if ui.add_enabled(can_install, egui::Button::new("Install")).on_hover_text(hint).on_disabled_hover_text(why).clicked() {
                                *action = Some(if self.mods.browse.account.is_some() { Action::Install(m.slug.clone(), None) } else { Action::SignIn });
                            }
                        }
                        State::Update { .. } => {
                            if ui.add_enabled(can_install, egui::Button::new(RichText::new("Update").color(Color32::BLACK)).fill(GOLD)).on_disabled_hover_text(why).clicked() {
                                *action = Some(if self.mods.browse.account.is_some() { Action::Install(m.slug.clone(), None) } else { Action::SignIn });
                            }
                        }
                        State::Installed { .. } => {
                            ui.label(RichText::new("✔ Installed").color(TEAL));
                        }
                    }
                    if ui.button("Details").clicked() {
                        *action = Some(Action::Open(m.slug.clone()));
                    }
                    ui.label(RichText::new(format!("⬇ {}", m.downloads)).color(GRAY));
                    ui.label(RichText::new(rating_text(m.rating, m.ratings)).color(if m.ratings > 0 { GOLD } else { GRAY }));
                });
            });
        });
        if action.is_none() && r.response.interact(egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            *action = Some(Action::Open(m.slug.clone()));
        }
        ui.add_space(-4.);
    }

    fn detail_view(&mut self, ui: &mut egui::Ui, slug: &str, installed: &[site::Installed], action: &mut Option<Action>) {
        let b = &self.mods.browse;
        let Some(d) = b.details.get(slug).cloned() else {
            if b.detail_error.is_empty() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Loading…");
                });
            } else {
                ui.colored_label(Color32::from_rgb(240, 128, 96), &b.detail_error);
                if ui.button("Try again").clicked() {
                    *action = Some(Action::Open(slug.to_string()));
                    self.mods.browse.details.remove(slug);
                }
            }
            return;
        };
        let signed_in = b.account.is_some();
        let saved = b.saved.contains(slug);
        let state = site::state(slug, d.versions.first().map(site::Latest::from), installed);
        let can_install = !self.busy && self.game_ready();
        ui.horizontal_wrapped(|ui| {
            ui.heading(&d.title);
            mods_ui::chip(ui, site::kind_for_category(&d.category));
            ds_badge(ui, &d.ds_status);
        });
        let mut facts = vec![format!("by {}", d.author), rating_text(d.rating, d.ratings), format!("⬇ {} downloads", d.downloads)];
        if let Some(l) = d.license.as_deref().filter(|l| !l.is_empty()) {
            facts.push(format!("License {l}"));
        }
        if !d.updated.is_empty() {
            facts.push(format!("Updated {}", date(&d.updated)));
        }
        ui.label(RichText::new(facts.join("   ·   ")).color(GRAY));
        ui.horizontal_wrapped(|ui| {
            let latest = d.versions.first();
            match &state {
                State::NotInstalled => {
                    let label = latest.map_or("Install".to_string(), |v| format!("Install v{}", v.version));
                    let enabled = can_install && latest.is_some();
                    if ui.add_enabled(enabled, egui::Button::new(RichText::new(label).strong().color(Color32::BLACK)).fill(if enabled { TEAL } else { TEAL.gamma_multiply(0.35) })).clicked() {
                        *action = Some(if signed_in { Action::Install(d.slug.clone(), None) } else { Action::SignIn });
                    }
                }
                State::Update { installed, latest, .. } => {
                    let fill = if can_install { GOLD } else { GOLD.gamma_multiply(0.35) };
                    let button = egui::Button::new(RichText::new(format!("Update v{installed} → v{latest}")).strong().color(Color32::BLACK)).fill(fill);
                    if ui.add_enabled(can_install, button).clicked() {
                        *action = Some(if signed_in { Action::Install(d.slug.clone(), None) } else { Action::SignIn });
                    }
                }
                State::Installed { version, .. } => {
                    ui.label(RichText::new(format!("✔ Installed v{version}")).color(TEAL));
                }
            }
            if let Some(id) = state.id() {
                if ui.add_enabled(!self.busy, egui::Button::new("Uninstall")).clicked() {
                    *action = Some(Action::Uninstall(id.to_string()));
                }
            }
            if signed_in {
                let (label, to) = if saved { ("Remove from library", false) } else { ("Save to library", true) };
                if ui.button(label).clicked() {
                    *action = Some(Action::Save(d.slug.clone(), to));
                }
            }
            let page = format!("{}/mods/{}", site::base(), d.slug);
            if ui.button("Open on mkdsrust.com  ↗").on_hover_text(&page).clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(page));
            }
        });
        if !signed_in {
            ui.label(RichText::new("Sign in to mkdsrust.com to download (browsing works without an account).").small().color(GRAY));
        }
        if !self.game_ready() {
            ui.colored_label(Color32::YELLOW, "Build your game first (Setup): mods are installed into the game folder.");
        }
        ui.add_space(6.);
        let platforms = if d.platforms.is_empty() { "PC".to_string() } else { d.platforms.iter().map(|p| p.to_uppercase()).collect::<Vec<_>>().join(", ") };
        ui.label(format!("Platforms: {platforms}"));
        if let Some(v) = d.ds_verified.as_ref().filter(|_| d.ds_status == "ds_verified") {
            ui.label(RichText::new(format!("Verified on DS by {} ({}) {}", v.by, date(&v.at), v.note)).small().color(GREEN));
        }
        if let Some(reasons) = d.versions.first().and_then(|v| v.ds.as_ref()).filter(|ds| !ds.compatible && !ds.reasons.is_empty()).map(|ds| &ds.reasons) {
            ui.label(RichText::new(format!("PC only: {}", reasons.join("; "))).small().color(GRAY));
        }
        if !d.tags.is_empty() {
            ui.label(RichText::new(format!("Tags: {}", d.tags.join(", "))).small().color(GRAY));
        }
        ui.add_space(6.);
        card().inner_margin(16.).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            let text = if d.description.trim().is_empty() { "No description." } else { d.description.trim() };
            ui.add(egui::Label::new(text).wrap());
        });
        ui.add_space(6.);
        ui.strong(format!("Versions ({})", d.versions.len()));
        if d.versions.is_empty() {
            ui.label(RichText::new("No downloadable version yet.").color(GRAY));
        }
        let current = match &state {
            State::Installed { version, .. } => Some(version.clone()),
            State::Update { installed, .. } => Some(installed.clone()),
            State::NotInstalled => None,
        };
        for (i, v) in d.versions.iter().enumerate() {
            row_frame().fill(Color32::from_rgb(24, 29, 34)).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("v{}", v.version)).strong());
                            if i == 0 {
                                badge(ui, "Latest", TEAL);
                            }
                            if current.as_deref() == Some(v.version.as_str()) {
                                badge(ui, "Installed", GOLD);
                            }
                            let platforms = v.platforms.iter().map(|p| p.to_uppercase()).collect::<Vec<_>>().join(" + ");
                            let facts: Vec<String> = [date(&v.created).to_string(), size_text(v.size), format!("{} downloads", v.downloads), platforms]
                                .into_iter()
                                .filter(|f| !f.is_empty())
                                .collect();
                            ui.label(RichText::new(facts.join("  ·  ")).small().color(GRAY));
                        });
                        if !v.changelog.trim().is_empty() {
                            ui.add(egui::Label::new(RichText::new(v.changelog.trim()).small()).wrap());
                        }
                        let mut needs = Vec::new();
                        if !v.depends.is_empty() {
                            needs.push(format!("Needs {}", v.depends.join(", ")));
                        }
                        if !v.conflicts.is_empty() {
                            needs.push(format!("Conflicts with {}", v.conflicts.join(", ")));
                        }
                        if !needs.is_empty() {
                            ui.label(RichText::new(needs.join("  ·  ")).small().color(GOLD));
                        }
                    });
                    if i > 0 && current.as_deref() != Some(v.version.as_str()) {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add_enabled(can_install, egui::Button::new("Install this version")).clicked() {
                                *action = Some(if signed_in { Action::Install(d.slug.clone(), Some(v.id)) } else { Action::SignIn });
                            }
                        });
                    }
                });
            });
        }
    }

    fn sign_in_window(&mut self, ctx: &egui::Context) {
        let Some(device) = &self.mods.browse.device else { return };
        let (code, url) = (device.code.clone(), device.url.clone());
        let mut open = true;
        let mut cancel = false;
        egui::Window::new("Sign in to mkdsrust.com")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0., 0.))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label("Your browser opened mkdsrust.com. Sign in there and approve this code:");
                ui.add_space(6.);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(&code).monospace().size(34.).strong().color(TEAL));
                });
                ui.add_space(6.);
                ui.horizontal(|ui| {
                    if ui.button("Open the page again").on_hover_text(&url).clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(&url));
                    }
                    if ui.button("Copy code").clicked() {
                        ui.ctx().copy_text(code.clone());
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Waiting for approval…").color(GRAY));
                });
            });
        if cancel || !open {
            self.mods.browse.cancel_sign_in();
            self.message = "Sign-in cancelled.".into();
        }
    }
}

impl Launcher {
    /// `--smoke-test`: open the first listed mod's page (or close it).
    pub(crate) fn smoke_detail(&mut self, open: bool) {
        let first = self.mods.browse.list.as_ref().and_then(|l| l.mods.first()).map(|m| m.slug.clone());
        // fetch it a screen early so the page has loaded when it is captured
        if let Some(slug) = &first {
            let b = &mut self.mods.browse;
            if !b.details.contains_key(slug) && b.checked.insert(slug.clone()) {
                b.fetch_detail(slug);
            }
        }
        match first.filter(|_| open) {
            Some(slug) if self.mods.browse.open.as_ref() != Some(&slug) => self.browse_action(Action::Open(slug)),
            Some(_) => {}
            None => self.mods.browse.open = None,
        }
    }
}

impl BrowseState {
    /// Fixed example rows instead of the site (`--smoke-test` with `MKDS_SMOKE_BROWSE_SAMPLE=1`, and tests), so the
    /// layout can be checked while mkdsrust.com has few mods.
    pub fn sample(&mut self) {
        let row = |slug: &str, title: &str, category: &str, author: &str, tags: &[&str], downloads, rating, ratings, ds: &str| ModSummary {
            slug: slug.into(),
            title: title.into(),
            category: category.into(),
            author: author.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            downloads,
            rating,
            ratings,
            ds_status: ds.into(),
            ..Default::default()
        };
        let mods = vec![
            row("sample-retro-castle", "Sample: Retro Castle", "track", "example", &["castle", "retro"], 1234, Some(4.75), 4, "ds_verified"),
            row("sample-fast-karts", "Sample: Faster Karts", "gameplay", "example", &["speed"], 87, Some(3.9), 12, "ds_compatible"),
            row("sample-menu-music", "Sample: Menu Music", "music", "example", &[], 5, None, 0, "pc_only"),
        ];
        self.sent = Some(self.query.clone());
        self.loading = false;
        self.list = Some(ListPage { total: mods.len() as u64, page: 1, page_count: 2, mods: mods.clone() });
        self.library = Some(mods);
        self.details.insert(
            "sample-retro-castle".into(),
            ModDetail {
                slug: "sample-retro-castle".into(),
                title: "Sample: Retro Castle".into(),
                description: "An example detail page with a description, platforms and versions.".into(),
                category: "track".into(),
                author: "example".into(),
                platforms: vec!["pc".into(), "ds".into()],
                ds_status: "ds_verified".into(),
                versions: vec![
                    site::Version { id: 2, version: "2.0.0".into(), size: 653, changelog: "Second version.".into(), ..Default::default() },
                    site::Version { id: 1, version: "1.0.0".into(), size: 600, ..Default::default() },
                ],
                ..Default::default()
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browse_library_detail_and_sign_in_render_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let mut app = crate::tests::test_launcher(&ctx, dir.path());
        app.page = 3;
        app.mods.browse.sample();
        app.mods.browse.account_loaded = true;
        app.mods.browse.account = Some(Account { token: format!("mkr_{}", "a".repeat(43)), username: "cam".into() });
        app.mods.browse.device = Some(Device { code: "BCDF-GHJK".into(), url: "https://mkdsrust.com/device".into(), cancel: Default::default() });
        let frame = |app: &mut Launcher| {
            let out = ctx.run(
                egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1020., 820.))), ..Default::default() },
                |ctx| app.draw(ctx),
            );
            assert!(!out.shapes.is_empty());
        };
        for (tab, open) in [(Tab::Browse, None), (Tab::Library, None), (Tab::Browse, Some("sample-retro-castle")), (Tab::Installed, None)] {
            app.mods.tab = tab;
            app.mods.browse.open = open.map(str::to_string);
            frame(&mut app);
            frame(&mut app);
        }
        // nothing went to the network: the sample list is still what is shown
        assert_eq!(app.mods.browse.list.as_ref().unwrap().mods.len(), 3);
        assert_eq!(app.mods.browse.generation, 0);
        app.mods.browse.cancel_sign_in();
        assert!(app.mods.browse.device.is_none());
    }
    #[test]
    fn labels() {
        assert_eq!(rating_text(Some(4.75), 4), "★ 4.8 (4)");
        assert_eq!(rating_text(None, 0), "No ratings");
        assert_eq!(category_label("battle_stage"), "Battle stage");
        assert_eq!(size_text(653), "653 B");
        assert_eq!(size_text(3 << 20), "3.0 MB");
        assert_eq!(date("2026-11-02T20:34:11.897Z"), "2026-11-02");
    }
}
