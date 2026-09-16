//! 字形圖集：把 `0`–`9`、`:`、`.`、`-` 各自渲染成一張點陣圖，解析度依 `backingScaleFactor`。
//!
//! 面板上每個數字位置是一個 `CALayer`，數字變了只換 `contents`，不重新排版文字。
//! 所有數字共用同一個格寬（取最寬的那個），任何字型都不會因字寬不同而晃動；
//! 每一格的工作量固定，跟字型無關。
//!
//! 渲染走 AppKit 的字串繪製（底層就是 Core Text）畫進 `NSBitmapImageRep`，
//! 再包成 `NSImage`、用 `layerContentsForContentsScale:` 取得給圖層用的內容，
//! 省掉 Core Graphics 位圖上下文與 Core Foundation 的橋接。

use std::collections::HashMap;
use std::ptr;

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSBitmapImageRep, NSCalibratedRGBColorSpace, NSColor,
    NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSGraphicsContext, NSImage,
};
use objc2_core_foundation::{CGPoint, CGSize};
use objc2_foundation::{NSMutableAttributedString, NSRange, NSString};

/// 圖集裡有的字元。
pub const GLYPHS: &[u8] = b"0123456789:.-";

pub struct GlyphAtlas {
    /// 以下都是點，不是像素。
    pub digit_width: f64,
    pub colon_width: f64,
    pub dot_width: f64,
    pub cell_height: f64,
    contents: HashMap<u8, Retained<AnyObject>>,
}

impl GlyphAtlas {
    /// `font_px` 是已經乘過 `scale` 的字型（例如 34 pt 在 2x 螢幕傳 68）。
    pub fn render(font_px: &NSFont, color: &NSColor, scale: f64) -> GlyphAtlas {
        // 先量，再決定格子大小。
        let mut advance_px = HashMap::new();
        let mut cell_h_px: f64 = 0.0;
        let mut digit_w_px: f64 = 0.0;
        for &ch in GLYPHS {
            let size = attributed(ch, font_px, color).size();
            advance_px.insert(ch, size.width);
            cell_h_px = cell_h_px.max(size.height);
            if ch.is_ascii_digit() {
                digit_w_px = digit_w_px.max(size.width);
            }
        }
        let cell_h_px = cell_h_px.ceil();
        let digit_w_px = digit_w_px.ceil();
        let colon_w_px = advance_px[&b':'].ceil();
        let dot_w_px = advance_px[&b'.'].ceil();

        let mut contents = HashMap::new();
        for &ch in GLYPHS {
            let w_px = match ch {
                b':' => colon_w_px,
                b'.' => dot_w_px,
                _ => digit_w_px,
            };
            let image = render_glyph(ch, font_px, color, w_px, cell_h_px, advance_px[&ch], scale);
            contents.insert(ch, image);
        }

        GlyphAtlas {
            digit_width: digit_w_px / scale,
            colon_width: colon_w_px / scale,
            dot_width: dot_w_px / scale,
            cell_height: cell_h_px / scale,
            contents,
        }
    }

    /// 某個字元的格寬（點）。
    pub fn width_of(&self, ch: u8) -> f64 {
        match ch {
            b':' => self.colon_width,
            b'.' => self.dot_width,
            _ => self.digit_width,
        }
    }

    /// 給 `CALayer.contents` 用的物件。不在圖集裡的字元退回 `-`。
    pub fn contents(&self, ch: u8) -> &AnyObject {
        self.contents
            .get(&ch)
            .or_else(|| self.contents.get(&b'-'))
            .expect("圖集至少有 -")
    }
}

fn attributed(ch: u8, font: &NSFont, color: &NSColor) -> Retained<NSMutableAttributedString> {
    let text = NSString::from_str(std::str::from_utf8(std::slice::from_ref(&ch)).unwrap());
    let a = NSMutableAttributedString::initWithString(NSMutableAttributedString::alloc(), &text);
    let range = NSRange::new(0, a.length());
    // SAFETY: 屬性名是 AppKit 的公開常數，值的型別正確（NSFont、NSColor）。
    unsafe {
        a.addAttribute_value_range(NSFontAttributeName, font, range);
        a.addAttribute_value_range(NSForegroundColorAttributeName, color, range);
    }
    a
}

