//! The browser shell: the eframe web runner around [`SoundApp`].

use soundcraft_audio_io::FileFormat;
use soundcraft_engine::{Engine, io};
use soundcraft_ui_egui::dialogs::Dialog;
use soundcraft_ui_egui::sampler_window::SampleRead;
use soundcraft_ui_egui::{Services, SoundApp};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use wasm_bindgen::JsCast as _;

use crate::drive;

const CANVAS_ID: &str = "soundcraft_canvas";
const LANG_KEY: &str = "avidschool_lang";

/// `it` or `en` according to the interface language.
fn t(it: &'static str, en: &'static str) -> &'static str {
    if soundcraft_ui_egui::i18n::lang() == soundcraft_ui_egui::i18n::Lang::Italian { it } else { en }
}

/// The language kept in this browser (`?lang=en` or `?lang=it` wins); Italian when none.
fn saved_language() -> soundcraft_ui_egui::i18n::Lang {
    use soundcraft_ui_egui::i18n::Lang;
    let asked = query().trim_start_matches('?').split('&').find_map(|p| p.strip_prefix("lang=").map(str::to_string));
    let kept = web_sys::window().and_then(|w| w.local_storage().ok().flatten()).and_then(|s| s.get_item(LANG_KEY).ok().flatten());
    match asked.or(kept).as_deref() {
        Some("en") => Lang::English,
        _ => Lang::Italian,
    }
}

/// Switch between Italian and English and keep the choice in this browser.
fn toggle_language() {
    use soundcraft_ui_egui::i18n::{Lang, lang, set_lang};
    let next = if lang() == Lang::Italian { Lang::English } else { Lang::Italian };
    set_lang(next);
    keep_language(next);
}

fn keep_language(lang: soundcraft_ui_egui::i18n::Lang) {
    use soundcraft_ui_egui::i18n::Lang;
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(LANG_KEY, if lang == Lang::English { "en" } else { "it" });
    }
}
const LOADING_ID: &str = "soundcraft_loading";

pub fn start() {
    eframe::WebLogger::init(log::LevelFilter::Info).ok();
    io::set_deliver(drive::deliver);
    wasm_bindgen_futures::spawn_local(async {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            log::error!("no document");
            return;
        };
        let Some(canvas) = document.get_element_by_id(CANVAS_ID).and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok()) else {
            log::error!("missing <canvas id=\"{CANVAS_ID}\">");
            return;
        };
        let mut options = eframe::WebOptions::default();
        if query().contains("webgl")
            && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup
        {
            create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
        }
        let result = eframe::WebRunner::new()
            .start(
                canvas,
                options,
                Box::new(move |cc| {
                    if let Some(rs) = &cc.wgpu_render_state {
                        log::info!("soundcraft-web: wgpu backend {:?}", rs.adapter.get_info().backend);
                    }
                    let engine = if query().contains("empty") { Engine::default() } else { soundcraft_engine::demo::demo_engine() };
                    let player = soundcraft_playback::Player::new(Arc::new(engine.session().clone()));
                    soundcraft_ui_egui::i18n::set_lang(saved_language());
                    let mut services = Services::default();
                    if !drive::disabled() {
                        drive::remember_context(&cc.egui_ctx);
                        // Google's script is loaded now, so the sign-in popup opens straight from the
                        // tap: a phone's browser blocks a popup that comes after a wait.
                        drive::preload();
                        services.school_about = Some(school_about());
                    }
                    let app = SoundApp::new(engine, Some(player), services);
                    Ok(Box::new(WebShell { app, inbox: Rc::default(), samples: Rc::default(), files: Vec::new(), listing_open: false, message: String::new() }))
                }),
            )
            .await;
        if let Some(el) = document.get_element_by_id(LOADING_ID) {
            match result {
                Ok(()) => el.remove(),
                Err(e) => el.set_inner_html(&format!("<p>SoundCraft failed to start: {e:?}</p><p>A browser with WebGPU or WebGL2 is required.</p>")),
            }
        }
    });
}

