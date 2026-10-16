//! Disk persistence: radar levels, cascade history and tuning params.
//!
//! `<data>/radar.json`    — armed magnet levels per coin (+ last_cascade)
//! `<data>/cascades.json` — global recent cascade records
//! `<data>/tuning.json`  — live-tuned parameters
//!
//! Everything is JSON, written atomically (tmp + rename) by a periodic task
//! and on graceful shutdown, so overhang signals survive restarts.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use shared::CascadeRecord;

use super::radar::PersistRadar;

pub struct Persist {
    dir: PathBuf,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct RadarFile {
    /// version for future migrations
    v: u32,
    saved_at: i64,
    coins: Vec<PersistRadar>,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct CascadeFile {
    v: u32,
    saved_at: i64,
    /// newest last
    records: Vec<CascadeRecord>,
}

pub const MAX_GLOBAL_CASCADES: usize = 1_000;

impl Persist {
    pub fn new(dir: &Path) -> Self {
        let _ = std::fs::create_dir_all(dir);
        Persist { dir: dir.to_path_buf() }
    }

    fn write_atomic(&self, name: &str, body: &str) {
        let final_path = self.dir.join(name);
        let tmp = self.dir.join(format!("{name}.tmp"));
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &final_path);
        }
    }

    // -- radar ------------------------------------------------------------

    pub fn save_radar(&self, coins: &HashMap<String, PersistRadar>) {
        let file = RadarFile {
            v: 1,
            saved_at: crate::engine::now_ms(),
            coins: coins.values().cloned().collect(),
        };
        if let Ok(s) = serde_json::to_string(&file) {
            self.write_atomic("radar.json", &s);
        }
    }

    pub fn load_radar(&self) -> Vec<PersistRadar> {
        std::fs::read_to_string(self.dir.join("radar.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<RadarFile>(&s).ok())
            .map(|f| f.coins)
            .unwrap_or_default()
    }

    // -- cascades -----------------------------------------------------------

    pub fn save_cascades(&self, records: &VecDeque<CascadeRecord>) {
        let file = CascadeFile {
            v: 1,
            saved_at: crate::engine::now_ms(),
            records: records.iter().rev().cloned().collect(),
        };
        if let Ok(s) = serde_json::to_string(&file) {
            self.write_atomic("cascades.json", &s);
        }
    }

    pub fn load_cascades(&self) -> VecDeque<CascadeRecord> {
        let mut q: VecDeque<CascadeRecord> = std::fs::read_to_string(self.dir.join("cascades.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<CascadeFile>(&s).ok())
            .map(|f| f.records)
            .unwrap_or_default()
            .into_iter()
            .collect();
        while q.len() > MAX_GLOBAL_CASCADES {
            q.pop_front();
        }
        q
    }
}
