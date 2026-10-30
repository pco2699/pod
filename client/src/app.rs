use std::{
    cell::{Cell, RefCell},
    collections::{BTreeSet, HashMap, HashSet},
    future::Future,
    rc::Rc,
};

use egui::{
    Align, Align2, Color32, FontId, Key, Layout, Mesh, Rect, RichText, Sense, Stroke, Ui, UiBuilder, pos2, vec2,
};
use pod_core::{Op, Project, Source, Track, peaks::Peaks};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    audio::{self, Player},
    net::{self, HttpError},
    store,
    transfer::{self, ExportInfo},
};

const LAST_PROJECT: &str = "pod_last_project";

thread_local! {
    /// Read by the beforeunload handler: warn before closing with unsaved edits.
    pub static UNSAVED: Cell<bool> = const { Cell::new(false) };
}

// ---------- messages from async tasks ----------

enum Msg {
    Toast(String),
    /// A 401; `keyed` says whether the request carried a key.
    Unauthorized { keyed: bool },
    Projects(Vec<ProjectInfo>),
    Opened(Box<Opened>),
    OpenFailed(String),
    /// Messages for one editor session; dropped if that editor has been closed.
    Ed(u32, EdMsg),
}

enum EdMsg {
    Synced(u64),
    Conflict(u64, Vec<Op>),
    Rejected(String),
    SyncFailed,
    Remote(u64, Vec<Op>),
    Peaks(String, Result<Peaks, String>),
    CacheProgress(String, u64),
    Cached(String, Result<(), String>),
    Chunk { generation: u32, start: u64, samples: Vec<f32>, ch: usize },
    ChunkFailed { generation: u32, start: u64, error: String },
    Task(u32, String, f32),
    TaskDone(u32),
    Imported(String, Source),
    Exported(ExportInfo),
    ExportDeleted(String),
}

#[derive(Clone)]
struct Tx {
    inbox: Rc<RefCell<Vec<Msg>>>,
    ctx: egui::Context,
}

impl Tx {
    fn send(&self, m: Msg) {
        self.inbox.borrow_mut().push(m);
        self.ctx.request_repaint();
    }

    fn ed(&self, session: u32, m: EdMsg) {
        self.send(Msg::Ed(session, m));
    }

    fn spawn(&self, f: impl Future<Output = Option<Msg>> + 'static) {
        let tx = self.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Some(m) = f.await {
                tx.send(m);
            }
        });
    }
}

fn http_msg(e: HttpError) -> Msg {
    if e.status == 401 { Msg::Unauthorized { keyed: e.keyed } } else { Msg::Toast(e.message()) }
}

// ---------- server shapes ----------

#[derive(Deserialize, Clone)]
struct ProjectInfo {
    id: String,
    name: String,
    tracks: usize,
    frames: u64,
    sample_rate: Option<u32>,
}

#[derive(Deserialize)]
struct Opened {
    id: String,
    name: String,
    version: u64,
    project: Project,
    view: Value,
    exports: Vec<ExportInfo>,
}

#[derive(Deserialize)]
struct OpsResponse {
    version: u64,
    ops: Vec<Op>,
}

/// Saved per project so another device resumes in the same spot.
#[derive(Serialize, Deserialize, Default, PartialEq, Clone)]
#[serde(default)]
struct ViewState {
    playhead: u64,
    view_start: f64,
    fpp: f64,
    sel: Option<(u64, u64)>,
    sel_tracks: Vec<String>,
    muted: Vec<String>,
    solo: Vec<String>,
    unlinked: bool,
}

// ---------- app ----------

pub struct PodApp {
    inbox: Rc<RefCell<Vec<Msg>>>,
    tx: Tx,
    /// The server turned us away without a key: show the key screen. Starts false,
    /// since behind a signing-in proxy no key is needed at all.
    need_key: bool,
    /// A stored key was just refused; say so if the key screen follows.
    key_rejected: bool,
    key_input: String,
    projects: Option<Vec<ProjectInfo>>,
    new_name: String,
    toast: Option<(String, f64)>,
    editor: Option<Editor>,
    opening: bool,
    next_session: u32,
}

impl PodApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let inbox = Rc::new(RefCell::new(Vec::new()));
        let tx = Tx { inbox: inbox.clone(), ctx: cc.egui_ctx.clone() };
        let mut app = Self {
            inbox,
            tx,
            need_key: false,
            key_rejected: false,
            key_input: String::new(),
            projects: None,
            new_name: String::new(),
            toast: None,
            editor: None,
            opening: false,
            next_session: 1,
        };
        net::init_key();
        app.load_last();
        store::request_persistence();
        app
    }

    /// The project list, and the project that was open last time.
    fn load_last(&mut self) {
        self.load_projects();
        if let Some(id) = net::get_local(LAST_PROJECT) {
            self.open(id);
        }
    }

    fn load_projects(&self) {
        self.tx.spawn(async {
            Some(match net::get::<Vec<ProjectInfo>>("/projects").await {
                Ok(p) => Msg::Projects(p),
                Err(e) => http_msg(e),
            })
        });
    }

    fn open(&mut self, id: String) {
        self.opening = true;
        self.tx.spawn(async move {
            Some(match net::get::<Opened>(&format!("/projects/{id}")).await {
                Ok(o) => Msg::Opened(Box::new(o)),
                Err(e) if e.status == 401 => Msg::Unauthorized { keyed: e.keyed },
                Err(e) => Msg::OpenFailed(e.message()),
            })
        });
    }

    fn toast(&mut self, ctx: &egui::Context, s: impl Into<String>) {
        self.toast = Some((s.into(), ctx.input(|i| i.time) + 5.0));
    }

    fn handle(&mut self, ctx: &egui::Context, m: Msg) {
        match m {
            Msg::Toast(s) => self.toast(ctx, s),
            Msg::Unauthorized { keyed: true } => {
                // The stored key is refused. Behind a signing-in proxy none may be needed,
                // so go again without it before asking for another. Several requests can
                // fail on the same key; only the first one retries.
                if net::key().is_some() {
                    net::set_key(None);
                    self.key_rejected = true;
                    self.editor = None;
                    self.load_last();
                }
            }
            Msg::Unauthorized { keyed: false } => {
                self.need_key = true;
                self.editor = None;
                if std::mem::take(&mut self.key_rejected) {
                    self.toast(ctx, "That key was not accepted.");
                }
            }
            Msg::Projects(p) => self.projects = Some(p),
            Msg::Opened(o) => {
                self.opening = false;
                net::set_local(LAST_PROJECT, Some(&o.id));
                let session = self.next_session;
                self.next_session += 1;
                self.editor = Some(Editor::new(session, *o, self.tx.clone()));
            }
            Msg::OpenFailed(e) => {
                self.opening = false;
                net::set_local(LAST_PROJECT, None);
                self.toast(ctx, format!("Could not open project: {e}"));
            }
            Msg::Ed(session, m) => {
                if let Some(ed) = self.editor.as_mut().filter(|e| e.session == session) {
                    if let Some(t) = ed.handle(m) {
                        self.toast(ctx, t);
                    }
                }
            }
        }
    }
}