/// 把一個字元畫進 `w_px × h_px` 的透明點陣圖，水平置中、基線貼底（非翻轉座標）。
fn render_glyph(
    ch: u8,
    font_px: &NSFont,
    color: &NSColor,
    w_px: f64,
    h_px: f64,
    advance_px: f64,
    scale: f64,
) -> Retained<AnyObject> {
    let (w, h) = (w_px as isize, h_px as isize);
    // SAFETY: planes 傳 null 讓 AppKit 自己配記憶體；其餘參數是標準的 RGBA8 非平面格式。
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            ptr::null_mut(),
            w,
            h,
            8,
            4,
            true,
            false,
            NSCalibratedRGBColorSpace,
            0,
            0,
        )
    }
    .expect("建立點陣圖失敗");
    // 剛配的記憶體不保證是零，先清成透明。
    // SAFETY: bitmapData 指向 bytesPerRow × h 位元組的可寫記憶體。
    unsafe {
        let bytes = rep.bytesPerRow() as usize * h as usize;
        ptr::write_bytes(rep.bitmapData(), 0, bytes);
    }

    let ctx =
        NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep).expect("建立繪圖上下文失敗");
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&ctx));
    attributed(ch, font_px, color)
        .drawAtPoint(CGPoint::new(((w_px - advance_px) / 2.0).floor(), 0.0));
    NSGraphicsContext::restoreGraphicsState_class();

    let size_pt = CGSize::new(w_px / scale, h_px / scale);
    rep.setSize(size_pt);
    let image = NSImage::initWithSize(NSImage::alloc(), size_pt);
    image.addRepresentation(&rep);
    image.layerContentsForContentsScale(scale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_app_kit::NSFontWeightMedium;

    fn test_font(size: f64) -> Retained<NSFont> {
        // SAFETY: AppKit 的公開常數。
        NSFont::monospacedDigitSystemFontOfSize_weight(size, unsafe { NSFontWeightMedium })
    }

    /// 渲染一個數字，讀回像素確認：不是空白、垂直方向沒有被裁掉（上下都有透明邊）。
    fn alpha_rows(ch: u8, scale: f64) -> Vec<bool> {
        let font = test_font(34.0 * scale);
        let color = NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 1.0);
        let a = attributed(ch, &font, &color);
        let size = a.size();
        let (w_px, h_px) = (size.width.ceil(), size.height.ceil());
        let (w, h) = (w_px as isize, h_px as isize);
        let rep = unsafe {
            NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(), ptr::null_mut(), w, h, 8, 4, true, false, NSCalibratedRGBColorSpace, 0, 0,
            )
        }
        .unwrap();
        unsafe { ptr::write_bytes(rep.bitmapData(), 0, rep.bytesPerRow() as usize * h as usize) };
        let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep).unwrap();
        NSGraphicsContext::saveGraphicsState_class();
        NSGraphicsContext::setCurrentContext(Some(&ctx));
        a.drawAtPoint(CGPoint::new(0.0, 0.0));
        NSGraphicsContext::restoreGraphicsState_class();

        let stride = rep.bytesPerRow() as usize;
        let data = unsafe { std::slice::from_raw_parts(rep.bitmapData(), stride * h as usize) };
        (0..h as usize)
            .map(|row| (0..w as usize).any(|x| data[row * stride + x * 4 + 3] != 0))
            .collect()
    }

    #[test]
    fn glyph_lands_inside_its_cell() {
        for scale in [1.0, 2.0] {
            let rows = alpha_rows(b'8', scale);
            let inked: Vec<usize> = rows
                .iter()
                .enumerate()
                .filter(|(_, r)| **r)
                .map(|(i, _)| i)
                .collect();
            assert!(!inked.is_empty(), "scale {scale}：畫出來是空白");
            let (first, last) = (inked[0], *inked.last().unwrap());
            // 字形要離上下邊都有距離，代表沒被裁掉、也沒畫錯方向。
            assert!(
                first > 0 && last < rows.len() - 1,
                "scale {scale}：字形貼邊 {first}..{last} / {}",
                rows.len()
            );
            // 字形高度應占格高的一半以上（34 pt 字級的數字約 24 pt 高）。
            assert!(
                (last - first) as f64 > rows.len() as f64 * 0.5,
                "scale {scale}：字形太矮"
            );
        }
    }

    #[test]
    fn atlas_has_every_glyph_with_uniform_digit_width() {
        let font = test_font(34.0 * 2.0);
        let color = NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 1.0);
        let atlas = GlyphAtlas::render(&font, &color, 2.0);
        assert!(atlas.digit_width > 0.0 && atlas.cell_height > 0.0);
        assert!(atlas.colon_width <= atlas.digit_width);
        for &ch in GLYPHS {
            let _ = atlas.contents(ch);
        }
        for ch in b"0123456789-" {
            assert_eq!(atlas.width_of(*ch), atlas.digit_width);
        }
    }
}
