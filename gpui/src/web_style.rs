//! The web control panel's look (openmouse `src/styles/tokens.css`, `shell.css`,
//! `home.css`) for the native shell: its tokens read off the live theme, and
//! its Lucide icons drawn from the same SVG paths the web app uses.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;
use std::sync::{Arc, Mutex};

/// The web tokens this shell paints with (`--ink-0`, `--text-1`, …).
#[derive(Clone, Copy)]
pub struct Tokens {
    pub ink0: Hsla,
    pub ink1: Hsla,
    pub text1: Hsla,
    pub text2: Hsla,
    pub text3: Hsla,
    pub text4: Hsla,
    pub accent: Hsla,
    pub on_accent: Hsla,
    pub ok: Hsla,
    pub warn: Hsla,
    pub bad: Hsla,
    tint: Hsla,
}

pub fn alpha(color: Hsla, a: f32) -> Hsla {
    Hsla { a, ..color }
}

fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let a: Rgba = a.into();
    let b: Rgba = b.into();
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.0,
    }
    .into()
}

impl Tokens {
    pub fn read(cx: &App) -> Self {
        let theme = cx.theme();
        let text1 = theme.foreground;
        let text3 = theme.muted_foreground;
        Self {
            ink0: alpha(theme.sidebar, 1.0),
            ink1: alpha(theme.background, 1.0),
            text1,
            text2: mix(text1, text3, 0.45),
            text3,
            text4: alpha(text3, 0.6),
            accent: theme.primary,
            on_accent: theme.primary_foreground,
            ok: rgb(0x5dde89).into(),
            warn: rgb(0xf2b84b).into(),
            bad: rgb(0xff6b6b).into(),
            tint: text1,
        }
    }

    /// `--surface-1..4`: the theme's tint at 3.5 / 6 / 9 / 13 %.
    pub fn surface(&self, level: u8) -> Hsla {
        alpha(self.tint, [0.035, 0.06, 0.09, 0.13][(level.clamp(1, 4) - 1) as usize])
    }

    /// `--line-1..3`: hairlines at 7 / 11 / 18 %.
    pub fn line(&self, level: u8) -> Hsla {
        alpha(self.tint, [0.07, 0.11, 0.18][(level.clamp(1, 3) - 1) as usize])
    }

    /// `--glint`: the hover wash.
    pub fn glint(&self) -> Hsla {
        alpha(self.tint, 0.03)
    }

    /// The `--lit` panel edge.
    pub fn lit(&self) -> Hsla {
        alpha(self.tint, 0.06)
    }
}

/// `--r-xs/sm/md/lg`.
pub const R_SM: f32 = 6.0;
pub const R_MD: f32 = 10.0;
pub const R_LG: f32 = 14.0;

/// Lucide icon bodies, as the web app draws them (24×24, stroke 2).
pub enum Lucide {
    LayoutGrid,
    Gamepad2,
    BookOpen,
    Wifi,
    Plug,
    Megaphone,
    Settings,
    ChevronLeft,
    ChevronRight,
    Plus,
    ArrowUpRight,
    MousePointer,
}

impl Lucide {
    fn key(&self) -> u8 {
        match self {
            Lucide::LayoutGrid => 0,
            Lucide::Gamepad2 => 1,
            Lucide::BookOpen => 2,
            Lucide::Wifi => 3,
            Lucide::Plug => 4,
            Lucide::Megaphone => 5,
            Lucide::Settings => 6,
            Lucide::ChevronLeft => 7,
            Lucide::ChevronRight => 8,
            Lucide::Plus => 9,
            Lucide::ArrowUpRight => 10,
            Lucide::MousePointer => 11,
        }
    }

