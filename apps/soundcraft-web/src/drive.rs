//! Google Drive for Avidschool: sign in with Google (Google Identity Services), save sessions and
//! exports in an "Audio" folder of the user's Drive and open them from there. The `drive.file` scope keeps the
//! app to the files it created itself. When Drive fails the file is downloaded instead, so work is
//! never lost. `?nodrive` turns Drive off.

use std::cell::{Cell, RefCell};

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};
use js_sys::Reflect;
use serde_json::{Value, json};
use wasm_bindgen_futures::JsFuture;

/// OAuth client of the school's Google Cloud project: public by design (a browser app has no
/// secret; Google only accepts the origins registered for it).
const CLIENT_ID: &str = "941646642229-v1amlshet3qalpu0qunksu7bee83dqc9.apps.googleusercontent.com";
const SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
const DOMAIN_HINT: &str = "smpavullo.com";
const GIS_URL: &str = "https://accounts.google.com/gsi/client";
const FOLDER_NAME: &str = "Audio";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const FILES_URL: &str = "https://www.googleapis.com/drive/v3/files";
const UPLOAD_URL: &str = "https://www.googleapis.com/upload/drive/v3/files";
/// Bigger files are downloaded instead of going through Drive's simple upload.
const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;
const MAX_OPEN_BYTES: usize = 256 * 1024 * 1024;
const TOKEN_MARGIN_MS: f64 = 60_000.0;
const BOUNDARY: &str = "avidschool_drive_boundary_7b2d90e1";
const SESSION_KEY: &str = "avidschool_drive_session";

/// A file in Drive: (id, name, last change).
pub type Entry = (String, String, String);

struct Token {
    value: String,
    expires_at_ms: f64,
}

thread_local! {
    static TOKEN: RefCell<Option<Token>> = const { RefCell::new(None) };
    static USER: RefCell<Option<String>> = const { RefCell::new(None) };
    static LISTING: RefCell<Option<Result<Vec<Entry>, String>>> = const { RefCell::new(None) };
    static NOTICE: RefCell<Option<String>> = const { RefCell::new(None) };
    static RESTORED: Cell<bool> = const { Cell::new(false) };
    static CONTEXT: RefCell<Option<egui::Context>> = const { RefCell::new(None) };
}

pub fn remember_context(ctx: &egui::Context) {
    CONTEXT.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_none() {
            *c = Some(ctx.clone());
        }
    });
}

fn wake() {
    CONTEXT.with(|c| {
        if let Some(ctx) = c.borrow().as_ref() {
            ctx.request_repaint();
        }
    });
}

/// Load Google's sign-in script in the background.
pub fn preload() {
    wasm_bindgen_futures::spawn_local(async {
        let _ = load_identity_script().await;
    });
}

pub fn disabled() -> bool {
    let search = web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default();
    search.trim_start_matches('?').split('&').any(|p| p == "nodrive")
}

/// A message for the user (what the last save or open did), taken once.
pub fn take_notice() -> Option<String> {
    NOTICE.with(|n| n.borrow_mut().take())
}

fn notify(message: &str) {
    NOTICE.with(|n| *n.borrow_mut() = Some(message.to_string()));
    wake();
}

pub fn take_listing() -> Option<Result<Vec<Entry>, String>> {
    LISTING.with(|l| l.borrow_mut().take())
}

fn session_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.session_storage().ok()?
}

fn store_session() {
    let token = TOKEN.with(|t| t.borrow().as_ref().map(|t| (t.value.clone(), t.expires_at_ms)));
    let (Some((value, expires)), Some(storage)) = (token, session_storage()) else { return };
    let name = USER.with(|u| u.borrow().clone());
    let _ = storage.set_item(SESSION_KEY, &json!({ "token": value, "expires": expires, "name": name }).to_string());
}

