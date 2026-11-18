//! Game browser + custom Rich Presence. Display only: nothing is sent to
//! Discord's tracking endpoints, so badges and real play time are untouched.

use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};
use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
use serde::Deserialize;
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
const GREEN: Color32 = Color32::from_rgb(35, 165, 90);

// ---------- data ----------

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
    fn image_url(&self) -> Option<String> {
        if let Some(h) = &self.cover_image_hash {
            return Some(format!("https://cdn.discordapp.com/app-icons/{}/{h}.webp?size=300", self.id));
        }
        self.icon_hash
            .as_ref()
            .map(|h| format!("https://cdn.discordapp.com/app-icons/{}/{h}.webp?size=128", self.id))
    }
}

fn fetch_apps() -> Result<Vec<App>, String> {
    let mut apps: Vec<App> = reqwest::blocking::get(DETECTABLE_URL)
        .and_then(|r| r.json())
        .map_err(|e| e.to_string())?;
    // Games with cover art first, then alphabetical.
    apps.sort_by_key(|a| (a.cover_image_hash.is_none(), a.name.to_lowercase()));
    Ok(apps)
}

// ---------- favorites ----------

fn fav_path() -> PathBuf {
    let base = env::var("APPDATA")
        .or_else(|_| env::var("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|_| env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|_| PathBuf::from("."));
    let dir = base.join("gts-rpc");
    let _ = fs::create_dir_all(&dir);
    dir.join("favorites.txt")
}

fn load_favs() -> BTreeSet<String> {
    fs::read_to_string(fav_path())
        .map(|s| s.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

fn save_favs(favs: &BTreeSet<String>) {
    let _ = fs::write(fav_path(), favs.iter().cloned().collect::<Vec<_>>().join("\n"));
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
    Set { app_id: String, started: i64, details: Option<String> },
    Clear,
}

fn presence_worker(rx: mpsc::Receiver<Cmd>, status: Arc<Mutex<String>>) {
    let mut client: Option<DiscordIpcClient> = None;
    for cmd in rx {
        if let Some(mut c) = client.take() {
            let _ = c.clear_activity();
            let _ = c.close();
        }
        match cmd {
            Cmd::Clear => *status.lock().unwrap() = "Status cleared.".into(),
            Cmd::Set { app_id, started, details } => {
                let result = (|| -> Result<DiscordIpcClient, Box<dyn std::error::Error>> {
                    // The game's own application ID makes Discord show "Playing <game>".
                    let mut c = DiscordIpcClient::new(&app_id)?;
                    c.connect()?;
                    let mut act = activity::Activity::new()
                        .timestamps(activity::Timestamps::new().start(started));
                    if let Some(d) = details.as_deref() {
                        act = act.details(d);
                    }
                    c.set_activity(act)?;
                    Ok(c)
                })();
                let msg = match result {
                    Ok(c) => {
                        client = Some(c);
                        "Status is live.".to_string()
                    }
                    Err(e) => format!("Couldn't reach the Discord desktop client: {e}"),
                };
                *status.lock().unwrap() = msg;
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
    Stop,
    Fav(String),
    Open(String),
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
    hours: String,
    details: String,
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
            hours: "0".into(),
            details: String::new(),
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

    fn apply(&mut self, act: Act) {
        match act {
            Act::Play(id) => {
                let hours: f64 = self.hours.trim().parse().unwrap_or(0.0);
                let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
                let details = (!self.details.trim().is_empty()).then(|| self.details.trim().to_string());
                let _ = self.tx.send(Cmd::Set {
                    app_id: id.clone(),
                    started: now - (hours.max(0.0) * 3600.0) as i64,
                    details,
                });
                self.playing = Some(id);
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
        }
    }
}

fn card(ui: &mut egui::Ui, app: &App, fav: bool, active: bool, acts: &mut Vec<Act>) {
    ui.allocate_ui(Vec2::new(CARD_W, ROW_H - 10.0), |ui| {
        let stroke = if active { Stroke::new(2.0, GREEN) } else { ui.visuals().widgets.noninteractive.bg_stroke };
        egui::Frame::group(ui.style()).stroke(stroke).show(ui, |ui| {
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
                ui.label(RichText::new("Playing").color(GREEN).strong());
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
        let mut acts: Vec<Act> = vec![];
        let mut retry = false;
        let mut rescan = false;

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut self.tab, Tab::All, "All Games");
                let fav_label = if self.favs.is_empty() { "Favorites".to_string() } else { format!("Favorites ({})", self.favs.len()) };
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
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Playing for (hours):");
                ui.add(egui::TextEdit::singleline(&mut self.hours).desired_width(50.0));
                ui.label("Details:");
                ui.add(egui::TextEdit::singleline(&mut self.details).hint_text("optional status line").desired_width(220.0));
                let status = self.status.lock().unwrap().clone();
                if !status.is_empty() {
                    ui.separator();
                    ui.label(status);
                }
            });
            ui.add_space(6.0);
        });

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
            // The game you're showing stays pinned first.
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
                            card(ui, app, self.favs.contains(&app.id), active, &mut acts);
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
        // Give the worker a moment to clear the status before the process ends.
        thread::sleep(std::time::Duration::from_millis(300));
    }
}

fn main() -> eframe::Result {
    eframe::run_native(
        "GameTimeSpoof RPC",
        eframe::NativeOptions::default(),
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(Gts::new(cc.egui_ctx.clone())))
        }),
    )
}
