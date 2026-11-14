//! The tray's battery display, ported from OpenMouse Bridge
//! (`src/tray_battery.rs` and `src/platform/windows.rs`, MIT): which colour the
//! battery is drawn in, the tooltip, and the icon itself. Windows draws the
//! Segoe Fluent battery glyph (or the percentage) through GDI; other desktops
//! get Bridge's simple outline battery.

use anyhow::Result;

const ACCENT: (u8, u8, u8) = (93, 222, 137);
const WARNING: (u8, u8, u8) = (242, 184, 75);
const DANGER: (u8, u8, u8) = (255, 107, 107);
const MUTED: (u8, u8, u8) = (170, 176, 183);

/// Windows' tooltip buffer tops out at 128 UTF-16 units.
const TOOLTIP_CHARS: usize = 120;

/// What fills the square tray icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayBatteryStyle {
    /// The native Segoe battery glyph.
    Glyph,
    /// The percentage number in bold text.
    Percentage,
}

/// One battery reading as the tray shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayBattery {
    pub device_name: String,
    pub percent: u8,
    pub charging: bool,
    pub style: TrayBatteryStyle,
}

impl TrayBattery {
    pub fn ink(&self, threshold: u8) -> (u8, u8, u8) {
        ink(self.percent, self.charging, false, threshold)
    }
}

pub fn ink(percent: u8, charging: bool, stale: bool, threshold: u8) -> (u8, u8, u8) {
    if stale {
        MUTED
    } else if charging {
        ACCENT
    } else if percent <= threshold {
        DANGER
    } else if percent <= 50 {
        WARNING
    } else {
        ACCENT
    }
}

/// `Device — 87% · Charging`.
pub fn tooltip(battery: &TrayBattery) -> String {
    let status = if battery.charging { " · Charging" } else { "" };
    let text = format!("{} — {}%{status}", battery.device_name, battery.percent);
    text.chars().take(TOOLTIP_CHARS).collect()
}

/// The icon's side in physical pixels.
pub fn icon_size() -> u32 {
    #[cfg(windows)]
    {
        windows::tray_icon_size()
    }
    #[cfg(not(windows))]
    {
        44
    }
}

/// The battery display as a square RGBA buffer tinted with `ink`.
pub fn icon_rgba(battery: &TrayBattery, ink: (u8, u8, u8)) -> Result<(Vec<u8>, u32, u32)> {
    #[cfg(windows)]
    {
        windows::battery_icon_rgba(battery.percent, battery.charging, battery.style, icon_size(), ink)
    }
    #[cfg(not(windows))]
    {
        outline_battery_rgba(battery.percent, battery.charging, icon_size(), ink)
    }
}

/// A bold outline battery with the charge fill, no anti-aliasing.
#[cfg(not(windows))]
fn outline_battery_rgba(percent: u8, charging: bool, size: u32, ink: (u8, u8, u8)) -> Result<(Vec<u8>, u32, u32)> {
    let size = size.max(8);
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let mut put = |x0: u32, y0: u32, x1: u32, y1: u32| {
        for y in y0..y1.min(size) {
            for x in x0..x1.min(size) {
                let index = ((y * size + x) * 4) as usize;
                rgba[index] = ink.0;
                rgba[index + 1] = ink.1;
                rgba[index + 2] = ink.2;
                rgba[index + 3] = u8::MAX;
            }
        }
    };
    let stroke = (size / 10).max(1);
    let body_height = size * 2 / 3;
    let body_top = (size - body_height) / 2;
    let body_bottom = body_top + body_height;
    let nub_width = (size / 16).max(1);
    let body_width = size.saturating_sub(nub_width + 2);
    put(0, body_top, body_width, body_top + stroke);
    put(0, body_bottom - stroke, body_width, body_bottom);
    put(0, body_top, stroke, body_bottom);
    put(body_width - stroke, body_top, body_width, body_bottom);
    put(body_width + 1, size / 3, body_width + 1 + nub_width, size * 2 / 3);
    let fill = if charging { 100 } else { u32::from(percent.min(100)) };
    let inner_x0 = stroke;
    let inner_x1 = body_width - stroke;
    let fill_columns = fill * (inner_x1 - inner_x0) / 100;
    put(inner_x0, body_top + stroke, inner_x0 + fill_columns, body_bottom - stroke);
    Ok((rgba, size, size))
}

