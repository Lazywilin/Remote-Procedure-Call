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
/// Badge used by the "PlayStation look" button: the white PlayStation logo PNG in `assets/`, loaded from
/// your repo's raw GitHub link, so it only shows once that file is pushed to a public repo.
/// If you move the project folder, change this link to match.
const PS_BADGE_URL: &str =
    "https://raw.githubusercontent.com/Lazywilin/Remote-Procedure-Call/main/Discord%20RPC%20Rust/assets/playstation-logo.png";
const CARD_W: f32 = 176.0;
const CARD_H: f32 = 320.0;

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
    third_party_skus: Option<Value>,
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

impl App {
    /// Steam app id from Discord's data, if the game lists one.
    fn steam_id(&self) -> Option<String> {
        self.third_party_skus
            .as_ref()?
            .as_array()?
            .iter()
            .find_map(|sku| {
                let dist = sku.get("distributor")?.as_str()?;
                if !dist.eq_ignore_ascii_case("steam") {
                    return None;
                }
                match sku.get("id").or_else(|| sku.get("sku"))? {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                }
            })
    }

    /// Up to two store buttons. Steam links straight to the page when Discord
    /// knows the Steam id, otherwise (and for PlayStation/Xbox) it's a search link.
    fn store_links(&self, s: &Style) -> Vec<(String, String)> {
        let q = url_encode(&self.name);
        let mut v = vec![];
        if s.store_steam {
            let url = match self.steam_id() {
                Some(id) => format!("https://store.steampowered.com/app/{id}"),
                None => format!("https://store.steampowered.com/search/?term={q}"),
            };
            v.push(("Get on Steam".to_string(), url));
        }
        if s.store_ps {
            v.push((
                "PlayStation Store".to_string(),
                format!("https://store.playstation.com/search/{q}"),
            ));
        }
        if s.store_xbox {
            v.push((
                "Xbox Store".to_string(),
                format!("https://www.xbox.com/search?q={q}"),
            ));
        }
        v.truncate(2);
        v
    }

    fn matches(&self, q: &str) -> bool {
        self.name.to_lowercase().contains(q)
            || self
                .aliases
                .iter()
                .flatten()
                .any(|a| a.to_lowercase().contains(q))
    }
    fn genres(&self) -> String {
        self.themes
            .iter()
            .flatten()
            .take(2)
            .cloned()
            .collect::<Vec<_>>()
            .join(" · ")
    }
    /// Card image in the grid.
    fn image_url(&self) -> Option<String> {
        if let Some(h) = &self.cover_image_hash {
            return Some(format!(
                "https://cdn.discordapp.com/app-icons/{}/{h}.webp?size=300",
                self.id
            ));
        }
        self.icon_hash.as_ref().map(|h| {
            format!(
                "https://cdn.discordapp.com/app-icons/{}/{h}.webp?size=128",
                self.id
            )
        })
    }
    /// Square art used as the status' large image.
    fn art_url(&self) -> Option<String> {
        self.icon_hash
            .as_ref()
            .map(|h| {
                format!(
                    "https://cdn.discordapp.com/app-icons/{}/{h}.png?size=512",
                    self.id
                )
            })
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
    const ALL: [Kind; 4] = [
        Kind::Playing,
        Kind::Listening,
        Kind::Watching,
        Kind::Competing,
    ];
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

/// Text-only platform tag added to line 2. Discord's real "Playing on PlayStation"
/// header comes from linked console accounts and can't be set by an RPC app.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Default)]
enum Platform {
    #[default]
    None,
    PlayStation5,
    PlayStation4,
    Xbox,
    Pc,
}

impl Platform {
    const ALL: [Platform; 5] = [
        Platform::None,
        Platform::PlayStation5,
        Platform::PlayStation4,
        Platform::Xbox,
        Platform::Pc,
    ];
    fn label(self) -> &'static str {
        match self {
            Platform::None => "None",
            Platform::PlayStation5 => "PlayStation 5",
            Platform::PlayStation4 => "PlayStation 4",
            Platform::Xbox => "Xbox Series X|S",
            Platform::Pc => "PC",
        }
    }
    /// Plain text only. Discord's own platform header and logo can't be set from here.
    fn tag(self) -> Option<&'static str> {
        match self {
            Platform::None => None,
            Platform::PlayStation5 => Some("on PlayStation 5"),
            Platform::PlayStation4 => Some("on PlayStation 4"),
            Platform::Xbox => Some("on Xbox Series X|S"),
            Platform::Pc => Some("on PC"),
        }
    }
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
    auto_stores: bool,
    store_steam: bool,
    store_ps: bool,
    store_xbox: bool,
    platform: Platform,
    small_image_url: String,
    small_text: String,
}

