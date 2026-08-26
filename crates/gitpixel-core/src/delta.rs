//! Delta-layer state sidecar (`.gitpixel/state.json`).
//!
//! Records which commit the base shard is pinned to, which commit the delta
//! shard (if any) covers, and the paths tombstoned out of the base (modified
//! or deleted between base and HEAD).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const STATE_FILE: &str = "state.json";
pub const DELTA_FILE: &str = "delta.shard";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeltaState {
    /// OID the base shard was built at.
    pub base_oid: String,
    /// OID the delta shard covers (base_oid..delta_oid). None = no delta.
    pub delta_oid: Option<String>,
    /// Paths superseded in base/delta by newer history (matched by path).
    pub tombstones: Vec<String>,
}

pub fn state_path(gitpixel_dir: &Path) -> PathBuf {
    gitpixel_dir.join(STATE_FILE)
}

pub fn delta_shard_path(gitpixel_dir: &Path) -> PathBuf {
    gitpixel_dir.join(DELTA_FILE)
}

impl DeltaState {
    pub fn load(gitpixel_dir: &Path) -> Option<Self> {
        let bytes = std::fs::read(state_path(gitpixel_dir)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn save(&self, gitpixel_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(gitpixel_dir)?;
        let tmp = state_path(gitpixel_dir).with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).expect("state serializes"))?;
        std::fs::rename(&tmp, state_path(gitpixel_dir))
    }
}
