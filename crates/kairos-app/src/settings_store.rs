//! 設定：`~/Library/Application Support/kairos/settings.toml`。程式寫、程式讀，人手改也讀得懂。
//!
//! 兩件事：面板的大小倍率 `zoom`（在視窗上拖出來的），以及 `[theme]` 底下**跟主題檔不一樣的鍵**
//! （設定視窗、選單、滾輪改的）。有效主題＝主題檔為底、`[theme]` 蓋上去。主題檔改了某個鍵，
//! 該鍵的覆寫就作廢——最後一次動作為準。主題檔本身程式永遠不寫。
//!
//! 覆寫層是通用的：不用一鍵一段程式。主題與有效設定都攤成 TOML 表逐葉子比對，差異就是覆寫。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml::{Table, Value};

use crate::theme::Theme;

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
pub struct Settings {
    /// 面板的大小倍率，乘在主題檔的所有幾何上；1.0 就是主題檔原樣。
    pub zoom: f64,
    /// 主題的覆寫：跟 `theme.toml` 同結構的部分表，只放跟主題檔不一樣的鍵。
    pub theme: Table,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            zoom: 1.0,
            theme: Table::new(),
        }
    }
}

impl Settings {
    pub fn default_path() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        home.join("Library/Application Support/kairos/settings.toml")
    }

    /// 讀檔；不存在就是預設值，壞掉也用預設值並印到 stderr。倍率一律夾回範圍。
    pub fn load(path: &Path) -> Settings {
        let mut s = match fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<Settings>(&text) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("設定：{} 解析失敗（{e}），用預設值", path.display());
                    Settings::default()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Settings::default(),
            Err(e) => {
                eprintln!("設定：{} 讀取失敗（{e}），用預設值", path.display());
                Settings::default()
            }
        };
        s.zoom = clamp_zoom(s.zoom);
        s
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(io::Error::other)?;
        fs::write(
            path,
            format!(
                "# kairos 設定。由程式寫入：zoom 是面板的大小倍率；[theme] 底下是設定視窗、選單、滾輪改過、\n# 跟 theme.toml 不一樣的鍵，沒寫的鍵依 theme.toml。theme.toml 改了同一個鍵，這裡的就作廢。\n{text}"
            ),
        )
    }

    /// 主題檔為底、覆寫蓋上去。覆寫套不上（例如舊版留下、現在不認得的鍵）就回錯誤，呼叫端決定怎麼辦。
    pub fn effective(&self, base: &Theme) -> Result<Theme, String> {
        effective(base, &self.theme)
    }
}

/// 主題檔為底、覆寫蓋上去，再反解成主題。
pub fn effective(base: &Theme, overrides: &Table) -> Result<Theme, String> {
    if overrides.is_empty() {
        return Ok(base.clone());
    }
    Theme::from_table(merge(&base.to_table(), overrides)).map_err(|e| e.message().to_string())
}

