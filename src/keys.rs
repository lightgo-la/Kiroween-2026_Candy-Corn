//! API keys for the LLM providers, entered in the 🔑 API keys window of the studio and the graph tool.
//!
//! A saved key lives in Windows Credential Manager as a generic credential named `Candy-Corn/<VARIABLE>` (for
//! example `Candy-Corn/ANTHROPIC_API_KEY`), readable only by the current Windows user. A key can also come from the
//! environment variable of the same name.
//! - The GUI uses a saved key first, then the environment variable. The terminal mode `--run-graph` reads only the
//!   environment, so its result does not depend on what is saved on the machine.
//! - Keys are never shown (only the last 4 characters) and never written to blueprints, graph files, settings or logs.
//! - Unit tests use a per-thread in-memory store, so `cargo test` never reads or changes the saved keys.

use crate::canvas::graph::Provider;
use crate::theme;
use eframe::egui::{self, RichText};

/// Prefix of the credential names in Windows Credential Manager.
const TARGET_PREFIX: &str = "Candy-Corn/";
/// Longest key accepted (Credential Manager allows 2,560 bytes per secret).
pub const MAX_KEY_LEN: usize = 1024;

/// Where a key comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Saved,
    Env,
}

/// Trims a pasted key and checks it. It goes into an HTTP header, so only printable ASCII without spaces is accepted.
pub fn clean_key(raw: &str) -> Result<String, String> {
    let key = raw.trim();
    if key.is_empty() {
        return Err(tr!("Paste a key first").into());
    }
    if !key.chars().all(|c| c.is_ascii_graphic()) {
        return Err(tr!("A key can contain only ASCII letters, digits and symbols, without spaces").into());
    }
    if key.len() > MAX_KEY_LEN {
        return Err(trf!(
            "The key is too long (at most {max} characters)",
            max = MAX_KEY_LEN
        ));
    }
    Ok(key.to_string())
}

/// Credential name for an environment variable name.
fn target(var: &str) -> String {
    format!("{TARGET_PREFIX}{var}")
}

/// Saves a key for `var` (for example `ANTHROPIC_API_KEY`), replacing a saved one.
pub fn save(var: &str, key: &str) -> Result<(), String> {
    let key = clean_key(key)?;
    store::write(&target(var), var, &key)
}

/// Removes the saved key for `var`. Removing a key that is not saved is not an error.
pub fn remove(var: &str) -> Result<(), String> {
    store::delete(&target(var))
}

/// The saved key for `var`, if there is one. An empty saved value counts as no key.
pub fn saved(var: &str) -> Result<Option<String>, String> {
    match store::read(&target(var))? {
        Some(k) if !k.trim().is_empty() => clean_key(&k).map(Some),
        _ => Ok(None),
    }
}

/// The key in the environment variable `var` (trimmed; an empty value counts as not set).
pub fn from_env(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

// ---------- custom base URLs (settings file) ----------

/// Settings-file object that maps a provider's key variable name to its custom base URL.
const BASE_URLS_KEY: &str = "base_urls";

/// The custom base URL saved for `p` in the settings file at `path`. Empty if there is none. A file that exists but
/// cannot be read or parsed is an error: ignoring it would send the key and the data to the provider's public
/// address although the user chose another one.
pub fn load_base_url(path: &std::path::Path, p: Provider) -> Result<String, String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(trf!("Could not read the settings file: {e}", e = e)),
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| String::from(tr!("The settings file is not UTF-8 text. Fix it or delete it")))?;
    if text.trim().is_empty() {
        return Ok(String::new());
    }
    let v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| trf!("The settings file is not valid JSON ({e}). Fix it or delete it", e = e))?;
    Ok(v.get(BASE_URLS_KEY)
        .and_then(|m| m.get(p.default_key_env()))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_default())
}

/// Checks a base URL typed in the window. Typing it here is the explicit trust that custom endpoints need: the key is
/// sent to that address, so it must be https (http only for localhost). An empty URL means the provider's own.
pub fn clean_base_url(p: Provider, raw: &str) -> Result<String, String> {
    let url = raw.trim().trim_end_matches('/').to_string();
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(trf!("Invalid endpoint address: {url}", url = url));
    }
    crate::canvas::exec::check_endpoint(p, &url, true)?;
    Ok(url)
}