impl eframe::App for PodApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let msgs: Vec<Msg> = self.inbox.borrow_mut().drain(..).collect();
        for m in msgs {
            self.handle(&ctx, m);
        }

        if self.need_key {
            self.key_screen(ui);
        } else if let Some(ed) = &mut self.editor {
            let mut close = false;
            let toast = ed.ui(ui, &mut close);
            if let Some(t) = toast {
                self.toast(&ctx, t);
            }
            if close {
                self.editor = None;
                net::set_local(LAST_PROJECT, None);
                self.load_projects();
            }
        } else {
            self.list_screen(ui);
        }
        UNSAVED.with(|u| u.set(self.editor.as_ref().is_some_and(|e| e.unsaved())));

        let now = ctx.input(|i| i.time);
        if let Some((msg, until)) = &self.toast {
            if now > *until {
                self.toast = None;
            } else {
                let msg = msg.clone();
                egui::Area::new(egui::Id::new("toast"))
                    .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -72.0))
                    .order(egui::Order::Foreground)
                    .show(&ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.label(msg);
                        });
                    });
                ctx.request_repaint_after_secs(0.5);
            }
        }
    }
}

impl PodApp {
    fn key_screen(&mut self, ui: &mut Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.3);
                ui.heading("pod");
                ui.add_space(12.0);
                ui.label("Enter your access key");
                let r = ui.add(egui::TextEdit::singleline(&mut self.key_input).password(true).desired_width(260.0));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if (ui.button("Continue").clicked() || enter) && !self.key_input.trim().is_empty() {
                    net::set_key(Some(self.key_input.trim()));
                    self.key_input.clear();
                    self.need_key = false;
                    self.load_projects();
                }
            });
        });
    }

    fn list_screen(&mut self, ui: &mut Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.set_max_width(640.0_f32.min(ui.available_width()));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading("Episodes");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Without the key the list is asked for again: a signing-in proxy may
                    // still let it through, and a server that wants a key shows the key screen.
                    if net::key().is_some() && ui.small_button("Forget key").clicked() {
                        net::set_key(None);
                        self.load_projects();
                    }
                    if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                        self.load_projects();
                    }
                });
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.new_name).hint_text("New episode name"));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if (ui.button("+ Create").clicked() || enter) && !self.new_name.trim().is_empty() {
                    let name = std::mem::take(&mut self.new_name);
                    let tx = self.tx.clone();
                    self.opening = true;
                    tx.clone().spawn(async move {
                        #[derive(Deserialize)]
                        struct Id {
                            id: String,
                        }
                        Some(match net::send::<Id>("POST", "/projects", &serde_json::json!({ "name": name })).await {
                            Ok(id) => match net::get::<Opened>(&format!("/projects/{}", id.id)).await {
                                Ok(o) => Msg::Opened(Box::new(o)),
                                Err(e) => Msg::OpenFailed(e.message()),
                            },
                            Err(e) => http_msg(e),
                        })
                    });
                }
            });
            ui.separator();
            if self.opening {
                ui.spinner();
            }
            match &self.projects {
                None => {
                    ui.spinner();
                }
                Some(ps) if ps.is_empty() => {
                    ui.label("No episodes yet. Create one, then import each speaker's WAV.");
                }
                Some(ps) => {
                    let mut open = None;
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for p in ps {
                            let dur = p.sample_rate.map(|r| fmt_time(p.frames, r, false)).unwrap_or_default();
                            let text = format!(
                                "{}\n{} track{} · {}",
                                p.name,
                                p.tracks,
                                if p.tracks == 1 { "" } else { "s" },
                                if dur.is_empty() { "empty".into() } else { dur }
                            );
                            let b = egui::Button::new(text).min_size(vec2(ui.available_width(), 48.0));
                            if ui.add(b).clicked() {
                                open = Some(p.id.clone());
                            }
                        }
                    });
                    if let Some(id) = open {
                        self.open(id);
                    }
                }
            }
        });
    }
}

// ---------- editor ----------

#[derive(Clone, Copy)]
enum Drag {
    /// Making a new selection from `anchor`, started on track `track`.
    Select { anchor: u64, track: usize },
    /// Moving one edge of the selection; the other edge is `fixed`.
    Edge { fixed: u64 },
    Scrollbar { grab: f32 },
}

enum CacheState {
    Remote,
    Downloading(u64),
    Local,
    Failed,
}

struct Editor {
    session: u32,
    tx: Tx,
    id: String,
    name: String,

    // Sync: `confirmed` is the server's state at `version`; `local` adds the ops
    // not yet acknowledged, which is what the user sees and edits.
    confirmed: Project,
    version: u64,
    in_flight: Option<Vec<Op>>,
    pending: Vec<Op>,
    local: Project,
    next_sync: f64,
    backoff: f64,
    offline: bool,
    last_poll: f64,
    polling: bool,
    was_focused: bool,

    peaks: HashMap<String, Option<Peaks>>,
    cache: HashMap<String, CacheState>,
    downloading: bool,

    // View
    playhead: u64,
    view_start: f64,
    /// Frames per pixel.
    fpp: f64,
    sel: Option<(u64, u64)>,
    sel_tracks: BTreeSet<String>,
    muted: HashSet<String>,
    solo: HashSet<String>,
    /// Cut applies to all tracks unless unlinked (keeps speakers in sync).
    unlinked: bool,
    drag: Option<Drag>,
    drag_backup: Option<((Option<(u64, u64)>, BTreeSet<String>), u64)>,
    fitted: bool,
    saved_view: ViewState,
    view_changed_at: Option<f64>,

    player: Player,

    tasks: HashMap<u32, (String, f32)>,
    next_task: u32,
    exports: Vec<ExportInfo>,
    show_exports: bool,
    export_name: String,
    rename: Option<(String, String)>,
    pending_remove: Option<String>,
}

impl Editor {
    fn new(session: u32, o: Opened, tx: Tx) -> Self {
        let view: ViewState = serde_json::from_value(o.view).unwrap_or_default();
        let mut ed = Self {
            session,
            tx,
            id: o.id,
            name: o.name.clone(),
            local: o.project.clone(),
            confirmed: o.project,
            version: o.version,
            in_flight: None,
            pending: Vec::new(),
            next_sync: 0.0,
            backoff: 1.0,
            offline: false,
            last_poll: 0.0,
            polling: false,
            was_focused: true,
            peaks: HashMap::new(),
            cache: HashMap::new(),
            downloading: false,
            playhead: view.playhead,
            view_start: view.view_start,
            fpp: view.fpp,
            sel: view.sel,
            sel_tracks: view.sel_tracks.iter().cloned().collect(),
            muted: view.muted.iter().cloned().collect(),
            solo: view.solo.iter().cloned().collect(),
            unlinked: view.unlinked,
            drag: None,
            drag_backup: None,
            fitted: view.fpp > 0.0,
            saved_view: view,
            view_changed_at: None,
            player: Player::new(),
            tasks: HashMap::new(),
            next_task: 1,
            exports: o.exports,
            show_exports: false,
            export_name: o.name,
            rename: None,
            pending_remove: None,
        };
        ed.load_assets();
        ed
    }

    fn rate(&self) -> u32 {
        self.local.sample_rate.unwrap_or(48000)
    }

    fn unsaved(&self) -> bool {
        self.in_flight.is_some() || !self.pending.is_empty()
    }

