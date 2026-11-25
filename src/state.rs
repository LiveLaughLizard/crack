//! Things the interface remembers between launches: window size and effort.
//! Kept in `~/.local/share/crack/state.json`, apart from the user's config.

use crate::providers::Effort;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Last window size in logical pixels.
    pub window_width: Option<f32>,
    pub window_height: Option<f32>,
    pub window_maximized: bool,
    /// Last chosen reasoning effort.
    pub effort: Option<Effort>,
}

fn path() -> PathBuf {
    crate::config::data_dir().join("state.json")
}

impl State {
    pub fn load() -> State {
        std::fs::read(path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("json.tmp");
        if let Ok(bytes) = serde_json::to_vec_pretty(self)
            && std::fs::write(&tmp, bytes).is_ok()
        {
            let _ = std::fs::rename(tmp, path);
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
