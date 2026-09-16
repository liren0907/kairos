//! 主題檔：字型、字級、顏色、間距、圓角、材質。存在
//! `~/Library/Application Support/kairos/theme.toml`，`notify` 監看資料夾，存檔即時生效。
//!
//! 檔案不存在時寫入 [`DEFAULT_THEME_TOML`]（含註解）；解析失敗時沿用上一版並印到 stderr。
//! 欄位名打錯會直接報錯（`deny_unknown_fields`），比默默忽略好找。

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameVibrantDark, NSAppearanceNameVibrantLight, NSColor, NSFont,
    NSFontWeightBold, NSFontWeightMedium, NSFontWeightRegular, NSFontWeightSemibold,
    NSVisualEffectMaterial,
};
use objc2_foundation::NSString;
use serde::Deserialize;

use kairos_core::beat::audio::TickSound;
use kairos_core::sync::{SamplerConfig, SyncSettings};

/// 預設主題，也是第一次啟動時寫到磁碟的內容。
pub const DEFAULT_THEME_TOML: &str = r##"# kairos 主題檔。存檔即時生效；寫錯會在終端機印出錯誤並沿用上一版。

[panel]
width = 320            # 點；高度依內容自動算
corner_radius = 14
material = "hud"       # hud | popover | sidebar | menu | under_window | window_background
appearance = "dark"    # dark | light | system
opacity = 1.0          # 整塊面板的不透明度

[font]
family = ""            # 空字串＝系統字型（數字等寬）；例如 "Menlo"、"JetBrains Mono"
weight = "medium"      # regular | medium | semibold | bold
time_size = 34
detail_size = 13
caption_size = 11

[colors]               # "#RRGGBB" 或 "#RRGGBBAA"
time = "#FFFFFF"
detail = "#FFFFFFD0"
caption = "#FFFFFF80"
bar = "#5AA9FF"
bar_track = "#FFFFFF26"

[layout]
padding = 16
line_gap = 6
bar_height = 4
bar_full_scale_ms = 50 # 不確定度長條滿格對應的 ± 毫秒數

[display]
lead_ms = 0            # 顯示提前量的預設值：畫面實際上屏比 targetTimestamp 晚多少毫秒
idle_fps = 60          # 平常的刷新率；節拍期間會拉到面板所在螢幕的最高刷新率

[display.screens]      # 依螢幕名稱覆寫 lead_ms；名稱看啟動時終端機印的螢幕清單，例如：
                       # "DELL P2421D" = 8

[beat]
style = "auto"         # auto | ball | ring | pulse；auto＝ball，系統開「減少動態效果」時改 pulse
renderer = "auto"      # auto | metal | layer；auto＝有 Metal 就用 Metal 畫節拍區並量每一格實際上屏的時刻，layer＝CALayer
period_ms = 1000       # 拍距
ticks = 4              # 拍數，含歸零那一拍；聲音從第一拍起，畫面提早一拍起跑
strip_height = 64      # 節拍區高度（點），只在節拍期間出現在數字上方
color = "#5AA9FF"
final_color = "#FFB454" # 歸零拍的顏色（尺寸也放大）
glow = true            # 螢幕邊緣光暈（每個螢幕一層，滑鼠穿透）
glow_width = 48        # 光暈寬度（點）
glow_opacity = 0.35    # 光暈最亮時的不透明度
volume = 0.5           # 0 到 1
tick_hz = 1000         # 前導拍與歸零拍同音高
tick_ms = 30           # 前導拍長度
final_ms = 200         # 歸零拍長度
visual_lead_ms = 0     # 畫面相對聲音的提前量：正值＝畫面先到；主觀覺得畫面慢就調正

[sync]                 # 時間來源；存檔就換上、立刻跑一輪（鎖定期間等解凍）。每台近況看選單的「時間來源」
servers = ["time.stdtime.gov.tw", "time.google.com", "time.cloudflare.com"]  # 1 到 8 台
burst = 4              # 每台每輪幾筆（1 到 8）
spacing_ms = 1000      # 同一台兩筆的間隔（≥ 200）
cycle_s = 180          # 兩輪之間的間隔（≥ 60）
"##;

#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(try_from = "String")]
pub struct Color {
    pub r: f64,
    pub g: f64,
    pub b: f64,
    pub a: f64,
}

impl Color {
    pub const fn rgba(r: f64, g: f64, b: f64, a: f64) -> Color {
        Color { r, g, b, a }
    }

    pub fn nscolor(&self) -> Retained<NSColor> {
        NSColor::colorWithSRGBRed_green_blue_alpha(self.r, self.g, self.b, self.a)
    }
}

impl TryFrom<String> for Color {
    type Error = String;

