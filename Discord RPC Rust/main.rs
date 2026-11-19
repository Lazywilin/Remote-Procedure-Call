//! Game browser + custom Rich Presence with a full status editor.
//! Display only: nothing is sent to Discord's tracking endpoints, so badges
//! and real play time are untouched.

use discord_rich_presence::{DiscordIpc, DiscordIpcClient};
use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, HashSet},
    env, fs,
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

const DETECTABLE_URL: &str = "https://discord.com/api/v9/applications/detectable";
const CARD_W: f32 = 170.0;
const ROW_H: f32 = 330.0;

// ---------- game data ----------

#[derive(Deserialize, Clone)]
struct Exe {
    name: String,
    #[serde(default)]
    is_launcher: bool,
}

#[derive(Deserialize, Clone)]
struct App {
    id: String,
    name: String,
    aliases: Option<Vec<String>>,
    themes: Option<Vec<String>>,
    cover_image_hash: Option<String>,
    icon_hash: Option<String>,
    executables: Option<Vec<Exe>>,
}

impl App {
    fn matches(&self, q: &str) -> bool {
        self.name.to_lowercase().contains(q)
            || self.aliases.iter().flatten().any(|a| a.to_lowercase().contains(q))
    }
    fn genres(&self) -> String {
        self.themes.iter().flatten().take(2).cloned().collect::<Vec<_>>().join(" · ")
    }
    /// Card image in the grid.
    fn image_url(&self) -> Option<String> {
        if let Some(h) = &self.cover_image_hash {
            return Some(format!("https://cdn.discordapp.com/app-icons/{}/{h}.webp?size=300", self.id));
        }
        self.icon_hash
            .as_ref()
            .map(|h| format!("https://cdn.discordapp.com/app-icons/{}/{h}.webp?size=128", self.id))
    }
    /// Square art used as the status' large image.
    fn art_url(&self) -> Option<String> {
        self.icon_hash
            .as_ref()
            .map(|h| format!("https://cdn.discordapp.com/app-icons/{}/{h}.png?size=512", self.id))
            .or_else(|| self.image_url())
    }
}

fn fetch_apps() -> Result<Vec<App>, String> {
    let mut apps: Vec<App> = reqwest::blocking::get(DETECTABLE_URL)
        .and_then(|r| r.json())
        .map_err(|e| e.to_string())?;
    apps.sort_by_key(|a| (a.cover_image_hash.is_none(), a.name.to_lowercase()));
    Ok(apps)
}

// ---------- status style ----------

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Default)]
enum Kind {
    #[default]
    Playing,
    Listening,
    Watching,
    Competing,
}