/// Maximum hours allowed for a timer.
///
/// - **Why this limit?** i64 timestamps must stay within safe bounds:
///   - Current Unix timestamp: ~1.7 billion seconds (as of 2026)
///   - i64 max: ~9.2e18 seconds (~292 billion years from now)
///   - Discord RPC may reject extreme timestamps
///   - For elapsed timers: `now - secs` must stay positive and reasonable
///   - For countdown timers: we avoid overflow when computing `now + secs`
///   
/// We cap at **one year** (31,536,000 seconds = 365 days) which is:
/// - ~8760 hours, or roughly 9000 to be precise
/// - More than sufficient for any legitimate use case
/// - Prevents overflow attacks and absurd timer values
const MAX_HOURS: f64 = 9000.0;

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parses hours from a string, clamping invalid inputs to safe bounds.
///
/// # Returns
/// - `Some(hours)` for valid, finite values within reasonable bounds (0 to MAX_HOURS)
/// - `None` if the input is empty, NaN, or Infinity
fn parse_hours(s: &str) -> Option<f64> {
    let trimmed = s.trim();

    // Reject empty strings immediately
    if trimmed.is_empty() {
        return None;
    }

    // Parse into f64
    let hours = trimmed.parse::<f64>().ok()?;

    // Reject NaN and Infinity before any arithmetic
    if !hours.is_finite() {
        return None;
    }

    // Ensure non-negative
    let hours = hours.max(0.0);

    // Clamp to maximum to prevent overflow in timestamp calculations
    Some(hours.min(MAX_HOURS))
}

/// Discord needs 2 to 128 characters per text line.
fn clean(s: &str) -> Option<String> {
    let t = s.trim();
    (t.chars().count() >= 2).then(|| t.chars().take(128).collect())
}

/// Line 2 = your own text plus the platform tag, if one is chosen.
fn state_line(s: &Style) -> Option<String> {
    match (clean(&s.state), s.platform.tag()) {
        (Some(u), Some(t)) => clean(&format!("{u} \u{b7} {t}")),
        (Some(u), None) => Some(u),
        (None, Some(t)) => clean(t),
        (None, None) => None,
    }
}

