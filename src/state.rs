// SPDX-License-Identifier: GPL-3.0-or-later

//! Persistent UI state for gander.
//!
//! `gander` stores its own state (open tabs, active tab, etc.) separately from
//! anything `geese` owns. Profile data lives under `$GEESE_ROOT` and is owned
//! by `geese`; this file only records *which* profiles are currently open as
//! tabs and in what order.
//!
//! Location: `$XDG_DATA_HOME/gander/state.toml` (override with `GANDER_STATE`).

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// Current on-disk state-file schema version.
pub const VERSION: u32 = 0;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no data dir available to store gander state")]
    NoDataDir,
    #[error("io error reading or writing state: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse state.toml: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("failed to serialise state.toml: {0}")]
    Serialise(#[from] toml::ser::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// One open tab.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TabState {
    /// `geese` profile name.
    pub name: String,
}

/// On-disk state for gander.
///
/// Deserialised from / serialised to `state.toml`. Use [`Storage::load`] and
/// [`Storage::save`] rather than constructing this directly when interacting
/// with the filesystem.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct State {
    pub version: u32,
    /// Active tab's `geese` profile name, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// Ordered list of open tabs (head = leftmost).
    #[serde(default, rename = "tab", skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<TabState>,
}

impl State {
    pub fn new() -> Self {
        Self {
            version: VERSION,
            ..Self::default()
        }
    }
}

/// Filesystem-backed store for [`State`].
#[derive(Clone, Debug)]
pub struct Storage {
    path: PathBuf,
}

impl Storage {
    /// Resolve the storage location from the environment, preferring
    /// `GANDER_STATE` and falling back to `$XDG_DATA_HOME/gander/state.toml`.
    pub fn from_env() -> Result<Self> {
        if let Some(path) = env::var_os("GANDER_STATE") {
            return Ok(Self::at(PathBuf::from(path)));
        }
        let dir = dirs::data_dir().ok_or(Error::NoDataDir)?;
        Ok(Self::at(dir.join("gander").join("state.toml")))
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load state from disk, returning a fresh default if the file does not
    /// exist. Parse errors are surfaced; corrupt state files are *not*
    /// silently overwritten — that's a decision for the caller.
    pub fn load(&self) -> Result<State> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => Ok(toml::from_str(&contents)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::new()),
            Err(error) => Err(error.into()),
        }
    }

    /// Persist `state` to disk, creating parent directories as needed.
    pub fn save(&self, state: &State) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(state)?;
        fs::write(&self.path, contents)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn loads_default_when_missing() {
        let dir = tempdir().unwrap();
        let storage = Storage::at(dir.path().join("missing.toml"));
        let state = storage.load().unwrap();
        assert_eq!(state, State::new());
    }

    #[test]
    fn roundtrips_state() {
        let dir = tempdir().unwrap();
        let storage = Storage::at(dir.path().join("nested").join("state.toml"));
        let state = State {
            version: VERSION,
            active: Some("work".into()),
            tabs: vec![
                TabState {
                    name: "work".into(),
                },
                TabState {
                    name: "scratch".into(),
                },
            ],
        };
        storage.save(&state).unwrap();
        let loaded = storage.load().unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn surfaces_parse_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.toml");
        std::fs::write(&path, "not = valid = toml").unwrap();
        let storage = Storage::at(path);
        assert!(matches!(storage.load(), Err(Error::Parse(_))));
    }

    #[test]
    fn from_env_respects_override() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("override.toml");
        // SAFETY: tests are single-threaded for env access via `cargo test`'s
        // default test harness *per test process*; this test owns the variable
        // for its duration. We restore it on exit.
        unsafe { env::set_var("GANDER_STATE", &path) };
        let storage = Storage::from_env().unwrap();
        assert_eq!(storage.path(), path);
        unsafe { env::remove_var("GANDER_STATE") };
    }
}