impl Kind {
    const ALL: [Kind; 4] = [Kind::Playing, Kind::Listening, Kind::Watching, Kind::Competing];
    fn code(self) -> u8 {
        match self {
            Kind::Playing => 0,
            Kind::Listening => 2,
            Kind::Watching => 3,
            Kind::Competing => 5,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Kind::Playing => "Playing",
            Kind::Listening => "Listening to",
            Kind::Watching => "Watching",
            Kind::Competing => "Competing in",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Default)]
enum Timer {
    #[default]
    Elapsed,
    Countdown,
    Hidden,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
struct Style {
    kind: Kind,
    details: String,
    state: String,
    timer: Timer,
    hours: String,
    party_cur: String,
    party_max: String,
    large_text: String,
    use_art: bool,
    btn1_label: String,
    btn1_url: String,
    btn2_label: String,
    btn2_url: String,
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn parse_hours(s: &str) -> f64 {
    s.trim().parse::<f64>().unwrap_or(0.0).max(0.0)
}

/// Discord needs 2 to 128 characters per text line.
fn clean(s: &str) -> Option<String> {
    let t = s.trim();
    (t.chars().count() >= 2).then(|| t.chars().take(128).collect())
}

fn payload(s: &Style, art: Option<String>) -> Value {
    let now = unix_now();
    let secs = (parse_hours(&s.hours) * 3600.0) as i64;
    let mut act = serde_json::Map::new();
    act.insert("type".into(), json!(s.kind.code()));
    if let Some(d) = clean(&s.details) {
        act.insert("details".into(), json!(d));
    }
    if let Some(d) = clean(&s.state) {
        act.insert("state".into(), json!(d));
    }
    match s.timer {
        Timer::Elapsed => {
            act.insert("timestamps".into(), json!({ "start": now - secs }));
        }
        Timer::Countdown => {
            act.insert("timestamps".into(), json!({ "end": now + secs }));
        }
        Timer::Hidden => {}
    }
    if let Some(img) = art {
        let mut assets = serde_json::Map::new();
        assets.insert("large_image".into(), json!(img));
        if let Some(t) = clean(&s.large_text) {
            assets.insert("large_text".into(), json!(t));
        }
        act.insert("assets".into(), Value::Object(assets));
    }
    let max: i64 = s.party_max.trim().parse().unwrap_or(0);
    if max > 0 {
        let cur = s.party_cur.trim().parse::<i64>().unwrap_or(1).clamp(1, max);
        act.insert("party".into(), json!({ "id": "gts-party", "size": [cur, max] }));
    }
    let buttons: Vec<Value> = [(&s.btn1_label, &s.btn1_url), (&s.btn2_label, &s.btn2_url)]
        .into_iter()
        .filter_map(|(l, u)| {
            let (l, u) = (l.trim(), u.trim());
            (!l.is_empty() && (u.starts_with("https://") || u.starts_with("http://")))
                .then(|| json!({ "label": l.chars().take(32).collect::<String>(), "url": u }))
        })
        .collect();
    if !buttons.is_empty() {
        act.insert("buttons".into(), Value::Array(buttons));
    }
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    json!({
        "cmd": "SET_ACTIVITY",
        "args": { "pid": std::process::id(), "activity": Value::Object(act) },
        "nonce": nonce.to_string()
    })
}

fn preview(game: &str, s: &Style) -> String {
    let mut out = format!("{} {game}", s.kind.label());
    if let Some(d) = clean(&s.details) {
        out += &format!("\n{d}");
    }
    if let Some(d) = clean(&s.state) {
        out += &format!("\n{d}");
    }
    let max: i64 = s.party_max.trim().parse().unwrap_or(0);
    if max > 0 {
        let cur = s.party_cur.trim().parse::<i64>().unwrap_or(1).clamp(1, max);
        out += &format!("  ({cur} of {max})");
    }
    let h = parse_hours(&s.hours);
    let (hh, mm) = (h as i64, ((h.fract()) * 60.0) as i64);
    match s.timer {
        Timer::Elapsed => out += &format!("\n{hh:02}:{mm:02}:00 elapsed"),
        Timer::Countdown => out += &format!("\n{hh:02}:{mm:02}:00 left"),
        Timer::Hidden => {}
    }
    out
}

// ---------- config / presets ----------

#[derive(Serialize, Deserialize, Clone)]
struct Preset {
    name: String,
    app_id: String,
    app_name: String,
    style: Style,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Config {
    presets: Vec<Preset>,
    light: bool,
    accent: [u8; 3],
    auto_preset: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self { presets: vec![], light: false, accent: [35, 165, 90], auto_preset: None }
    }
}

fn config_dir() -> PathBuf {
    let base = env::var("APPDATA")
        .or_else(|_| env::var("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|_| env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|_| PathBuf::from("."));
    let dir = base.join("gts-rpc");
    let _ = fs::create_dir_all(&dir);
    dir
}

fn load_config() -> Config {
    fs::read_to_string(config_dir().join("config.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(cfg: &Config) {
    if let Ok(s) = serde_json::to_string_pretty(cfg) {
        let _ = fs::write(config_dir().join("config.json"), s);
    }
}

fn load_favs() -> BTreeSet<String> {
    fs::read_to_string(config_dir().join("favorites.txt"))
        .map(|s| s.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

fn save_favs(favs: &BTreeSet<String>) {
    let _ = fs::write(config_dir().join("favorites.txt"), favs.iter().cloned().collect::<Vec<_>>().join("\n"));
}

fn accent_color(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

fn apply_theme(ctx: &egui::Context, cfg: &Config) {
    let mut v = if cfg.light { egui::Visuals::light() } else { egui::Visuals::dark() };
    v.selection.bg_fill = accent_color(cfg.accent);
    v.hyperlink_color = accent_color(cfg.accent);
    ctx.set_visuals(v);
}

// ---------- detected games (local process scan) ----------

fn scan(apps: &[App]) -> HashSet<String> {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let running: HashSet<String> =
        sys.processes().values().map(|p| p.name().to_string_lossy().to_lowercase()).collect();
    apps.iter()
        .filter(|a| {
            a.executables.iter().flatten().filter(|e| !e.is_launcher).any(|e| {
                let name = e.name.trim_start_matches('>').rsplit('/').next().unwrap_or("").to_lowercase();
                running.contains(&name)
            })
        })
        .map(|a| a.id.clone())
        .collect()
}

// ---------- presence worker ----------

enum Cmd {
    Set { app_id: String, payload: Value },
    Clear,
}

fn presence_worker(rx: mpsc::Receiver<Cmd>, status: Arc<Mutex<String>>) {
    let mut client: Option<(String, DiscordIpcClient)> = None;
    for cmd in rx {
        match cmd {
            Cmd::Clear => {
                if let Some((_, mut c)) = client.take() {
                    let _ = c.clear_activity();
                    let _ = c.close();
                }
                *status.lock().unwrap() = "Status cleared.".into();
            }
            Cmd::Set { app_id, payload } => {
                // A different game needs a new connection; the same game just updates.
                if client.as_ref().is_some_and(|(id, _)| *id != app_id) {
                    if let Some((_, mut c)) = client.take() {
                        let _ = c.clear_activity();
                        let _ = c.close();
                    }
                }
                let result: Result<(), String> = (|| {
                    if client.is_none() {
                        let mut c = DiscordIpcClient::new(&app_id).map_err(|e| e.to_string())?;
                        c.connect().map_err(|e| e.to_string())?;
                        client = Some((app_id.clone(), c));
                    }
                    let (_, c) = client.as_mut().unwrap();
                    c.send(payload, 1).map_err(|e| e.to_string())
                })();
                *status.lock().unwrap() = match result {
                    Ok(()) => "Status is live.".to_string(),
                    Err(e) => {
                        client = None;
                        format!("Couldn't reach the Discord desktop client: {e}")
                    }
                };
            }
        }
    }
}

// ---------- UI ----------

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    All,
    Favorites,
    Detected,
}

enum Act {
    Play(String),
    Update,
    Stop,
    Fav(String),
    Open(String),
    ApplyPreset(usize),
    SavePreset,
    DeletePreset(usize),
    ToggleAuto(String),
    ThemeChanged,
}

struct Gts {
    ctx: egui::Context,
    apps: Vec<App>,
    load_rx: Option<mpsc::Receiver<Result<Vec<App>, String>>>,
    load_error: Option<String>,
    scan_rx: Option<mpsc::Receiver<HashSet<String>>>,
    detected: HashSet<String>,
    favs: BTreeSet<String>,
    tab: Tab,
    query: String,
    genre: String,
    style: Style,
    config: Config,
    preset_name: String,
    auto_done: bool,
    playing: Option<String>,
    tx: mpsc::Sender<Cmd>,
    status: Arc<Mutex<String>>,
}

impl Gts {
    fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(String::new()));
        let s = status.clone();
        thread::spawn(move || presence_worker(rx, s));

        let config = load_config();
        apply_theme(&ctx, &config);

        let mut gts = Self {
            ctx,
            apps: vec![],
            load_rx: None,
            load_error: None,
            scan_rx: None,
            detected: HashSet::new(),
            favs: load_favs(),
            tab: Tab::All,
            query: String::new(),
            genre: String::new(),
            style: Style { hours: "0".into(), ..Style::default() },
            config,
            preset_name: String::new(),
            auto_done: false,
            playing: None,
            tx,
            status,
        };
        gts.load();
        gts
    }

    fn load(&mut self) {
        self.load_error = None;
        let (tx, rx) = mpsc::channel();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(fetch_apps());
            ctx.request_repaint();
        });
        self.load_rx = Some(rx);
    }

    fn rescan(&mut self) {
        let apps = self.apps.clone();
        let (tx, rx) = mpsc::channel();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(scan(&apps));
            ctx.request_repaint();
        });
        self.scan_rx = Some(rx);
    }

    fn push(&self, id: &str) {
        let art = if self.style.use_art {
            self.apps.iter().find(|a| a.id == id).and_then(|a| a.art_url())
        } else {
            None
        };
        let _ = self.tx.send(Cmd::Set { app_id: id.to_string(), payload: payload(&self.style, art) });
    }

    fn apply_preset(&mut self, i: usize) {
        if let Some(p) = self.config.presets.get(i).cloned() {
            self.style = p.style;
            self.playing = Some(p.app_id.clone());
            self.push(&p.app_id);
        }
    }

    fn apply(&mut self, act: Act) {
        match act {
            Act::Play(id) => {
                self.playing = Some(id.clone());
                self.push(&id);
            }
            Act::Update => {
                if let Some(id) = self.playing.clone() {
                    self.push(&id);
                }
            }
            Act::Stop => {
                let _ = self.tx.send(Cmd::Clear);
                self.playing = None;
            }
            Act::Fav(id) => {
                if !self.favs.remove(&id) {
                    self.favs.insert(id);
                }
                save_favs(&self.favs);
            }
            Act::Open(id) => self
                .ctx
                .open_url(egui::OpenUrl::new_tab(format!("https://discord.com/application-directory/{id}"))),
            Act::ApplyPreset(i) => self.apply_preset(i),
            Act::SavePreset => {
                let Some(id) = self.playing.clone() else { return };
                let name = self.preset_name.trim().to_string();
                if name.is_empty() {
                    return;
                }
                let app_name = self.apps.iter().find(|a| a.id == id).map(|a| a.name.clone()).unwrap_or_default();
                let preset = Preset { name: name.clone(), app_id: id, app_name, style: self.style.clone() };
                match self.config.presets.iter_mut().find(|p| p.name == name) {
                    Some(existing) => *existing = preset,
                    None => self.config.presets.push(preset),
                }
                save_config(&self.config);
            }
            Act::DeletePreset(i) => {
                if i < self.config.presets.len() {
                    let removed = self.config.presets.remove(i);
                    if self.config.auto_preset.as_deref() == Some(removed.name.as_str()) {
                        self.config.auto_preset = None;
                    }
                    save_config(&self.config);
                }
            }
            Act::ToggleAuto(name) => {
                self.config.auto_preset =
                    if self.config.auto_preset.as_deref() == Some(name.as_str()) { None } else { Some(name) };
                save_config(&self.config);
            }
            Act::ThemeChanged => {
                apply_theme(&self.ctx, &self.config);
                save_config(&self.config);
            }
        }
    }
}

fn card(ui: &mut egui::Ui, app: &App, fav: bool, active: bool, accent: Color32, acts: &mut Vec<Act>) {
    ui.allocate_ui(Vec2::new(CARD_W, ROW_H - 10.0), |ui| {
        let stroke = if active { Stroke::new(2.0, accent) } else { ui.visuals().widgets.noninteractive.bg_stroke };
        egui::Frame::group(ui.style()).stroke(stroke).fill(ui.visuals().faint_bg_color).show(ui, |ui| {
            ui.set_width(CARD_W - 20.0);
            ui.set_height(ROW_H - 34.0);
            match app.image_url() {
                Some(url) => {
                    let r = ui
                        .add(egui::Image::new(url).fit_to_exact_size(Vec2::new(CARD_W - 20.0, 200.0)).sense(egui::Sense::click()))
                        .on_hover_text("View game page");
                    if r.clicked() {
                        acts.push(Act::Open(app.id.clone()));
                    }
                }
                None => {
                    ui.allocate_ui(Vec2::new(CARD_W - 20.0, 200.0), |ui| ui.centered_and_justified(|ui| ui.label("🎮")));
                }
            }
            if active {
                ui.label(RichText::new("● Showing").color(accent).strong());
            }
            ui.add(egui::Label::new(RichText::new(&app.name).strong()).truncate());
            ui.add(egui::Label::new(RichText::new(app.genres()).weak().small()).truncate());
            ui.horizontal(|ui| {
                if active {
                    if ui.button("Stop").clicked() {
                        acts.push(Act::Stop);
                    }
                } else if ui.button("Spoof").clicked() {
                    acts.push(Act::Play(app.id.clone()));
                }
                if ui.button(if fav { "★" } else { "☆" }).on_hover_text("Favorite").clicked() {
                    acts.push(Act::Fav(app.id.clone()));
                }
            });
        });
    });
}

impl eframe::App for Gts {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if let Some(rx) = &self.load_rx {
            if let Ok(res) = rx.try_recv() {
                match res {
                    Ok(apps) => {
                        self.apps = apps;
                        self.rescan();
                        if !self.auto_done {
                            self.auto_done = true;
                            if let Some(name) = self.config.auto_preset.clone() {
                                if let Some(i) = self.config.presets.iter().position(|p| p.name == name) {
                                    self.apply_preset(i);
                                }
                            }
                        }
                    }
                    Err(e) => self.load_error = Some(e),
                }
                self.load_rx = None;
            }
        }
        if let Some(rx) = &self.scan_rx {
            if let Ok(set) = rx.try_recv() {
                self.detected = set;
                self.scan_rx = None;
            }
        }

