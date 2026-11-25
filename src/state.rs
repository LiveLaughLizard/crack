//! Things the interface remembers between launches: window size and effort.
//! (Open tabs are remembered per project; see `ui::view`.) Kept in the database (`kv` table, key `state`), apart from the user's config.

use crate::providers::Effort;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Last window size in logical pixels.
    pub window_width: Option<f32>,
    pub window_height: Option<f32>,
    pub window_maximized: bool,
    /// Last chosen reasoning effort.
    pub effort: Option<Effort>,
    /// Sandbox shell commands (ctrl-p toggle); `None` means the default (on).
    pub sandbox: Option<bool>,
    /// Let sandboxed commands use the network; `None` means the default (yes).
    pub sandbox_network: Option<bool>,
    /// Chosen in Settings: "dark" or "light".
    #[serde(default)]
    pub theme: Option<String>,
    /// Chosen in Settings: the interface font.
    #[serde(default)]
    pub font: Option<String>,
}

/// The state as last loaded or saved by this process. Saves go through the store's
/// background writer, so reading back from the database could miss a recent change.
static CACHE: std::sync::Mutex<Option<State>> = std::sync::Mutex::new(None);

impl State {
    pub fn load() -> State {
        let mut cache = CACHE.lock().unwrap();
        cache
            .get_or_insert_with(|| {
                crate::store::get()
                    .kv("state")
                    .and_then(|text| serde_json::from_str(&text).ok())
                    .unwrap_or_default()
            })
            .clone()
    }

    pub fn save(&self) {
        *CACHE.lock().unwrap() = Some(self.clone());
        if let Ok(text) = serde_json::to_string_pretty(self) {
            crate::store::get().set_kv("state", text);
        }
    }

    /// Loads, changes and saves in one step.
    pub fn update(change: impl FnOnce(&mut State)) {
        let mut state = State::load();
        let before = state.clone();
        change(&mut state);
        if state != before {
            state.save();
        }
    }
}