    /// Starts fetching peaks for new tracks and caching their audio locally.
    fn load_assets(&mut self) {
        for t in &self.local.tracks {
            let h = t.source.peaks.clone();
            if self.peaks.contains_key(&h) {
                continue;
            }
            self.peaks.insert(h.clone(), None);
            let (tx, s) = (self.tx.clone(), self.session);
            wasm_bindgen_futures::spawn_local(async move {
                let r = store::read_all(&h).await.and_then(|b| Peaks::parse(&b));
                tx.ed(s, EdMsg::Peaks(h, r));
            });
        }
        for t in &self.local.tracks {
            self.cache.entry(t.source.wav.clone()).or_insert(CacheState::Remote);
        }
        if self.downloading {
            return;
        }
        // One download at a time so playback's range requests aren't starved.
        let next = self.local.tracks.iter().find(|t| matches!(self.cache.get(&t.source.wav), Some(CacheState::Remote)));
        if let Some(t) = next {
            let h = t.source.wav.clone();
            self.downloading = true;
            self.cache.insert(h.clone(), CacheState::Downloading(0));
            let (tx, s) = (self.tx.clone(), self.session);
            wasm_bindgen_futures::spawn_local(async move {
                let h2 = h.clone();
                let tx2 = tx.clone();
                let mut last = 0u64;
                let r = store::download(&h, move |n| {
                    // Throttle repaints to every 4 MB.
                    if n - last > 4 << 20 {
                        last = n;
                        tx2.ed(s, EdMsg::CacheProgress(h2.clone(), n));
                    }
                })
                .await;
                tx.ed(s, EdMsg::Cached(h, r));
            });
        }
    }

    /// Applies a user edit locally and queues it for the server.
    fn edit(&mut self, op: Op) -> Option<String> {
        match self.local.apply(&op) {
            Ok(()) => {
                self.pending.push(op);
                if self.player.playing {
                    let at = self.player.position();
                    self.play_from(at);
                }
                self.clamp_view(None);
                self.load_assets();
                None
            }
            Err(e) => Some(e),
        }
    }

    fn rebuild_local(&mut self) -> usize {
        self.local = self.confirmed.clone();
        let before = self.pending.len();
        let local = &mut self.local;
        self.pending.retain(|op| local.apply(op).is_ok());
        before - self.pending.len()
    }

    fn handle(&mut self, m: EdMsg) -> Option<String> {
        match m {
            EdMsg::Synced(v) => {
                for op in self.in_flight.take().unwrap_or_default() {
                    let _ = self.confirmed.apply(&op);
                }
                self.version = v;
                self.offline = false;
                self.backoff = 1.0;
            }
            EdMsg::Conflict(v, ops) => {
                // Another device edited since we last synced. Positions in our ops
                // refer to a timeline that no longer exists, so take theirs.
                for op in &ops {
                    let _ = self.confirmed.apply(op);
                }
                self.version = v;
                let lost = self.in_flight.take().map_or(0, |o| o.len()) + self.pending.len();
                self.pending.clear();
                self.rebuild_local();
                self.load_assets();
                return Some(format!(
                    "This episode was edited on another device; your last {lost} edit(s) here were discarded."
                ));
            }
            EdMsg::Rejected(e) => {
                self.in_flight = None;
                self.rebuild_local();
                return Some(format!("Server rejected an edit: {e}"));
            }
            EdMsg::SyncFailed => {
                let mut ops = self.in_flight.take().unwrap_or_default();
                ops.append(&mut self.pending);
                self.pending = ops;
                self.offline = true;
                self.backoff = (self.backoff * 2.0).min(30.0);
                self.next_sync = self.tx.ctx.input(|i| i.time) + self.backoff;
            }
            EdMsg::Remote(v, ops) => {
                self.polling = false;
                if v > self.version && self.in_flight.is_none() && self.pending.is_empty() {
                    for op in &ops {
                        let _ = self.confirmed.apply(op);
                    }
                    self.version = v;
                    self.rebuild_local();
                    self.load_assets();
                    self.clamp_view(None);
                    return Some("Updated with edits from another device.".into());
                }
            }
            EdMsg::Peaks(h, r) => match r {
                Ok(p) => {
                    self.peaks.insert(h, Some(p));
                }
                Err(e) => return Some(format!("Waveform failed to load: {e}")),
            },
            EdMsg::CacheProgress(h, n) => {
                self.cache.insert(h, CacheState::Downloading(n));
            }
            EdMsg::Cached(h, r) => {
                self.downloading = false;
                let failed = r.is_err();
                self.cache.insert(h, if failed { CacheState::Failed } else { CacheState::Local });
                self.load_assets();
                if let Err(e) = r {
                    return Some(format!("Could not keep audio on this device ({e}); it will stream instead."));
                }
            }
            EdMsg::Chunk { generation, start, samples, ch } => self.player.deliver(generation, start, samples, ch),
            EdMsg::ChunkFailed { generation, start, error } => {
                self.player.failed(generation, start);
                if generation == self.player.generation {
                    self.player.stop();
                    return Some(format!("Playback stopped: {error}"));
                }
            }
            EdMsg::Task(id, label, f) => {
                self.tasks.insert(id, (label, f));
            }
            EdMsg::TaskDone(id) => {
                self.tasks.remove(&id);
            }
            EdMsg::Imported(name, source) => {
                let id = format!("{:08x}", (js_sys::Math::random() * u32::MAX as f64) as u32);
                self.sel_tracks.insert(id.clone());
                return self.edit(Op::AddTrack { id, name, source });
            }
            EdMsg::Exported(e) => {
                self.exports.insert(0, e);
                self.show_exports = true;
                return Some("Export finished.".into());
            }
            EdMsg::ExportDeleted(id) => self.exports.retain(|e| e.id != id),
        }
        None
    }