fn restore_session() {
    if RESTORED.with(|r| r.replace(true)) {
        return;
    }
    let Some(storage) = session_storage() else { return };
    let Some(kept) = storage.get_item(SESSION_KEY).ok().flatten().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { return };
    let (Some(value), Some(expires)) = (kept.get("token").and_then(Value::as_str), kept.get("expires").and_then(Value::as_f64)) else { return };
    if expires - TOKEN_MARGIN_MS <= js_sys::Date::now() {
        let _ = storage.remove_item(SESSION_KEY);
        return;
    }
    TOKEN.with(|t| *t.borrow_mut() = Some(Token { value: value.to_string(), expires_at_ms: expires }));
    USER.with(|u| *u.borrow_mut() = kept.get("name").and_then(Value::as_str).map(str::to_string));
}

/// Whether nobody has signed in yet in this tab. After that, an expired access only makes the
/// next save ask Google again, so open work is never covered up.
pub fn needs_sign_in() -> bool {
    restore_session();
    USER.with(|u| u.borrow().is_none())
}

/// The signed-in user's name while their access is valid.
pub fn account() -> Option<String> {
    restore_session();
    let valid = TOKEN.with(|t| t.borrow().as_ref().is_some_and(|t| t.expires_at_ms - TOKEN_MARGIN_MS > js_sys::Date::now()));
    valid.then(|| USER.with(|u| u.borrow().clone()).unwrap_or_else(|| "Google".to_string()))
}

pub fn sign_in() {
    wasm_bindgen_futures::spawn_local(async {
        match access_token().await {
            Ok(_) => wake(),
            Err(e) => notify(&format!("Accesso non riuscito: {e}")),
        }
    });
}

pub fn sign_out() {
    TOKEN.with(|t| *t.borrow_mut() = None);
    USER.with(|u| *u.borrow_mut() = None);
    if let Some(storage) = session_storage() {
        let _ = storage.remove_item(SESSION_KEY);
    }
    wake();
}

/// Look for the user's files in the "Audio" folder; [`take_listing`] gets the answer.
pub fn list() {
    wasm_bindgen_futures::spawn_local(async {
        let result = list_files().await;
        LISTING.with(|l| *l.borrow_mut() = Some(result));
        wake();
    });
}

async fn list_files() -> Result<Vec<Entry>, String> {
    let token = access_token().await?;
    let folder = find_or_create_folder(&token).await?;
    let query = format!("'{}' in parents and trashed = false and mimeType != '{FOLDER_MIME}'", quote(&folder));
    let url = format!("{FILES_URL}?q={}&orderBy=modifiedTime%20desc&pageSize=100&fields=files(id,name,modifiedTime)&spaces=drive", encode(&query));
    let answer = request(&token, "GET", &url, None, None).await?;
    let files = answer.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(files
        .iter()
        .filter_map(|f| {
            let id = f.get("id")?.as_str()?.to_string();
            let name = f.get("name")?.as_str()?.to_string();
            let modified = f.get("modifiedTime").and_then(Value::as_str).map(local_time).unwrap_or_default();
            Some((id, name, modified))
        })
        .collect())
}

fn local_time(rfc: &str) -> String {
    let date = js_sys::Date::new(&JsValue::from_str(rfc));
    if date.get_time().is_nan() {
        return String::new();
    }
    format!("{:02}/{:02} {:02}:{:02}", date.get_date(), date.get_month() + 1, date.get_hours(), date.get_minutes())
}

/// Download the file `id` and hand it to `then` to open as `name`.
pub fn open(id: &str, name: &str, then: Box<dyn FnOnce(String, Vec<u8>)>) {
    let (id, name) = (id.to_string(), name.to_string());
    wasm_bindgen_futures::spawn_local(async move {
        match download(&id).await {
            Ok(bytes) => {
                then(name, bytes);
                wake();
            }
            Err(e) => notify(&format!("Non riesco ad aprire il file dal Drive: {e}")),
        }
    });
}