        let genres: BTreeSet<&String> = self.apps.iter().flat_map(|a| a.themes.iter().flatten()).collect();
        let accent = accent_color(self.config.accent);
        let mut acts: Vec<Act> = vec![];
        let mut retry = false;
        let mut rescan = false;

        // ----- toolbar -----
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut self.tab, Tab::All, "All Games");
                let fav_label =
                    if self.favs.is_empty() { "Favorites".to_string() } else { format!("Favorites ({})", self.favs.len()) };
                ui.selectable_value(&mut self.tab, Tab::Favorites, fav_label);
                ui.selectable_value(&mut self.tab, Tab::Detected, "Detected");
                if self.tab == Tab::Detected && ui.button("Rescan").clicked() {
                    rescan = true;
                }
                ui.separator();
                ui.add(egui::TextEdit::singleline(&mut self.query).hint_text("Search games").desired_width(200.0));
                egui::ComboBox::from_id_salt("genre")
                    .selected_text(if self.genre.is_empty() { "All genres" } else { &self.genre })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.genre, String::new(), "All genres");
                        for g in &genres {
                            ui.selectable_value(&mut self.genre, (*g).clone(), g.as_str());
                        }
                    });
                let status = self.status.lock().unwrap().clone();
                if !status.is_empty() {
                    ui.separator();
                    ui.label(status);
                }
            });
            ui.add_space(6.0);
        });

        // ----- status editor -----
        egui::SidePanel::right("editor").default_width(310.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Status style");
                ui.add_space(4.0);

                ui.label("Activity type");
                egui::ComboBox::from_id_salt("kind").selected_text(self.style.kind.label()).show_ui(ui, |ui| {
                    for k in Kind::ALL {
                        ui.selectable_value(&mut self.style.kind, k, k.label());
                    }
                });
                ui.small("Discord may show everything as \"Playing\" on some versions.");

                ui.add_space(6.0);
                ui.label("Line 1");
                ui.add(egui::TextEdit::singleline(&mut self.style.details).hint_text("details (2+ characters)").desired_width(f32::INFINITY));
                ui.label("Line 2");
                ui.add(egui::TextEdit::singleline(&mut self.style.state).hint_text("state (2+ characters)").desired_width(f32::INFINITY));

                ui.add_space(6.0);
                ui.label("Timer");
                ui.horizontal(|ui| {
                    for (t, l) in [(Timer::Elapsed, "Elapsed"), (Timer::Countdown, "Countdown"), (Timer::Hidden, "Hidden")] {
                        ui.selectable_value(&mut self.style.timer, t, l);
                    }
                });
                if self.style.timer != Timer::Hidden {
                    ui.horizontal(|ui| {
                        ui.label(if self.style.timer == Timer::Elapsed { "Started (hours ago)" } else { "Ends in (hours)" });
                        ui.add(egui::TextEdit::singleline(&mut self.style.hours).desired_width(60.0));
                    });
                }

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Party size");
                    ui.add(egui::TextEdit::singleline(&mut self.style.party_cur).hint_text("1").desired_width(36.0));
                    ui.label("of");
                    ui.add(egui::TextEdit::singleline(&mut self.style.party_max).hint_text("0 = off").desired_width(50.0));
                });

                ui.add_space(6.0);
                ui.checkbox(&mut self.style.use_art, "Use the game's icon as the large image");
                if self.style.use_art {
                    ui.add(egui::TextEdit::singleline(&mut self.style.large_text).hint_text("image hover text").desired_width(f32::INFINITY));
                }

                ui.add_space(6.0);
                ui.label("Buttons (https links only, others can see them, you can't)");
                for (label, url, n) in [
                    (&mut self.style.btn1_label, &mut self.style.btn1_url, 1),
                    (&mut self.style.btn2_label, &mut self.style.btn2_url, 2),
                ] {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(label).hint_text(format!("Button {n}")).desired_width(90.0));
                        ui.add(egui::TextEdit::singleline(url).hint_text("https://…").desired_width(f32::INFINITY));
                    });
                }

                ui.add_space(8.0);
                let game = self
                    .playing
                    .as_ref()
                    .and_then(|id| self.apps.iter().find(|a| &a.id == id))
                    .map(|a| a.name.as_str())
                    .unwrap_or("(pick a game)");
                egui::Frame::group(ui.style()).fill(ui.visuals().faint_bg_color).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new("Preview").weak().small());
                    ui.label(preview(game, &self.style));
                });
                if ui.add_enabled(self.playing.is_some(), egui::Button::new("Update status")).clicked() {
                    acts.push(Act::Update);
                }

                ui.separator();
                ui.heading("Presets");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.preset_name).hint_text("preset name").desired_width(150.0));
                    let can = self.playing.is_some() && !self.preset_name.trim().is_empty();
                    if ui.add_enabled(can, egui::Button::new("Save")).on_hover_text("Saves the current game + style").clicked() {
                        acts.push(Act::SavePreset);
                    }
                });
                if self.config.presets.is_empty() {
                    ui.small("Spoof a game, style it, name it, then Save.");
                }
                for (i, p) in self.config.presets.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(format!("{} ({})", p.name, p.app_name)).truncate());
                        if ui.small_button("Apply").clicked() {
                            acts.push(Act::ApplyPreset(i));
                        }
                        let auto = self.config.auto_preset.as_deref() == Some(p.name.as_str());
                        if ui.selectable_label(auto, "⚡").on_hover_text("Apply automatically on launch").clicked() {
                            acts.push(Act::ToggleAuto(p.name.clone()));
                        }
                        if ui.small_button("🗑").clicked() {
                            acts.push(Act::DeletePreset(i));
                        }
                    });
                }

                ui.separator();
                ui.heading("Look");
                let mut changed = false;
                changed |= ui.checkbox(&mut self.config.light, "Light theme").changed();
                ui.horizontal(|ui| {
                    ui.label("Accent color");
                    changed |= ui.color_edit_button_srgb(&mut self.config.accent).changed();
                });
                if changed {
                    acts.push(Act::ThemeChanged);
                }
                ui.small("Tip: launch with --minimized and use a ⚡ preset to start with your status already set.");
            });
        });

        // ----- game grid -----
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(e) = &self.load_error {
                ui.vertical_centered(|ui| {
                    ui.label(format!("Couldn't load Discord's game list: {e}"));
                    if ui.button("Try again").clicked() {
                        retry = true;
                    }
                });
                return;
            }
            if self.apps.is_empty() {
                ui.vertical_centered(|ui| ui.label("Loading games..."));
                return;
            }

            let q = self.query.trim().to_lowercase();
            let mut shown: Vec<usize> = (0..self.apps.len())
                .filter(|&i| {
                    let a = &self.apps[i];
                    (self.tab != Tab::Favorites || self.favs.contains(&a.id))
                        && (self.tab != Tab::Detected || self.detected.contains(&a.id))
                        && (self.genre.is_empty() || a.themes.iter().flatten().any(|t| *t == self.genre))
                        && (q.is_empty() || a.matches(&q))
                })
                .collect();
            if !q.is_empty() {
                shown.sort_by_key(|&i| !self.apps[i].name.to_lowercase().starts_with(&q));
            }
            if let Some(p) = &self.playing {
                if let Some(pos) = self.apps.iter().position(|a| &a.id == p) {
                    shown.retain(|&i| i != pos);
                    shown.insert(0, pos);
                }
            }

            ui.label(format!("{} games", shown.len()));
            if shown.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.label(match self.tab {
                        Tab::Favorites if self.favs.is_empty() => "No favorites yet. Click the star on a game to add it here.",
                        Tab::Detected => "No detected games running. Start one and press Rescan.",
                        _ => "No games match your search.",
                    })
                });
                return;
            }

            let cols = ((ui.available_width() / (CARD_W + 8.0)).floor() as usize).max(1);
            let rows = shown.len().div_ceil(cols);
            egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, ROW_H, rows, |ui, range| {
                for r in range {
                    ui.horizontal_top(|ui| {
                        for &i in shown.iter().skip(r * cols).take(cols) {
                            let app = &self.apps[i];
                            let active = self.playing.as_deref() == Some(app.id.as_str());
                            card(ui, app, self.favs.contains(&app.id), active, accent, &mut acts);
                        }
                    });
                }
            });
        });

        if retry {
            self.load();
        }
        if rescan {
            self.rescan();
        }
        for act in acts {
            self.apply(act);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.tx.send(Cmd::Clear);
        thread::sleep(std::time::Duration::from_millis(300));
    }
}

fn main() -> eframe::Result {
    let minimized = env::args().any(|a| a == "--minimized");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 800.0]).with_minimized(minimized),
        ..Default::default()
    };
    eframe::run_native(
        "GameTimeSpoof RPC",
        options,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(Gts::new(cc.egui_ctx.clone())))
        }),
    )
}