/// 深合併：`over` 裡的表遞迴進去，其他值整個蓋掉（陣列也是整個換）。
pub fn merge(base: &Table, over: &Table) -> Table {
    let mut out = base.clone();
    for (k, v) in over {
        match (out.get_mut(k), v) {
            (Some(Value::Table(b)), Value::Table(o)) => {
                *b = merge(b, o);
            }
            _ => {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    out
}

/// `effective` 裡跟 `base` 不一樣的葉子（表遞迴、陣列當葉子），沒差的段落不會出現。
pub fn diff(base: &Table, effective: &Table) -> Table {
    let mut out = Table::new();
    for (k, v) in effective {
        match (base.get(k), v) {
            (Some(Value::Table(b)), Value::Table(e)) => {
                let d = diff(b, e);
                if !d.is_empty() {
                    out.insert(k.clone(), Value::Table(d));
                }
            }
            (Some(b), e) if b == e => {}
            (_, e) => {
                out.insert(k.clone(), e.clone());
            }
        }
    }
    out
}

/// 主題檔從 `old_base` 變成 `new_base`：變過的鍵，覆寫作廢。回傳作廢的鍵路徑（`panel.opacity`）。
pub fn prune(over: &mut Table, old_base: &Table, new_base: &Table) -> Vec<String> {
    let mut removed = Vec::new();
    prune_into(over, old_base, new_base, "", &mut removed);
    removed
}

fn prune_into(
    over: &mut Table,
    old_base: &Table,
    new_base: &Table,
    prefix: &str,
    removed: &mut Vec<String>,
) {
    let keys: Vec<String> = over.keys().cloned().collect();
    for k in keys {
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        let (old, new) = (old_base.get(&k), new_base.get(&k));
        let is_table = matches!(over.get(&k), Some(Value::Table(_)));
        if is_table {
            if let (Some(Value::Table(o)), Some(Value::Table(n))) = (old, new) {
                if let Some(Value::Table(sub)) = over.get_mut(&k) {
                    prune_into(sub, o, n, &path, removed);
                    if sub.is_empty() {
                        over.remove(&k);
                    }
                }
                continue;
            }
        }
        if old != new {
            over.remove(&k);
            removed.push(path);
        }
    }
}

/// 表裡有沒有這條葉子路徑（`beat.style`）。
pub fn has_leaf(table: &Table, path: &str) -> bool {
    let mut cur = table;
    let mut parts = path.split('.').peekable();
    while let Some(p) = parts.next() {
        match cur.get(p) {
            Some(Value::Table(t)) if parts.peek().is_some() => cur = t,
            Some(v) if parts.peek().is_none() => return !v.is_table(),
            _ => return false,
        }
    }
    false
}

/// 給人看的一句：「panel.opacity = 0.7、beat.style = "ring"」；空表回「沒有覆寫」。
pub fn describe(table: &Table) -> String {
    let mut out = Vec::new();
    collect_leaves(table, "", &mut out);
    if out.is_empty() {
        "沒有覆寫".to_string()
    } else {
        out.join("、")
    }
}

fn collect_leaves(table: &Table, prefix: &str, out: &mut Vec<String>) {
    for (k, v) in table {
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            Value::Table(t) => collect_leaves(t, &path, out),
            v => out.push(format!("{path} = {v}")),
        }
    }
}

/// 覆寫了幾個鍵。
pub fn leaf_count(table: &Table) -> usize {
    let mut out = Vec::new();
    collect_leaves(table, "", &mut out);
    out.len()
}

/// 「大小 125% · 不透明度 70%（依主題檔）」。
pub fn describe_view(zoom: f64, opacity: f64, opacity_overridden: bool) -> String {
    format!(
        "大小 {:.0}% · 不透明度 {:.0}%{}",
        zoom * 100.0,
        opacity * 100.0,
        if opacity_overridden {
            ""
        } else {
            "（依主題檔）"
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::BeatStyle;

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
        let dir = std::env::temp_dir().join(format!("kairos-settings-{}", std::process::id()));
        let path = dir.join("settings.toml");
        assert_eq!(Settings::load(&path), Settings::default());

        let mut theme = Theme::default();
        theme.panel.opacity = 0.7;
        theme.beat.style = BeatStyle::Ring;
        let s = Settings {
            zoom: 1.5,
            theme: diff(&Theme::default().to_table(), &theme.to_table()),
        };
        s.save(&path).unwrap();
        let back = Settings::load(&path);
        assert_eq!(back, s);
        assert_eq!(back.effective(&Theme::default()).unwrap(), theme);
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("[theme.panel]") && text.contains("opacity = 0.7"),
            "{text}"
        );

        // 只有 zoom、沒有 [theme]：依主題檔；超出範圍的 zoom 讀進來會夾住。
        fs::write(&path, "zoom = 12\n").unwrap();
        let s = Settings::load(&path);
        assert_eq!(s.zoom, MAX_ZOOM);
        assert!(s.theme.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn diff_merge_and_prune_work_leaf_by_leaf() {
        let base_theme = Theme::default();
        let base = base_theme.to_table();
        let mut t = base_theme.clone();
        t.panel.opacity = 0.6;
        t.beat.ticks = 6;
        t.display.screens.insert("DELL".into(), 8.0);
        let over = diff(&base, &t.to_table());
        assert_eq!(leaf_count(&over), 3);
        assert!(has_leaf(&over, "panel.opacity"));
        assert!(has_leaf(&over, "beat.ticks"));
        assert!(has_leaf(&over, "display.screens.DELL"));
        assert!(!has_leaf(&over, "panel.width"));
        assert!(!has_leaf(&over, "panel"));
        assert!(!over.contains_key("font"), "沒差的段落不出現：{over}");
        assert_eq!(Theme::from_table(merge(&base, &over)).unwrap(), t);
        assert!(
            describe(&over).contains("panel.opacity = 0.6"),
            "{}",
            describe(&over)
        );
        assert_eq!(describe(&Table::new()), "沒有覆寫");

        // 主題檔改了 opacity 與 ticks 以外的鍵：覆寫都留著。
        let mut new_base_theme = base_theme.clone();
        new_base_theme.panel.width = 400.0;
        let mut o = over.clone();
        assert!(prune(&mut o, &base, &new_base_theme.to_table()).is_empty());
        assert_eq!(o, over);

        // 主題檔改了 opacity：那一條作廢，段落空了就整段消失；其他留著。
        new_base_theme.panel.opacity = 0.9;
        let mut o = over.clone();
        let removed = prune(&mut o, &base, &new_base_theme.to_table());
        assert_eq!(removed, vec!["panel.opacity".to_string()]);
        assert!(!o.contains_key("panel"));
        assert!(has_leaf(&o, "beat.ticks"));
        let eff = effective(&new_base_theme, &o).unwrap();
        assert_eq!(eff.panel.opacity, 0.9);
        assert_eq!(eff.beat.ticks, 6);

        // 不認得的鍵套不上。
        let mut bad = Table::new();
        let mut beat = Table::new();
        beat.insert("bounce".into(), Value::Integer(1));
        bad.insert("beat".into(), Value::Table(beat));
        assert!(effective(&base_theme, &bad).is_err());
        assert_eq!(effective(&base_theme, &Table::new()).unwrap(), base_theme);
    }

    #[test]
    fn describe_view_mentions_theme_when_opacity_not_overridden() {
        assert_eq!(
            describe_view(1.25, 0.9, false),
            "大小 125% · 不透明度 90%（依主題檔）"
        );
        assert_eq!(describe_view(0.5, 0.7, true), "大小 50% · 不透明度 70%");
    }
}