async fn download(id: &str) -> Result<Vec<u8>, String> {
    let token = access_token().await?;
    let headers = js_sys::Object::new();
    set(&headers, "Authorization", &JsValue::from_str(&format!("Bearer {token}")))?;
    let init = web_sys::RequestInit::new();
    init.set_method("GET");
    init.set_headers(&headers);
    let window = web_sys::window().ok_or("nessuna finestra del browser")?;
    let response = JsFuture::from(window.fetch_with_str_and_init(&format!("{FILES_URL}/{}?alt=media", encode(id)), &init))
        .await
        .map_err(js_error)?
        .dyn_into::<web_sys::Response>()
        .map_err(|_| "risposta non valida da Google".to_string())?;
    if !response.ok() {
        return Err(format!("Drive ha risposto {}", response.status()));
    }
    let buffer = JsFuture::from(response.array_buffer().map_err(js_error)?).await.map_err(js_error)?;
    let bytes = js_sys::Uint8Array::new(&buffer);
    if bytes.length() as usize > MAX_OPEN_BYTES {
        return Err("il file è troppo grande".to_string());
    }
    Ok(bytes.to_vec())
}

/// The engine's delivery hook: every saved session and export goes to Drive in the background;
/// if that fails it is downloaded instead.
pub fn deliver(name: &str, bytes: &[u8]) -> Result<(), String> {
    if disabled() || bytes.len() > MAX_UPLOAD_BYTES {
        return crate::web::download(name, bytes);
    }
    let (name, bytes) = (name.to_string(), bytes.to_vec());
    wasm_bindgen_futures::spawn_local(async move {
        match save(&name, &bytes).await {
            Ok(()) => notify(&format!("Salvato nel tuo Google Drive, cartella «{FOLDER_NAME}»: {name}")),
            Err(e) => match crate::web::download(&name, &bytes) {
                Ok(()) => notify(&format!("Non sono riuscito a salvare sul Drive ({e}). Il file è stato scaricato sul computer: {name}")),
                Err(d) => notify(&format!("Salvataggio non riuscito. Drive: {e}. Download: {d}")),
            },
        }
    });
    Ok(())
}

async fn save(name: &str, bytes: &[u8]) -> Result<(), String> {
    let token = access_token().await?;
    let folder = find_or_create_folder(&token).await?;
    let query = format!("name = '{}' and '{}' in parents and trashed = false", quote(name), quote(&folder));
    let url = format!("{FILES_URL}?q={}&fields=files(id)&pageSize=1&spaces=drive", encode(&query));
    let existing = first_id(&request(&token, "GET", &url, None, None).await?);
    let (url, method, meta) = match existing {
        Some(id) => (format!("{UPLOAD_URL}/{id}?uploadType=multipart&fields=id"), "PATCH", json!({ "name": name })),
        None => (format!("{UPLOAD_URL}?uploadType=multipart&fields=id"), "POST", json!({ "name": name, "parents": [folder] })),
    };
    let mut body = Vec::with_capacity(bytes.len().saturating_add(512));
    body.extend_from_slice(
        format!("--{BOUNDARY}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n--{BOUNDARY}\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--").as_bytes());
    let content_type = format!("multipart/related; boundary={BOUNDARY}");
    request(&token, method, &url, Some(&content_type), Some(body)).await.map(|_| ())
}

async fn find_or_create_folder(token: &str) -> Result<String, String> {
    let query = format!("name = '{}' and mimeType = '{FOLDER_MIME}' and trashed = false", quote(FOLDER_NAME));
    let url = format!("{FILES_URL}?q={}&fields=files(id)&pageSize=1&spaces=drive", encode(&query));
    if let Some(id) = first_id(&request(token, "GET", &url, None, None).await?) {
        return Ok(id);
    }
    let meta = json!({ "name": FOLDER_NAME, "mimeType": FOLDER_MIME });
    let created = request(token, "POST", &format!("{FILES_URL}?fields=id"), Some("application/json"), Some(meta.to_string().into_bytes())).await?;
    created.get("id").and_then(Value::as_str).map(str::to_string).ok_or_else(|| "Drive non ha creato la cartella".to_string())
}

fn first_id(answer: &Value) -> Option<String> {
    answer.get("files")?.get(0)?.get("id")?.as_str().map(str::to_string)
}

fn quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

fn encode(s: &str) -> String {
    String::from(js_sys::encode_uri_component(s))
}