    fn body(&self) -> &'static str {
        match self {
            Lucide::LayoutGrid => r#"<rect width="7" height="7" x="3" y="3" rx="1"/><rect width="7" height="7" x="14" y="3" rx="1"/><rect width="7" height="7" x="14" y="14" rx="1"/><rect width="7" height="7" x="3" y="14" rx="1"/>"#,
            Lucide::Gamepad2 => r#"<line x1="6" x2="10" y1="11" y2="11"/><line x1="8" x2="8" y1="9" y2="13"/><line x1="15" x2="15.01" y1="12" y2="12"/><line x1="18" x2="18.01" y1="10" y2="10"/><path d="M17.32 5H6.68a4 4 0 0 0-3.978 3.59c-.006.052-.01.101-.017.152C2.604 9.416 2 14.456 2 16a3 3 0 0 0 3 3c1 0 1.5-.5 2-1l1.414-1.414A2 2 0 0 1 9.828 16h4.344a2 2 0 0 1 1.414.586L17 18c.5.5 1 1 2 1a3 3 0 0 0 3-3c0-1.545-.604-6.584-.685-7.258-.007-.05-.011-.1-.017-.151A4 4 0 0 0 17.32 5z"/>"#,
            Lucide::BookOpen => r#"<path d="M2 3h6a4 4 0 0 1 4 4v14a3 3 0 0 0-3-3H2z"/><path d="M22 3h-6a4 4 0 0 0-4 4v14a3 3 0 0 1 3-3h7z"/>"#,
            Lucide::Wifi => r#"<path d="M12 20h.01"/><path d="M2 8.82a15 15 0 0 1 20 0"/><path d="M5 12.859a10 10 0 0 1 14 0"/><path d="M8.5 16.429a5 5 0 0 1 7 0"/>"#,
            Lucide::Plug => r#"<path d="M12 22v-5"/><path d="M9 8V2"/><path d="M15 8V2"/><path d="M18 8v5a4 4 0 0 1-4 4h-4a4 4 0 0 1-4-4V8Z"/>"#,
            Lucide::Megaphone => r#"<path d="m3 11 18-5v12L3 14v-3z"/><path d="M11.6 16.8a3 3 0 1 1-5.8-1.6"/>"#,
            Lucide::Settings => r#"<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/>"#,
            Lucide::ChevronLeft => r#"<path d="m15 18-6-6 6-6"/>"#,
            Lucide::ChevronRight => r#"<path d="m9 18 6-6-6-6"/>"#,
            Lucide::Plus => r#"<path d="M5 12h14"/><path d="M12 5v14"/>"#,
            Lucide::ArrowUpRight => r#"<path d="M7 7h10v10"/><path d="M7 17 17 7"/>"#,
            Lucide::MousePointer => r#"<rect x="5" y="2" width="14" height="20" rx="7"/><path d="M12 6v4"/>"#,
        }
    }
}

/// The icon in `color`, rasterized from its SVG (cached per icon and colour).
pub fn icon(which: Lucide, color: Hsla) -> Arc<Image> {
    static CACHE: Mutex<Vec<(u8, u32, Arc<Image>)>> = Mutex::new(Vec::new());
    let rgba: Rgba = color.into();
    let hex = ((rgba.r * 255.0).round() as u32) << 16 | ((rgba.g * 255.0).round() as u32) << 8 | (rgba.b * 255.0).round() as u32;
    let key = which.key();
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, _, image)) = cache.iter().find(|(k, h, _)| *k == key && *h == hex) {
        return image.clone();
    }
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="#{hex:06X}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">{}</svg>"##,
        which.body()
    );
    let image = Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes()));
    cache.push((key, hex, image.clone()));
    image
}

/// An icon element `size` px square.
pub fn icon_el(which: Lucide, color: Hsla, size: f32) -> Img {
    img(icon(which, color)).w(px(size)).h(px(size)).flex_none()
}

/// The small battery glyph the web sidebar and device blocks draw.
pub fn battery_glyph(t: &Tokens, percent: u8, charging: bool) -> Div {
    let fill = if charging || percent > 50 {
        t.ok
    } else if percent > 20 {
        t.warn
    } else {
        t.bad
    };
    let inner = 14.0 * f32::from(percent.min(100)) / 100.0;
    div()
        .flex()
        .items_center()
        .flex_none()
        .child(
            div()
                .w(px(18.))
                .h(px(9.))
                .p(px(1.5))
                .rounded(px(2.5))
                .border_1()
                .border_color(t.text3)
                .child(div().h_full().w(px(inner.max(1.0))).rounded(px(1.)).bg(fill)),
        )
        .child(div().w(px(1.5)).h(px(4.)).rounded_r(px(1.)).bg(t.text3))
}