#[cfg(windows)]
mod windows {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt, ptr::null_mut};

    use anyhow::{anyhow, Result};
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateFontW, DeleteDC, DeleteObject, GetGlyphOutlineW, SelectObject,
        ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, FF_DONTCARE,
        FIXED, FW_BOLD, FW_NORMAL, GDI_ERROR, GGO_GRAY8_BITMAP, GGO_METRICS, GLYPHMETRICS, HDC,
        HFONT, MAT2, OUT_TT_PRECIS,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};

    use super::TrayBatteryStyle;

    /// The notification area's icon size in physical pixels at the current
    /// system DPI: 16 at 100% scaling, 20/24/32 as the display scaling rises.
    pub fn tray_icon_size() -> u32 {
        // SAFETY: a trivial metrics query with no side effects.
        let width = unsafe { GetSystemMetrics(SM_CXSMICON) };
        width.max(16) as u32
    }

    fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
        value.as_ref().encode_wide().chain(Some(0)).collect()
    }

    /// Segoe icon-font code points for the battery glyphs, verified against
    /// Microsoft's Segoe Fluent Icons documentation: `Battery0`..`Battery9`
    /// (E850..E859) cover 0–90% in 10% steps, `Battery10` (E83F) is full, and
    /// every level below full has a charging variant carrying the bolt
    /// (`BatteryCharging0`..8 at E85A..E862, `BatteryCharging9` at E83E).
    pub fn battery_glyph(percent: u8, charging: bool) -> u32 {
        let level = u32::from(percent.min(100)) / 10;
        if !charging {
            return if level == 10 { 0xE83F } else { 0xE850 + level };
        }
        match level {
            0..=8 => 0xE85A + level,
            9 => 0xE83E,
            // The font has no charging glyph for a full battery; the filled
            // Battery10 is what Windows itself falls back to.
            _ => 0xE83F,
        }
    }

    /// Renders the tray battery display as a square `size` × `size` RGBA
    /// buffer, tinted with `ink` — the Windows 11 taskbar only renders square
    /// tray icons faithfully. The `style` picks what fills the square: the
    /// Segoe Fluent battery glyph for the level (the charging variant with the
    /// bolt when `charging`), or the percentage number in bold UI text (the
    /// `%` is left off — at tray sizes it would shrink the digits to a
    /// squint; the tooltip spells the full `NN%` out). Returns the buffer with
    /// its width and height. `Segoe Fluent Icons` ships with Windows 11;
    /// `Segoe MDL2 Assets` carries the same glyphs on Windows 10.
    pub fn battery_icon_rgba(
        percent: u8,
        charging: bool,
        style: TrayBatteryStyle,
        height: u32,
        ink: (u8, u8, u8),
    ) -> Result<(Vec<u8>, u32, u32)> {
        let codepoint = battery_glyph(percent, charging);
        // The label carries only the digits for the percentage style; the
        // glyph style never rasterizes text.
        let label: Vec<u16> = match style {
            TrayBatteryStyle::Percentage => {
                percent.min(100).to_string().encode_utf16().collect()
            }
            TrayBatteryStyle::Glyph => Vec::new(),
        };
        for family in ["Segoe Fluent Icons", "Segoe MDL2 Assets"] {
            match render_battery_icon(codepoint, &label, family, height, ink) {
                Ok(icon) => return Ok(icon),
                // A missing family or glyph means the fallback font should be
                // tried; anything else is a real error.
                Err(error) if MISSING_GLYPH.with(|flag| flag.get()) => {
                    let _ = error;
                    MISSING_GLYPH.with(|flag| flag.set(false));
                }
                Err(error) => return Err(error),
            }
        }
        Err(anyhow!(
            "no Segoe icon font could render the battery display"
        ))
    }

    thread_local! {
        /// Set by the renderer when the font family or glyph is missing, telling
        /// `battery_icon_rgba` to fall through to the next font family.
        static MISSING_GLYPH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// One rasterized glyph: GDI's 8-bit coverage bitmap plus the metrics that
    /// place it.
    struct Raster {
        metrics: GLYPHMETRICS,
        coverage: Vec<u8>,
    }

    impl Raster {
        fn width(&self) -> u32 {
            self.metrics.gmBlackBoxX
        }

        fn height(&self) -> u32 {
            self.metrics.gmBlackBoxY
        }

        /// The bitmap's bytes per row (DWORD-aligned, as GDI reports).
        fn stride(&self) -> usize {
            ((self.metrics.gmBlackBoxX + 3) & !3) as usize
        }

        /// The coverage compacted to tight rows (no DWORD padding).
        fn tight(&self) -> Vec<u8> {
            let stride = self.stride();
            let width = self.width() as usize;
            let mut out = Vec::with_capacity(width * self.height() as usize);
            for row in 0..self.height() as usize {
                out.extend_from_slice(&self.coverage[row * stride..row * stride + width]);
            }
            out
        }
    }

    /// The label's text face: the system UI font in bold, so the percentage
    /// reads clearly at tray sizes.
    const TEXT_FAMILY: &str = "Segoe UI";

    fn render_battery_icon(
        codepoint: u32,
        label: &[u16],
        family: &str,
        height: u32,
        ink: (u8, u8, u8),
    ) -> Result<(Vec<u8>, u32, u32)> {
        MISSING_GLYPH.with(|flag| flag.set(false));
        let family = wide(family);
        let text_family = wide(TEXT_FAMILY);
        // SAFETY: the GDI calls below use their own memory DC, every handle is
        // deleted on the way out, and the coverage buffers are sized to what
        // `GetGlyphOutlineW` reports.
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
            if dc.is_null() {
                return Err(anyhow!("could not create a memory device context"));
            }
            let result =
                compose_battery_display(dc, codepoint, label, &family, &text_family, height, ink);
            DeleteDC(dc);
            result
        }
    }

    fn compose_battery_display(
        dc: HDC,
        codepoint: u32,
        label: &[u16],
        family: &[u16],
        text_family: &[u16],
        height: u32,
        ink: (u8, u8, u8),
    ) -> Result<(Vec<u8>, u32, u32)> {
        // An empty label is the glyph style: the battery fills the square.
        // Otherwise the digits fill it.
        if label.is_empty() {
            // A landscape glyph in a square icon: fitting its width to all but
            // 1px of the canvas — the footprint the old square renderer used —
            // is the largest it can be, and the height lands near half.
            let battery = fit_and_rasterize(
                dc,
                codepoint,
                family,
                FW_NORMAL as i32,
                height.saturating_sub(1).max(8),
            )
            .ok_or_else(|| anyhow!("could not rasterize the battery glyph U+{codepoint:04X}"))?;
            let mut canvas = Canvas {
                rgba: vec![0u8; (height * height * 4) as usize],
                width: height,
                height,
            };
            canvas.blit(
                (height - battery.width()) / 2,
                (height - battery.height()) / 2,
                &battery.tight(),
                battery.width(),
                ink,
            );
            return Ok((canvas.rgba, height, height));
        }

        // The digits are drawn as tall as the square allows — a 2px margin —
        // and squeezed proportionally when the string is wider than tall
        // ("100" is, at every size that matters).
        let mut target = height.saturating_sub(4).max(8);
        let mut raster = rasterize_label(dc, label, text_family, target)
            .ok_or_else(|| anyhow!("could not rasterize the battery label"))?;
        if raster.width > height.saturating_sub(2) {
            target =
                (f64::from(target) * f64::from(height - 2) / f64::from(raster.width)).max(6.0) as u32;
            raster = rasterize_label(dc, label, text_family, target)
                .ok_or_else(|| anyhow!("could not re-fit the battery label"))?;
        }
        let mut canvas = Canvas {
            rgba: vec![0u8; (height * height * 4) as usize],
            width: height,
            height,
        };
        canvas.blit(
            (height - raster.width) / 2,
            (height - raster.height) / 2,
            &raster.coverage,
            raster.width,
            ink,
        );
        Ok((canvas.rgba, height, height))
    }

    /// A laid-out label: one tight coverage bitmap for the whole string.
    struct LabelRaster {
        coverage: Vec<u8>,
        width: u32,
        height: u32,
    }

    /// Rasterizes the label into one tight coverage bitmap in a bold UI face,
    /// sized so a digit stands `target` pixels tall. Characters are placed by
    /// the font's own advances and side bearings, top-aligned — digits and
    /// `%` share the cap height, so the row lines up.
    fn rasterize_label(dc: HDC, label: &[u16], family: &[u16], target: u32) -> Option<LabelRaster> {
        if label.is_empty() {
            return None;
        }
        // SAFETY: `dc` is a live memory DC and the font created here is
        // deselected and deleted on every exit path.
        unsafe {
            let mut font = create_glyph_font(family, target as i32, FW_BOLD as i32);
            if font.is_null() {
                return None;
            }
            let previous_font = SelectObject(dc, font);
            let mut font_height = target as i32;
            for _ in 0..3 {
                let Some(metrics) = glyph_metrics(dc, u32::from(b'0')) else {
                    break;
                };
                let size = metrics.gmBlackBoxY;
                if size == 0 || size.abs_diff(target) <= 1 {
                    break;
                }
                let scaled =
                    (f64::from(font_height) * f64::from(target) / f64::from(size)).round() as i32;
                if scaled == font_height {
                    break;
                }
                font_height = scaled;
                let replacement = create_glyph_font(family, font_height, FW_BOLD as i32);
                SelectObject(dc, replacement);
                DeleteObject(font);
                font = replacement;
                if font.is_null() {
                    SelectObject(dc, previous_font);
                    return None;
                }
            }

            let mut rasters = Vec::with_capacity(label.len());
            for &character in label {
                match glyph_metrics(dc, u32::from(character)) {
                    // `glyph_bitmap` writes the bitmap's real bounds back into
                    // the metrics, and the raster needs those.
                    Some(mut metrics) => match glyph_bitmap(dc, u32::from(character), &mut metrics) {
                        Some(coverage) => rasters.push(Raster { metrics, coverage }),
                        None => break,
                    },
                    None => break,
                }
            }
            SelectObject(dc, previous_font);
            DeleteObject(font);
            if rasters.len() != label.len() {
                return None;
            }

            let height = rasters.iter().map(Raster::height).max().unwrap_or(0);
            let width: u32 = rasters
                .iter()
                .map(|r| r.metrics.gmCellIncX.max(0) as u32)
                .sum();
            let mut coverage = vec![0u8; (width * height) as usize];
            let mut pen_x = 0u32;
            for raster in &rasters {
                let tight = raster.tight();
                let x = pen_x + u32::try_from(raster.metrics.gmptGlyphOrigin.x.max(0)).unwrap_or(0);
                for row in 0..raster.height() as usize {
                    for column in 0..raster.width() as usize {
                        let level = tight[row * raster.width() as usize + column];
                        if level == 0 {
                            continue;
                        }
                        let index = row * width as usize + x as usize + column;
                        if index < coverage.len() {
                            coverage[index] = coverage[index].max(level);
                        }
                    }
                }
                pen_x += raster.metrics.gmCellIncX.max(0) as u32;
            }
            Some(LabelRaster {
                coverage,
                width,
                height,
            })
        }
    }

    /// The RGBA canvas the display is composed onto.
    struct Canvas {
        rgba: Vec<u8>,
        width: u32,
        height: u32,
    }

    impl Canvas {
        /// Stamps a tight coverage bitmap at (x0, y0), tinted with `ink`.
        /// Coverage levels are normalized so the raster's densest pixel is
        /// fully opaque (`GGO_GRAY8_BITMAP` does not promise a 255 peak).
        fn blit(&mut self, x0: u32, y0: u32, coverage: &[u8], width: u32, ink: (u8, u8, u8)) {
            let peak = coverage.iter().copied().max().unwrap_or(0);
            if peak == 0 {
                return;
            }
            let height = coverage.len() as u32 / width.max(1);
            for row in 0..height as usize {
                let y = y0 as usize + row;
                if y as u32 >= self.height {
                    break;
                }
                for column in 0..width as usize {
                    let x = x0 as usize + column;
                    if x as u32 >= self.width {
                        break;
                    }
                    let level = coverage[row * width as usize + column];
                    if level == 0 {
                        continue;
                    }
                    let alpha = ((u16::from(level) * 255) / u16::from(peak)).min(255) as u8;
                    let index = (y as u32 * self.width + x as u32) as usize * 4;
                    self.rgba[index] = ink.0;
                    self.rgba[index + 1] = ink.1;
                    self.rgba[index + 2] = ink.2;
                    self.rgba[index + 3] = alpha;
                }
            }
        }
    }

    /// Fits the font so the battery glyph's visible bounds span `target`
    /// pixels wide — the right fit for a landscape glyph in a square icon (the
    /// fit loop accepts within a pixel) — then rasterizes the glyph as an
    /// 8-bit coverage bitmap. `None` when the font or glyph is missing.
    fn fit_and_rasterize(
        dc: HDC,
        codepoint: u32,
        family: &[u16],
        weight: i32,
        target: u32,
    ) -> Option<Raster> {
        // SAFETY: `dc` is a live memory DC and the font created here is
        // deselected and deleted on every exit path.
        unsafe {
            let mut font = create_glyph_font(family, target as i32, weight);
            if font.is_null() {
                MISSING_GLYPH.with(|flag| flag.set(true));
                return None;
            }
            let previous_font = SelectObject(dc, font);
            let mut font_height = target as i32;
            let mut raster = None;
            for _ in 0..4 {
                let Some(mut metrics) = glyph_metrics(dc, codepoint) else {
                    MISSING_GLYPH.with(|flag| flag.set(true));
                    break;
                };
                let size = metrics.gmBlackBoxX;
                if size == 0 {
                    break;
                }
                if size.abs_diff(target) <= 1 {
                    raster = glyph_bitmap(dc, codepoint, &mut metrics)
                        .map(|coverage| Raster { metrics, coverage });
                    break;
                }
                let scaled =
                    (f64::from(font_height) * f64::from(target) / f64::from(size)).round() as i32;
                if scaled == font_height {
                    raster = glyph_bitmap(dc, codepoint, &mut metrics)
                        .map(|coverage| Raster { metrics, coverage });
                    break;
                }
                // Recreate the font at the scaled height and keep fitting.
                font_height = scaled;
                let replacement = create_glyph_font(family, font_height, weight);
                SelectObject(dc, replacement);
                DeleteObject(font);
                font = replacement;
                if font.is_null() {
                    MISSING_GLYPH.with(|flag| flag.set(true));
                    break;
                }
            }
            SelectObject(dc, previous_font);
            DeleteObject(font);
            raster
        }
    }

    /// Rasterizes the glyph as an 8-bit grayscale coverage bitmap
    /// (`GGO_GRAY8_BITMAP`): GDI's own anti-aliased glyph raster, no drawing
    /// surface involved. GDI writes the bitmap's *actual* bounds back into
    /// `metrics` — which can come back a pixel wider than `GGO_METRICS`
    /// reported (the anti-aliased edge column), so the caller must lay the
    /// buffer out by the written-back values or every row skews.
    fn glyph_bitmap(dc: HDC, codepoint: u32, metrics: &mut GLYPHMETRICS) -> Option<Vec<u8>> {
        // SAFETY: `dc` is a live DC with a font selected; the metrics and
        // matrix structs are valid for the call and the buffer is sized to hold
        // the reported black box (rows are DWORD-aligned) plus slack — GDI
        // rejects an exact-size buffer for some glyphs, asking for a few bytes
        // more than stride × rows.
        unsafe {
            let stride = (metrics.gmBlackBoxX + 3) & !3;
            let byte_count = stride as usize * metrics.gmBlackBoxY as usize;
            for slack in [64usize, 1024, 8192] {
                let mut buffer = vec![0u8; byte_count + slack];
                let mut matrix: MAT2 = std::mem::zeroed();
                matrix.eM11 = FIXED { fract: 0, value: 1 };
                matrix.eM22 = FIXED { fract: 0, value: 1 };
                let filled = GetGlyphOutlineW(
                    dc,
                    codepoint,
                    GGO_GRAY8_BITMAP,
                    metrics,
                    buffer.len() as u32,
                    buffer.as_mut_ptr().cast(),
                    &matrix,
                );
                if filled != GDI_ERROR as u32 && filled as usize <= buffer.len() {
                    return Some(buffer);
                }
            }
            None
        }
    }

    fn create_glyph_font(family: &[u16], height: i32, weight: i32) -> HFONT {
        // SAFETY: a plain font creation; the face name is null-terminated and
        // outlives the call.
        unsafe {
            CreateFontW(
                -height,
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                u32::from(DEFAULT_CHARSET),
                u32::from(OUT_TT_PRECIS),
                u32::from(CLIP_DEFAULT_PRECIS),
                u32::from(ANTIALIASED_QUALITY),
                u32::from(DEFAULT_PITCH | FF_DONTCARE),
                family.as_ptr(),
            )
        }
    }

    /// The glyph's measured visible bounds (`GGO_METRICS`): `None` when the
    /// selected font cannot draw the character at all.
    fn glyph_metrics(dc: HDC, codepoint: u32) -> Option<GLYPHMETRICS> {
        // SAFETY: `dc` is a live DC with a font selected, and the metrics
        // struct and transform matrix are valid for the call.
        unsafe {
            let mut matrix: MAT2 = std::mem::zeroed();
            matrix.eM11 = FIXED { fract: 0, value: 1 };
            matrix.eM22 = FIXED { fract: 0, value: 1 };
            let mut metrics: GLYPHMETRICS = std::mem::zeroed();
            let ok = GetGlyphOutlineW(
                dc,
                codepoint,
                GGO_METRICS,
                &mut metrics,
                0,
                null_mut(),
                &matrix,
            );
            (ok != GDI_ERROR as u32).then_some(metrics)
        }
    }
}