async fn request(token: &str, method: &str, url: &str, content_type: Option<&str>, body: Option<Vec<u8>>) -> Result<Value, String> {
    let headers = js_sys::Object::new();
    set(&headers, "Authorization", &JsValue::from_str(&format!("Bearer {token}")))?;
    if let Some(content_type) = content_type {
        set(&headers, "Content-Type", &JsValue::from_str(content_type))?;
    }
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    init.set_headers(&headers);
    if let Some(body) = body {
        init.set_body(&js_sys::Uint8Array::from(body.as_slice()));
    }
    let window = web_sys::window().ok_or("nessuna finestra del browser")?;
    let response = JsFuture::from(window.fetch_with_str_and_init(url, &init))
        .await
        .map_err(js_error)?
        .dyn_into::<web_sys::Response>()
        .map_err(|_| "risposta non valida da Google".to_string())?;
    let text = JsFuture::from(response.text().map_err(js_error)?).await.map_err(js_error)?.as_string().unwrap_or_default();
    if !response.ok() {
        return Err(format!("Drive ha risposto {}: {}", response.status(), text.chars().take(200).collect::<String>()));
    }
    serde_json::from_str(&text).map_err(|e| format!("risposta di Drive non leggibile: {e}"))
}

/// The school's email domain: the only accounts that may sign in.
pub const SCHOOL_DOMAIN: &str = DOMAIN_HINT;

/// Whether `email` is an address of the school (not of a lookalike domain).
fn is_school_email(email: &str) -> bool {
    let email = email.trim().to_lowercase();
    email.strip_suffix(&format!("@{SCHOOL_DOMAIN}")).is_some_and(|local| !local.is_empty() && !local.contains('@'))
}

/// Ask Drive who the token belongs to and let only school accounts in: the account is forgotten
/// and an error returned for any other (Google's `hd` hint only pre-selects, it doesn't enforce).
async fn verify_account(token: &str) -> Result<(), String> {
    let answer = request(token, "GET", "https://www.googleapis.com/drive/v3/about?fields=user(displayName,emailAddress)", None, None).await?;
    let user = answer.get("user");
    let email = user.and_then(|u| u.get("emailAddress")).and_then(Value::as_str).unwrap_or_default().to_string();
    if !is_school_email(&email) {
        TOKEN.with(|t| *t.borrow_mut() = None);
        USER.with(|u| *u.borrow_mut() = None);
        let shown = if email.is_empty() { "sconosciuto".to_string() } else { email };
        return Err(format!("l'account {shown} non è della scuola: usa un account @{SCHOOL_DOMAIN}"));
    }
    let name = user.and_then(|u| u.get("displayName")).and_then(Value::as_str).unwrap_or("Google");
    USER.with(|u| *u.borrow_mut() = Some(format!("{name} ({email})")));
    Ok(())
}

async fn access_token() -> Result<String, String> {
    restore_session();
    let now = js_sys::Date::now();
    let kept = TOKEN.with(|t| t.borrow().as_ref().filter(|t| t.expires_at_ms - TOKEN_MARGIN_MS > now).map(|t| t.value.clone()));
    if let Some(value) = kept {
        return Ok(value);
    }
    load_identity_script().await?;
    let promise = js_sys::Promise::new(&mut |resolve, reject| {
        if let Err(e) = start_token_request(&resolve, &reject) {
            let _ = reject.call1(&JsValue::NULL, &JsValue::from_str(&e));
        }
    });
    let response = JsFuture::from(promise).await.map_err(|e| e.as_string().unwrap_or_else(|| "accesso a Google non riuscito".to_string()))?;
    if let Some(error) = field(&response, "error").and_then(|e| e.as_string()) {
        return Err(format!("accesso a Google non riuscito: {error}"));
    }
    let value = field(&response, "access_token").and_then(|v| v.as_string()).ok_or("Google non ha dato il permesso di salvare")?;
    let seconds = field(&response, "expires_in").and_then(|v| v.as_f64().or_else(|| v.as_string().and_then(|s| s.parse().ok()))).unwrap_or(3000.0);
    TOKEN.with(|t| *t.borrow_mut() = Some(Token { value: value.clone(), expires_at_ms: js_sys::Date::now() + seconds * 1000.0 }));
    verify_account(&value).await?;
    store_session();
    Ok(value)
}