fn query() -> String {
    web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default()
}

/// What a file read from the browser is for.
#[derive(Debug, Clone, Copy)]
enum Purpose {
    Session,
    Audio,
    Midi,
}

const SESSION_EXTS: &[&str] = &[io::BUNDLE_EXTENSION, soundcraft_model::SESSION_EXTENSION];
const MIDI_EXTS: &[&str] = &["mid", "midi", "smf"];

impl Purpose {
    /// A dropped file goes where its extension says: sessions open, MIDI and audio import.
    fn of(name: &str) -> Self {
        let ext = name.rsplit_once('.').map(|(_, x)| x.to_ascii_lowercase()).unwrap_or_default();
        if SESSION_EXTS.contains(&ext.as_str()) {
            Self::Session
        } else if MIDI_EXTS.contains(&ext.as_str()) {
            Self::Midi
        } else {
            Self::Audio
        }
    }

    /// The command whose path prompt this replaces in the browser.
    fn for_command(cmd: &str) -> Option<Self> {
        match cmd {
            "session.open" => Some(Self::Session),
            "file.import_audio" => Some(Self::Audio),
            "file.import_midi" => Some(Self::Midi),
            _ => None,
        }
    }

    fn filter(self) -> (&'static str, Vec<&'static str>) {
        match self {
            Self::Session => ("SoundCraft Session", SESSION_EXTS.to_vec()),
            Self::Audio => ("Audio", FileFormat::readable_extensions()),
            Self::Midi => ("MIDI", MIDI_EXTS.to_vec()),
        }
    }
}

/// Files picked or dropped, read asynchronously (browsers can only read files that way) and
/// handled on the next frame. The browser has no paths: files travel as bytes.
type Inbox = Rc<RefCell<Vec<(Purpose, String, Vec<u8>)>>>;

/// Audio dropped on an instrument track or a Sampler window, its bytes read, waiting for the
/// next frame to load into that track's sampler.
type SampleInbox = Rc<RefCell<Vec<(SampleRead, Result<Vec<u8>, String>)>>>;

struct WebShell {
    app: SoundApp,
    inbox: Inbox,
    samples: SampleInbox,
    /// The files found in the user's Drive folder, shown in the "Apri da Drive" window.
    files: Vec<drive::Entry>,
    listing_open: bool,
    /// The last thing Drive had to tell the user.
    message: String,
}

/// The About text of the school edition: what it is, who adapted it and where it comes from.
fn school_about() -> soundcraft_ui_egui::SchoolAbout {
    soundcraft_ui_egui::SchoolAbout {
        name: "Avidschool".to_string(),
        lines: vec![
            t("Versione didattica per la scuola, basata su SoundCraft.", "Classroom edition for school, based on SoundCraft.").to_string(),
            t(
                "A cura dei docenti della Scuola Secondaria di I grado R. Montecuccoli, Pavullo nel Frignano.",
                "By the teachers of the Scuola Secondaria di I grado R. Montecuccoli, Pavullo nel Frignano (Italy).",
            )
            .to_string(),
            t(
                "SoundCraft è software libero (licenza MIT oppure Apache-2.0) del team ArtCraft e dei suoi collaboratori. Questa versione è stata modificata: accesso con l'account Google della scuola, salvataggio e apertura su Google Drive.",
                "SoundCraft is free software (MIT or Apache-2.0 licence) by the ArtCraft team and its contributors. This version was modified: sign-in with the school's Google account, saving to and opening from Google Drive.",
            )
            .to_string(),
            t("Non è un prodotto del team ArtCraft né di Avid Technology, Inc.", "It is not a product of the ArtCraft team or of Avid Technology, Inc.").to_string(),
        ],
        link: Some((t("Progetto originale: getartcraft.com/apps/soundcraft", "Original project: getartcraft.com/apps/soundcraft").to_string(), "https://getartcraft.com/apps/soundcraft".to_string())),
        account: Some(Box::new(|| drive::account().map(|a| format!("{} {a}", t("Accesso effettuato come", "Signed in as"))))),
    }
}