/// Saves (or, if empty, removes) the custom base URL for `p` in the settings file at `path`, keeping other keys.
/// Returns the cleaned URL.
pub fn save_base_url(path: &std::path::Path, p: Provider, raw: &str) -> Result<String, String> {
    let url = clean_base_url(p, raw)?;
    crate::i18n::update(path, |v| {
        if !v.get(BASE_URLS_KEY).is_some_and(serde_json::Value::is_object) {
            v[BASE_URLS_KEY] = serde_json::json!({});
        }
        let map = &mut v[BASE_URLS_KEY];
        if url.is_empty() {
            if let Some(o) = map.as_object_mut() {
                o.remove(p.default_key_env());
            }
        } else {
            map[p.default_key_env()] = serde_json::json!(url);
        }
    })?;
    Ok(url)
}

/// The base URL the studio's LLM run uses for `p`: the saved custom one if it is valid, else empty (the provider's own).
/// Re-checked here because the settings file can be edited by hand.
pub fn base_url(p: Provider) -> Result<String, String> {
    let Some(path) = crate::i18n::settings_path() else {
        return Ok(String::new());
    };
    clean_base_url(p, &load_base_url(&path, p)?)
}

/// "▾" button that lists the provider's current models; picking one writes its ID into `model`. Returns true when
/// the model changed. The model field next to it still accepts any other ID.
pub fn model_menu(ui: &mut egui::Ui, models: &[(&str, &str)], model: &mut String) -> bool {
    let mut changed = false;
    ui.menu_button("▾", |ui| {
        ui.label(RichText::new(tr!("Latest models")).small().weak());
        for (id, name) in models {
            let on = model.trim() == *id;
            if ui.selectable_label(on, format!("{name}  ·  {id}")).clicked() {
                if !on {
                    *model = (*id).to_string();
                    changed = true;
                }
                ui.close();
            }
        }
    })
    .response
    .on_hover_text(tr!("Pick one of the latest models (or type any model ID)"));
    changed
}

/// The key to use for `var`: with `use_saved`, a saved key first, then the environment variable.
/// A saved key that cannot be read counts as missing.
pub fn lookup(var: &str, use_saved: bool) -> Option<(String, Source)> {
    let saved = if use_saved { saved(var).ok().flatten() } else { None };
    pick(saved, || from_env(var))
}

/// A saved key wins over the environment variable.
fn pick(saved: Option<String>, env: impl FnOnce() -> Option<String>) -> Option<(String, Source)> {
    match saved {
        Some(k) => Some((k, Source::Saved)),
        None => env().map(|k| (k, Source::Env)),
    }
}

/// "…abcd": the last 4 characters, for display. Short keys show nothing of the key.
pub fn tail(key: &str) -> String {
    let n = key.chars().count();
    if n < 12 {
        return "…".into();
    }
    let last: String = key.chars().skip(n - 4).collect();
    format!("…{last}")
}

/// Overwrites the text before freeing it, so a typed key does not stay in released memory.
fn wipe(s: &mut String) {
    let mut bytes = std::mem::take(s).into_bytes();
    bytes.fill(0);
}

/// What the UI knows about one provider's key (never the key itself).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyState {
    /// Last characters of the saved key ("…abcd")
    pub saved_tail: Option<String>,
    /// Whether the environment variable is set
    pub env: bool,
    /// Error from reading the saved key
    pub error: Option<String>,
}

impl KeyState {
    fn read(var: &str) -> Self {
        let (saved_tail, error) = match saved(var) {
            Ok(k) => (k.as_deref().map(tail), None),
            Err(e) => (None, Some(e)),
        };
        KeyState {
            saved_tail,
            env: from_env(var).is_some(),
            error,
        }
    }

    pub fn available(&self) -> bool {
        self.saved_tail.is_some() || self.env
    }
}

fn index(p: Provider) -> usize {
    Provider::ALL.iter().position(|x| *x == p).unwrap_or(0)
}

/// The 🔑 API keys window (shared by the studio and the graph tool) and the cached key states it shows.
pub struct KeysWindow {
    pub open: bool,
    states: [KeyState; 3],
    inputs: [String; 3],
    /// Widget ids of the input fields, so their edit state (which holds the typed text) can be dropped
    input_ids: [Option<egui::Id>; 3],
    /// (is error, text) of the last save or remove
    message: Option<(bool, String)>,
    /// Text of the base URL fields (loaded from the settings file when the window opens)
    urls: [String; 3],
    /// The addresses as they are saved in the settings file (to tell a tested address that was never saved)
    saved_urls: [String; 3],
    /// Model used by each provider's connection test (picked in the window)
    test_models: [String; 3],
    /// Providers whose connection test is running
    testing: [bool; 3],
    test_tx: std::sync::mpsc::Sender<(usize, Result<String, String>)>,
    test_rx: std::sync::mpsc::Receiver<(usize, Result<String, String>)>,
    /// Size of the window in the last frame (for the layout test)
    #[cfg(test)]
    last_size: egui::Vec2,
}