fn start_token_request(resolve: &js_sys::Function, reject: &js_sys::Function) -> Result<(), String> {
    let window = web_sys::window().ok_or("nessuna finestra del browser")?;
    let oauth2 = path(window.as_ref(), &["google", "accounts", "oauth2"])?;
    let init = field(&oauth2, "initTokenClient").and_then(|f| f.dyn_into::<js_sys::Function>().ok()).ok_or("il login di Google non è disponibile")?;
    let on_token = {
        let resolve = resolve.clone();
        Closure::<dyn FnMut(JsValue)>::new(move |response: JsValue| {
            let _ = resolve.call1(&JsValue::NULL, &response);
        })
        .into_js_value()
    };
    let on_error = {
        let reject = reject.clone();
        Closure::<dyn FnMut(JsValue)>::new(move |error: JsValue| {
            let kind = field(&error, "type").and_then(|t| t.as_string()).unwrap_or_else(|| "errore".to_string());
            let _ = reject.call1(&JsValue::NULL, &JsValue::from_str(&format!("accesso a Google annullato ({kind})")));
        })
        .into_js_value()
    };
    let config = js_sys::Object::new();
    set(&config, "client_id", &JsValue::from_str(CLIENT_ID))?;
    set(&config, "scope", &JsValue::from_str(SCOPE))?;
    set(&config, "hd", &JsValue::from_str(DOMAIN_HINT))?;
    set(&config, "callback", &on_token)?;
    set(&config, "error_callback", &on_error)?;
    let client = init.call1(&oauth2, &config).map_err(js_error)?;
    let ask = field(&client, "requestAccessToken").and_then(|f| f.dyn_into::<js_sys::Function>().ok()).ok_or("il login di Google non è disponibile")?;
    ask.call0(&client).map_err(js_error)?;
    Ok(())
}

async fn load_identity_script() -> Result<(), String> {
    let window = web_sys::window().ok_or("nessuna finestra del browser")?;
    if path(window.as_ref(), &["google", "accounts", "oauth2"]).is_ok() {
        return Ok(());
    }
    let document = window.document().ok_or("nessun documento del browser")?;
    let body = document.body().ok_or("la pagina non ha un corpo")?;
    let script = document.create_element("script").map_err(js_error)?;
    script.set_attribute("src", GIS_URL).map_err(js_error)?;
    script.set_attribute("async", "").map_err(js_error)?;
    let promise = js_sys::Promise::new(&mut |resolve, reject| {
        let on_load = Closure::once_into_js(move || {
            let _ = resolve.call0(&JsValue::NULL);
        });
        let on_error = Closure::once_into_js(move || {
            let _ = reject.call0(&JsValue::NULL);
        });
        let _ = script.add_event_listener_with_callback("load", on_load.unchecked_ref());
        let _ = script.add_event_listener_with_callback("error", on_error.unchecked_ref());
    });
    body.append_child(&script).map_err(js_error)?;
    if JsFuture::from(promise).await.is_err() {
        script.remove();
        return Err("impossibile caricare il login di Google (rete bloccata?)".to_string());
    }
    Ok(())
}

fn path(root: &JsValue, keys: &[&str]) -> Result<JsValue, String> {
    let mut current = root.clone();
    for key in keys {
        current = field(&current, key).ok_or_else(|| format!("manca {key}"))?;
    }
    Ok(current)
}

fn field(object: &JsValue, key: &str) -> Option<JsValue> {
    Reflect::get(object, &JsValue::from_str(key)).ok().filter(|v| !v.is_undefined() && !v.is_null())
}

fn set(object: &js_sys::Object, key: &str, value: &JsValue) -> Result<(), String> {
    Reflect::set(object, &JsValue::from_str(key), value).map(|_| ()).map_err(js_error)
}

fn js_error(e: JsValue) -> String {
    e.as_string().or_else(|| field(&e, "message").and_then(|m| m.as_string())).unwrap_or_else(|| format!("{e:?}"))
}