impl WebShell {
    /// File > Open Session… / Import > Audio… / MIDI… would show a text box for a path, which the
    /// browser can't read (#63, #64). Show the browser's file picker instead.
    fn take_path_prompt(&mut self, ctx: &egui::Context) {
        let purpose = match &self.app.dialogs.open {
            Some(Dialog::PathPrompt { cmd, .. }) => Purpose::for_command(cmd),
            _ => None,
        };
        let Some(purpose) = purpose else { return };
        self.app.dialogs.open = None;
        let (inbox, ctx) = (self.inbox.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let (label, exts) = purpose.filter();
            let Some(file) = rfd::AsyncFileDialog::new().add_filter(label, &exts).pick_file().await else { return };
            let bytes = file.read().await;
            inbox.borrow_mut().push((purpose, file.file_name(), bytes));
            ctx.request_repaint();
        });
    }

    /// Read files dropped on the browser window asynchronously: audio for an instrument track or a Sampler goes to
    /// that sampler, while other drops are classified by extension and handled on a later frame.
    fn take_drops(&mut self, ctx: &egui::Context) {
        // In the browser a dropped file's `path()` is only its name: read its bytes instead.
        // Audio dropped on an instrument track or a Sampler window goes to its sampler instead.
        let dropped = self.app.take_dropped_files();
        for read in self.app.take_sample_reads() {
            let (samples, ctx) = (self.samples.clone(), ctx.clone());
            wasm_bindgen_futures::spawn_local(async move {
                let bytes = read.file.bytes_async().await;
                samples.borrow_mut().push((read, bytes));
                ctx.request_repaint();
            });
        }
        for f in dropped {
            let name = f.path().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Audio.wav".into());
            let (inbox, ctx) = (self.inbox.clone(), ctx.clone());
            wasm_bindgen_futures::spawn_local(async move {
                match f.bytes_async().await {
                    Ok(bytes) => {
                        inbox.borrow_mut().push((Purpose::of(&name), name, bytes));
                        ctx.request_repaint();
                    }
                    Err(e) => log::error!("couldn't read dropped file {name}: {e}"),
                }
            });
        }
    }

    fn handle(&mut self, purpose: Purpose, name: String, bytes: Vec<u8>) {
        let e = &mut self.app.engine;
        let at = e.session().edit.selection.start.max(0);
        let result = match purpose {
            // Asks first when the open session has changes: the bytes wait for the answer.
            Purpose::Session => return soundcraft_ui_egui::dialogs::open_session_bytes(&mut self.app, name, bytes),
            // Imports are edits: one undo step each, like the import commands.
            Purpose::Audio => {
                let before = e.session_arc();
                io::import_audio_bytes(e, &name, &bytes, None, None, at).map(|_| {
                    e.push_undo("Import Audio", before);
                    format!("Imported {name}")
                })
            }
            Purpose::Midi => {
                let before = e.session_arc();
                io::import_midi_bytes(e, &bytes, at, true).map(|_| {
                    e.push_undo("Import MIDI", before);
                    format!("Imported {name}")
                })
            }
        };
        self.app.ui.status = result.unwrap_or_else(|err| err.to_string());
    }
}

impl eframe::App for WebShell {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.take_drops(ctx);
        self.take_path_prompt(ctx);
        let pending = std::mem::take(&mut *self.inbox.borrow_mut());
        for (purpose, name, bytes) in pending {
            self.handle(purpose, name, bytes);
        }
        let samples = std::mem::take(&mut *self.samples.borrow_mut());
        for (read, bytes) in samples {
            self.app.load_sample_bytes(&read, bytes);
        }
        self.app.logic(ctx);
        // A prompt opened during this frame becomes a picker on the next one.
        self.take_path_prompt(ctx);
    }

    /// Forward raw input to the shared app hook so scripted gestures and host-key protection behave like the desktop
    /// shell.
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.app.raw_input_hook(ctx, raw);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !drive::disabled() {
            self.drive_bar(ui);
        }
        self.app.ui(ui);
        if !drive::disabled() && drive::needs_sign_in() && sign_in_gate(ui.ctx(), &self.message) {
            self.app.services.school_about = Some(school_about());
        }
    }
}