enum KeyAct {
    Save(usize),
    Remove(usize),
    SaveUrl(usize),
    Test(usize),
}

impl KeysWindow {
    /// Reads which keys are saved or set. Call it once at startup, not every frame.
    pub fn load() -> Self {
        let (test_tx, test_rx) = std::sync::mpsc::channel();
        let mut w = KeysWindow {
            open: false,
            states: Default::default(),
            inputs: Default::default(),
            input_ids: [None; 3],
            message: None,
            urls: Default::default(),
            saved_urls: Default::default(),
            test_models: Provider::ALL.map(|p| p.default_model().to_string()),
            testing: [false; 3],
            test_tx,
            test_rx,
            #[cfg(test)]
            last_size: egui::Vec2::ZERO,
        };
        w.refresh();
        w
    }

    pub fn refresh(&mut self) {
        for (state, p) in self.states.iter_mut().zip(Provider::ALL) {
            *state = KeyState::read(p.default_key_env());
        }
    }

    pub fn state(&self, p: Provider) -> &KeyState {
        &self.states[index(p)]
    }

    /// Whether a run for `p` has a key (saved or in the environment), from the cached state.
    pub fn has_key(&self, p: Provider) -> bool {
        self.state(p).available()
    }

    pub fn show_window(&mut self) {
        // Already open (the button of the separate result window can be pressed while the window is up): keep what
        // was typed
        if self.open {
            return;
        }
        self.refresh();
        self.message = None;
        let path = crate::i18n::settings_path();
        for (i, p) in Provider::ALL.into_iter().enumerate() {
            let loaded = path
                .as_deref()
                .map(|f| load_base_url(f, p))
                .unwrap_or(Ok(String::new()));
            match loaded {
                Ok(u) => self.urls[i] = u,
                Err(e) => {
                    self.urls[i] = String::new();
                    self.message = Some((true, e));
                }
            }
            self.saved_urls[i] = self.urls[i].clone();
        }
        self.open = true;
    }

    fn close(&mut self, ctx: &egui::Context) {
        for i in 0..self.inputs.len() {
            self.forget_input(ctx, i);
        }
        self.open = false;
    }