fn payload(s: &Style, art: Option<String>, stores: Vec<(String, String)>) -> Value {
    let now = unix_now();
    let secs = parse_hours(&s.hours)
        .map(|h| (h * 3600.0) as i64)
        .unwrap_or(0);
    let mut act = serde_json::Map::new();
    act.insert("type".into(), json!(s.kind.code()));
    if let Some(d) = clean(&s.details) {
        act.insert("details".into(), json!(d));
    }
    if let Some(d) = state_line(s) {
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
    let mut assets = serde_json::Map::new();
    if let Some(img) = art {
        assets.insert("large_image".into(), json!(img));
        if let Some(t) = clean(&s.large_text) {
            assets.insert("large_text".into(), json!(t));
        }
    }
    let small = s.small_image_url.trim();
    if small.starts_with("https://") {
        assets.insert("small_image".into(), json!(small));
        if let Some(t) = clean(&s.small_text) {
            assets.insert("small_text".into(), json!(t));
        }
    }
    if !assets.is_empty() {
        act.insert("assets".into(), Value::Object(assets));
    }
    let max: i64 = s.party_max.trim().parse().unwrap_or(0);
    if max > 0 {
        let cur = s.party_cur.trim().parse::<i64>().unwrap_or(1).clamp(1, max);
        act.insert(
            "party".into(),
            json!({ "id": "gts-party", "size": [cur, max] }),
        );
    }
    let mut buttons: Vec<Value> = [(&s.btn1_label, &s.btn1_url), (&s.btn2_label, &s.btn2_url)]
        .into_iter()
        .filter_map(|(l, u)| {
            let (l, u) = (l.trim(), u.trim());
            (!l.is_empty() && (u.starts_with("https://") || u.starts_with("http://")))
                .then(|| json!({ "label": l.chars().take(32).collect::<String>(), "url": u }))
        })
        .collect();
    if buttons.is_empty() && s.auto_stores {
        buttons = stores
            .into_iter()
            .map(|(label, url)| json!({ "label": label.chars().take(32).collect::<String>(), "url": url }))
            .collect();
    }
    if !buttons.is_empty() {
        act.insert("buttons".into(), Value::Array(buttons));
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    json!({
        "cmd": "SET_ACTIVITY",
        "args": { "pid": std::process::id(), "activity": Value::Object(act) },
        "nonce": nonce.to_string()
    })
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
    auto_preset: Option<String>,
    live_update: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            presets: vec![],
            light: false,
            auto_preset: None,
            live_update: true,
        }
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
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn save_favs(favs: &BTreeSet<String>) {
    let _ = fs::write(
        config_dir().join("favorites.txt"),
        favs.iter().cloned().collect::<Vec<_>>().join("\n"),
    );
}

/// One neutral ramp. Text has three roles (text, text2, muted) and the primary
/// action is the inverse of the background, so the whole app stays monochrome.
#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    panel: Color32,
    surface: Color32,
    raised: Color32,
    line: Color32,
    text: Color32,
    text2: Color32,
    muted: Color32,
    primary: Color32,
    on_primary: Color32,
}

fn palette(light: bool) -> Palette {
    if light {
        Palette {
            bg: Color32::from_rgb(244, 244, 244),
            panel: Color32::from_rgb(255, 255, 255),
            surface: Color32::from_rgb(240, 240, 240),
            raised: Color32::from_rgb(228, 228, 228),
            line: Color32::from_black_alpha(24),
            text: Color32::from_rgb(23, 23, 23),
            text2: Color32::from_rgb(82, 82, 82),
            muted: Color32::from_rgb(107, 107, 107),
            primary: Color32::from_rgb(23, 23, 23),
            on_primary: Color32::from_rgb(250, 250, 250),
        }
    } else {
        Palette {
            bg: Color32::from_rgb(20, 20, 20),
            panel: Color32::from_rgb(26, 26, 26),
            surface: Color32::from_rgb(34, 34, 34),
            raised: Color32::from_rgb(46, 46, 46),
            line: Color32::from_white_alpha(20),
            text: Color32::from_rgb(237, 237, 237),
            text2: Color32::from_rgb(170, 170, 170),
            muted: Color32::from_rgb(138, 138, 138),
            primary: Color32::from_rgb(237, 237, 237),
            on_primary: Color32::from_rgb(20, 20, 20),
        }
    }
}

fn apply_theme(ctx: &egui::Context, cfg: &Config) {
    use egui::{CornerRadius, FontId, TextStyle};
    let p = palette(cfg.light);
    let mut v = if cfg.light { egui::Visuals::light() } else { egui::Visuals::dark() };
    v.panel_fill = p.bg;
    v.window_fill = p.panel;
    v.extreme_bg_color = p.surface;
    v.faint_bg_color = p.surface;
    v.hyperlink_color = p.text;
    v.selection.bg_fill = p.raised;
    v.selection.stroke = Stroke::new(1.0, p.text);
    let radius = CornerRadius::same(8);
    let text = Stroke::new(1.0, p.text);
    v.widgets.noninteractive.bg_fill = p.panel;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
    v.widgets.noninteractive.fg_stroke = text;
    v.widgets.noninteractive.corner_radius = radius;
    for (w, fill) in [
        (&mut v.widgets.inactive, p.surface),
        (&mut v.widgets.hovered, p.raised),
        (&mut v.widgets.active, p.raised),
        (&mut v.widgets.open, p.raised),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::new(1.0, p.line);
        w.fg_stroke = text;
        w.corner_radius = radius;
    }
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = Vec2::new(8.0, 8.0);
        s.spacing.button_padding = Vec2::new(12.0, 6.0);
        s.spacing.interact_size = Vec2::new(40.0, 32.0);
        s.text_styles.insert(TextStyle::Heading, FontId::proportional(16.0));
        s.text_styles.insert(TextStyle::Body, FontId::proportional(14.0));
        s.text_styles.insert(TextStyle::Button, FontId::proportional(14.0));
        s.text_styles.insert(TextStyle::Small, FontId::proportional(12.0));
    });
}

// ---------- detected games (local process scan) ----------