impl WebShell {
    /// The strip above the program: sign in, open from Drive, save to Drive.
    fn drive_bar(&mut self, ui: &mut egui::Ui) {
        if let Some(note) = drive::take_notice() {
            self.message = note;
        }
        if let Some(result) = drive::take_listing() {
            match result {
                Ok(list) => {
                    self.files = list;
                    self.listing_open = true;
                }
                Err(e) => self.message = format!("{} {e}", t("Non riesco a leggere il Drive:", "Can't read Drive:")),
            }
        }
        let (mut sign_in, mut sign_out, mut list, mut save, mut switch) = (false, false, false, false, false);
        let account = drive::account();
        egui::Panel::top("drive_bar").exact_size(44.0).show(ui, |ui| {
            egui::ScrollArea::horizontal().auto_shrink([false, true]).show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    let button = |ui: &mut egui::Ui, text: &str| ui.add(egui::Button::new(text).min_size(egui::vec2(0.0, 32.0))).clicked();
                    ui.label("Google Drive:");
                    switch = button(ui, t("English", "Italiano"));
                    match &account {
                        Some(name) => {
                            ui.label(name.as_str());
                            list = button(ui, t("Apri da Drive", "Open from Drive"));
                            save = button(ui, t("Salva su Drive", "Save to Drive"));
                            sign_out = button(ui, t("Esci", "Sign out"));
                        }
                        None => {
                            sign_in = button(ui, t("Accedi con Google", "Sign in with Google"));
                            list = button(ui, t("Apri da Drive", "Open from Drive"));
                            save = button(ui, t("Salva su Drive", "Save to Drive"));
                        }
                    }
                    if !self.message.is_empty() {
                        ui.add(egui::Label::new(self.message.as_str()).truncate());
                    }
                });
            });
        });
        if switch {
            toggle_language();
            self.app.services.school_about = Some(school_about());
        }
        if sign_in {
            drive::sign_in();
        }
        if sign_out {
            drive::sign_out();
            self.listing_open = false;
        }
        if list {
            drive::list();
        }
        if save {
            // The save-as prompt asks for the file name; the bytes then go to Drive.
            self.app.dialogs.open_for_command(&self.app.engine, "session.save_as");
        }
        let mut picked: Option<(String, String)> = None;
        egui::Window::new(t("Apri da Google Drive", "Open from Google Drive")).open(&mut self.listing_open).collapsible(false).show(ui.ctx(), |ui| {
            if self.files.is_empty() {
                ui.label(t("Nessun file nella cartella «Audio» del tuo Drive. Salva un progetto per trovarlo qui.", "No files in the \"Audio\" folder of your Drive. Save a project to find it here."));
            }
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                for (id, name, modified) in &self.files {
                    if ui.button(format!("{name}   {modified}")).clicked() {
                        picked = Some((id.clone(), name.clone()));
                    }
                }
            });
        });
        if let Some((id, name)) = picked {
            let inbox = self.inbox.clone();
            drive::open(&id, &name, Box::new(move |name, bytes| inbox.borrow_mut().push((Purpose::of(&name), name, bytes))));
            self.listing_open = false;
        }
    }
}