    fn forget_input(&mut self, ctx: &egui::Context, i: usize) {
        wipe(&mut self.inputs[i]);
        if let Some(id) = self.input_ids[i] {
            ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(id));
        }
    }

    /// Sends a tiny request on a worker thread with the saved key and the address typed in the field. The result
    /// comes back through `test_rx`.
    fn start_test(&mut self, i: usize, ctx: &egui::Context) {
        let p = Provider::ALL[i];
        let url = match clean_base_url(p, &self.urls[i]) {
            Ok(u) => u,
            Err(e) => {
                self.message = Some((true, e));
                return;
            }
        };
        let Some((key, _)) = lookup(p.default_key_env(), true) else {
            self.message = Some((true, tr!("No key to test").into()));
            return;
        };
        self.testing[i] = true;
        self.message = None;
        let model = match self.test_models[i].trim() {
            "" => p.default_model().to_string(),
            m => m.to_string(),
        };
        let (tx, ctx) = (self.test_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let req = crate::canvas::llm::LlmRequest {
                provider: p,
                model: &model,
                system: "",
                prompt: "ping",
                api_key: &key,
                base_url: &url,
                max_tokens: 16,
                temperature: None,
            };
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::canvas::llm::call(&req)))
                .unwrap_or_else(|_| Err(tr!("internal error").into()))
                .map(|_| model.clone());
            let _ = tx.send((i, out));
            ctx.request_repaint();
        });
    }

    /// Draws the window while it is open. Returns true when a key was saved or removed.
    pub fn show(&mut self, ctx: &egui::Context) -> bool {
        if !self.open {
            return false;
        }
        let mut act = None;
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::unique("api-keys")).show(ctx, |ui| {
            // Fit small windows: the width follows the window (up to 640) and the body scrolls, with Close always visible.
            let screen = ctx.content_rect().size();
            ui.set_max_width(640.0_f32.min((screen.x - 80.0).max(280.0)));
            egui::ScrollArea::vertical()
                .max_height((screen.y - 160.0).max(200.0))
                .auto_shrink([false, true])
                .show(ui, |ui| {
            ui.label(RichText::new(tr!("🔑 API keys")).strong().size(18.0));
            ui.label(
                RichText::new(tr!(
                    "Keys are saved in Windows Credential Manager for your Windows user and are never written to blueprints, graph files or logs. A saved key is used before the environment variable."
                ))
                .small(),
            );
            ui.add_space(8.0);
            for (i, p) in Provider::ALL.into_iter().enumerate() {
                let var = p.default_key_env();
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(p.label()).strong().size(15.0));
                        ui.label(RichText::new(var).small().weak());
                        let st = &self.states[i];
                        let status = match (&st.error, &st.saved_tail) {
                            (Some(e), _) => {
                                RichText::new(trf!("Could not read the saved key: {e}", e = e)).color(theme::ERR)
                            }
                            (None, Some(t)) => RichText::new(trf!("Saved · {tail}", tail = t)).color(theme::OK),
                            (None, None) if st.env => {
                                RichText::new(trf!("From the {var} environment variable", var = var))
                            }
                            (None, None) => RichText::new(tr!("Not set")).weak(),
                        };
                        ui.label(status);
                    });
                    ui.horizontal(|ui| {
                        let input_w = (ui.available_width() - 150.0).max(160.0);
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.inputs[i])
                                .password(true)
                                .id_salt(("api-key-input", i))
                                .hint_text(trf!("Paste your {} API key here", p.label()))
                                .desired_width(input_w),
                        );
                        self.input_ids[i] = Some(resp.id);
                        let typed = !self.inputs[i].trim().is_empty();
                        let enter = resp.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter));
                        if ui.add_enabled(typed, egui::Button::new(tr!("Save"))).clicked() || (enter && typed) {
                            act = Some(KeyAct::Save(i));
                        }
                        if ui
                            .add_enabled(self.states[i].saved_tail.is_some(), egui::Button::new(tr!("Remove")))
                            .clicked()
                        {
                            act = Some(KeyAct::Remove(i));
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(tr!("Model")).small());
                        model_menu(ui, p.models(), &mut self.test_models[i]);
                        // A text field, not a fixed list: a local server or a proxy has its own model names
                        ui.add(
                            egui::TextEdit::singleline(&mut self.test_models[i])
                                .id_salt(("api-test-model", i))
                                .hint_text(p.default_model())
                                .desired_width(200.0),
                        )
                        .on_hover_text(tr!("Model used by Test connection. The model of a run is set in the LLM node"));
                    });
                });
            }            ui.add_space(10.0);
            ui.label(RichText::new(tr!("Endpoint (optional)")).strong());
            ui.label(
                RichText::new(tr!(
                    "Leave a field empty to use the provider's own address. A custom address (https, or http for localhost) receives your API key, for example a proxy or a local OpenAI-compatible server. It is saved in the settings file, not in blueprints."
                ))
                .small(),
            );
            egui::Grid::new("api-url-grid")
                .num_columns(3)
                .spacing([12.0, 10.0])
                .show(ui, |ui| {
                    for (i, p) in Provider::ALL.into_iter().enumerate() {
                        ui.label(RichText::new(p.label()).strong());
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.urls[i])
                                .id_salt(("api-url-input", i))
                                .hint_text(p.default_base_url())
                                .desired_width(300.0),
                        );
                        let enter = resp.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter));
                        ui.horizontal(|ui| {
                            if ui.button(tr!("Save")).clicked() || enter {
                                act = Some(KeyAct::SaveUrl(i));
                            }
                            let can_test = !self.testing[i] && self.states[i].available();
                            if ui
                                .add_enabled(can_test, egui::Button::new(tr!("Test connection")))
                                .on_hover_text(tr!(
                                    "Sends a tiny request with the saved key and the address in the field"
                                ))
                                .on_disabled_hover_text(tr!("Needs a key and no test already running"))
                                .clicked()
                            {
                                act = Some(KeyAct::Test(i));
                            }
                            if self.testing[i] {
                                ui.spinner();
                            }
                        });
                        ui.end_row();
                    }
                });
            if let Some((err, text)) = &self.message {
                ui.add_space(4.0);
                ui.label(RichText::new(text).color(if *err { theme::ERR } else { theme::OK }));
            }
                });
            ui.add_space(8.0);
            if ui.button(tr!("Close")).clicked() {
                close = true;
            }
        });
        #[cfg(test)]
        {
            self.last_size = modal.response.rect.size();
        }
        let changed = match act {
            Some(KeyAct::Save(i)) => {
                let p = Provider::ALL[i];
                let result = save(p.default_key_env(), &self.inputs[i]);
                self.forget_input(ctx, i);
                self.refresh();
                match result {
                    Ok(()) => {
                        self.message = Some((false, trf!("Saved the {} key. It is used from the next run", p.label())));
                        true
                    }
                    Err(e) => {
                        self.message = Some((true, trf!("Could not save the key: {e}", e = e)));
                        false
                    }
                }
            }
            Some(KeyAct::Remove(i)) => {
                let p = Provider::ALL[i];
                let result = remove(p.default_key_env());
                self.refresh();
                match result {
                    Ok(()) => {
                        self.message = Some((false, trf!("Removed the saved {} key", p.label())));
                        true
                    }
                    Err(e) => {
                        self.message = Some((true, trf!("Could not remove the key: {e}", e = e)));
                        false
                    }
                }
            }
            Some(KeyAct::SaveUrl(i)) => {
                let p = Provider::ALL[i];
                let saved = match crate::i18n::settings_path() {
                    Some(f) => save_base_url(&f, p, &self.urls[i]),
                    None => Err(tr!("The settings file is disabled (CANDY_CORN_SETTINGS is empty)").into()),
                };
                match saved {
                    Ok(url) => {
                        self.message = Some(if url.is_empty() {
                            (
                                false,
                                trf!("The {} endpoint is back to the provider's own address", p.label()),
                            )
                        } else {
                            (
                                false,
                                trf!("Saved the {} endpoint. It is used from the next run", p.label()),
                            )
                        });
                        self.saved_urls[i] = url.clone();
                        self.urls[i] = url;
                    }
                    Err(e) => self.message = Some((true, e)),
                }
                false
            }
            Some(KeyAct::Test(i)) => {
                self.start_test(i, ctx);
                false
            }
            None => false,
        };
        while let Ok((i, res)) = self.test_rx.try_recv() {
            self.testing[i] = false;
            let label = Provider::ALL[i].label();
            // The test used the address in the field; tell when that address is not the saved one yet
            let unsaved = self.urls[i].trim().trim_end_matches('/') != self.saved_urls[i];
            self.message = Some(match res {
                Ok(model) if unsaved => (
                    false,
                    trf!(
                        "{} connection works (model {}). This address is not saved yet: press Save next to it",
                        label,
                        model
                    ),
                ),
                Ok(model) => (false, trf!("{} connection works (model {})", label, model)),
                Err(e) => (true, trf!("{} connection failed: {e}", label, e = e)),
            });
        }
        if close || modal.should_close() {
            self.close(ctx);
        }
        changed
    }
}