fn scan(apps: &[App]) -> HashSet<String> {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let running: HashSet<String> = sys
        .processes()
        .values()
        .map(|p| p.name().to_string_lossy().to_lowercase())
        .collect();
    apps.iter()
        .filter(|a| {
            a.executables
                .iter()
                .flatten()
                .filter(|e| !e.is_launcher)
                .any(|e| {
                    let name = e
                        .name
                        .trim_start_matches('>')
                        .rsplit('/')
                        .next()
                        .unwrap_or("")
                        .to_lowercase();
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
                *status.lock().unwrap() = "Status cleared".into();
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
                    Ok(()) => "Status is live".to_string(),
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

#[derive(PartialEq, Clone, Copy)]
enum Pane {
    Status,
    Extras,
    Presets,
    Settings,
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
    SaveConfig,
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
    pane: Pane,
    pal: Palette,
    auto_done: bool,
    last_sent: String,
    dirty_since: Option<std::time::Instant>,
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
            style: Style {
                hours: "0".into(),
                auto_stores: true,
                store_steam: true,
                store_ps: true,
                ..Style::default()
            },
            pane: Pane::Status,
            pal: palette(config.light),
            config,
            preset_name: String::new(),
            auto_done: false,
            last_sent: String::new(),
            dirty_since: None,
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

    fn push(&mut self, id: &str) {
        let art = if self.style.use_art {
            self.apps
                .iter()
                .find(|a| a.id == id)
                .and_then(|a| a.art_url())
        } else {
            None
        };
        let stores = self
            .apps
            .iter()
            .find(|a| a.id == id)
            .map(|a| a.store_links(&self.style))
            .unwrap_or_default();
        let _ = self.tx.send(Cmd::Set {
            app_id: id.to_string(),
            payload: payload(&self.style, art, stores),
        });
        self.last_sent = serde_json::to_string(&self.style).unwrap_or_default();
        self.dirty_since = None;
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
            Act::Open(id) => self.ctx.open_url(egui::OpenUrl::new_tab(format!(
                "https://discord.com/application-directory/{id}"
            ))),
            Act::ApplyPreset(i) => self.apply_preset(i),
            Act::SavePreset => {
                let Some(id) = self.playing.clone() else {
                    return;
                };
                let name = self.preset_name.trim().to_string();
                if name.is_empty() {
                    return;
                }
                let app_name = self
                    .apps
                    .iter()
                    .find(|a| a.id == id)
                    .map(|a| a.name.clone())
                    .unwrap_or_default();
                let preset = Preset {
                    name: name.clone(),
                    app_id: id,
                    app_name,
                    style: self.style.clone(),
                };
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
                    if self.config.auto_preset.as_deref() == Some(name.as_str()) {
                        None
                    } else {
                        Some(name)
                    };
                save_config(&self.config);
            }
            Act::ThemeChanged => {
                apply_theme(&self.ctx, &self.config);
                self.pal = palette(self.config.light);
                save_config(&self.config);
            }
            Act::SaveConfig => save_config(&self.config),
        }
    }
}

// ---------- widgets ----------

fn rt(text: impl Into<String>, color: Color32) -> RichText {
    RichText::new(text).color(color)
}

fn primary_button(p: Palette, text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).color(p.on_primary).strong()).fill(p.primary)
}

fn seg<T: PartialEq + Copy>(ui: &mut egui::Ui, current: &mut T, items: &[(T, &str)]) {
    ui.horizontal(|ui| {
        for (value, label) in items {
            ui.selectable_value(current, *value, *label);
        }
    });
}

fn field(ui: &mut egui::Ui, p: Palette, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.label(rt(label, p.text2).size(12.0));
        add(ui);
    });
    ui.add_space(8.0);
}

/// Filled dot = showing, hollow dot = not showing.
fn status_dot(ui: &mut egui::Ui, on: bool, p: Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), egui::Sense::hover());
    let c = rect.center();
    if on {
        ui.painter().circle_filled(c, 4.5, p.text);
    } else {
        ui.painter().circle_stroke(c, 4.0, Stroke::new(1.5, p.muted));
    }
}

fn star_button(ui: &mut egui::Ui, filled: bool, p: Palette) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(28.0), egui::Sense::click());
    let color = if filled || resp.hovered() { p.text } else { p.muted };
    let c = rect.center();
    let pts: Vec<egui::Pos2> = (0..10)
        .map(|i| {
            let r = if i % 2 == 0 { 9.0 } else { 3.8 };
            let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
            egui::pos2(c.x + r * a.cos(), c.y + r * a.sin())
        })
        .collect();
    if filled {
        for i in 0..10 {
            ui.painter().add(egui::Shape::convex_polygon(
                vec![c, pts[i], pts[(i + 1) % 10]],
                color,
                Stroke::NONE,
            ));
        }
    }
    ui.painter().add(egui::Shape::closed_line(pts, Stroke::new(1.5, color)));
    resp.on_hover_text(if filled { "Remove from favorites" } else { "Add to favorites" })
}

fn tile(ui: &mut egui::Ui, size: Vec2, name: &str, p: Palette, font: f32) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter().rect_filled(rect, egui::CornerRadius::same(6), p.raised);
    let letter = name
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        letter,
        egui::FontId::proportional(font),
        p.muted,
    );
}

fn timer_text(s: &Style) -> Option<String> {
    let h = parse_hours(&s.hours).unwrap_or(0.0);
    let (hh, mm) = (h as i64, (h.fract() * 60.0) as i64);
    match s.timer {
        Timer::Elapsed => Some(format!("{hh:02}:{mm:02}:00 elapsed")),
        Timer::Countdown => Some(format!("{hh:02}:{mm:02}:00 left")),
        Timer::Hidden => None,
    }
}