/// Until someone signs in with a school account, a full-screen cover with one big button sits over
/// the window and swallows every click, so nothing is worked on without a place to save it.
/// Returns whether the language was switched on it.
fn sign_in_gate(ctx: &egui::Context, message: &str) -> bool {
    let screen = ctx.content_rect();
    // A phone held upright is narrow: smaller type and a card that fits it.
    let k: f32 = if screen.width() < 560.0 { 0.62 } else { 1.0 };
    let visuals = ctx.global_style().visuals.clone();
    let mut switched = false;
    egui::Area::new(egui::Id::new("sign_in_gate")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
        ui.allocate_rect(screen, egui::Sense::click_and_drag());
        ui.painter().rect_filled(screen, 0.0, egui::Color32::from_black_alpha(225));
        let card = egui::Rect::from_center_size(
            screen.center(),
            egui::vec2((screen.width() - 24.0).clamp(240.0, 620.0), (400.0 * k).max(300.0).min(screen.height() - 24.0)),
        );
        ui.painter().rect_filled(card, 14.0, visuals.panel_fill);
        ui.painter().rect_stroke(card, 14.0, egui::Stroke::new(1.5, visuals.widgets.noninteractive.bg_stroke.color), egui::StrokeKind::Inside);
        ui.scope_builder(egui::UiBuilder::new().max_rect(card.shrink(30.0 * k)), |ui| {
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(t("Benvenuto", "Welcome")).size(34.0 * k).strong());
                ui.add_space(12.0);
                ui.label(egui::RichText::new(t("Accedi con l'account Google della scuola per aprire e salvare i tuoi progetti. I file vengono salvati nel tuo Google Drive.", "Sign in with your school Google account to open and save your projects. Files are saved in your Google Drive.")).size(19.0 * k));
                ui.add_space(22.0);
                let button = egui::Button::new(egui::RichText::new(t("Accedi con Google", "Sign in with Google")).size(24.0 * k)).min_size(egui::vec2(card.width() - 60.0 * k, 70.0 * k));
                if ui.add(button).clicked() {
                    drive::sign_in();
                }
                ui.add_space(14.0);
                ui.label(egui::RichText::new(format!("{} name@{}", t("Si aprirà una finestra di Google: scegli l'account della scuola,", "A Google window will open: choose your school account,"), drive::SCHOOL_DOMAIN)).size(15.0 * k));
                if !message.is_empty() {
                    ui.add_space(10.0);
                    ui.label(egui::RichText::new(message).size(15.0 * k).color(egui::Color32::LIGHT_RED));
                }
                ui.add_space(10.0);
                if ui.button(t("English", "Italiano")).clicked() {
                    toggle_language();
                    switched = true;
                }
            });
        });
    });
    switched
}

/// Hand a saved or exported file to the person as a download named `name` (the engine's delivery
/// hook on the web).
pub(crate) fn download(name: &str, bytes: &[u8]) -> Result<(), String> {
    let js = |e: wasm_bindgen::JsValue| format!("download failed: {e:?}");
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let opts = web_sys::BlobPropertyBag::new();
    let ext = name.rsplit_once('.').map(|(_, x)| x.to_ascii_lowercase());
    opts.set_type(match ext.as_deref() {
        Some("scraftz") => "application/zip",
        Some("mid" | "midi") => "audio/midi",
        Some("svg") => "image/svg+xml",
        Some("txt") => "text/plain",
        Some("json" | "scraft") => "application/json",
        Some("musicxml") => "application/vnd.recordare.musicxml+xml",
        ext => ext.and_then(FileFormat::from_extension).map_or("application/octet-stream", FileFormat::mime_type),
    });
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).map_err(js)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(js)?;
    let a: web_sys::HtmlAnchorElement = document.create_element("a").map_err(js)?.dyn_into().map_err(|_| "not an anchor")?;
    a.set_href(&url);
    a.set_download(name);
    a.style().set_property("display", "none").map_err(js)?;
    document.body().ok_or("no body")?.append_child(&a).map_err(js)?;
    a.click();
    a.remove();
    // Revoke after the click was dispatched; the download keeps its own reference.
    let revoke = wasm_bindgen::closure::Closure::once_into_js(move || {
        let _ = web_sys::Url::revoke_object_url(&url);
    });
    window.set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), 10_000).map_err(js)?;
    Ok(())
}