// ---------- storage ----------

#[cfg(all(windows, not(test)))]
use credman as store;

/// Unit tests keep keys in memory, one store per test thread.
#[cfg(test)]
mod store {
    use std::cell::RefCell;
    use std::collections::HashMap;

    thread_local! {
        static MEM: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    }

    pub fn read(target: &str) -> Result<Option<String>, String> {
        Ok(MEM.with(|m| m.borrow().get(target).cloned()))
    }

    pub fn write(target: &str, _user: &str, secret: &str) -> Result<(), String> {
        MEM.with(|m| m.borrow_mut().insert(target.to_string(), secret.to_string()));
        Ok(())
    }

    pub fn delete(target: &str) -> Result<(), String> {
        MEM.with(|m| m.borrow_mut().remove(target));
        Ok(())
    }
}

#[cfg(all(not(windows), not(test)))]
mod store {
    pub fn read(_target: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    pub fn write(_target: &str, _user: &str, _secret: &str) -> Result<(), String> {
        Err(tr!("Saving API keys needs Windows Credential Manager").into())
    }

    pub fn delete(_target: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Windows Credential Manager: generic credentials of the current user, through advapi32.
#[cfg(windows)]
mod credman {
    use std::ffi::c_void;
    use std::ptr;

    const CRED_TYPE_GENERIC: u32 = 1;
    /// Kept on this computer for this user across logons (not roamed to other computers).
    const CRED_PERSIST_LOCAL_MACHINE: u32 = 2;
    const ERROR_NOT_FOUND: i32 = 1168;

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    /// CREDENTIALW
    #[repr(C)]
    struct Credential {
        flags: u32,
        kind: u32,
        target_name: *mut u16,
        comment: *mut u16,
        last_written: FileTime,
        blob_size: u32,
        blob: *mut u8,
        persist: u32,
        attribute_count: u32,
        attributes: *mut c_void,
        target_alias: *mut u16,
        user_name: *mut u16,
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn CredWriteW(credential: *const Credential, flags: u32) -> i32;
        fn CredReadW(target: *const u16, kind: u32, flags: u32, credential: *mut *mut Credential) -> i32;
        fn CredDeleteW(target: *const u16, kind: u32, flags: u32) -> i32;
        fn CredFree(buffer: *const c_void);
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Secrets are stored as UTF-16LE, like the ones Windows itself writes (Control Panel, cmdkey). UTF-8 blobs
    /// written by other tools are read too: a key is ASCII, so UTF-16LE text has a zero in every second byte.
    fn decode_blob(bytes: &[u8]) -> Option<String> {
        let (pairs, rest) = bytes.as_chunks::<2>();
        let utf16 = !pairs.is_empty() && rest.is_empty() && pairs.iter().all(|p| p[1] == 0);
        if utf16 {
            let units: Vec<u16> = pairs.iter().map(|p| u16::from_le_bytes(*p)).collect();
            String::from_utf16(&units).ok()
        } else {
            String::from_utf8(bytes.to_vec()).ok()
        }
    }

    pub fn read(target: &str) -> Result<Option<String>, String> {
        let name = wide(target);
        let mut cred: *mut Credential = ptr::null_mut();
        // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the call. On success the system allocates
        // the credential, and it is released with CredFree below.
        let ok = unsafe { CredReadW(name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) };
        if ok == 0 {
            let e = std::io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(ERROR_NOT_FOUND) => Ok(None),
                _ => Err(e.to_string()),
            };
        }
        // SAFETY: CredReadW succeeded, so `cred` points to a valid credential whose blob holds `blob_size` bytes.
        let mut bytes = unsafe {
            let c = &*cred;
            let bytes = if c.blob.is_null() || c.blob_size == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(c.blob, c.blob_size as usize).to_vec()
            };
            CredFree(cred.cast());
            bytes
        };
        let key = decode_blob(&bytes);
        bytes.fill(0);
        key.map(Some)
            .ok_or_else(|| tr!("The saved key is not valid text").into())
    }

    pub fn write(target: &str, user: &str, secret: &str) -> Result<(), String> {
        let mut name = wide(target);
        let mut user = wide(user);
        let mut blob: Vec<u8> = secret.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let cred = Credential {
            flags: 0,
            kind: CRED_TYPE_GENERIC,
            target_name: name.as_mut_ptr(),
            comment: ptr::null_mut(),
            last_written: FileTime { low: 0, high: 0 },
            blob_size: u32::try_from(blob.len()).unwrap_or(u32::MAX),
            blob: blob.as_mut_ptr(),
            persist: CRED_PERSIST_LOCAL_MACHINE,
            attribute_count: 0,
            attributes: ptr::null_mut(),
            target_alias: ptr::null_mut(),
            user_name: user.as_mut_ptr(),
        };
        // SAFETY: every pointer in `cred` points into a buffer above that lives until the call returns, and
        // blob_size is the length of `blob`.
        let ok = unsafe { CredWriteW(&cred, 0) };
        let result = if ok == 0 {
            Err(std::io::Error::last_os_error().to_string())
        } else {
            Ok(())
        };
        blob.fill(0);
        result
    }

    pub fn delete(target: &str) -> Result<(), String> {
        let name = wide(target);
        // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the call.
        let ok = unsafe { CredDeleteW(name.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if ok != 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            Some(ERROR_NOT_FOUND) => Ok(()),
            _ => Err(e.to_string()),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Deletes the test credential even if an assertion fails.
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = delete(&self.0);
            }
        }

        #[test]
        fn credential_manager_round_trip() {
            // A real credential under a name of its own; the user's saved keys are not touched.
            let name = format!("Candy-Corn-test/{}-{}", std::process::id(), line!());
            let _cleanup = Cleanup(name.clone());
            assert_eq!(read(&name).unwrap(), None);
            write(&name, "CC_TEST_API_KEY", "sk-test-0123456789abcdef").unwrap();
            assert_eq!(read(&name).unwrap().as_deref(), Some("sk-test-0123456789abcdef"));
            write(&name, "CC_TEST_API_KEY", "sk-test-replaced").unwrap();
            assert_eq!(read(&name).unwrap().as_deref(), Some("sk-test-replaced"));
            delete(&name).unwrap();
            assert_eq!(read(&name).unwrap(), None);
            // Deleting a missing credential is not an error.
            delete(&name).unwrap();
        }

        #[test]
        fn blobs_from_windows_and_from_other_tools_are_read() {
            let utf16: Vec<u8> = "sk-abc".encode_utf16().flat_map(u16::to_le_bytes).collect();
            assert_eq!(decode_blob(&utf16).as_deref(), Some("sk-abc"));
            assert_eq!(decode_blob(b"sk-abc").as_deref(), Some("sk-abc"));
            assert_eq!(decode_blob(&[0xff, 0xfe, 0xfd]), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{self, Lang};

    #[test]
    fn pasted_keys_are_trimmed_and_checked() {
        assert_eq!(clean_key("  sk-ant-abc_123-XYZ \r\n").unwrap(), "sk-ant-abc_123-XYZ");
        assert!(clean_key("   ").is_err());
        // Inner spaces, line breaks and non-ASCII text would break the HTTP header.
        for bad in ["sk ant", "sk\nant", "sk\u{0}ant", "sk-caf\u{e9}"] {
            assert!(clean_key(bad).is_err(), "{bad:?}");
        }
        assert!(clean_key(&"k".repeat(MAX_KEY_LEN)).is_ok());
        assert!(clean_key(&"k".repeat(MAX_KEY_LEN + 1)).is_err());
    }

    #[test]
    fn a_saved_key_comes_before_the_environment_and_only_when_allowed() {
        const VAR: &str = "CC_KEYS_TEST_UNSET_API_KEY";
        assert!(std::env::var(VAR).is_err(), "the test variable must not be set");
        assert_eq!(lookup(VAR, true), None);
        save(VAR, " sk-saved-0123456789 ").unwrap();
        assert_eq!(lookup(VAR, true), Some(("sk-saved-0123456789".into(), Source::Saved)));
        // Terminal modes do not use saved keys.
        assert_eq!(lookup(VAR, false), None);
        remove(VAR).unwrap();
        assert_eq!(lookup(VAR, true), None);
        remove(VAR).unwrap();
        // An invalid key is refused and nothing is saved.
        assert!(save(VAR, "two words").is_err());
        assert_eq!(saved(VAR).unwrap(), None);
    }

    #[test]
    fn a_saved_key_wins_over_the_environment_variable() {
        let env = || Some("sk-env".to_string());
        assert_eq!(
            pick(Some("sk-saved".into()), env),
            Some(("sk-saved".into(), Source::Saved))
        );
        assert_eq!(pick(None, env), Some(("sk-env".into(), Source::Env)));
        assert_eq!(pick(None, || None), None);
    }

    #[test]
    fn only_the_end_of_a_key_is_shown() {
        assert_eq!(tail("sk-ant-api03-abcdefgh1234"), "…1234");
        assert_eq!(tail("short-key"), "…");
        let mut typed = String::from("sk-secret");
        wipe(&mut typed);
        assert!(typed.is_empty());
    }

    #[test]
    fn key_states_follow_saves() {
        let var = Provider::Gemini.default_key_env();
        let mut w = KeysWindow::load();
        let env = from_env(var).is_some();
        assert_eq!(w.has_key(Provider::Gemini), env);
        save(var, "AIza-test-0123456789").unwrap();
        w.refresh();
        assert_eq!(w.state(Provider::Gemini).saved_tail.as_deref(), Some("…6789"));
        assert!(w.has_key(Provider::Gemini));
        remove(var).unwrap();
        w.refresh();
        assert_eq!(w.state(Provider::Gemini).saved_tail, None);
    }

    #[test]
    fn the_window_draws_in_both_languages() {
        for lang in Lang::ALL {
            let ctx = egui::Context::default();
            let mut w = KeysWindow::load();
            w.show_window();
            w.message = Some((true, "x".repeat(80)));
            for _ in 0..2 {
                let out = ctx.run_ui(egui::RawInput::default(), |ui| {
                    i18n::with_lang(lang, || {
                        let _ = w.show(ui.ctx());
                    });
                });
                out.drop_without_applying_deltas();
            }
            assert!(
                w.last_size.x <= 700.0 && w.last_size.y <= 500.0,
                "{lang:?}: {:?}",
                w.last_size
            );
            assert!(w.open, "{lang:?}");
            assert!(w.input_ids.iter().all(Option::is_some), "{lang:?}");
        }
    }

    #[test]
    fn base_url_round_trip_keeps_other_settings() {
        let dir = std::env::temp_dir().join(format!("cc_keys_url_{}", std::process::id()));
        let path = dir.join("settings.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, r#"{"lang":"ko"}"#).unwrap();
        assert_eq!(load_base_url(&path, Provider::OpenAi).unwrap(), "");
        let saved = save_base_url(&path, Provider::OpenAi, " http://localhost:8080/ ").unwrap();
        assert_eq!(saved, "http://localhost:8080");
        assert_eq!(load_base_url(&path, Provider::OpenAi).unwrap(), "http://localhost:8080");
        assert_eq!(load_base_url(&path, Provider::Gemini).unwrap(), "");
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["lang"], "ko");
        // An empty URL removes the entry
        assert_eq!(save_base_url(&path, Provider::OpenAi, "  ").unwrap(), "");
        assert_eq!(load_base_url(&path, Provider::OpenAi).unwrap(), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_lists_include_the_default_and_have_unique_ids() {
        for p in Provider::ALL {
            let ids: Vec<&str> = p.models().iter().map(|(id, _)| *id).collect();
            assert!(ids.contains(&p.default_model()), "{p:?}");
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), ids.len(), "{p:?}");
            assert!(
                ids.iter()
                    .all(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_graphic()))
            );
        }
    }

    #[test]
    fn base_url_must_be_https_or_loopback() {
        for bad in [
            "http://evil.example.com",
            "ftp://x.example.com",
            "not a url",
            "https://a b.example.com",
        ] {
            assert!(clean_base_url(Provider::OpenAi, bad).is_err(), "{bad}");
        }
        for good in [
            "https://llm.example.com/",
            "http://127.0.0.1:11434",
            "http://localhost:1234/v1",
            "",
        ] {
            assert!(clean_base_url(Provider::OpenAi, good).is_ok(), "{good}");
        }
    }

    #[test]
    fn the_window_fits_a_small_screen() {
        let screen = egui::vec2(560.0, 360.0);
        for lang in Lang::ALL {
            let ctx = egui::Context::default();
            let mut w = KeysWindow::load();
            w.show_window();
            w.message = Some((true, "x".repeat(200)));
            for _ in 0..2 {
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, screen)),
                    ..Default::default()
                };
                let out = ctx.run_ui(input, |ui| {
                    i18n::with_lang(lang, || {
                        let _ = w.show(ui.ctx());
                    });
                });
                out.drop_without_applying_deltas();
            }
            assert!(
                w.last_size.x <= screen.x && w.last_size.y <= screen.y,
                "{lang:?}: {:?}",
                w.last_size
            );
        }
    }

    #[test]
    fn an_unreadable_settings_file_stops_the_endpoint_lookup_and_is_kept_when_rewritten() {
        let dir = std::env::temp_dir().join(format!("cc_keys_bad_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        // A missing file just means no custom endpoint
        assert_eq!(load_base_url(&path, Provider::OpenAi).unwrap(), "");
        // A hand-edit mistake (trailing comma) is an error, not "use the public address"
        let broken = r#"{"lang":"ko","base_urls":{"OPENAI_API_KEY":"http://localhost:1234"},}"#;
        std::fs::write(&path, broken).unwrap();
        let e = i18n::with_lang(Lang::En, || load_base_url(&path, Provider::OpenAi)).unwrap_err();
        assert!(e.contains("not valid JSON"), "{e}");
        // A UTF-16 file is not UTF-8 text either
        std::fs::write(&path, [0xFF, 0xFE, b'{', 0, b'}', 0]).unwrap();
        assert!(load_base_url(&path, Provider::OpenAi).is_err());
        // Saving over the broken file keeps a copy of what was there
        std::fs::write(&path, broken).unwrap();
        save_base_url(&path, Provider::Gemini, "https://gw.example.com").unwrap();
        assert_eq!(
            std::fs::read_to_string(path.with_extension("json.bad")).unwrap(),
            broken
        );
        assert_eq!(
            load_base_url(&path, Provider::Gemini).unwrap(),
            "https://gw.example.com"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_endpoint_may_not_carry_a_query_or_fragment() {
        for bad in [
            "https://gw.example.com/?k=x",
            "https://gw.example.com/v1#x",
            "http://localhost:1234/?a=b",
        ] {
            assert!(clean_base_url(Provider::OpenAi, bad).is_err(), "{bad}");
        }
        assert_eq!(
            clean_base_url(Provider::OpenAi, "http://localhost:1234/v1/").unwrap(),
            "http://localhost:1234/v1"
        );
    }

    #[test]
    fn the_window_keeps_typed_endpoints_when_opened_again() {
        let mut w = KeysWindow::load();
        w.show_window();
        w.urls[0] = "http://localhost:1234".into();
        w.show_window(); // the separate result window's button does this while the window is up
        assert_eq!(w.urls[0], "http://localhost:1234");
    }
}
