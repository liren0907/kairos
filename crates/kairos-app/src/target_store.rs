//! 目標時刻的持久化：`~/Library/Application Support/kairos/target.toml`。
//! app 自己寫、啟動時讀回來；搶票當天程式意外重啟不用重填。人手改也讀得懂。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use kairos_core::target::{DEFAULT_LOCK_BEFORE, DEFAULT_MEASURE_BEFORE, LeadParams, TargetConfig};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TargetFile {
    /// 目標時刻，標準時間的 Unix 秒；沒有目標就是 `None`。
    pub target_unix_s: Option<i64>,
    pub reaction_ms: f64,
    pub browser_ms: f64,
    pub one_way_ms: f64,
    pub safety_ms: f64,
    pub measure_before_s: u64,
    pub lock_before_s: u64,
    /// 最近一次校正各輪的反應（毫秒），給人看的。
    pub calibration_ms: Vec<f64>,
}

impl Default for TargetFile {
    fn default() -> Self {
        let lead = LeadParams::default();
        TargetFile {
            target_unix_s: None,
            reaction_ms: lead.reaction_ms,
            browser_ms: lead.browser_ms,
            one_way_ms: lead.one_way_ms,
            safety_ms: lead.safety_ms,
            measure_before_s: DEFAULT_MEASURE_BEFORE.as_secs(),
            lock_before_s: DEFAULT_LOCK_BEFORE.as_secs(),
            calibration_ms: Vec::new(),
        }
    }
}

impl TargetFile {
    pub fn default_path() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        home.join("Library/Application Support/kairos/target.toml")
    }

    /// 讀檔；不存在就是預設值，壞掉也用預設值並印到 stderr。
    pub fn load(path: &Path) -> TargetFile {
        match fs::read_to_string(path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("目標：{} 解析失敗（{e}），用預設值", path.display());
                    TargetFile::default()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => TargetFile::default(),
            Err(e) => {
                eprintln!("目標：{} 讀取失敗（{e}），用預設值", path.display());
                TargetFile::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(io::Error::other)?;
        fs::write(
            path,
            format!("# kairos 目標時刻。由程式寫入；target_unix_s 是標準時間的 Unix 秒。\n{text}"),
        )
    }

    pub fn lead(&self) -> LeadParams {
        LeadParams {
            reaction_ms: self.reaction_ms,
            browser_ms: self.browser_ms,
            one_way_ms: self.one_way_ms,
            safety_ms: self.safety_ms,
        }
    }

    pub fn set_lead(&mut self, lead: LeadParams) {
        self.reaction_ms = lead.reaction_ms;
        self.browser_ms = lead.browser_ms;
        self.one_way_ms = lead.one_way_ms;
        self.safety_ms = lead.safety_ms;
    }

    /// 有目標時的狀態機設定。
    pub fn config(&self) -> Option<TargetConfig> {
        self.target_unix_s.map(|s| TargetConfig {
            target_unix_ns: s as i128 * 1_000_000_000,
            lead: self.lead(),
            measure_before: Duration::from_secs(self.measure_before_s),
            lock_before: Duration::from_secs(self.lock_before_s),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load_roundtrips_and_missing_file_is_default() {
        let dir = std::env::temp_dir().join(format!("kairos-target-{}", std::process::id()));
        let path = dir.join("target.toml");
        assert_eq!(TargetFile::load(&path), TargetFile::default());

        let t = TargetFile {
            target_unix_s: Some(1_800_000_000),
            reaction_ms: 188.5,
            calibration_ms: vec![180.0, 195.0, 190.0],
            ..TargetFile::default()
        };
        t.save(&path).unwrap();
        let back = TargetFile::load(&path);
        assert_eq!(back, t);
        let cfg = back.config().unwrap();
        assert_eq!(cfg.target_unix_ns, 1_800_000_000 * 1_000_000_000);
        assert_eq!(cfg.lead.reaction_ms, 188.5);
        assert_eq!(cfg.lock_before, Duration::from_secs(60));

        fs::write(&path, "reaction_ms = \"abc\"\n").unwrap();
        assert_eq!(TargetFile::load(&path), TargetFile::default());
        let _ = fs::remove_dir_all(&dir);
    }
}