    fn sync(&mut self, now: f64) {
        let (tx, s, id) = (self.tx.clone(), self.session, self.id.clone());
        if self.in_flight.is_none() && !self.pending.is_empty() && now >= self.next_sync {
            let ops = std::mem::take(&mut self.pending);
            let body = serde_json::json!({ "base": self.version, "ops": &ops });
            self.in_flight = Some(ops);
            wasm_bindgen_futures::spawn_local(async move {
                let r = net::send::<Value>("POST", &format!("/projects/{id}/ops"), &body).await;
                match r {
                    Ok(v) => tx.ed(s, EdMsg::Synced(v["version"].as_u64().unwrap_or(0))),
                    Err(e) if e.status == 409 => match serde_json::from_str::<OpsResponse>(&e.body) {
                        Ok(r) => tx.ed(s, EdMsg::Conflict(r.version, r.ops)),
                        Err(_) => tx.ed(s, EdMsg::SyncFailed),
                    },
                    Err(e) if e.status == 401 => tx.send(Msg::Unauthorized { keyed: e.keyed }),
                    // Keep the edits; they go out once the user has signed in again.
                    Err(e) if e.status == net::SIGNED_OUT => {
                        tx.send(Msg::Toast(e.message()));
                        tx.ed(s, EdMsg::SyncFailed)
                    }
                    Err(e) if e.status == 400 => tx.ed(s, EdMsg::Rejected(e.message())),
                    Err(_) => tx.ed(s, EdMsg::SyncFailed),
                }
            });
            return;
        }
        // Pick up edits made elsewhere: every 15 s, and right away when the window
        // regains focus (e.g. switching back from the other machine).
        let focused = self.tx.ctx.input(|i| i.viewport().focused.unwrap_or(true));
        let refocus = focused && !self.was_focused;
        self.was_focused = focused;
        if !self.unsaved() && !self.polling && (refocus || now - self.last_poll > 15.0) {
            self.last_poll = now;
            self.polling = true;
            let since = self.version;
            wasm_bindgen_futures::spawn_local(async move {
                match net::get::<OpsResponse>(&format!("/projects/{id}/ops?since={since}")).await {
                    Ok(r) => tx.ed(s, EdMsg::Remote(r.version, r.ops)),
                    Err(_) => tx.ed(s, EdMsg::Remote(0, Vec::new())),
                }
            });
        }
        // Save where we are (debounced) so the next device opens here.
        let view = self.view_state();
        if view != self.saved_view {
            let since = *self.view_changed_at.get_or_insert(now);
            if now - since > 1.5 {
                self.saved_view = view.clone();
                self.view_changed_at = None;
                let id = self.id.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let _ = net::send::<Value>("PUT", &format!("/projects/{id}/view"), &view).await;
                });
            }
        }
    }

    fn view_state(&self) -> ViewState {
        ViewState {
            playhead: if self.player.playing { self.saved_view.playhead } else { self.playhead },
            view_start: self.view_start.round(),
            fpp: self.fpp,
            sel: self.sel,
            sel_tracks: self.sel_tracks.iter().cloned().collect(),
            muted: self.muted.iter().cloned().collect(),
            solo: self.solo.iter().cloned().collect(),
            unlinked: self.unlinked,
        }
    }

    fn audible(&self) -> Vec<Track> {
        self.local
            .tracks
            .iter()
            .filter(|t| {
                if self.solo.is_empty() { !self.muted.contains(&t.id) } else { self.solo.contains(&t.id) }
            })
            .cloned()
            .collect()
    }

    fn play_from(&mut self, at: u64) {
        let end = match self.sel {
            Some((s, e)) if at >= s && at < e => e,
            _ => self.local.len(),
        };
        if at >= end {
            return;
        }
        self.player.start(at, end, self.rate());
    }

    fn toggle_play(&mut self) {
        if self.player.playing {
            self.playhead = self.player.position();
            self.player.stop();
        } else {
            let at = match self.sel {
                Some((s, _)) => s,
                None if self.playhead >= self.local.len() => 0,
                None => self.playhead,
            };
            self.play_from(at);
        }
    }

    /// Requests audio chunks as the player needs them.
    fn feed_player(&mut self) {
        if self.player.finished() {
            self.playhead = self.player.position();
            self.player.stop();
        }
        if let Some(req) = self.player.want_chunk() {
            let tracks = self.audible();
            let ch = audio::out_channels(&tracks);
            let (tx, s) = (self.tx.clone(), self.session);
            wasm_bindgen_futures::spawn_local(async move {
                match audio::render(&tracks, req.start, req.len, ch).await {
                    Ok(samples) => tx.ed(s, EdMsg::Chunk { generation: req.generation, start: req.start, samples, ch }),
                    Err(error) => tx.ed(s, EdMsg::ChunkFailed { generation: req.generation, start: req.start, error }),
                }
            });
        }
    }

    fn silence(&mut self) -> Option<String> {
        let (s, e) = self.sel?;
        if self.sel_tracks.is_empty() {
            return Some("Select one or more tracks first (tap a track's name).".into());
        }
        let tracks = self.sel_tracks.iter().cloned().collect();
        self.edit(Op::Silence { tracks, start: s, end: e })
    }

    fn cut(&mut self) -> Option<String> {
        let (s, e) = self.sel?;
        let tracks: Vec<String> = if self.unlinked {
            self.sel_tracks.iter().cloned().collect()
        } else {
            self.local.tracks.iter().map(|t| t.id.clone()).collect()
        };
        if tracks.is_empty() {
            return Some("Select one or more tracks first (tap a track's name).".into());
        }
        let r = self.edit(Op::Cut { tracks, start: s, end: e });
        if r.is_none() {
            self.sel = None;
            self.playhead = s;
        }
        r
    }

    fn import(&mut self) {
        let (tx, s) = (self.tx.clone(), self.session);
        let task = self.next_task;
        self.next_task += 100;
        store::pick_files(".wav,audio/wav,audio/x-wav,audio/wave", move |files| {
            wasm_bindgen_futures::spawn_local(async move {
                for (i, f) in files.into_iter().enumerate() {
                    let id = task + i as u32;
                    let name = f.name();
                    let label = name.clone();
                    let tx2 = tx.clone();
                    let r = transfer::import(f, move |phase, frac| {
                        tx2.ed(s, EdMsg::Task(id, format!("{phase} {label}"), frac));
                    })
                    .await;
                    tx.ed(s, EdMsg::TaskDone(id));
                    match r {
                        Ok(src) => {
                            let stem = name.rsplit_once('.').map_or(name.as_str(), |(a, _)| a).to_string();
                            tx.ed(s, EdMsg::Imported(stem, src));
                        }
                        Err(e) => tx.send(Msg::Toast(format!("{name}: {e}"))),
                    }
                }
            });
        });
    }

    fn export(&mut self) {
        let tracks = self.audible();
        let frames = tracks.iter().map(Track::len).max().unwrap_or(0);
        if frames == 0 {
            return;
        }
        let (tx, s, pid, rate) = (self.tx.clone(), self.session, self.id.clone(), self.rate());
        let name = if self.export_name.trim().is_empty() { self.name.clone() } else { self.export_name.clone() };
        let id = self.next_task;
        self.next_task += 1;
        self.tasks.insert(id, (format!("Exporting {name}"), 0.0));
        wasm_bindgen_futures::spawn_local(async move {
            let tx2 = tx.clone();
            let label = format!("Exporting {name}");
            let r = transfer::export(&pid, &name, tracks, rate, frames, move |f| {
                tx2.ed(s, EdMsg::Task(id, label.clone(), f));
            })
            .await;
            tx.ed(s, EdMsg::TaskDone(id));
            match r {
                Ok(e) => tx.ed(s, EdMsg::Exported(e)),
                Err(e) => tx.send(Msg::Toast(format!("Export failed: {e}"))),
            }
        });
    }

    // ----- view math -----

    fn clamp_view(&mut self, width: Option<f32>) {
        let w = width.unwrap_or(800.0).max(1.0) as f64;
        let len = self.local.len().max(1) as f64;
        self.fpp = self.fpp.clamp(1.0, (len / w * 1.5).max(1.0));
        let max_start = (len - w * self.fpp * 0.5).max(0.0);
        self.view_start = self.view_start.clamp(0.0, max_start);
    }

    fn zoom_at(&mut self, factor: f64, anchor_frame: f64, anchor_px: f32, width: f32) {
        self.fpp /= factor;
        self.clamp_view(Some(width));
        self.view_start = anchor_frame - anchor_px as f64 * self.fpp;
        self.clamp_view(Some(width));
    }

    fn fit(&mut self, width: f32) {
        self.fpp = self.local.len().max(1) as f64 / width.max(1.0) as f64;
        self.view_start = 0.0;
        self.clamp_view(Some(width));
    }

    // ----- UI -----

    /// Returns a message to show, if any.
    fn ui(&mut self, ui: &mut Ui, close: &mut bool) -> Option<String> {
        let ctx = ui.ctx().clone();
        let now = ctx.input(|i| i.time);
        let mut toast = None;
        self.sync(now);
        self.feed_player();
        if self.player.playing {
            ctx.request_repaint();
        } else if self.unsaved() || !self.tasks.is_empty() || self.downloading {
            ctx.request_repaint_after_secs(0.25);
        } else {
            ctx.request_repaint_after_secs(5.0);
        }

        let narrow = ctx.content_rect().width() < 720.0;
        let mut action: Option<Action> = None;

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                let back = ui.add_enabled(!self.unsaved(), egui::Button::new("◀"));
                if back.on_hover_text("All episodes").clicked() {
                    self.player.stop();
                    *close = true;
                }
                ui.label(RichText::new(&self.name).strong());
                ui.separator();
                if !narrow {
                    transport(ui, self.player.playing, &mut action, 30.0);
                    ui.separator();
                    edit_buttons(ui, self.sel.is_some(), &self.local, &mut action, 30.0, false);
                    ui.separator();
                }
                let pos = if self.player.playing { self.player.position() } else { self.playhead };
                ui.label(RichText::new(fmt_time(pos, self.rate(), true)).monospace().size(15.0));
                if let Some((s, e)) = self.sel {
                    ui.label(
                        RichText::new(format!("sel {}", fmt_time(e - s, self.rate(), true))).monospace().weak(),
                    );
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.menu_button("☰", |ui| {
                        if ui.button("Import WAV…").clicked() {
                            action = Some(Action::Import);
                        }
                        if ui.button("Export / downloads…").clicked() {
                            self.show_exports = true;
                        }
                        ui.separator();
                        ui.checkbox(&mut self.unlinked, "Cut only selected tracks")
                            .on_hover_text("Off (default): Cut removes the range from every track so speakers stay in sync.");
                        ui.separator();
                        if ui.button("Zoom to fit  (Ctrl+F)").clicked() {
                            action = Some(Action::Fit);
                        }
                    });
                    let (txt, col) = if self.offline {
                        (format!("offline · {} unsaved", self.pending.len()), Color32::from_rgb(220, 120, 40))
                    } else if self.unsaved() {
                        ("saving…".to_string(), ui.visuals().weak_text_color())
                    } else {
                        ("✔ saved".to_string(), Color32::from_rgb(70, 170, 90))
                    };
                    ui.label(RichText::new(txt).color(col).small());
                });
            });
            for (label, f) in self.tasks.values() {
                ui.add(egui::ProgressBar::new(*f).text(format!("{label} {:.0}%", f * 100.0)).desired_height(14.0));
            }
            ui.add_space(2.0);
        });

        if narrow {
            egui::Panel::bottom("mobile_bar").show(ui, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let h = 44.0;
                    transport(ui, self.player.playing, &mut action, h);
                    ui.separator();
                    edit_buttons(ui, self.sel.is_some(), &self.local, &mut action, h, true);
                });
                ui.add_space(4.0);
            });
        }

        egui::CentralPanel::default().frame(egui::Frame::central_panel(ui.style()).inner_margin(0.0)).show(ui, |ui| {
            if self.local.tracks.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.label("No tracks yet.");
                    ui.add_space(8.0);
                    if ui.add(egui::Button::new("Import WAV files…").min_size(vec2(200.0, 40.0))).clicked() {
                        action = Some(Action::Import);
                    }
                    ui.label(RichText::new("One file per speaker. All files need the same sample rate.").weak());
                });
            } else {
                self.timeline(ui, narrow, &mut action);
            }
        });

        // Keyboard shortcuts (Audacity-style), unless typing in a text field.
        if !ctx.egui_wants_keyboard_input() {
            // Use the modifiers carried by each key event: a quick shortcut can be
            // pressed and released within a single frame.
            let keys: Vec<(Key, egui::Modifiers)> = ctx.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::Key { key, pressed: true, modifiers, .. } => Some((*key, *modifiers)),
                        _ => None,
                    })
                    .collect()
            });
            for (key, m) in keys {
                let cmd = m.command;
                action = Some(match key {
                    Key::Space => Action::Play,
                    Key::L if cmd => Action::Silence,
                    Key::S if !cmd => Action::Silence,
                    Key::K if cmd => Action::Cut,
                    Key::X if !cmd => Action::Cut,
                    Key::Delete | Key::Backspace => Action::Cut,
                    Key::Z if cmd && m.shift => Action::Redo,
                    Key::Y if cmd => Action::Redo,
                    Key::Z if cmd => Action::Undo,
                    Key::A if cmd => Action::SelectAll,
                    Key::F if cmd => Action::Fit,
                    Key::Num1 if cmd => Action::Zoom(2.0),
                    Key::Num3 if cmd => Action::Zoom(0.5),
                    Key::Home => Action::Seek(0),
                    Key::End => Action::Seek(u64::MAX),
                    Key::Escape => Action::ClearSelection,
                    _ => continue,
                });
            }
        }

        if let Some(a) = action {
            let width = ctx.content_rect().width() - header_width(narrow);
            toast = match a {
                Action::Play => {
                    self.toggle_play();
                    None
                }
                Action::Home => {
                    self.player.stop();
                    self.playhead = 0;
                    self.view_start = 0.0;
                    None
                }
                Action::Silence => self.silence(),
                Action::Cut => self.cut(),
                Action::Undo => self.edit(Op::Undo),
                Action::Redo => self.edit(Op::Redo),
                Action::SelectAll => {
                    self.sel = Some((0, self.local.len()));
                    self.sel_tracks = self.local.tracks.iter().map(|t| t.id.clone()).collect();
                    None
                }
                Action::ClearSelection => {
                    self.sel = None;
                    None
                }
                Action::Fit => {
                    self.fit(width);
                    None
                }
                Action::Zoom(f) => {
                    let center = self.view_start + width as f64 * self.fpp / 2.0;
                    self.zoom_at(f, center, width / 2.0, width);
                    None
                }
                Action::Seek(f) => {
                    self.playhead = f.min(self.local.len());
                    if self.player.playing {
                        self.play_from(self.playhead);
                    }
                    let vis = width as f64 * self.fpp;
                    let p = self.playhead as f64;
                    if p < self.view_start || p > self.view_start + vis {
                        self.view_start = p - vis * 0.1;
                        self.clamp_view(Some(width));
                    }
                    None
                }
                Action::Import => {
                    self.import();
                    None
                }
            };
        }

        self.exports_window(&ctx);
        self.rename_window(&ctx);
        toast
    }

    fn exports_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_exports;
        let mut start = false;
        let mut delete = None;
        egui::Window::new("Export").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            ui.label("Renders the edit as a 24-bit WAV in your browser and saves it on the server, so it can be downloaded from any device. Muted tracks are left out.");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.export_name).desired_width(180.0));
                let busy = self.tasks.values().any(|(l, _)| l.starts_with("Exporting"));
                if ui.add_enabled(!busy && !self.local.tracks.is_empty(), egui::Button::new("Render")).clicked() {
                    start = true;
                }
            });
            ui.separator();
            if self.exports.is_empty() {
                ui.label(RichText::new("No exports yet.").weak());
            }
            for e in &self.exports {
                ui.horizontal(|ui| {
                    ui.hyperlink_to(
                        format!("⬇ {}.wav ({:.0} MB)", e.name, e.size as f64 / 1e6),
                        net::keyed_url(&format!("/exports/{}", e.id)),
                    );
                    if ui.small_button("🗑").on_hover_text("Delete").clicked() {
                        delete = Some(e.id.clone());
                    }
                });
            }
        });
        self.show_exports = open;
        if start {
            self.export();
        }
        if let Some(id) = delete {
            let (tx, s) = (self.tx.clone(), self.session);
            wasm_bindgen_futures::spawn_local(async move {
                if net::fetch("DELETE", &format!("/exports/{id}"), net::Body::None, None).await.is_ok() {
                    tx.ed(s, EdMsg::ExportDeleted(id));
                }
            });
        }
    }

    fn rename_window(&mut self, ctx: &egui::Context) {
        let Some((id, mut name)) = self.rename.take() else { return };
        let mut keep = true;
        let mut commit = false;
        egui::Window::new("Rename track").collapsible(false).resizable(false).show(ctx, |ui| {
            let r = ui.text_edit_singleline(&mut name);
            r.request_focus();
            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                commit = true;
            }
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    commit = true;
                }
                if ui.button("Cancel").clicked() {
                    keep = false;
                }
            });
        });
        if commit {
            self.edit(Op::RenameTrack { id, name });
        } else if keep {
            self.rename = Some((id, name));
        }
    }

    fn timeline(&mut self, ui: &mut Ui, narrow: bool, action: &mut Option<Action>) {
        let full = ui.available_rect_before_wrap();
        let hw = header_width(narrow);
        let ruler_h = 24.0;
        let bar_h = if narrow { 20.0 } else { 14.0 };
        let wave_x0 = full.left() + hw;
        let width = (full.right() - wave_x0).max(1.0);
        if !self.fitted && !self.local.is_empty() {
            self.fit(width);
            self.fitted = true;
        }
        self.clamp_view(Some(width));
        let rate = self.rate();
        let n = self.local.tracks.len();
        let area_top = full.top() + ruler_h;
        let area_bottom = full.bottom() - bar_h;
        let th = ((area_bottom - area_top) / n as f32).clamp(56.0, 240.0);
        let ruler = Rect::from_min_max(pos2(wave_x0, full.top()), pos2(full.right(), area_top));
        let waves = Rect::from_min_max(pos2(wave_x0, area_top), pos2(full.right(), area_top + th * n as f32));
        let bar = Rect::from_min_max(pos2(wave_x0, area_bottom), pos2(full.right(), full.bottom()));

        let to_x = |f: f64, vs: f64, fpp: f64| wave_x0 + ((f - vs) / fpp) as f32;
        let dark = ui.visuals().dark_mode;
        let pal = Palette::new(dark);
        let painter = ui.painter_at(full);
        painter.rect_filled(full, 0.0, pal.backdrop);

        // ---- input ----
        let wave_resp = ui.interact(waves, ui.id().with("waves"), Sense::click_and_drag());
        let ruler_resp = ui.interact(ruler, ui.id().with("ruler"), Sense::click_and_drag());
        let bar_resp = ui.interact(bar, ui.id().with("bar"), Sense::click_and_drag());
        let frame_at = |x: f32, vs: f64, fpp: f64| -> u64 { (vs + (x - wave_x0).max(0.0) as f64 * fpp).max(0.0) as u64 };
        let track_at = |y: f32| -> usize { (((y - area_top) / th).floor().max(0.0) as usize).min(n - 1) };

        let (zoom, scroll, touch) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta(), i.multi_touch()));
        let hover = ui.input(|i| i.pointer.hover_pos()).filter(|p| full.contains(*p));
        if let Some(t) = touch.filter(|t| t.num_touches >= 2) {
            // Two fingers: pinch to zoom, drag to scroll. Undo any selection the
            // first finger started.
            if let Some((sel, ph)) = self.drag_backup.take() {
                (self.sel, self.sel_tracks) = sel;
                self.playhead = ph;
            }
            self.drag = None;
            let anchor = self.view_start + (t.center_pos.x - wave_x0) as f64 * self.fpp;
            if t.zoom_delta != 1.0 {
                self.zoom_at(t.zoom_delta as f64, anchor, t.center_pos.x - wave_x0, width);
            }
            self.view_start -= t.translation_delta.x as f64 * self.fpp;
            self.clamp_view(Some(width));
        } else if hover.is_some() {
            if zoom != 1.0 {
                let px = hover.unwrap().x - wave_x0;
                let anchor = self.view_start + px as f64 * self.fpp;
                self.zoom_at(zoom as f64, anchor, px, width);
            }
            let d = if scroll.x.abs() > scroll.y.abs() { scroll.x } else { scroll.y };
            if d != 0.0 {
                self.view_start -= d as f64 * self.fpp;
                self.clamp_view(Some(width));
            }
        }

        let (vs, fpp) = (self.view_start, self.fpp);
        if wave_resp.drag_started()
            && let Some(p) = wave_resp.interact_pointer_pos()
        {
            self.drag_backup = Some(((self.sel, self.sel_tracks.clone()), self.playhead));
            let f = frame_at(p.x, vs, fpp);
            let grab = if narrow { 22.0 } else { 8.0 };
            self.drag = match self.sel {
                Some((s, e)) if (to_x(s as f64, vs, fpp) - p.x).abs() < grab => Some(Drag::Edge { fixed: e }),
                Some((s, e)) if (to_x(e as f64, vs, fpp) - p.x).abs() < grab => Some(Drag::Edge { fixed: s }),
                _ => Some(Drag::Select { anchor: f, track: track_at(p.y) }),
            };
        }
        if wave_resp.dragged()
            && let (Some(d), Some(p)) = (self.drag, wave_resp.interact_pointer_pos())
        {
            let f = frame_at(p.x, vs, fpp).min(self.local.len());
            match d {
                Drag::Select { anchor, track } => {
                    let (a, b) = (anchor.min(f), anchor.max(f));
                    self.sel = (b > a).then_some((a, b));
                    let (t0, t1) = (track.min(track_at(p.y)), track.max(track_at(p.y)));
                    self.sel_tracks = self.local.tracks[t0..=t1].iter().map(|t| t.id.clone()).collect();
                    self.playhead = a;
                }
                Drag::Edge { fixed } => {
                    self.sel = Some((fixed.min(f), fixed.max(f))).filter(|(a, b)| b > a);
                    if let Some((a, _)) = self.sel {
                        self.playhead = a;
                    }
                }
                Drag::Scrollbar { .. } => {}
            }
            // Auto-scroll when dragging past an edge.
            if p.x > full.right() - 10.0 {
                self.view_start += 8.0 * fpp;
            } else if p.x < wave_x0 + 10.0 {
                self.view_start -= 8.0 * fpp;
            }
        }
        if wave_resp.drag_stopped() {
            self.drag = None;
            self.drag_backup = None;
        }
        if wave_resp.clicked()
            && let Some(p) = wave_resp.interact_pointer_pos()
        {
            // Click: put the cursor here and select just this track (as Audacity does).
            self.sel = None;
            let t = track_at(p.y);
            self.sel_tracks = [self.local.tracks[t].id.clone()].into();
            *action = Some(Action::Seek(frame_at(p.x, vs, fpp)));
        }
        if wave_resp.double_clicked() {
            let p = wave_resp.interact_pointer_pos().unwrap_or_default();
            let t = &self.local.tracks[track_at(p.y)];
            self.sel = Some((0, t.len()));
        }
        if (ruler_resp.clicked() || ruler_resp.dragged())
            && let Some(p) = ruler_resp.interact_pointer_pos()
        {
            *action = Some(Action::Seek(frame_at(p.x, vs, fpp)));
        }

        // Scrollbar
        let total = self.local.len().max(1) as f64;
        let vis = width as f64 * fpp;
        let tw = ((vis / total) as f32 * width).clamp(24.0, width);
        let tx0 = wave_x0 + ((vs / total) as f32 * width).min(width - tw);
        let thumb = Rect::from_min_size(pos2(tx0, bar.top() + 2.0), vec2(tw, bar.height() - 4.0));
        if bar_resp.drag_started()
            && let Some(p) = bar_resp.interact_pointer_pos()
        {
            let grab = if thumb.contains(p) { p.x - thumb.left() } else { tw / 2.0 };
            self.drag = Some(Drag::Scrollbar { grab });
        }
        if let (true, Some(Drag::Scrollbar { grab }), Some(p)) =
            (bar_resp.dragged(), self.drag, bar_resp.interact_pointer_pos())
        {
            self.view_start = ((p.x - grab - wave_x0) / width) as f64 * total;
            self.clamp_view(Some(width));
        }
        if bar_resp.drag_stopped() {
            self.drag = None;
        }
        if bar_resp.clicked()
            && let Some(p) = bar_resp.interact_pointer_pos()
        {
            self.view_start = ((p.x - wave_x0) / width) as f64 * total - vis / 2.0;
            self.clamp_view(Some(width));
        }

        // Follow the playhead while playing.
        let pos = if self.player.playing { self.player.position() } else { self.playhead };
        if self.player.playing && self.drag.is_none() {
            let x = to_x(pos as f64, self.view_start, self.fpp);
            if x > full.right() - 4.0 || x < wave_x0 {
                self.view_start = pos as f64;
                self.clamp_view(Some(width));
            }
        }
        let (vs, fpp) = (self.view_start, self.fpp);

        // ---- drawing ----
        draw_ruler(&painter, ruler, vs, fpp, rate, &pal);
        painter.rect_filled(bar, 0.0, pal.backdrop);
        painter.rect_filled(thumb, 4.0, pal.thumb);

        let vis_end = (vs + width as f64 * fpp).ceil() as u64 + 1;
        for i in 0..n {
            let t = self.local.tracks[i].clone();
            let t = &t;
            let top = area_top + i as f32 * th;
            let lane = Rect::from_min_max(pos2(wave_x0, top), pos2(full.right(), top + th - 2.0));
            let selected = self.sel_tracks.contains(&t.id);
            painter.rect_filled(lane, 0.0, pal.lane);
            if let (true, Some((s, e))) = (selected, self.sel) {
                let r = Rect::from_x_y_ranges(
                    to_x(s as f64, vs, fpp).max(lane.left())..=to_x(e as f64, vs, fpp).min(lane.right()),
                    lane.y_range(),
                );
                if r.width() > 0.0 {
                    painter.rect_filled(r, 0.0, pal.selection);
                }
            }
            let end_x = to_x(t.len() as f64, vs, fpp);
            if end_x < lane.right() {
                painter.rect_filled(Rect::from_x_y_ranges(end_x.max(lane.left())..=lane.right(), lane.y_range()), 0.0, pal.backdrop);
            }
            let quiet = if self.solo.is_empty() { self.muted.contains(&t.id) } else { !self.solo.contains(&t.id) };
            match self.peaks.get(&t.source.peaks) {
                Some(Some(p)) => draw_wave(&painter, lane, t, p, vs, fpp, vis_end, &pal, quiet),
                _ => {
                    painter.text(lane.center(), Align2::CENTER_CENTER, "loading waveform…", FontId::proportional(13.0), pal.text_weak);
                }
            }

            // Header
            let head = Rect::from_min_max(pos2(full.left(), top), pos2(wave_x0 - 1.0, top + th - 2.0));
            painter.rect_filled(head, 0.0, if selected { pal.head_selected } else { pal.head });
            let hr = ui.interact(head, ui.id().with(("head", &t.id)), Sense::click());
            if hr.clicked() {
                if !self.sel_tracks.remove(&t.id) {
                    self.sel_tracks.insert(t.id.clone());
                }
            }
            let mut remove = false;
            hr.context_menu(|ui| {
                if ui.button("Rename…").clicked() {
                    self.rename = Some((t.id.clone(), t.name.clone()));
                    ui.close();
                }
                if ui.button("Remove track").clicked() {
                    remove = true;
                    ui.close();
                }
            });
            if remove {
                self.pending_remove = Some(t.id.clone());
            }
            let mut hui = ui.new_child(UiBuilder::new().max_rect(head.shrink(6.0)));
            hui.label(RichText::new(&t.name).strong().color(pal.text));
            hui.horizontal(|ui| {
                let m = self.muted.contains(&t.id);
                if ui.selectable_label(m, "M").on_hover_text("Mute").clicked() {
                    if m { self.muted.remove(&t.id); } else { self.muted.insert(t.id.clone()); }
                    if self.player.playing { let at = self.player.position(); self.play_from(at); }
                }
                let s = self.solo.contains(&t.id);
                if ui.selectable_label(s, "S").on_hover_text("Solo").clicked() {
                    if s { self.solo.remove(&t.id); } else { self.solo.insert(t.id.clone()); }
                    if self.player.playing { let at = self.player.position(); self.play_from(at); }
                }
            });
            let badge = match self.cache.get(&t.source.wav) {
                Some(CacheState::Local) => "✔ on device".to_string(),
                Some(CacheState::Downloading(b)) => {
                    let total = t.source.format.data_offset + t.source.format.frames * t.source.format.bytes_per_frame();
                    format!("⬇ {:.0}%", *b as f64 / total.max(1) as f64 * 100.0)
                }
                Some(CacheState::Failed) => "☁ streaming".into(),
                _ => "☁".into(),
            };
            hui.label(RichText::new(badge).small().color(pal.text_weak));
        }

        // Selection edges and playhead across all lanes.
        if let Some((s, e)) = self.sel {
            for f in [s, e] {
                let x = to_x(f as f64, vs, fpp);
                if x >= wave_x0 && x <= full.right() {
                    painter.line_segment([pos2(x, area_top), pos2(x, waves.bottom())], Stroke::new(1.0, pal.sel_edge));
                }
            }
        }
        let x = to_x(pos as f64, vs, fpp);
        if x >= wave_x0 && x <= full.right() {
            painter.line_segment([pos2(x, full.top()), pos2(x, waves.bottom())], Stroke::new(1.5, pal.playhead));
            painter.add(egui::Shape::convex_polygon(
                vec![pos2(x - 6.0, full.top()), pos2(x + 6.0, full.top()), pos2(x, full.top() + 9.0)],
                pal.playhead,
                Stroke::NONE,
            ));
        }
        if let Some(id) = self.pending_remove.take() {
            self.sel_tracks.remove(&id);
            if let Some(t) = self.edit(Op::RemoveTrack { id }) {
                self.tx.send(Msg::Toast(t));
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Action {
    Play,
    Home,
    Silence,
    Cut,
    Undo,
    Redo,
    SelectAll,
    ClearSelection,
    Fit,
    Zoom(f64),
    Seek(u64),
    Import,
}

fn header_width(narrow: bool) -> f32 {
    if narrow { 86.0 } else { 140.0 }
}

fn transport(ui: &mut Ui, playing: bool, action: &mut Option<Action>, h: f32) {
    let b = |t: &str| egui::Button::new(RichText::new(t).size(h * 0.5)).min_size(vec2(h, h));
    if ui.add(b("⏮")).on_hover_text("To start (Home)").clicked() {
        *action = Some(Action::Home);
    }
    if ui.add(b(if playing { "⏸" } else { "▶" })).on_hover_text("Play / pause (Space)").clicked() {
        *action = Some(Action::Play);
    }
}

fn edit_buttons(ui: &mut Ui, has_sel: bool, p: &Project, action: &mut Option<Action>, h: f32, compact: bool) {
    let b = |t: &str| egui::Button::new(RichText::new(t).size(if compact { h * 0.5 } else { h * 0.42 })).min_size(vec2(h, h));
    if ui.add_enabled(has_sel, b(if compact { "🔇" } else { "🔇 Silence" })).on_hover_text("Silence selection on selected tracks (Ctrl+L or S)").clicked() {
        *action = Some(Action::Silence);
    }
    if ui.add_enabled(has_sel, b(if compact { "✂" } else { "✂ Cut" })).on_hover_text("Remove selection (Delete, Ctrl+K or X)").clicked() {
        *action = Some(Action::Cut);
    }
    if ui.add_enabled(!p.undo.is_empty(), b("⟲")).on_hover_text("Undo (Ctrl+Z)").clicked() {
        *action = Some(Action::Undo);
    }
    if ui.add_enabled(!p.redo.is_empty(), b("⟳")).on_hover_text("Redo (Ctrl+Shift+Z)").clicked() {
        *action = Some(Action::Redo);
    }
}

struct Palette {
    backdrop: Color32,
    lane: Color32,
    selection: Color32,
    wave: Color32,
    wave_quiet: Color32,
    silenced: Color32,
    cut_mark: Color32,
    head: Color32,
    head_selected: Color32,
    ruler: Color32,
    text: Color32,
    text_weak: Color32,
    playhead: Color32,
    sel_edge: Color32,
    thumb: Color32,
}

impl Palette {
    /// Audacity-like: blue waveform on a light lane; a dark variant for dark mode.
    fn new(dark: bool) -> Self {
        if dark {
            Self {
                backdrop: Color32::from_rgb(30, 31, 35),
                lane: Color32::from_rgb(44, 46, 54),
                selection: Color32::from_rgb(64, 74, 118),
                wave: Color32::from_rgb(118, 144, 255),
                wave_quiet: Color32::from_rgb(90, 94, 110),
                silenced: Color32::from_rgba_unmultiplied(0, 0, 0, 70),
                cut_mark: Color32::from_rgb(230, 80, 80),
                head: Color32::from_rgb(52, 54, 62),
                head_selected: Color32::from_rgb(70, 80, 120),
                ruler: Color32::from_rgb(40, 42, 48),
                text: Color32::from_rgb(225, 228, 235),
                text_weak: Color32::from_rgb(150, 155, 170),
                playhead: Color32::from_rgb(90, 220, 120),
                sel_edge: Color32::from_rgb(200, 210, 255),
                thumb: Color32::from_rgb(85, 90, 105),
            }
        } else {
            Self {
                backdrop: Color32::from_rgb(200, 200, 206),
                lane: Color32::from_rgb(255, 255, 255),
                selection: Color32::from_rgb(196, 204, 240),
                wave: Color32::from_rgb(50, 50, 200),
                wave_quiet: Color32::from_rgb(160, 160, 175),
                silenced: Color32::from_rgba_unmultiplied(120, 120, 130, 45),
                cut_mark: Color32::from_rgb(210, 40, 40),
                head: Color32::from_rgb(228, 228, 234),
                head_selected: Color32::from_rgb(196, 204, 240),
                ruler: Color32::from_rgb(236, 236, 240),
                text: Color32::from_rgb(25, 25, 35),
                text_weak: Color32::from_rgb(105, 105, 120),
                playhead: Color32::from_rgb(20, 150, 50),
                sel_edge: Color32::from_rgb(40, 40, 120),
                thumb: Color32::from_rgb(150, 150, 165),
            }
        }
    }
}

fn draw_ruler(painter: &egui::Painter, r: Rect, vs: f64, fpp: f64, rate: u32, pal: &Palette) {
    painter.rect_filled(r, 0.0, pal.ruler);
    let secs_per_px = fpp / rate as f64;
    const STEPS: [f64; 16] =
        [0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 5.0, 10.0, 15.0, 30.0, 60.0, 300.0, 600.0, 1800.0];
    let step = STEPS.iter().copied().find(|s| s / secs_per_px >= 80.0).unwrap_or(3600.0);
    let t0 = vs / rate as f64;
    let mut t = (t0 / step).floor() * step;
    let t_end = t0 + r.width() as f64 * secs_per_px;
    while t <= t_end {
        let x = r.left() + ((t - t0) / secs_per_px) as f32;
        if x >= r.left() {
            painter.line_segment([pos2(x, r.bottom() - 8.0), pos2(x, r.bottom())], Stroke::new(1.0, pal.text_weak));
            painter.text(
                pos2(x + 3.0, r.top() + 3.0),
                Align2::LEFT_TOP,
                fmt_secs(t, step < 1.0),
                FontId::monospace(11.0),
                pal.text_weak,
            );
        }
        for k in 1..5 {
            let xm = x + (step / 5.0 * k as f64 / secs_per_px) as f32;
            if xm >= r.left() && xm <= r.right() {
                painter.line_segment([pos2(xm, r.bottom() - 4.0), pos2(xm, r.bottom())], Stroke::new(1.0, pal.text_weak));
            }
        }
        t += step;
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_wave(
    painter: &egui::Painter,
    lane: Rect,
    t: &Track,
    peaks: &Peaks,
    vs: f64,
    fpp: f64,
    vis_end: u64,
    pal: &Palette,
    quiet: bool,
) {
    let pieces = t.pieces(vs.max(0.0) as u64, vis_end);
    let mid = lane.center().y;
    let half = lane.height() / 2.0 - 2.0;
    let color = if quiet { pal.wave_quiet } else { pal.wave };
    let mut mesh = Mesh::default();
    let cols = lane.width().ceil() as usize;
    let mut pi = 0;
    for col in 0..cols {
        let f0 = vs + col as f64 * fpp;
        let (a, b) = (f0.max(0.0) as u64, ((f0 + fpp).ceil() as u64).max(f0 as u64 + 1));
        while pi < pieces.len() && pieces[pi].at + pieces[pi].len <= a {
            pi += 1;
        }
        let (mut lo, mut hi, mut any, mut muted) = (1.0f32, -1.0f32, false, false);
        let mut j = pi;
        while j < pieces.len() && pieces[j].at < b {
            let p = pieces[j];
            let (s, e) = (a.max(p.at), b.min(p.at + p.len));
            if p.muted {
                muted = true;
            } else if e > s
                && let Some((l, h)) = peaks.range(p.src + (s - p.at), p.src + (e - p.at))
            {
                lo = lo.min(l);
                hi = hi.max(h);
                any = true;
            }
            j += 1;
        }
        let x = lane.left() + col as f32;
        if any {
            let y0 = mid - hi * half;
            let y1 = (mid - lo * half).max(y0 + 1.0);
            mesh.add_colored_rect(Rect::from_min_max(pos2(x, y0), pos2(x + 1.0, y1)), color);
        } else if muted {
            mesh.add_colored_rect(Rect::from_min_max(pos2(x, mid - 0.5), pos2(x + 1.0, mid + 0.5)), pal.wave_quiet);
        }
    }
    painter.add(mesh);
    let to_x = |f: u64| lane.left() + ((f as f64 - vs) / fpp) as f32;
    for (i, p) in pieces.iter().enumerate() {
        let (x0, x1) = (to_x(p.at).max(lane.left()), to_x(p.at + p.len).min(lane.right()));
        if p.muted && x1 > x0 {
            painter.rect_filled(Rect::from_x_y_ranges(x0..=x1, lane.y_range()), 0.0, pal.silenced);
        }
        // Mark cut points: where the source jumps.
        if i > 0 {
            let q = pieces[i - 1];
            if q.src + q.len != p.src && x0 > lane.left() {
                painter.line_segment([pos2(x0, lane.top()), pos2(x0, lane.bottom())], Stroke::new(1.0, pal.cut_mark));
            }
        }
    }
}

fn fmt_secs(t: f64, ms: bool) -> String {
    let total = t.max(0.0);
    let h = (total / 3600.0) as u64;
    let m = ((total % 3600.0) / 60.0) as u64;
    let s = total % 60.0;
    match (h, ms) {
        (0, false) => format!("{m}:{:02}", s as u64),
        (0, true) => format!("{m}:{s:06.3}"),
        (_, false) => format!("{h}:{m:02}:{:02}", s as u64),
        (_, true) => format!("{h}:{m:02}:{s:06.3}"),
    }
}

fn fmt_time(frames: u64, rate: u32, ms: bool) -> String {
    fmt_secs(frames as f64 / rate.max(1) as f64, ms)
}