    fn try_from(s: String) -> Result<Color, String> {
        let hex = s
            .strip_prefix('#')
            .ok_or_else(|| format!("顏色 {s:?} 要以 # 開頭"))?;
        if hex.len() != 6 && hex.len() != 8 {
            return Err(format!("顏色 {s:?} 要是 #RRGGBB 或 #RRGGBBAA"));
        }
        let byte = |i: usize| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map(|v| v as f64 / 255.0)
                .map_err(|_| format!("顏色 {s:?} 含非十六進位字元"))
        };
        Ok(Color {
            r: byte(0)?,
            g: byte(2)?,
            b: byte(4)?,
            a: if hex.len() == 8 { byte(6)? } else { 1.0 },
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Material {
    Hud,
    Popover,
    Sidebar,
    Menu,
    UnderWindow,
    WindowBackground,
}

impl Material {
    pub fn ns(&self) -> NSVisualEffectMaterial {
        match self {
            Material::Hud => NSVisualEffectMaterial::HUDWindow,
            Material::Popover => NSVisualEffectMaterial::Popover,
            Material::Sidebar => NSVisualEffectMaterial::Sidebar,
            Material::Menu => NSVisualEffectMaterial::Menu,
            Material::UnderWindow => NSVisualEffectMaterial::UnderWindowBackground,
            Material::WindowBackground => NSVisualEffectMaterial::WindowBackground,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    Dark,
    Light,
    System,
}

impl Appearance {
    /// `None` 代表跟著系統。
    pub fn ns(&self) -> Option<Retained<NSAppearance>> {
        // SAFETY: AppKit 的公開常數，程式生命週期內不變。
        let name = unsafe {
            match self {
                Appearance::Dark => NSAppearanceNameVibrantDark,
                Appearance::Light => NSAppearanceNameVibrantLight,
                Appearance::System => return None,
            }
        };
        NSAppearance::appearanceNamed(name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Weight {
    Regular,
    Medium,
    Semibold,
    Bold,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Panel {
    pub width: f64,
    pub corner_radius: f64,
    pub material: Material,
    pub appearance: Appearance,
    pub opacity: f64,
}

impl Default for Panel {
    fn default() -> Self {
        Panel {
            width: 320.0,
            corner_radius: 14.0,
            material: Material::Hud,
            appearance: Appearance::Dark,
            opacity: 1.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Font {
    pub family: String,
    pub weight: Weight,
    pub time_size: f64,
    pub detail_size: f64,
    pub caption_size: f64,
}

impl Default for Font {
    fn default() -> Self {
        Font {
            family: String::new(),
            weight: Weight::Medium,
            time_size: 34.0,
            detail_size: 13.0,
            caption_size: 11.0,
        }
    }
}

impl Font {
    /// 指定字級的 `NSFont`。空的 family 或找不到的字型都退回系統等寬數字字型。
    pub fn nsfont(&self, size: f64) -> Retained<NSFont> {
        // SAFETY: AppKit 的公開常數，程式生命週期內不變。
        let weight = unsafe {
            match self.weight {
                Weight::Regular => NSFontWeightRegular,
                Weight::Medium => NSFontWeightMedium,
                Weight::Semibold => NSFontWeightSemibold,
                Weight::Bold => NSFontWeightBold,
            }
        };
        if !self.family.is_empty() {
            if let Some(f) = NSFont::fontWithName_size(&NSString::from_str(&self.family), size) {
                return f;
            }
            eprintln!("主題：找不到字型 {:?}，改用系統字型", self.family);
        }
        NSFont::monospacedDigitSystemFontOfSize_weight(size, weight)
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Colors {
    pub time: Color,
    pub detail: Color,
    pub caption: Color,
    pub bar: Color,
    pub bar_track: Color,
}

impl Default for Colors {
    fn default() -> Self {
        Colors {
            time: Color::rgba(1.0, 1.0, 1.0, 1.0),
            detail: Color::rgba(1.0, 1.0, 1.0, 0xD0 as f64 / 255.0),
            caption: Color::rgba(1.0, 1.0, 1.0, 0x80 as f64 / 255.0),
            bar: Color::rgba(0x5A as f64 / 255.0, 0xA9 as f64 / 255.0, 1.0, 1.0),
            bar_track: Color::rgba(1.0, 1.0, 1.0, 0x26 as f64 / 255.0),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Layout {
    pub padding: f64,
    pub line_gap: f64,
    pub bar_height: f64,
    pub bar_full_scale_ms: f64,
}

impl Default for Layout {
    fn default() -> Self {
        Layout {
            padding: 16.0,
            line_gap: 6.0,
            bar_height: 4.0,
            bar_full_scale_ms: 50.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Display {
    pub lead_ms: f64,
    pub idle_fps: f64,
    /// 依螢幕名稱（`NSScreen.localizedName`）覆寫 `lead_ms`。
    pub screens: BTreeMap<String, f64>,
}

impl Default for Display {
    fn default() -> Self {
        Display {
            lead_ms: 0.0,
            idle_fps: 60.0,
            screens: BTreeMap::new(),
        }
    }
}

impl Display {
    /// 這個螢幕的顯示提前量：有覆寫就用覆寫，否則用預設值。
    pub fn lead_ms_for(&self, screen: Option<&str>) -> f64 {
        screen
            .and_then(|n| self.screens.get(n))
            .copied()
            .unwrap_or(self.lead_ms)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeatStyle {
    Auto,
    Ball,
    Ring,
    Pulse,
}

/// 節拍區用什麼畫：`auto` 有 Metal 就用（順便量實際上屏時刻），`metal` 一樣但拿不到會大聲抱怨，
/// `layer` 維持 CALayer。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeatRenderer {
    Auto,
    Metal,
    Layer,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Beat {
    pub style: BeatStyle,
    pub renderer: BeatRenderer,
    pub period_ms: f64,
    pub ticks: u32,
    pub strip_height: f64,
    pub color: Color,
    pub final_color: Color,
    pub glow: bool,
    pub glow_width: f64,
    pub glow_opacity: f64,
    pub volume: f64,
    pub tick_hz: f64,
    pub tick_ms: f64,
    pub final_ms: f64,
    pub visual_lead_ms: f64,
}

impl Default for Beat {
    fn default() -> Self {
        Beat {
            style: BeatStyle::Auto,
            renderer: BeatRenderer::Auto,
            period_ms: 1000.0,
            ticks: 4,
            strip_height: 64.0,
            color: Color::rgba(0x5A as f64 / 255.0, 0xA9 as f64 / 255.0, 1.0, 1.0),
            final_color: Color::rgba(1.0, 0xB4 as f64 / 255.0, 0x54 as f64 / 255.0, 1.0),
            glow: true,
            glow_width: 48.0,
            glow_opacity: 0.35,
            volume: 0.5,
            tick_hz: 1000.0,
            tick_ms: 30.0,
            final_ms: 200.0,
            visual_lead_ms: 0.0,
        }
    }
}

impl Beat {
    pub fn period(&self) -> Duration {
        Duration::from_secs_f64(self.period_ms.max(1.0) / 1e3)
    }

    pub fn sound(&self) -> TickSound {
        TickSound {
            hz: self.tick_hz.clamp(20.0, 20_000.0),
            tick: Duration::from_secs_f64(self.tick_ms.max(1.0) / 1e3),
            final_tick: Duration::from_secs_f64(self.final_ms.max(1.0) / 1e3),
            volume: self.volume.clamp(0.0, 1.0) as f32,
        }
    }

    /// 畫面相對聲音的提前量，可負。
    pub fn visual_lead(&self) -> (bool, Duration) {
        (
            self.visual_lead_ms >= 0.0,
            Duration::from_secs_f64(self.visual_lead_ms.abs() / 1e3),
        )
    }
}

/// `[sync]`：伺服器清單與取樣節奏。驗證在 core 的 `SyncSettings::new`。
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sync {
    pub servers: Vec<String>,
    pub burst: usize,
    pub spacing_ms: f64,
    pub cycle_s: f64,
}

impl Default for Sync {
    fn default() -> Self {
        let c = SamplerConfig::default();
        Sync {
            servers: c.servers,
            burst: c.burst,
            spacing_ms: c.spacing.as_secs_f64() * 1e3,
            cycle_s: c.cycle.as_secs_f64(),
        }
    }
}

impl Sync {
    pub fn settings(&self) -> Result<SyncSettings, String> {
        SyncSettings::new(
            self.servers.clone(),
            self.burst,
            Duration::from_secs_f64(self.spacing_ms.max(0.0) / 1e3),
            Duration::from_secs_f64(self.cycle_s.max(0.0)),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    pub panel: Panel,
    pub font: Font,
    pub colors: Colors,
    pub layout: Layout,
    pub display: Display,
    pub beat: Beat,
    pub sync: Sync,
}

impl Theme {
    pub fn parse(text: &str) -> Result<Theme, toml::de::Error> {
        toml::from_str(text)
    }

    /// `~/Library/Application Support/kairos/theme.toml`。
    pub fn default_path() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        home.join("Library/Application Support/kairos/theme.toml")
    }

    /// 讀主題檔；不存在就先寫入預設內容再讀。
    pub fn load_or_create(path: &Path) -> io::Result<Theme> {
        if !path.exists() {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::write(path, DEFAULT_THEME_TOML)?;
            eprintln!("主題：已建立 {}", path.display());
        }
        let text = fs::read_to_string(path)?;
        for section in ["[display.screens]", "[beat]", "[sync]"] {
            if !text.contains(section) {
                eprintln!(
                    "主題：{} 沒有 {section} 段落，用預設值；想看所有可調的鍵，刪掉這個檔案重啟就會重新產生",
                    path.display()
                );
            }
        }
        Theme::parse(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }
}

/// 監看主題檔所在的資料夾（編輯器多半是寫暫存檔再改名，監看檔案本身會漏）。
/// 任何變動只設一個旗標，由主執行緒在下一格看到後讀檔。
pub fn watch(
    path: &Path,
    on_change: impl Fn() + Send + 'static,
) -> notify::Result<RecommendedWatcher> {
    let dir = path.parent().unwrap_or(path).to_path_buf();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            on_change();
        }
    })?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_parses_to_default_theme() {
        let parsed = Theme::parse(DEFAULT_THEME_TOML).expect("預設主題檔要能解析");
        assert_eq!(parsed, Theme::default());
    }

    #[test]
    fn colors_parse_with_and_without_alpha() {
        let c = Color::try_from("#5AA9FF".to_string()).unwrap();
        assert!((c.r - 0x5A as f64 / 255.0).abs() < 1e-9);
        assert_eq!(c.a, 1.0);
        let c = Color::try_from("#00000080".to_string()).unwrap();
        assert!((c.a - 128.0 / 255.0).abs() < 1e-9);
        assert!(Color::try_from("5AA9FF".to_string()).is_err());
        assert!(Color::try_from("#12345".to_string()).is_err());
        assert!(Color::try_from("#GG0000".to_string()).is_err());
    }

    #[test]
    fn partial_file_fills_defaults_and_typos_are_errors() {
        let t = Theme::parse("[font]\ntime_size = 40\n").unwrap();
        assert_eq!(t.font.time_size, 40.0);
        assert_eq!(t.panel, Panel::default());
        assert!(Theme::parse("[font]\ntime_sizee = 40\n").is_err());
        assert!(Theme::parse("[panel]\nmaterial = \"glass\"\n").is_err());
    }

    #[test]
    fn screen_overrides_and_beat_section() {
        let t = Theme::parse(
            "[display]\nlead_ms = 3\n[display.screens]\n\"DELL P2421D\" = 8\n[beat]\nstyle = \"ring\"\nvisual_lead_ms = -5\n",
        )
        .unwrap();
        assert_eq!(t.display.lead_ms_for(Some("DELL P2421D")), 8.0);
        assert_eq!(t.display.lead_ms_for(Some("Built-in")), 3.0);
        assert_eq!(t.display.lead_ms_for(None), 3.0);
        assert_eq!(t.beat.style, BeatStyle::Ring);
        assert_eq!(t.beat.visual_lead(), (false, Duration::from_millis(5)));
        assert_eq!(t.beat.sound().tick, Duration::from_millis(30));
        assert!(Theme::parse("[beat]\nstyle = \"bounce\"\n").is_err());
        assert_eq!(
            Theme::parse("[beat]\nrenderer = \"layer\"\n")
                .unwrap()
                .beat
                .renderer,
            BeatRenderer::Layer
        );
        assert!(Theme::parse("[beat]\nrenderer = \"vulkan\"\n").is_err());
    }

    #[test]
    fn sync_section_maps_to_settings_and_reports_bad_values() {
        let t = Theme::parse(
            "[sync]\nservers = [\"tock.stdtime.gov.tw\", \"time.apple.com\"]\nburst = 2\nspacing_ms = 500\ncycle_s = 120\n",
        )
        .unwrap();
        let s = t.sync.settings().unwrap();
        assert_eq!(s.servers, vec!["tock.stdtime.gov.tw", "time.apple.com"]);
        assert_eq!(s.burst, 2);
        assert_eq!(s.spacing, Duration::from_millis(500));
        assert_eq!(s.cycle, Duration::from_secs(120));

        let defaults = Theme::default().sync.settings().unwrap();
        assert_eq!(
            defaults,
            SyncSettings::from_config(&SamplerConfig::default())
        );

        let bad = Theme::parse("[sync]\nservers = []\n").unwrap();
        assert!(bad.sync.settings().is_err());
        let bad = Theme::parse("[sync]\ncycle_s = 5\n").unwrap();
        assert!(bad.sync.settings().unwrap_err().contains("兩輪"));
        assert!(Theme::parse("[sync]\nserver = [\"a\"]\n").is_err());
    }
}
