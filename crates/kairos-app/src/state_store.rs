//! 面板外觀的持久化：`~/Library/Application Support/kairos/state.toml`。
//! 存的是「程式記的」——在視窗上拖出來的大小倍率、滾出來的不透明度——跟「你手寫的」
//! 主題檔分開，兩邊不互相蓋。app 自己寫、啟動時讀回來；人手改也讀得懂。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 大小倍率的範圍：一半到三倍。
pub const MIN_ZOOM: f64 = 0.5;
pub const MAX_ZOOM: f64 = 3.0;
/// 不透明度最低兩成，再低就找不到面板了。
pub const MIN_OPACITY: f64 = 0.2;
pub const MAX_OPACITY: f64 = 1.0;

pub fn clamp_zoom(zoom: f64) -> f64 {
    if zoom.is_finite() {
        zoom.clamp(MIN_ZOOM, MAX_ZOOM)
    } else {
        1.0
    }
}

pub fn clamp_opacity(opacity: f64) -> f64 {
    if opacity.is_finite() {
        opacity.clamp(MIN_OPACITY, MAX_OPACITY)
    } else {
        MAX_OPACITY
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewState {
    /// 面板的大小倍率，乘在主題檔的所有幾何上；1.0 就是主題檔原樣。
    pub zoom: f64,
    /// 滾輪或選單指定的不透明度；`None` 依主題檔。
    pub opacity: Option<f64>,
}

impl Default for ViewState {
    fn default() -> Self {
        ViewState {
            zoom: 1.0,
            opacity: None,
        }
    }
}

impl ViewState {
    pub fn default_path() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        home.join("Library/Application Support/kairos/state.toml")
    }

    /// 讀檔；不存在就是預設值，壞掉也用預設值並印到 stderr。讀進來的值一律夾回範圍。
    pub fn load(path: &Path) -> ViewState {
        let mut state = match fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<ViewState>(&text) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("外觀：{} 解析失敗（{e}），用預設值", path.display());
                    ViewState::default()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => ViewState::default(),
            Err(e) => {
                eprintln!("外觀：{} 讀取失敗（{e}），用預設值", path.display());
                ViewState::default()
            }
        };
        state.zoom = clamp_zoom(state.zoom);
        state.opacity = state.opacity.map(clamp_opacity);
        state
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(io::Error::other)?;
        fs::write(
            path,
            format!(
                "# kairos 面板外觀。由程式寫入：zoom 是大小倍率，opacity 沒寫就依主題檔。\n{text}"
            ),
        )
    }

    /// 給人看的一句：「大小 125%・不透明度 70%」。`theme_opacity` 是主題檔的值，依主題檔時拿來顯示。
    pub fn describe(&self, theme_opacity: f64) -> String {
        let opacity = self.opacity.unwrap_or(theme_opacity);
        format!(
            "大小 {:.0}% · 不透明度 {:.0}%{}",
            self.zoom * 100.0,
            opacity * 100.0,
            if self.opacity.is_none() {
                "（依主題檔）"
            } else {
                ""
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_keep_values_in_range_and_reject_nan() {
        assert_eq!(clamp_zoom(0.1), MIN_ZOOM);
        assert_eq!(clamp_zoom(9.0), MAX_ZOOM);
        assert_eq!(clamp_zoom(1.25), 1.25);
        assert_eq!(clamp_zoom(f64::NAN), 1.0);
        assert_eq!(clamp_opacity(0.0), MIN_OPACITY);
        assert_eq!(clamp_opacity(1.5), MAX_OPACITY);
        assert_eq!(clamp_opacity(f64::INFINITY), MAX_OPACITY);
    }

    #[test]
    fn save_then_load_roundtrips_and_missing_file_is_default() {
        let dir = std::env::temp_dir().join(format!("kairos-state-{}", std::process::id()));
        let path = dir.join("state.toml");
        assert_eq!(ViewState::load(&path), ViewState::default());

        let s = ViewState {
            zoom: 1.5,
            opacity: Some(0.7),
        };
        s.save(&path).unwrap();
        assert_eq!(ViewState::load(&path), s);

        // 沒寫 opacity 就是依主題檔；超出範圍的 zoom 讀進來會夾住。
        fs::write(&path, "zoom = 12\n").unwrap();
        assert_eq!(
            ViewState::load(&path),
            ViewState {
                zoom: MAX_ZOOM,
                opacity: None
            }
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn describe_mentions_theme_when_opacity_not_overridden() {
        let s = ViewState {
            zoom: 1.25,
            opacity: None,
        };
        assert_eq!(s.describe(0.9), "大小 125% · 不透明度 90%（依主題檔）");
        let s = ViewState {
            zoom: 0.5,
            opacity: Some(0.7),
        };
        assert_eq!(s.describe(0.9), "大小 50% · 不透明度 70%");
    }
}