/// The button labels the status will actually show.
fn button_labels(app: Option<&App>, s: &Style) -> Vec<String> {
    let valid = |u: &str| u.trim().starts_with("https://") || u.trim().starts_with("http://");
    let manual: Vec<String> = [(&s.btn1_label, &s.btn1_url), (&s.btn2_label, &s.btn2_url)]
        .into_iter()
        .filter(|(l, u)| !l.trim().is_empty() && valid(u))
        .map(|(l, _)| l.trim().to_string())
        .collect();
    if !manual.is_empty() {
        return manual;
    }
    if s.auto_stores {
        if let Some(a) = app {
            return a.store_links(s).into_iter().map(|(l, _)| l).collect();
        }
    }
    vec![]
}

/// Looks like Discord's own activity card, so you see what others will see.
fn preview_card(ui: &mut egui::Ui, p: Palette, app: Option<&App>, s: &Style) {
    egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.line))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let Some(a) = app else {
                ui.label(rt("Pick a game to preview your status", p.muted));
                return;
            };
            ui.label(rt(s.kind.label(), p.muted).size(12.0));
            ui.add_space(4.0);
            ui.horizontal_top(|ui| {
                match a.art_url().or_else(|| a.image_url()) {
                    Some(url) => {
                        ui.add(egui::Image::new(url).fit_to_exact_size(Vec2::splat(64.0)));
                    }
                    None => tile(ui, Vec2::splat(64.0), &a.name, p, 24.0),
                }
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.add(egui::Label::new(RichText::new(&a.name).strong()).truncate());
                    if let Some(d) = clean(&s.details) {
                        ui.add(egui::Label::new(rt(d, p.text2)).truncate());
                    }
                    if let Some(d) = state_line(s) {
                        ui.add(egui::Label::new(rt(d, p.text2)).truncate());
                    }
                    if let Some(t) = timer_text(s) {
                        ui.label(rt(t, p.muted).size(12.0));
                    }
                });
            });
            let buttons = button_labels(Some(a), s);
            if !buttons.is_empty() {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    for b in buttons {
                        ui.add(egui::Button::new(rt(b, p.text2).size(12.0)).fill(p.panel));
                    }
                });
            }
        });
}

fn card(ui: &mut egui::Ui, p: Palette, app: &App, fav: bool, active: bool, acts: &mut Vec<Act>) {
    ui.allocate_ui(Vec2::new(CARD_W, CARD_H), |ui| {
        let stroke = if active { Stroke::new(2.0, p.primary) } else { Stroke::new(1.0, p.line) };
        egui::Frame::new()
            .fill(p.surface)
            .stroke(stroke)
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                let w = CARD_W - 20.0;
                ui.set_width(w);
                ui.set_height(CARD_H - 20.0);
                ui.spacing_mut().item_spacing.y = 8.0;
                let size = Vec2::new(w, 208.0);
                match app.image_url() {
                    Some(url) => {
                        let r = ui
                            .add(egui::Image::new(url).fit_to_exact_size(size).sense(egui::Sense::click()))
                            .on_hover_text("View game page");
                        if r.clicked() {
                            acts.push(Act::Open(app.id.clone()));
                        }
                    }
                    None => tile(ui, size, &app.name, p, 40.0),
                }
                ui.add(egui::Label::new(RichText::new(&app.name).strong()).truncate());
                ui.add(egui::Label::new(rt(app.genres(), p.muted).size(12.0)).truncate());
                ui.horizontal(|ui| {
                    let btn = Vec2::new(w - 28.0 - ui.spacing().item_spacing.x, 28.0);
                    if active {
                        if ui.add_sized(btn, egui::Button::new("Stop").fill(p.raised)).clicked() {
                            acts.push(Act::Stop);
                        }
                    } else if ui.add_sized(btn, primary_button(p, "Spoof")).clicked() {
                        acts.push(Act::Play(app.id.clone()));
                    }
                    if star_button(ui, fav, p).clicked() {
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

        let p = self.pal;
        let genres: BTreeSet<&String> = self.apps.iter().flat_map(|a| a.themes.iter().flatten()).collect();
        let mut acts: Vec<Act> = vec![];
        let mut retry = false;
        let mut rescan = false;
        let playing_name: Option<String> = self
            .playing
            .as_ref()
            .and_then(|id| self.apps.iter().find(|a| &a.id == id))
            .map(|a| a.name.clone());

        // ----- header -----
        egui::TopBottomPanel::top("header")
            .frame(egui::Frame::new().fill(p.panel).inner_margin(egui::Margin::symmetric(16, 12)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tab, Tab::All, "All");
                    let fav = if self.favs.is_empty() {
                        "Favorites".to_string()
                    } else {
                        format!("Favorites {}", self.favs.len())
                    };
                    ui.selectable_value(&mut self.tab, Tab::Favorites, fav);
                    ui.selectable_value(&mut self.tab, Tab::Detected, "Detected");
                    if self.tab == Tab::Detected && ui.button("Rescan").clicked() {
                        rescan = true;
                    }
                    ui.add_space(8.0);
                    ui.add(egui::TextEdit::singleline(&mut self.query).hint_text("Search").desired_width(220.0));
                    egui::ComboBox::from_id_salt("genre")
                        .selected_text(if self.genre.is_empty() { "All genres" } else { &self.genre })
                        .width(150.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.genre, String::new(), "All genres");
                            for g in &genres {
                                ui.selectable_value(&mut self.genre, (*g).clone(), g.as_str());
                            }
                        });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match &playing_name {
                        Some(name) => {
                            if ui.add(egui::Button::new("Stop").fill(p.raised)).clicked() {
                                acts.push(Act::Stop);
                            }
                            ui.add(egui::Label::new(RichText::new(format!("Showing {name}")).strong()).truncate());
                            status_dot(ui, true, p);
                        }
                        None => {
                            ui.label(rt("Not showing", p.muted));
                            status_dot(ui, false, p);
                        }
                    });
                });
            });

        // ----- editor -----
        let play_app = self.playing.as_ref().and_then(|id| self.apps.iter().find(|a| &a.id == id));
        egui::SidePanel::right("editor")
            .exact_width(344.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(p.panel).inner_margin(egui::Margin::same(16)))
            .show(ctx, |ui| {
                ui.label(rt("Preview", p.muted).size(12.0));
                preview_card(ui, p, play_app, &self.style);
                let status = self.status.lock().unwrap().clone();
                if status.starts_with("Couldn't") {
                    ui.label(rt(status, p.text));
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    for (pane, label) in [
                        (Pane::Status, "Status"),
                        (Pane::Extras, "Extras"),
                        (Pane::Presets, "Presets"),
                        (Pane::Settings, "Settings"),
                    ] {
                        ui.selectable_value(&mut self.pane, pane, label);
                    }
                });
                ui.separator();
                let pane = self.pane;
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| match pane {
                    Pane::Status => {
                        field(ui, p, "Activity", |ui| {
                            egui::ComboBox::from_id_salt("kind")
                                .selected_text(self.style.kind.label())
                                .width(ui.available_width())
                                .show_ui(ui, |ui| {
                                    for k in Kind::ALL {
                                        ui.selectable_value(&mut self.style.kind, k, k.label());
                                    }
                                });
                        });
                        field(ui, p, "Line 1", |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.style.details)
                                    .hint_text("Shown under the game name")
                                    .desired_width(f32::INFINITY),
                            );
                        });
                        field(ui, p, "Line 2", |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.style.state)
                                    .hint_text("Shown under line 1")
                                    .desired_width(f32::INFINITY),
                            );
                        });
                        field(ui, p, "Timer", |ui| {
                            seg(
                                ui,
                                &mut self.style.timer,
                                &[(Timer::Elapsed, "Elapsed"), (Timer::Countdown, "Countdown"), (Timer::Hidden, "Hidden")],
                            );
                        });
                        if self.style.timer != Timer::Hidden {
                            let label = if self.style.timer == Timer::Elapsed { "Started hours ago" } else { "Ends in hours" };
                            field(ui, p, label, |ui| {
                                ui.add(egui::TextEdit::singleline(&mut self.style.hours).desired_width(80.0));
                            });
                        }
                        field(ui, p, "Party size", |ui| {
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut self.style.party_cur).hint_text("1").desired_width(48.0));
                                ui.label(rt("of", p.text2));
                                ui.add(egui::TextEdit::singleline(&mut self.style.party_max).hint_text("Off").desired_width(48.0));
                            });
                        });
                        if !self.config.live_update {
                            if ui.add_enabled(self.playing.is_some(), primary_button(p, "Update status")).clicked() {
                                acts.push(Act::Update);
                            }
                        }
                    }
                    Pane::Extras => {
                        field(ui, p, "Platform", |ui| {
                            ui.horizontal(|ui| {
                                egui::ComboBox::from_id_salt("platform")
                                    .selected_text(self.style.platform.label())
                                    .show_ui(ui, |ui| {
                                        for pl in Platform::ALL {
                                            ui.selectable_value(&mut self.style.platform, pl, pl.label());
                                        }
                                    });
                                if ui.button("PlayStation look").clicked() {
                                    self.style.platform = Platform::PlayStation5;
                                    self.style.use_art = true;
                                    self.style.kind = Kind::Playing;
                                    self.style.timer = Timer::Elapsed;
                                    self.style.small_image_url = PS_BADGE_URL.to_string();
                                    self.style.small_text = "PlayStation".to_string();
                                }
                            });
                        });
                        field(ui, p, "Large image", |ui| {
                            ui.checkbox(&mut self.style.use_art, "Use the game's icon");
                            if self.style.use_art {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.style.large_text)
                                        .hint_text("Hover text")
                                        .desired_width(f32::INFINITY),
                                );
                            }
                        });
                        field(ui, p, "Badge image (PNG link)", |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.style.small_image_url)
                                    .hint_text("https://\u{2026}/logo.png")
                                    .desired_width(f32::INFINITY),
                            );
                            if !self.style.small_image_url.trim().is_empty() {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.style.small_text)
                                        .hint_text("Hover text")
                                        .desired_width(f32::INFINITY),
                                );
                            }
                        });
                        field(ui, p, "Buttons", |ui| {
                            ui.checkbox(&mut self.style.auto_stores, "Add store buttons automatically");
                            if self.style.auto_stores {
                                ui.horizontal(|ui| {
                                    ui.checkbox(&mut self.style.store_steam, "Steam");
                                    ui.checkbox(&mut self.style.store_ps, "PlayStation");
                                    ui.checkbox(&mut self.style.store_xbox, "Xbox");
                                });
                                ui.label(rt("Two buttons at most. PlayStation and Xbox open a store search", p.muted).size(12.0));
                            }
                            for (label, url, n) in [
                                (&mut self.style.btn1_label, &mut self.style.btn1_url, 1),
                                (&mut self.style.btn2_label, &mut self.style.btn2_url, 2),
                            ] {
                                ui.horizontal(|ui| {
                                    ui.add(egui::TextEdit::singleline(label).hint_text(format!("Button {n}")).desired_width(96.0));
                                    ui.add(egui::TextEdit::singleline(url).hint_text("https://\u{2026}").desired_width(f32::INFINITY));
                                });
                            }
                            ui.label(rt("Your own buttons replace the store ones. Others see them, you don't", p.muted).size(12.0));
                        });
                    }
                    Pane::Presets => {
                        field(ui, p, "Save the current setup", |ui| {
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut self.preset_name).hint_text("Preset name").desired_width(200.0));
                                let can = self.playing.is_some() && !self.preset_name.trim().is_empty();
                                if ui.add_enabled(can, primary_button(p, "Save")).clicked() {
                                    acts.push(Act::SavePreset);
                                }
                            });
                            if self.playing.is_none() {
                                ui.label(rt("Spoof a game first", p.muted).size(12.0));
                            }
                        });
                        if self.config.presets.is_empty() {
                            ui.label(rt("No presets yet", p.muted));
                        }
                        for (i, pr) in self.config.presets.iter().enumerate() {
                            egui::Frame::new()
                                .fill(p.surface)
                                .corner_radius(egui::CornerRadius::same(8))
                                .inner_margin(egui::Margin::same(8))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        ui.vertical(|ui| {
                                            ui.spacing_mut().item_spacing.y = 2.0;
                                            ui.add(egui::Label::new(RichText::new(&pr.name).strong()).truncate());
                                            ui.add(egui::Label::new(rt(pr.app_name.clone(), p.muted).size(12.0)).truncate());
                                        });
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            if ui.button("Delete").clicked() {
                                                acts.push(Act::DeletePreset(i));
                                            }
                                            let auto = self.config.auto_preset.as_deref() == Some(pr.name.as_str());
                                            if ui.selectable_label(auto, "On launch").clicked() {
                                                acts.push(Act::ToggleAuto(pr.name.clone()));
                                            }
                                            if ui.button("Apply").clicked() {
                                                acts.push(Act::ApplyPreset(i));
                                            }
                                        });
                                    });
                                });
                        }
                    }
                    Pane::Settings => {
                        field(ui, p, "Theme", |ui| {
                            let mut light = self.config.light;
                            seg(ui, &mut light, &[(false, "Dark"), (true, "Light")]);
                            if light != self.config.light {
                                self.config.light = light;
                                acts.push(Act::ThemeChanged);
                            }
                        });
                        field(ui, p, "Updates", |ui| {
                            if ui.checkbox(&mut self.config.live_update, "Update the status as I edit").changed() {
                                acts.push(Act::SaveConfig);
                            }
                        });
                        ui.label(
                            rt(
                                "Start with --minimized and mark a preset On launch to have your status ready when Discord opens",
                                p.muted,
                            )
                            .size(12.0),
                        );
                    }
                });
            });

        // ----- games -----
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin::same(16)))
            .show(ctx, |ui| {
                if let Some(e) = &self.load_error {
                    ui.vertical_centered(|ui| {
                        ui.label(rt(format!("Couldn't load Discord's game list: {e}"), p.text2));
                        if ui.button("Try again").clicked() {
                            retry = true;
                        }
                    });
                    return;
                }
                if self.apps.is_empty() {
                    ui.vertical_centered(|ui| ui.label(rt("Loading games", p.muted)));
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
                if let Some(pid) = &self.playing {
                    if let Some(pos) = self.apps.iter().position(|a| &a.id == pid) {
                        shown.retain(|&i| i != pos);
                        shown.insert(0, pos);
                    }
                }

                ui.label(rt(format!("{} games", shown.len()), p.muted).size(12.0));
                if shown.is_empty() {
                    ui.add_space(48.0);
                    ui.vertical_centered(|ui| {
                        ui.label(rt(
                            match self.tab {
                                Tab::Favorites if self.favs.is_empty() => "No favorites yet. Use the star on a game to add one",
                                Tab::Detected => "No detected games are running. Start one and press Rescan",
                                _ => "No games match your search",
                            },
                            p.muted,
                        ))
                    });
                    return;
                }

                let cols = (((ui.available_width() + 8.0) / (CARD_W + 8.0)).floor() as usize).max(1);
                let rows = shown.len().div_ceil(cols);
                egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, CARD_H, rows, |ui, range| {
                    for r in range {
                        ui.horizontal_top(|ui| {
                            for &i in shown.iter().skip(r * cols).take(cols) {
                                let app = &self.apps[i];
                                let active = self.playing.as_deref() == Some(app.id.as_str());
                                card(ui, p, app, self.favs.contains(&app.id), active, &mut acts);
                            }
                        });
                    }
                });
            });

        // Apply edits to the live status shortly after you stop changing things.
        if self.config.live_update {
            if let Some(id) = self.playing.clone() {
                let current = serde_json::to_string(&self.style).unwrap_or_default();
                if current != self.last_sent {
                    let since = *self.dirty_since.get_or_insert_with(std::time::Instant::now);
                    if since.elapsed() >= std::time::Duration::from_millis(600) {
                        self.push(&id);
                    } else {
                        ctx.request_repaint_after(std::time::Duration::from_millis(200));
                    }
                }
            }
        }

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
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "GameTimeSpoof RPC",
        options,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            if minimized {
                cc.egui_ctx
                    .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
            Ok(Box::new(Gts::new(cc.egui_ctx.clone())))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hours_normal_values() {
        assert_eq!(parse_hours("0").unwrap(), 0.0);
        assert_eq!(parse_hours("1").unwrap(), 1.0);
        assert_eq!(parse_hours("25").unwrap(), 25.0);
        assert_eq!(parse_hours("100").unwrap(), 100.0);
        assert_eq!(parse_hours(&format!("{}", MAX_HOURS)).unwrap(), MAX_HOURS);
        assert_eq!(parse_hours("10000").unwrap(), MAX_HOURS);
    }

    #[test]
    fn test_parse_hours_empty_or_whitespace() {
        assert_eq!(parse_hours("").is_none(), true);
        assert_eq!(parse_hours("   ").is_none(), true);
        assert_eq!(parse_hours("hello").is_none(), true);
    }

    #[test]
    fn test_parse_hours_nan_infinity() {
        assert_eq!(parse_hours("NaN").is_none(), true);
        assert_eq!(parse_hours("Infinity").is_none(), true);
        assert_eq!(parse_hours(&f64::INFINITY.to_string()), None);
    }

    #[test]
    fn test_parse_hours_negative_values() {
        // Negative becomes 0 via .max(0.0)
        assert_eq!(parse_hours("-1").unwrap(), 0.0);
        assert_eq!(parse_hours("-5.5").unwrap(), 0.0);
    }

    #[test]
    fn test_seconds_no_overflow() {
        let max_secs: i64 = (MAX_HOURS * 3600.0) as i64;
        assert!((max_secs + 1i64) > 0);
        assert!(max_secs < i64::MAX / 2);
    }

    #[test]
    fn test_parse_hours_zero_cases() {
        assert_eq!(parse_hours("0").unwrap(), 0.0);
    }
}
