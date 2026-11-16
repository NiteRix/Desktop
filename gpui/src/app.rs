use crate::games::{self, Game};
use crate::settings::{Settings, MAX_BLUR, MAX_VIBRANCY, MIN_OPACITY};
use crate::theme::ThemeChoice;
use gpui_kit::assets::IconName;
use gpui_kit::base::slider::{SliderEvent, SliderState};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::Icon;
use gpui_kit::component::slider::Slider;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{h_flex, ActiveTheme as _, Collapsible as _, Disableable as _, Sizable as _, TitleBar};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::time::Duration;
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Background art: a static ASCII density ramp painted behind the shell.
/// Static (no animation), seeded once per launch, and only regenerated when the
/// window's character grid changes size.
const ASCII_RAMP: &[u8] = b" .:-=+*x#%@";
/// Cell width as a fraction of the font size. Monospace advances run from
/// ~0.55em (Consolas) to ~0.6em (Menlo, DejaVu); deliberately under all of
/// them, so the trailing columns overflow the window and get clipped instead
/// of leaving a strip of bare background on the right.
const ASCII_CELL_RATIO: f32 = 0.5;
/// Ink opacity at the corner the field starts from.
const ASCII_ALPHA: f32 = 0.09;
/// Alpha multiplier on the OLED theme, where the field would otherwise vanish.
const ASCII_OLED_GAIN: f32 = 2.2;
/// The field slides back in from the right whenever the theme changes.
const ASCII_SWEEP_MS: u64 = 520;
/// How far a row starts to the right of its resting place, in px.
const ASCII_SWEEP_OFFSET: f32 = 220.0;
/// Share of the sweep spent waiting for the wave to reach the last row, so
/// rows arrive top-down instead of all at once.
const ASCII_SWEEP_STAGGER: f32 = 0.55;
/// How far the falloff reaches, as a fraction of the corner-to-opposite
/// corner distance.
const ASCII_FADE_REACH: f32 = 0.9;

/// Number of alpha bands per row; each is its own text run.
const ASCII_BANDS: usize = 6;

/// Falloff from the top-right corner, slightly wider than tall. `u` runs
/// left→right, `v` top→bottom; returns 0..1.
fn ascii_falloff(u: f32, v: f32) -> f32 {
    let dx = 1.0 - u;
    let distance = (dx * dx + v * v * 0.8).sqrt();
    let s = (1.0 - distance / ASCII_FADE_REACH).clamp(0.0, 1.0);
    // Smoothstep: no hard edge where the field reaches zero.
    s * s * (3.0 - 2.0 * s)
}

/// Ink opacity for one cell region. The alpha fades out with the density in
/// both directions, so the gradient dims the layer instead of veiling the
/// whole window at one flat opacity.
fn ascii_cell_alpha(u: f32, v: f32) -> f32 {
    ASCII_ALPHA * ascii_falloff(u, v).sqrt()
}

/// Deterministic lattice hash → 0..1.
fn ascii_hash(x: i32, y: i32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x9E37_79B1) ^ (y as u32).wrapping_mul(0x85EB_CA77);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^= h >> 15;
    (h & 0xFFFF) as f32 / 65535.0
}

/// Smooth value noise, 0..1.
fn ascii_value_noise(x: f32, y: f32) -> f32 {
    let (xi, yi) = (x.floor(), y.floor());
    let (fx, fy) = (x - xi, y - yi);
    let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let (xi, yi) = (xi as i32, yi as i32);
    let top = ascii_hash(xi, yi) + (ascii_hash(xi + 1, yi) - ascii_hash(xi, yi)) * sx;
    let bottom = ascii_hash(xi, yi + 1) + (ascii_hash(xi + 1, yi + 1) - ascii_hash(xi, yi + 1)) * sx;
    top + (bottom - top) * sy
}

/// Three-octave fractal noise, 0..1, in cell units (cells are ~2x taller
/// than wide, so `y` is scaled harder than `x`).
fn ascii_fbm(x: f32, y: f32) -> f32 {
    let (mut sum, mut amp, mut freq, mut norm) = (0.0, 1.0, 1.0, 0.0);
    for octave in 0..3 {
        let o = octave as f32 * 17.0;
        sum += amp * ascii_value_noise(x * freq + o, y * freq - o);
        norm += amp;
        amp *= 0.5;
        freq *= 2.0;
    }
    sum / norm
}

/// Interleaved gradient noise, 0..1: an unstructured dither without the
/// visible grid a Bayer matrix leaves on flat areas.
fn ascii_dither(x: usize, y: usize) -> f32 {
    (52.982_92 * ((0.067_110_56 * x as f32 + 0.005_837_15 * y as f32).fract())).fract()
}

/// Where in the noise field this launch samples, so the clouds differ every
/// time the app starts. Fixed for the life of the process: resizing the window
/// regenerates the grid and must not make the pattern jump.
fn ascii_seed() -> (f32, f32) {
    static SEED: std::sync::OnceLock<(f32, f32)> = std::sync::OnceLock::new();
    *SEED.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u32);
        let hash = |salt: i32| ascii_hash(nanos as i32, salt) * 1000.0;
        (hash(1), hash(2))
    })
}

/// Build one character row per screen row. Density is highest at the
/// top-right corner and falls away toward the bottom-left; fractal noise
/// breaks the gradient into cloud-like contours instead of a clean radial.
/// The noise is offset by [`ascii_seed`], so each launch gets its own clouds.
fn ascii_background_rows(cols: usize, rows: usize) -> Vec<SharedString> {
    let last = (ASCII_RAMP.len() - 1) as f32;
    let (seed_x, seed_y) = ascii_seed();
    let mut out = Vec::with_capacity(rows);
    for y in 0..rows {
        let v = (y as f32 + 0.5) / rows as f32;
        let mut line = String::with_capacity(cols);
        for x in 0..cols {
            let u = (x as f32 + 0.5) / cols as f32;
            let cloud = ascii_fbm(x as f32 * 0.07 + seed_x, y as f32 * 0.14 + seed_y);
            let t = (ascii_falloff(u, v) * (0.3 + 1.2 * cloud)).clamp(0.0, 1.0).powf(1.25);
            let dithered = t * last + (ascii_dither(x, y) - 0.5) * 0.9;
            let index = dithered.round().clamp(0.0, last) as usize;
            line.push(ASCII_RAMP[index] as char);
        }
        out.push(line.into());
    }
    out
}
/// Release stage label shown in the titlebar badge: every 0.x build is a
/// Beta, 1.0 and later are Public.
fn release_stage(version: &str) -> &'static str {
    let major = version
        .trim_start_matches('v')
        .split('.')
        .next()
        .unwrap_or("0")
        .parse::<u64>()
        .unwrap_or(0);
    if major == 0 {
        "Beta"
    } else {
        "Public"
    }
}

/// Mirrors `Page` in `src/layouts/FullDesktopView.tsx`: the four top-level
/// screens behind the app sidebar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Overview,
    Games,
    Supported,
    Settings,
    /// Version and release notes; opened from the sidebar footer, not the nav list.
    Updates,
}

impl Page {
    pub fn label(self) -> &'static str {
        match self {
            Page::Overview => "Overview",
            Page::Games => "Games",
            Page::Supported => "Supported mice",
            Page::Settings => "Settings",
            Page::Updates => "Updates",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Page::Overview => IconName::LayoutDashboard,
            Page::Games => IconName::Gamepad2,
            Page::Supported => IconName::BookOpen,
            Page::Settings => IconName::Settings,
            Page::Updates => IconName::CircleArrowUp,
        }
    }
}

/// A short-lived message at the bottom of the window.
struct Toast {
    id: u64,
    text: String,
    error: bool,
}

/// While a game's profile is in control: what to put back when it closes.
struct ProfileOverride {
    game_id: String,
    game_name: String,
    /// The device the profile was applied to (and will be restored on).
    device: String,
    restore: crate::profiles::GameProfile,
}

/// Overview grid sizing: the smallest card width a column may shrink to, and
/// the layout constants the column count is estimated from.
const MIN_CARD_W: f32 = 320.0;
const GRID_GAP: f32 = 16.0;
const PAGE_PAD: f32 = 16.0;
const SIDEBAR_W: f32 = 248.0;
const SIDEBAR_COLLAPSED_W: f32 = 68.0;
/// The device page's side padding and its render rail's width.
const PAGE_PAD_X: f32 = 32.0;
const RAIL_W: f32 = 280.0;
/// Height of the title bar row that carries the sidebar head (two lines of
/// brand text need more than the toolkit's default caption height).
const TITLEBAR_H: f32 = 48.0;

/// The community server, the same invite openmouse.app links to.
const DISCORD_INVITE_URL: &str = "https://discord.gg/yxC9jzMdw6";

/// The Discord glyph (drawn in the theme colour by `themed_discord`), embedded so
/// there is no file to ship or find at runtime.
static DISCORD_SVG: &str = include_str!("../assets/discord.svg");


/// Rows above the releases on the Updates page: heading, version card, notes title.
const UPDATES_LEADING_ROWS: usize = 3;

/// The Discord glyph drawn in `color`, so it follows the theme like the other
/// sidebar icons instead of being a fixed brand blue. One image per colour.
/// The OpenMouse mark the web sidebar head shows (`.om-brand-mark`).
fn brand_mark() -> std::sync::Arc<Image> {
    static MARK: std::sync::OnceLock<std::sync::Arc<Image>> = std::sync::OnceLock::new();
    MARK.get_or_init(|| std::sync::Arc::new(Image::from_bytes(ImageFormat::Png, include_bytes!("../assets/logo.png").to_vec())))
        .clone()
}

/// The Add device block's render (the web block crossfades supported mice;
/// this shows the X8 SE artwork, dimmed).
fn add_device_art(index: usize) -> std::sync::Arc<Image> {
    static ART: std::sync::OnceLock<Vec<std::sync::Arc<Image>>> = std::sync::OnceLock::new();
    let art = ART.get_or_init(|| {
        [
            include_bytes!("../../public/devices/attackshark-x8-se-official.png").as_slice(),
            include_bytes!("../../public/devices/attackshark-r1-official.png").as_slice(),
            include_bytes!("../../public/devices/attackshark-x11.png").as_slice(),
        ]
        .iter()
        .map(|bytes| std::sync::Arc::new(Image::from_bytes(ImageFormat::Png, bytes.to_vec())))
        .collect()
    });
    art[index % art.len()].clone()
}

fn themed_discord(color: Hsla) -> std::sync::Arc<Image> {
    static CACHE: std::sync::Mutex<Vec<(u32, std::sync::Arc<Image>)>> = std::sync::Mutex::new(Vec::new());
    let rgba: Rgba = color.into();
    let hex = ((rgba.r * 255.0).round() as u32) << 16 | ((rgba.g * 255.0).round() as u32) << 8 | (rgba.b * 255.0).round() as u32;
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, image)) = cache.iter().find(|(h, _)| *h == hex) {
        return image.clone();
    }
    let svg = DISCORD_SVG.replace("#5865F2", &format!("#{hex:06X}"));
    let image = std::sync::Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes()));
    cache.push((hex, image.clone()));
    image
}

/// The selected device's detail card (battery bar, DPI, polling rate,
/// firmware) under the grid. Hidden until per-device menus exist to hold it.
const SHOW_DEVICE_DETAILS: bool = false;

/// Overview device state, driven by `crate::hid` (the shared protocol
/// drivers; see that module for how).
#[derive(Default)]
struct Devices {
    scanning: bool,
    error: Option<String>,
    tiles: Vec<Tile>,
    selected: usize,
    /// Key of the device whose settings page is open, if any.
    open: Option<String>,
    /// Edits made on that page and not yet written (instant apply off).
    staged: Staged,
    /// X and Y DPI edited separately (mice with separate axes); linked by default,
    /// since a single number on screen hides a differing Y.
    unlinked_axes: bool,
    tab: DeviceTab,
    /// Stored profiles of the open Logitech device, once read (on demand).
    onboard: Option<crate::hid::OnboardProfiles>,
    onboard_loading: bool,
    /// The long-list control whose options are currently unfolded.
    expanded: Option<String>,
    /// Label of the write in flight (disables further edits meanwhile).
    writing: Option<String>,
    write_error: Option<String>,
    /// The stage count to go back to when "Stages" follows "Single".
    last_stage_count: usize,
    /// Bumped when a write starts and when it lands: a status read that began
    /// before either is older than the page and is dropped.
    write_seq: u64,
}

/// Pending, unwritten lighting edits (each `None` field is untouched).
#[derive(Default, Clone, PartialEq)]
struct LightEdit {
    mode: Option<String>,
    color: Option<String>,
    color2: Option<String>,
    speed: Option<f64>,
}

/// Pending, unwritten edits on a device page.
#[derive(Default, Clone, PartialEq)]
struct Staged {
    dpi: Option<f64>,
    /// Y-axis DPI, only while the axes are unlinked.
    dpi_y: Option<f64>,
    polling: Option<u32>,
    lod: Option<String>,
    surface: Option<String>,
    light: LightEdit,
}

impl Staged {
    fn is_empty(&self) -> bool {
        *self == Staged::default()
    }
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum DeviceTab {
    #[default]
    Overview,
    Performance,
    Buttons,
    Lighting,
    Advanced,
    /// The mouse's own stored profiles (Logitech).
    Profiles,
}

impl DeviceTab {
    fn label(self) -> &'static str {
        match self {
            DeviceTab::Overview => "Overview",
            DeviceTab::Performance => "Performance",
            DeviceTab::Buttons => "Buttons",
            DeviceTab::Lighting => "Lighting",
            DeviceTab::Advanced => "Advanced",
            DeviceTab::Profiles => "Profiles",
        }
    }
}

/// A small status badge: a faint accent-tinted pill with an accent dot and the
/// theme's own text colour. (The toolkit's `Tag::success()` is a fixed green
/// that ignores the palette and reads poorly on the Dynamic theme; coloured
/// text on a tint of itself falls under 4.5:1 on some wallpapers.)
fn accent_badge(text: impl Into<SharedString>, accent: Hsla, ink: Hsla) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1p5()
        .px_2()
        .py_0p5()
        .rounded_full()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .bg(Hsla { a: 0.20, ..accent })
        .text_color(ink)
        .child(div().size(px(6.)).flex_none().rounded_full().bg(accent))
        .child(text.into())
}

/// Swatches offered for lighting colours.
const LIGHT_SWATCHES: [&str; 14] = [
    "#ff0000", "#ff6a00", "#ffc800", "#a6ff00", "#00ff40", "#00ffd0", "#00aaff",
    "#0040ff", "#6a00ff", "#c800ff", "#ff00a0", "#ffffff", "#9aa0a6", "#000000",
];

fn hex_color(hex: &str) -> Hsla {
    let h = hex.trim_start_matches('#');
    let v = u32::from_str_radix(h, 16).unwrap_or(0x888888);
    rgb(v).into()
}

/// How often the monitor looks for plugged/unplugged devices and re-reads
/// battery (Bridge reads batteries on an interval for the same reasons).
const MONITOR_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
/// Status (battery) is re-read every this many monitor ticks (30 s).
const STATUS_EVERY_TICKS: u32 = 15;
/// One low-battery alert per mouse per this long (Bridge's default cooldown).
const LOW_BATTERY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

#[derive(Default)]
struct Monitor {
    started: bool,
    busy: bool,
    ticks: u32,
    /// Devices that appeared after launch: announced once their status reads.
    announce: std::collections::HashSet<String>,
    /// When each mouse last raised the low-battery alert.
    low_alerted: std::collections::HashMap<String, std::time::Instant>,
    /// Mice that already raised "fully charged" this charge.
    charged_alerted: std::collections::HashSet<String>,
    /// The device block under the pointer (its render lifts, as on the web).
    hovered: Option<String>,
    /// Battery readings of the past week, per mouse name, for the Overview chart.
    battery_log: std::collections::HashMap<String, Vec<(std::time::SystemTime, u8)>>,
}

struct Tile {
    key: String,
    name: String,
    /// The registry's label for the driver; what gating and write routing use.
    brand: String,
    /// The brand to show (the product's own — see `ConnectedDevice::display_brand`).
    display_brand: String,
    art: Option<LoadedArt>,
    status: TileStatus,
}

struct LoadedArt {
    path: std::path::PathBuf,
    placement: Option<crate::hid::ArtPlacement>,
}

enum TileStatus {
    Idle,
    Connecting,
    Connected(DeviceReadout),
    Failed(String),
}

struct DeviceReadout {
    /// Y-axis DPI, when the mouse reports one (it can differ from `dpi`).
    dpi_y: Option<f64>,
    /// The mouse can hold X and Y sensitivity apart.
    separate_axes: bool,
    /// The driver's own UI hints (`MouseUiHints`): what is meaningful for this
    /// protocol generation.
    polling_read_only: bool,
    hide_lod_low: bool,
    lod_requires_surface: bool,
    /// A Logitech mouse with stored profiles the library can read.
    onboard_profiles: bool,
    /// `Onboard`, `Host` or `Unknown` (Logitech).
    mode: Option<String>,
    tabs: crate::hid::Tabs,
    controls: Option<crate::hid::Controls>,
    polling_rates: Vec<u32>,
    lod: Option<String>,
    lod_options: Vec<String>,
    surface: Option<String>,
    battery: Option<f64>,
    charging: bool,
    /// The on-device DPI stages (`dpiStages[].dpi`) and the active one.
    stages: Vec<f64>,
    active_stage: Option<usize>,
    /// How many stages (from the first) the DPI button cycles.
    stage_count: Option<usize>,
    /// The mouse lets that count be changed.
    stage_count_editable: bool,
    /// Each stage's DPI light, `#rrggbb` (empty when the mouse has none).
    stage_colors: Vec<String>,
    dpi: Option<f64>,
    polling_hz: Option<f64>,
    connection: Option<String>,
    firmware: Vec<String>,
    /// The driver's whole status, for "Copy diagnostics".
    raw: serde_json::Value,
}

impl DeviceReadout {
    fn from_status(s: &serde_json::Value, controls: Option<crate::hid::Controls>, tabs: crate::hid::Tabs) -> Self {
        let num = |k: &str| s.get(k).and_then(|v| v.as_f64());
        let strings = |k: &str| -> Vec<String> {
            s.get(k)
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };
        let ui_flag = |k: &str| s.get("ui").and_then(|u| u.get(k)).and_then(|v| v.as_bool()).unwrap_or(false);
        Self {
            dpi_y: num("dpiY"),
            separate_axes: s.get("supportsSeparateDpiAxes").and_then(|v| v.as_bool()).unwrap_or(false),
            polling_read_only: ui_flag("pollingReadOnly"),
            hide_lod_low: ui_flag("hideLodLow"),
            lod_requires_surface: ui_flag("lodRequiresSurface"),
            onboard_profiles: s
                .get("onboardProfileFormat")
                .and_then(|f| f.get("supported"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                && s.get("brand").and_then(|b| b.as_str()) == Some("Logitech"),
            mode: s.get("deviceMode").and_then(|v| v.as_str()).map(str::to_string),
            tabs,
            controls,
            polling_rates: s
                .get("supportedPollingRates")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_u64().map(|n| n as u32)).collect())
                .unwrap_or_default(),
            lod: s.get("liftOffDistance").and_then(|v| v.as_str()).map(str::to_string),
            lod_options: strings("supportedLiftOffDistances"),
            surface: s.get("gamingSurfaceMode").and_then(|v| v.as_str()).map(str::to_string),
            battery: num("batteryPercent"),
            stages: s
                .get("dpiStages")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.get("dpi").and_then(|d| d.as_f64()).or_else(|| x.as_f64())).collect())
                .unwrap_or_default(),
            active_stage: s.get("activeDpiStage").and_then(|v| v.as_u64()).map(|n| n as usize),
            stage_count: s.get("dpiStageCount").and_then(|v| v.as_u64()).map(|n| n as usize),
            stage_count_editable: s
                .get("ui")
                .and_then(|u| u.get("dpiStageEditor"))
                .and_then(|e| e.get("countEditable"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            stage_colors: strings("dpiStageColors"),
            charging: s
                .get("batteryState")
                .and_then(|v| v.as_str())
                .is_some_and(|b| b.starts_with("Charging")),
            dpi: num("dpi"),
            polling_hz: num("pollingRateHz"),
            connection: s.get("connectionType").and_then(|v| v.as_str()).map(str::to_string),
            firmware: s
                .get("firmware")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            raw: s.clone(),
        }
    }
}

/// Root view: custom titlebar + icon sidebar + page content.
/// Mirrors `FullDesktopView` (`TitleBar` + `AppSidebar` + page switch).
pub struct AppView {
    page: Page,
    sidebar_collapsed: bool,
    settings: Settings,
    games: Vec<Game>,
    devices: Devices,
    hid: Option<crate::hid::HidService>,
    presence: std::sync::Arc<openmouse_presence::Presence>,
    profiles: crate::profiles::Profiles,
    /// Game whose profile is being edited on the Games page.
    profile_editor: Option<String>,
    /// One per (game, device) whose profile is currently in control.
    profile_overrides: Vec<ProfileOverride>,
    /// (game, device) pairs with a write in flight, so none is applied twice.
    profile_inflight: Vec<(String, String)>,
    /// The device being edited on the profile screen (None: the first connected).
    profile_device: Option<String>,
    toast: Option<Toast>,
    toast_seq: u64,
    /// The Updates page's virtualized list (only the rows on screen are built).
    updates_list: ListState,
    /// Newest version `update_check` has seen, if it's ahead of `APP_VERSION`.
    update_available: Option<crate::update_check::UpdateInfo>,
    /// A newer version's installer, downloaded and verified.
    update_ready: Option<crate::update_check::Pending>,
    update_downloading: bool,
    checking_for_updates: bool,
    /// The on-screen game overlay while one is up.
    overlay: Option<AnyWindowHandle>,
    /// Newest scheduled presence update wins (Discord rate-limits them).
    presence_seq: u64,
    /// Last failure from the Launch-on-startup / Discord switches, shown under them.
    startup_error: Option<String>,
    discord_error: Option<String>,
    badge_hovered: bool,
    opacity_slider: Option<Entity<SliderState>>,
    vibrancy_slider: Option<Entity<SliderState>>,
    /// One slider per number control on the open device page (`<device>/<control id>`),
    /// minted by `ensure_control_sliders` because rendering has no window.
    control_sliders: std::collections::HashMap<String, Entity<SliderState>>,
    /// What a slider shows while it is being dragged, before the write lands.
    slider_live: std::collections::HashMap<String, f64>,
    slider_seq: u64,
    /// The typeable value box of each DPI stage (`<device>/stage/<n>`).
    stage_inputs: std::collections::HashMap<String, Entity<InputState>>,
    _subscriptions: Vec<Subscription>,
    _game_watcher: Option<crate::game_watcher::GameWatcher>,
    /// Cached ASCII background rows, keyed by their grid dimensions.
    ascii_rows: Vec<SharedString>,
    ascii_dims: (usize, usize),
    /// Bumped on every theme change; the ASCII field's animation id carries
    /// it, so each switch replays the entrance.
    theme_seq: u64,
    /// Background device watch: plugs, unplugs, battery, tray and alerts.
    monitor: Monitor,
}

/// Themes in Settings, in display order: follow the system, the web app's
/// twelve interface themes, then the native-only OLED and Dynamic.
const THEME_CHOICES: [ThemeChoice; 15] = [
    ThemeChoice::System,
    ThemeChoice::Web(0),
    ThemeChoice::Web(1),
    ThemeChoice::Web(2),
    ThemeChoice::Web(3),
    ThemeChoice::Web(4),
    ThemeChoice::Web(5),
    ThemeChoice::Web(6),
    ThemeChoice::Web(7),
    ThemeChoice::Web(8),
    ThemeChoice::Web(9),
    ThemeChoice::Web(10),
    ThemeChoice::Web(11),
    ThemeChoice::Oled,
    ThemeChoice::Dynamic,
];

/// Accent swatches in Settings: (label, hue in degrees).
const ACCENT_SWATCHES: [(&str, f32); 8] = [
    ("Red", 0.0),
    ("Orange", 25.0),
    ("Yellow", 48.0),
    ("Green", 130.0),
    ("Teal", 175.0),
    ("Blue", 215.0),
    ("Purple", 265.0),
    ("Pink", 330.0),
];
const ACCENT_SWATCH_SIZE: f32 = 28.0;

impl AppView {
    /// Build the root view, owning the Settings sliders. Call from the
    /// `open_window` closure where `cx` can mint the slider entities.
    /// `initial_theme`/`wallpaper` mirror the startup path (Dark first paint
    /// or sampled Dynamic); persisted opacity/vibrancy seed the sliders.
    pub fn build(
        initial_theme: ThemeChoice,
        wallpaper: Option<(u8, u8, u8)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = Settings {
            theme: initial_theme,
            ..Settings::load()
        };
        // NOTE: theme application moved out of construction (see `main.rs`):
        // `Theme::change/update` inside the `open_window` build closure runs
        // mid-window-construction and poisoned first render. `wallpaper` is
        // kept for the post-open apply.
        let _ = wallpaper;

        let opacity_slider = cx.new(|_| {
            SliderState::new()
                .min(MIN_OPACITY)
                .max(100.0)
                .step(1.0)
                .default_value(settings.window_opacity)
        });
        let vibrancy_slider = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(MAX_VIBRANCY)
                .step(1.0)
                .default_value(settings.dynamic_vibrancy)
        });

        // Live repaint on drag (mirrors `onInput` → `apply*`); persistence
        // happens immediately too — the slider is the setting's own state,
        // and `Settings::save` is a small synchronous file write.
        let sub_opacity = cx.subscribe_in(
            &opacity_slider,
            window,
            |this: &mut Self, _state, event, _window, cx| {
                if let SliderEvent::Change(value) = event {
                    this.settings.window_opacity = value.end();
                    crate::theme::set_surface_opacity(value.end() / 100.0, cx);
                    this.settings.save();
                    cx.notify();
                }
            },
        );
        let sub_vibrancy = cx.subscribe_in(
            &vibrancy_slider,
            window,
            |this: &mut Self, _state, event, window, cx| {
                if let SliderEvent::Change(value) = event {
                    this.settings.dynamic_vibrancy = value.end();
                    this.repaint_theme(Some(window), cx);
                    this.settings.save();
                }
            },
        );

        // The background scans slow down while this window is not in front
        // (see `activity.rs`). Only the main window reports: the game overlay
        // never takes focus and must not count as the app being used.
        let sub_activity = cx.observe_window_activation(window, |_this: &mut Self, window, _cx| {
            crate::activity::ACTIVITY.set_active(window.is_window_active());
        });

        let games = games::load_catalog();
        let game_watcher = crate::game_watcher::GameWatcher::start(cx);
        let mut view = Self {
            page: Page::Overview,
            sidebar_collapsed: false,
            settings,
            games,
            devices: Devices::default(),
            hid: None,
            presence: Default::default(),
            profiles: Default::default(),
            profile_editor: None,
            profile_overrides: Vec::new(),
            profile_inflight: Vec::new(),
            profile_device: None,
            toast: None,
            toast_seq: 0,
            updates_list: ListState::new(crate::changelog::entries().len() + UPDATES_LEADING_ROWS, ListAlignment::Top, px(400.)),
            update_available: None,
            update_ready: None,
            update_downloading: false,
            checking_for_updates: false,
            overlay: None,
            presence_seq: 0,
            startup_error: None,
            discord_error: None,
            badge_hovered: false,
            opacity_slider: Some(opacity_slider),
            vibrancy_slider: Some(vibrancy_slider),
            control_sliders: Default::default(),
            slider_live: Default::default(),
            slider_seq: 0,
            stage_inputs: Default::default(),
            _subscriptions: vec![sub_opacity, sub_vibrancy, sub_activity],
            _game_watcher: Some(game_watcher),
            ascii_rows: Vec::new(),
            ascii_dims: (0, 0),
            theme_seq: 0,
            monitor: Monitor { battery_log: crate::battery_history::load(), ..Monitor::default() },
        };
        // The OS entry is the truth for launch-on-startup, not the saved flag.
        view.profiles = crate::profiles::Profiles::load();
        view.settings.launch_on_startup = crate::autostart::is_enabled();
        if view.settings.discord_presence {
            view.set_discord(true, cx);
        }
        view.scan_devices(cx);
        view.start_monitor(cx);
        let weak = cx.entity().downgrade();
        crate::tray::on_stage(move |stage, cx| {
            let _ = weak.update(cx, |v: &mut Self, cx| v.tray_stage(stage, cx));
        });
        view
    }

    /// Harness-free constructor for tests (no window/cx for slider entities;
    /// Settings UI paths require `build`).
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for AppView {
    fn default() -> Self {
        Self {
            page: Page::Overview,
            control_sliders: Default::default(),
            slider_live: Default::default(),
            slider_seq: 0,
            stage_inputs: Default::default(),
            sidebar_collapsed: false,
            settings: Settings::default(),
            games: Vec::new(),
            devices: Devices::default(),
            hid: None,
            presence: Default::default(),
            profiles: Default::default(),
            profile_editor: None,
            profile_overrides: Vec::new(),
            profile_inflight: Vec::new(),
            profile_device: None,
            toast: None,
            toast_seq: 0,
            updates_list: ListState::new(crate::changelog::entries().len() + UPDATES_LEADING_ROWS, ListAlignment::Top, px(400.)),
            update_available: None,
            update_ready: None,
            update_downloading: false,
            checking_for_updates: false,
            overlay: None,
            presence_seq: 0,
            startup_error: None,
            discord_error: None,
            badge_hovered: false,
            opacity_slider: None,
            vibrancy_slider: None,
            _subscriptions: Vec::new(),
            _game_watcher: None,
            ascii_rows: Vec::new(),
            ascii_dims: (0, 0),
            theme_seq: 0,
            monitor: Monitor::default(),
        }
    }
}

impl AppView {
    /// What Rich Presence should say right now, from the settings and state:
    /// the game being played (top line) and the mouse and its stats (second line).
    fn presence_text(&self) -> (String, String) {
        let s = &self.settings;
        // Stats come from the first mouse-like connected device (one with a DPI).
        let device = self.connected_devices().into_iter().find(|(_, r)| r.dpi.is_some());
        let name = device
            .as_ref()
            .and_then(|(key, _)| self.devices.tiles.iter().find(|t| &t.key == key))
            .map(|t| t.name.as_str());
        crate::presence_text::compose(
            crate::presence_text::Share { game: s.discord_show_game, device: s.discord_show_device, stats: s.discord_show_stats },
            &crate::presence_text::Facts {
                running_game: self.games.iter().find(|g| g.running).map(|g| g.name.as_str()),
                device_name: name,
                dpi: device.as_ref().and_then(|(_, r)| r.dpi),
                polling_hz: device.as_ref().and_then(|(_, r)| if r.polling_read_only && r.polling_rates.is_empty() { None } else { r.polling_hz }),
            },
        )
    }

    /// Pushes the current presence text to Discord (a moment later, and only the
    /// newest request, because Discord rate-limits activity updates). A no-op
    /// while presence is off or not connected.
    fn refresh_presence(&mut self, cx: &mut Context<Self>) {
        if !self.settings.discord_presence {
            return;
        }
        self.presence_seq += 1;
        let seq = self.presence_seq;
        cx.spawn(async move |view, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(1500)).await;
            let Ok(Some((presence, text))) = view.update(cx, |v: &mut Self, _| {
                (v.presence_seq == seq && v.settings.discord_presence)
                    .then(|| (v.presence.clone(), v.presence_text()))
            }) else {
                return;
            };
            let (details, state) = text;
            let _ = cx.background_executor().spawn(async move { presence.update_activity(details, state) }).await;
        })
        .detach();
    }

    /// Connects or disconnects Discord Rich Presence off the UI thread. A
    /// failure (Discord not running) is shown under the switch; the preference
    /// stays on, as in the Tauri app, so it connects next time Discord is up.
    fn set_discord(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.discord_error = None;
        let presence = self.presence.clone();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if enabled { presence.enable() } else { presence.disable() }
                })
                .await;
            let _ = view.update(cx, |v: &mut Self, cx| {
                let connected = enabled && result.is_ok();
                v.discord_error = result.err().map(|error| {
                    if enabled {
                        format!("Discord Rich Presence is on but unavailable: {error}. Is Discord running?")
                    } else {
                        format!("Could not disconnect from Discord: {error}")
                    }
                });
                if connected {
                    v.refresh_presence(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Writes or removes the OS launch-on-login entry and reflects the real
    /// outcome in the switch.
    fn set_launch_on_startup(&mut self, enabled: bool) {
        let result = if enabled { crate::autostart::enable() } else { crate::autostart::disable() };
        self.startup_error = result.err();
        self.settings.launch_on_startup = crate::autostart::is_enabled();
    }

    fn hid_service(&mut self) -> crate::hid::HidService {
        self.hid.get_or_insert_with(crate::hid::HidService::start).clone()
    }

    /// Lists supported devices, loads their artwork, and connects the first.
    /// Everything blocking runs on `crate::hid`'s threads or the background
    /// executor.
    fn scan_devices(&mut self, cx: &mut Context<Self>) {
        if self.devices.scanning {
            return;
        }
        self.devices.scanning = true;
        self.devices.error = None;
        cx.notify();
        let service = self.hid_service();
        cx.spawn(async move |view, cx| {
            let listed = service.list_candidates().await;
            let candidates = match listed {
                Ok(Ok(list)) => list,
                other => {
                    let message = match other {
                        Ok(Err(error)) => error,
                        _ => "device runtime stopped".to_string(),
                    };
                    let _ = view.update(cx, |v: &mut Self, cx| {
                        v.devices.scanning = false;
                        v.devices.error = Some(message);
                        cx.notify();
                    });
                    return;
                }
            };
            let _ = view.update(cx, |v: &mut Self, cx| {
                // Keep what is already known about devices that are still here.
                let mut old: Vec<Tile> = std::mem::take(&mut v.devices.tiles);
                v.devices.tiles = candidates
                    .iter()
                    .map(|c| {
                        match old.iter().position(|t| t.key == c.info.key) {
                            Some(i) => old.swap_remove(i),
                            None => Tile {
                                key: c.info.key.clone(),
                                name: c.info.product_string.clone(),
                                brand: c.brands.first().cloned().unwrap_or_default(),
                                display_brand: c.brands.first().cloned().unwrap_or_default(),
                                art: None,
                                status: TileStatus::Idle,
                            },
                        }
                    })
                    .collect();
                v.devices.selected = v.devices.selected.min(v.devices.tiles.len().saturating_sub(1));
                v.devices.scanning = false;
                cx.notify();
            });
            for c in &candidates {
                // Use the name the tile carries now: a receiver that was
                // connected is known by its mouse's name, and resolving the
                // receiver's generic product string would swap the mouse's
                // artwork for the placeholder.
                let current = view
                    .update(cx, |v: &mut Self, _| {
                        v.devices
                            .tiles
                            .iter()
                            .find(|t| t.key == c.info.key)
                            .map(|t| (t.name.clone(), t.art.is_some()))
                    })
                    .ok()
                    .flatten();
                let Some((name, has_art)) = current else { continue };
                if has_art {
                    continue;
                }
                Self::load_art(&service, &c.info.key, &name, &view, cx).await;
            }
            let _ = view.update(cx, |v: &mut Self, cx| {
                if let Some(tile) = v.devices.tiles.first() {
                    if matches!(tile.status, TileStatus::Idle) {
                        let key = tile.key.clone();
                        v.connect_tile(&key, false, cx);
                    }
                }
            });
        })
        .detach();
    }

    async fn load_art(
        service: &crate::hid::HidService,
        key: &str,
        name: &str,
        view: &WeakEntity<Self>,
        cx: &mut AsyncApp,
    ) {
        let Ok(Ok(art)) = service.device_image(key, name).await else { return };
        let placement = art.art.clone();
        let Some(path) = cx.background_executor().spawn(async move { art.fetch() }).await else {
            return;
        };
        let key = key.to_string();
        let _ = view.update(cx, |v: &mut Self, cx| {
            if let Some(tile) = v.devices.tiles.iter_mut().find(|t| t.key == key) {
                tile.art = Some(LoadedArt { path, placement });
                cx.notify();
            }
        });
    }

    /// Starts the background watch (see [`Monitor`]).
    fn start_monitor(&mut self, cx: &mut Context<Self>) {
        if self.monitor.started {
            return;
        }
        self.monitor.started = true;
        cx.spawn(async move |view, cx| loop {
            cx.background_executor().timer(MONITOR_EVERY).await;
            if view.update(cx, |v: &mut Self, cx| v.monitor_tick(cx)).is_err() {
                break;
            }
        })
        .detach();
    }

    /// Adds devices that were plugged in, drops ones that were unplugged and
    /// re-reads every connected device's status (battery above all).
    fn monitor_tick(&mut self, cx: &mut Context<Self>) {
        if self.monitor.busy || self.devices.scanning {
            return;
        }
        self.monitor.busy = true;
        self.monitor.ticks = self.monitor.ticks.wrapping_add(1);
        // The Add device block's showcase moves on every other tick.
        if self.page == Page::Overview && self.devices.open.is_none() && self.monitor.ticks % 2 == 0 {
            cx.notify();
        }
        let reread = self.monitor.ticks % STATUS_EVERY_TICKS == 0;
        // A receiver whose mouse is asleep or on its cable is asked more often,
        // so switching the mouse on is noticed within a few seconds.
        let receivers_due = self.monitor.ticks % 3 == 0;
        // The open device is re-read every other tick, so a stage changed with
        // the mouse's own DPI button shows within a few seconds.
        let open_due = self.monitor.ticks % 2 == 0 && self.devices.writing.is_none() && self.page == Page::Overview;
        let service = self.hid_service();
        cx.spawn(async move |view, cx| {
            let Ok(Ok(candidates)) = service.list_candidates().await else {
                let _ = view.update(cx, |v: &mut Self, _| v.monitor.busy = false);
                return;
            };
            let mut refresh: Vec<String> = Vec::new();
            let mut added: Vec<(String, String)> = Vec::new();
            let _ = view.update(cx, |v: &mut Self, cx| {
                let keys: Vec<&str> = candidates.iter().map(|c| c.info.key.as_str()).collect();
                let before = v.devices.tiles.len();
                v.devices.tiles.retain(|t| keys.contains(&t.key.as_str()));
                let removed = before != v.devices.tiles.len();
                if v.devices.open.as_ref().is_some_and(|key| !keys.contains(&key.as_str())) {
                    v.devices.open = None;
                }
                for c in &candidates {
                    if v.devices.tiles.iter().any(|t| t.key == c.info.key) {
                        continue;
                    }
                    v.devices.tiles.push(Tile {
                        key: c.info.key.clone(),
                        name: c.info.product_string.clone(),
                        brand: c.brands.first().cloned().unwrap_or_default(),
                        display_brand: c.brands.first().cloned().unwrap_or_default(),
                        art: None,
                        status: TileStatus::Idle,
                    });
                    v.monitor.announce.insert(c.info.key.clone());
                    added.push((c.info.key.clone(), c.info.product_string.clone()));
                }
                v.devices.selected = v.devices.selected.min(v.devices.tiles.len().saturating_sub(1));
                refresh = v
                    .devices
                    .tiles
                    .iter()
                    .filter(|t| match t.status {
                        TileStatus::Idle => true,
                        TileStatus::Connected(_) => {
                            reread
                                || v.monitor.announce.contains(&t.key)
                                || (receivers_due && t.name.contains("receiver"))
                                || (open_due && v.devices.open.as_deref() == Some(t.key.as_str()))
                        }
                        _ => false,
                    })
                    .map(|t| t.key.clone())
                    .collect();
                if removed || !added.is_empty() {
                    v.update_tray();
                    cx.notify();
                }
            });
            for (key, name) in &added {
                Self::load_art(&service, key, name, &view, cx).await;
            }
            for key in refresh {
                let _ = view.update(cx, |v: &mut Self, cx| v.refresh_tile(&key, cx));
            }
            let _ = view.update(cx, |v: &mut Self, _| v.monitor.busy = false);
        })
        .detach();
    }

    /// Re-reads one device's status in the background without touching which
    /// device is selected or showing "Connecting…".
    fn refresh_tile(&mut self, key: &str, cx: &mut Context<Self>) {
        let service = self.hid_service();
        let key = key.to_string();
        let seq = self.devices.write_seq;
        cx.spawn(async move |view, cx| {
            let connected = service.connect(&key).await;
            let mut new_name = None;
            let _ = view.update(cx, |v: &mut Self, cx| {
                // A write began or landed meanwhile: this read predates it.
                if v.devices.write_seq != seq || v.devices.writing.is_some() {
                    return;
                }
                let Some(tile) = v.devices.tiles.iter_mut().find(|t| t.key == key) else { return };
                let Ok(Ok(device)) = connected else {
                    if matches!(tile.status, TileStatus::Idle) {
                        tile.status = TileStatus::Failed("No driver answered on this interface.".into());
                    }
                    return;
                };
                let mut woke = false;
                if let Some(name) = device
                    .status
                    .get("name")
                    .and_then(|n| n.as_str())
                    .filter(|n| !n.is_empty() && *n != tile.name)
                {
                    // A receiver that only knew itself now names its mouse: the
                    // mouse was switched on or came back from the cable.
                    woke = tile.name.contains("receiver") && !name.contains("receiver");
                    tile.name = name.to_string();
                    new_name = Some(name.to_string());
                }
                tile.brand = device.brand.clone();
                tile.display_brand = device.display_brand.clone().filter(|b| !b.is_empty()).unwrap_or_else(|| device.brand.clone());
                tile.status = TileStatus::Connected(DeviceReadout::from_status(&device.status, device.controls.clone(), device.tabs.clone().unwrap_or_default()));
                if woke {
                    v.monitor.announce.insert(key.clone());
                }
                v.after_status(&key, cx);
                cx.notify();
            });
            if let Some(name) = new_name {
                Self::load_art(&service, &key, &name, &view, cx).await;
            }
        })
        .detach();
    }

    /// After a status read: the connect sound for a device that just appeared,
    /// the low-battery alert, and the tray's battery.
    fn after_status(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(tile) = self.devices.tiles.iter().find(|t| t.key == key) else { return };
        let TileStatus::Connected(readout) = &tile.status else { return };
        let name = tile.name.clone();
        let battery = readout.battery.map(|b| b.clamp(0.0, 100.0).round() as u8);
        let charging = readout.charging;
        if self.monitor.announce.remove(key) {
            let detail = match battery {
                Some(percent) if charging => format!("Connected · {percent}% · Charging"),
                Some(percent) => format!("Connected · {percent}% battery"),
                None => "Connected".to_string(),
            };
            self.notify_banner(name.clone(), detail, cx);
        }
        if let Some(percent) = battery {
            let log = self.monitor.battery_log.entry(name.clone()).or_default();
            if crate::battery_history::record(log, percent) {
                crate::battery_history::save(&self.monitor.battery_log);
            }
        }
        if let Some(percent) = battery {
            let due = self
                .monitor
                .low_alerted
                .get(key)
                .is_none_or(|at| at.elapsed() >= LOW_BATTERY_COOLDOWN);
            if !charging && percent <= self.settings.low_battery_percent && due {
                self.monitor.low_alerted.insert(key.to_string(), std::time::Instant::now());
                self.notify_banner("Mouse battery is low".into(), format!("{name} has {percent}% battery remaining"), cx);
            }
        }
        // Fully charged: once per charge, when a charging mouse reaches 100 %.
        if charging && battery == Some(100) {
            if self.monitor.charged_alerted.insert(name.clone()) {
                self.notify_banner(format!("{name} is fully charged"), "100% battery · you can unplug it".into(), cx);
            }
        } else if !charging {
            self.monitor.charged_alerted.remove(&name);
        }
        self.update_tray();
    }

    /// Bridge's banner and chime: a mouse connecting or running low.
    fn notify_banner(&mut self, title: String, detail: String, cx: &mut Context<Self>) {
        if self.settings.device_banner {
            let corner = self.settings.overlay_corner;
            self.overlay = crate::overlay::show_banner(cx, self.overlay.take(), title, Some(detail), corner);
        }
        if self.settings.device_sound {
            crate::audio::play_notification();
        }
    }

    /// The tray shows the open device's battery, else the first connected
    /// device that reports one, else the app icon.
    fn update_tray(&self) {
        let reports = |t: &&Tile| matches!(&t.status, TileStatus::Connected(r) if r.battery.is_some());
        let tile = self
            .devices
            .open
            .as_ref()
            .and_then(|key| self.devices.tiles.iter().find(|t| &t.key == key))
            .filter(reports)
            .or_else(|| self.devices.tiles.iter().find(reports));
        let battery = tile.and_then(|t| match &t.status {
            TileStatus::Connected(r) => r.battery.map(|b| crate::tray_battery::TrayBattery {
                device_name: t.name.clone(),
                percent: b.clamp(0.0, 100.0).round() as u8,
                charging: r.charging,
                style: crate::tray_battery::TrayBatteryStyle::Percentage,
            }),
            _ => None,
        });
        crate::tray::show_battery(battery.as_ref(), self.settings.low_battery_percent);
        let menu = tile.and_then(|t| match &t.status {
            TileStatus::Connected(r) => {
                let in_use = r.stage_count.unwrap_or(r.stages.len()).min(r.stages.len());
                let title = match r.battery {
                    Some(b) => format!("{} · {}%{}", t.name, b.clamp(0.0, 100.0).round() as u8, if r.charging { " (charging)" } else { "" }),
                    None => t.name.clone(),
                };
                let stages = if in_use > 1 {
                    r.stages
                        .iter()
                        .take(in_use)
                        .enumerate()
                        .map(|(i, dpi)| (format!("Stage {} · {} DPI", i + 1, crate::web_style::group(*dpi)), r.active_stage == Some(i)))
                        .collect()
                } else {
                    Vec::new()
                };
                Some(crate::tray::TrayMenu { title, stages })
            }
            _ => None,
        });
        crate::tray::show_menu(menu.as_ref());
    }

    /// The device the tray shows: the open one, else the first that reports a battery.
    fn tray_device(&self) -> Option<String> {
        let reports = |t: &&Tile| matches!(&t.status, TileStatus::Connected(r) if r.battery.is_some());
        self.devices
            .open
            .as_ref()
            .and_then(|key| self.devices.tiles.iter().find(|t| &t.key == key))
            .filter(reports)
            .or_else(|| self.devices.tiles.iter().find(reports))
            .map(|t| t.key.clone())
    }

    /// A DPI stage picked from the tray menu.
    fn tray_stage(&mut self, stage: usize, cx: &mut Context<Self>) {
        let Some(key) = self.tray_device() else { return };
        let Some(index) = self.devices.tiles.iter().position(|t| t.key == key) else { return };
        self.devices.selected = index;
        self.set_control("perf.dpiStage".into(), serde_json::json!(stage), cx);
    }

    /// Opens a device's settings page, connecting it first if needed.
    fn open_device(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(index) = self.devices.tiles.iter().position(|t| t.key == key) else { return };
        self.devices.open = Some(key.to_string());
        self.devices.selected = index;
        self.devices.staged = Staged::default();
        self.devices.unlinked_axes = false;
        self.devices.tab = DeviceTab::Overview;
        self.devices.expanded = None;
        self.devices.onboard = None;
        self.devices.write_error = None;
        if matches!(
            self.devices.tiles[index].status,
            TileStatus::Idle | TileStatus::Failed(_)
        ) {
            self.connect_tile(key, false, cx);
        }
        cx.notify();
    }

    /// Reflects a confirmed DPI stage write straight away (the active stage, or
    /// one stage's value), instead of waiting for the next status read.
    fn patch_stage(&mut self, key: &str, id: &str, value: &serde_json::Value) {
        let Some(Tile { status: TileStatus::Connected(r), .. }) = self.devices.tiles.iter_mut().find(|t| t.key == key) else { return };
        match id {
            "perf.dpiStageCount" => {
                if let Some(count) = value.as_u64().map(|n| (n as usize).clamp(1, r.stages.len().max(1))) {
                    r.stage_count = Some(count);
                    if r.active_stage.is_some_and(|a| a >= count) {
                        r.active_stage = Some(count - 1);
                        r.dpi = r.stages.get(count - 1).copied().or(r.dpi);
                        r.dpi_y = r.dpi;
                    }
                }
                return;
            }
            "perf.dpiStageColor" => {
                let stage = value.get("stage").and_then(|s| s.as_u64()).map(|n| n as usize);
                let color = value.get("color").and_then(|c| c.as_str());
                if let (Some(stage), Some(color)) = (stage, color) {
                    if let Some(slot) = r.stage_colors.get_mut(stage) {
                        *slot = color.to_ascii_lowercase();
                    }
                }
                return;
            }
            _ => {}
        }
        let (stage, dpi) = match id {
            "perf.dpiStage" => (value.as_u64().map(|n| n as usize), None),
            "perf.dpiStageValue" => (
                value.get("stage").and_then(|s| s.as_u64()).map(|n| n as usize),
                value.get("dpi").and_then(|d| d.as_f64()),
            ),
            _ => return,
        };
        let Some(stage) = stage.filter(|s| *s < r.stages.len()) else { return };
        if let Some(dpi) = dpi {
            r.stages[stage] = dpi;
            let live_key = format!("{key}/stage/{stage}");
            if self.slider_live.get(&live_key).is_some_and(|v| (*v - dpi).abs() < 0.5) {
                self.slider_live.remove(&live_key);
            }
        } else {
            r.active_stage = Some(stage);
        }
        if r.active_stage == Some(stage) {
            r.dpi = Some(r.stages[stage]);
            r.dpi_y = r.dpi;
        }
    }

    /// Records the Y sensitivity of a confirmed unlinked write.
    fn patch_dpi_y(&mut self, key: &str, y: f64) {
        if let Some(Tile { status: TileStatus::Connected(r), .. }) = self.devices.tiles.iter_mut().find(|t| t.key == key) {
            r.dpi_y = Some(y);
        }
    }

    /// Reflects a confirmed write in the page. The value is what the driver
    /// reports the mouse applied (it can clamp or round), and it is used
    /// instead of a re-read: a Logitech mouse answers its get-report-rate call
    /// with a stale value after a live change (the driver only knows the real
    /// rate from the change notification it saw), so re-reading would show the
    /// old rate straight after a successful write. The Tauri app patches its
    /// status the same way.
    fn patch_readout(&mut self, key: &str, action: &str, applied: &serde_json::Value) {
        let mut left_onboard = false;
        let Some(tile) = self.devices.tiles.iter_mut().find(|t| t.key == key) else { return };
        let TileStatus::Connected(r) = &mut tile.status else { return };
        // A Logitech mouse behind its receiver is put under software control by
        // these writes, which stops it running its stored profiles.
        if matches!(action, "dpi" | "pollingRate" | "liftOff" | "surface")
            && r.onboard_profiles
            && r.connection.as_deref() == Some("Wireless")
            && r.mode.as_deref() == Some("Onboard")
        {
            r.mode = Some("Host".to_string());
            left_onboard = true;
        }
        match action {
            "dpi" => {
                r.dpi = applied.as_f64().or(r.dpi);
                // Linked writes put Y on X as well; an unlinked pair sets Y after this.
                r.dpi_y = r.dpi;
                // A stage mouse takes the new value as its active stage's.
                if let (Some(active), Some(dpi)) = (r.active_stage, r.dpi) {
                    if let Some(slot) = r.stages.get_mut(active) {
                        *slot = dpi;
                    }
                }
            }
            "pollingRate" => r.polling_hz = applied.as_f64().or(r.polling_hz),
            "liftOff" => r.lod = applied.as_str().map(str::to_string).or_else(|| r.lod.take()),
            "surface" => r.surface = applied.as_str().map(str::to_string).or_else(|| r.surface.take()),
            "lighting" => {
                if let Some(l) = &mut r.tabs.lighting {
                    if let Some(mode) = applied.get("mode").and_then(|v| v.as_str()) {
                        l.mode = Some(mode.to_string());
                    }
                    l.color = applied.get("color").and_then(|v| v.as_str()).map(str::to_string).or_else(|| l.color.take());
                    l.color2 = applied.get("color2").and_then(|v| v.as_str()).map(str::to_string).or_else(|| l.color2.take());
                    l.speed = applied.get("speed").and_then(|v| v.as_f64()).or(l.speed);
                }
            }
            _ => {}
        }
        if left_onboard {
            // Which profile is running is no longer what we last read.
            self.devices.onboard = None;
        }
    }

    /// Reads the open Logitech mouse's stored profiles. On demand: a full pass
    /// is dozens of HID++ round trips.
    fn load_onboard_profiles(&mut self, cx: &mut Context<Self>) {
        if self.devices.onboard_loading {
            return;
        }
        let Some(tile) = self.devices.tiles.get(self.devices.selected) else { return };
        let key = tile.key.clone();
        self.devices.onboard_loading = true;
        self.devices.write_error = None;
        cx.notify();
        let service = self.hid_service();
        cx.spawn(async move |view, cx| {
            let result = service.onboard_profiles(&key).await;
            let _ = view.update(cx, |v: &mut Self, cx| {
                v.devices.onboard_loading = false;
                match result {
                    Ok(Ok(list)) => v.devices.onboard = Some(list),
                    Ok(Err(error)) => v.devices.write_error = Some(error),
                    Err(_) => v.devices.write_error = Some("device runtime stopped".into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// `mode`: switch Onboard / Host. `useProfile`: run a stored profile (this
    /// also puts the mouse in Onboard mode, which is the only mode that runs
    /// them). Neither writes flash.
    fn onboard_action(&mut self, action: &'static str, value: serde_json::Value, cx: &mut Context<Self>) {
        if self.devices.writing.is_some() {
            return;
        }
        let Some(tile) = self.devices.tiles.get(self.devices.selected) else { return };
        let key = tile.key.clone();
        self.devices.writing = Some(action.to_string());
        self.devices.write_error = None;
        cx.notify();
        let service = self.hid_service();
        cx.spawn(async move |view, cx| {
            let result = service.onboard_action(&key, action, value.clone()).await;
            let _ = view.update(cx, |v: &mut Self, cx| {
                v.devices.writing = None;
                match result {
                    Ok(Ok(applied)) => {
                        if let Some(tile) = v.devices.tiles.iter_mut().find(|t| t.key == key) {
                            if let TileStatus::Connected(r) = &mut tile.status {
                                r.mode = Some(if action == "mode" {
                                    applied.as_str().unwrap_or("Host").to_string()
                                } else {
                                    "Onboard".to_string()
                                });
                            }
                        }
                        if action == "useProfile" {
                            v.show_toast(format!("Now running profile {}.", value), false, cx);
                        }
                        // Which profile is running changed; read the list again.
                        v.load_onboard_profiles(cx);
                    }
                    Ok(Err(error)) => v.devices.write_error = Some(error),
                    Err(_) => v.devices.write_error = Some("device runtime stopped".into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Writes one Buttons/Advanced control (always immediate, as in the Tauri
    /// app) and shows the value that was sent once the mouse accepts it.
    fn set_control(&mut self, id: String, value: serde_json::Value, cx: &mut Context<Self>) {
        // A stage edit made while another write is in flight (typing, then
        // dragging) waits its turn instead of being dropped.
        if self.devices.writing.is_some() && id.starts_with("perf.dpiStage") {
            cx.spawn(async move |view, cx| {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let _ = view.update(cx, |v: &mut Self, cx| v.set_control(id, value, cx));
            })
            .detach();
            return;
        }
        if self.devices.writing.is_some() || (id.starts_with("perf.") && self.blocked_by_profile(cx)) {
            return;
        }
        let Some(tile) = self.devices.tiles.get(self.devices.selected) else { return };
        let (key, brand) = (tile.key.clone(), tile.brand.clone());
        // "Link left & right" is one preference for every device that has it.
        if id.ends_with(".link") {
            if let Some(linked) = value.as_bool() {
                self.settings.link_left_right = Some(linked);
                self.settings.save();
                self.hid_service().set_prefs(Some(linked));
            }
        }
        self.devices.writing = Some(id.clone());
        self.devices.write_seq += 1;
        self.devices.write_error = None;
        cx.notify();
        let service = self.hid_service();
        cx.spawn(async move |view, cx| {
            let result = service.write(&key, &brand, &format!("control:{id}"), value.clone()).await;
            let _ = view.update(cx, |v: &mut Self, cx| {
                v.devices.writing = None;
                v.devices.write_seq += 1;
                match result {
                    Ok(Ok(applied)) => {
                        v.patch_stage(&key, &id, &value);
                        if id.starts_with("perf.") {
                            v.refresh_presence(cx);
                            v.update_tray();
                        }
                        // A control that reshapes the page (a switch gaining analog settings) returns the new layout.
                        let new_tabs = applied.get("tabs").and_then(|t| serde_json::from_value::<crate::hid::Tabs>(t.clone()).ok());
                        if let Some(tile) = v.devices.tiles.iter_mut().find(|t| t.key == key) {
                            if let (Some(tabs), TileStatus::Connected(r)) = (new_tabs, &mut tile.status) {
                                r.tabs = tabs;
                            }
                        }
                        // A Logitech button: reflect it in the stored profile we hold.
                        if let Some(n) = id.strip_prefix("logi.button.primary.").and_then(|n| n.parse::<usize>().ok()) {
                            if let Some(running) = v
                                .devices
                                .onboard
                                .as_mut()
                                .and_then(|o| o.profiles.iter_mut().find(|p| p.is_current))
                            {
                                if let Some(slot) = running.button_assignments.get_mut(n) {
                                    slot["action"] = value.clone();
                                }
                            }
                        }
                        if id.starts_with("logi.button.") {
                            v.show_toast(format!("Button set to {}.", value.as_str().unwrap_or("the new action")), false, cx);
                        }
                        if let Some(tile) = v.devices.tiles.iter_mut().find(|t| t.key == key) {
                            if let TileStatus::Connected(r) = &mut tile.status {
                                for section in r.tabs.buttons.iter_mut().chain(r.tabs.advanced.iter_mut()).chain(r.tabs.performance.iter_mut()) {
                                    for control in &mut section.controls {
                                        if control.id == id {
                                            control.current = value.clone();
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(Err(error)) => v.devices.write_error = Some(error),
                    Err(_) => v.devices.write_error = Some("device runtime stopped".into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The lighting as it would be written now: staged fields over the device's.
    fn effective_lighting(&self, spec: &crate::hid::LightingSpec) -> serde_json::Value {
        let e = &self.devices.staged.light;
        serde_json::json!({
            "mode": e.mode.clone().or_else(|| spec.mode.clone()),
            "color": e.color.clone().or_else(|| spec.color.clone()),
            "color2": e.color2.clone().or_else(|| spec.color2.clone()),
            "speed": e.speed.or(spec.speed),
        })
    }

    /// One lighting edit: written now with instant apply, staged otherwise.
    fn edit_lighting(
        &mut self,
        apply: impl FnOnce(&mut LightEdit),
        spec: &crate::hid::LightingSpec,
        cx: &mut Context<Self>,
    ) {
        apply(&mut self.devices.staged.light);
        if self.settings.instant_apply {
            let value = self.effective_lighting(spec);
            self.devices.staged.light = LightEdit::default();
            self.write_setting("lighting", value, cx);
        }
        cx.notify();
        }

    /// Writes one setting and shows the value the mouse confirmed.
    fn write_setting(&mut self, action: &'static str, value: serde_json::Value, cx: &mut Context<Self>) {
        if self.devices.writing.is_some() {
            return;
        }
        let Some(tile) = self.devices.tiles.get(self.devices.selected) else { return };
        let (key, brand) = (tile.key.clone(), tile.brand.clone());
        let requested_y = value.get("y").and_then(|y| y.as_f64());
        self.devices.writing = Some(action.to_string());
        self.devices.write_seq += 1;
        self.devices.write_error = None;
        cx.notify();
        let service = self.hid_service();
        cx.spawn(async move |view, cx| {
            let result = service.write(&key, &brand, action, value).await;
            let _ = view.update(cx, |v: &mut Self, cx| {
                v.devices.writing = None;
                v.devices.write_seq += 1;
                match result {
                    Ok(Ok(applied)) => {
                        v.patch_readout(&key, action, &applied);
                        if let Some(y) = requested_y {
                            v.patch_dpi_y(&key, y);
                        }
                        v.refresh_presence(cx);
                        v.update_tray();
                    }
                    Ok(Err(error)) => v.devices.write_error = Some(error),
                    Err(_) => v.devices.write_error = Some("device runtime stopped".into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Writes every staged edit in order (one device session each) and shows
    /// what the mouse confirmed. Stops at the first failure and keeps the rest
    /// staged so nothing is silently dropped.
    fn apply_staged(&mut self, cx: &mut Context<Self>) {
        if self.devices.writing.is_some() || self.devices.staged.is_empty() {
            return;
        }
        let Some(tile) = self.devices.tiles.get(self.devices.selected) else { return };
        let (key, brand) = (tile.key.clone(), tile.brand.clone());
        let staged = self.devices.staged.clone();
        let mut jobs: Vec<(&'static str, serde_json::Value)> = Vec::new();
        if staged.dpi.is_some() || staged.dpi_y.is_some() {
            let current_x = match self.devices.tiles.get(self.devices.selected).map(|t| &t.status) {
                Some(TileStatus::Connected(r)) => r.dpi,
                _ => None,
            };
            let x = staged.dpi.or(current_x).unwrap_or(0.0);
            jobs.push(("dpi", match staged.dpi_y {
                Some(y) => serde_json::json!({ "x": x, "y": y }),
                None => serde_json::json!(x),
            }));
        }
        if let Some(v) = staged.polling {
            jobs.push(("pollingRate", serde_json::json!(v)));
        }
        if let Some(v) = staged.lod.clone() {
            jobs.push(("liftOff", serde_json::json!(v)));
        }
        if let Some(v) = staged.surface.clone() {
            jobs.push(("surface", serde_json::json!(v)));
        }
        let light_job = if staged.light == LightEdit::default() {
            None
        } else {
            match &self.devices.tiles.get(self.devices.selected).map(|t| &t.status) {
                Some(TileStatus::Connected(r)) => r.tabs.lighting.as_ref().map(|spec| self.effective_lighting(spec)),
                _ => None,
            }
        };
        if let Some(value) = light_job {
            jobs.push(("lighting", value));
        }
        self.devices.writing = Some("apply".into());
        self.devices.write_seq += 1;
        self.devices.write_error = None;
        cx.notify();
        let service = self.hid_service();
        cx.spawn(async move |view, cx| {
            let mut failure = None;
            let mut done = Staged::default();
            let mut applied_values: Vec<(&'static str, serde_json::Value)> = Vec::new();
            for (action, value) in jobs {
                match service.write(&key, &brand, action, value).await {
                    Ok(Ok(applied)) => {
                        applied_values.push((action, applied));
                        match action {
                        "dpi" => {
                            done.dpi = staged.dpi;
                            done.dpi_y = staged.dpi_y;
                        }
                        "pollingRate" => done.polling = staged.polling,
                        "liftOff" => done.lod = staged.lod.clone(),
                        "lighting" => done.light = staged.light.clone(),
                        _ => done.surface = staged.surface.clone(),
                        }
                    }
                    Ok(Err(error)) => {
                        failure = Some(error);
                        break;
                    }
                    Err(_) => {
                        failure = Some("device runtime stopped".to_string());
                        break;
                    }
                }
            }
            let _ = view.update(cx, |v: &mut Self, cx| {
                v.devices.writing = None;
                v.devices.write_seq += 1;
                let s = &mut v.devices.staged;
                if done.dpi.is_some() { s.dpi = None; }
                if done.dpi_y.is_some() { s.dpi_y = None; }
                if done.polling.is_some() { s.polling = None; }
                if done.lod.is_some() { s.lod = None; }
                if done.surface.is_some() { s.surface = None; }
                if done.light != LightEdit::default() { s.light = LightEdit::default(); }
                v.devices.write_error = failure;
                for (action, applied) in &applied_values {
                    v.patch_readout(&key, action, applied);
                    if *action == "dpi" {
                        if let Some(y) = staged.dpi_y {
                            v.patch_dpi_y(&key, y);
                        }
                    }
                }
                v.refresh_presence(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// The game whose profile currently controls the open device, if any. While
    /// one does, the Performance tab is locked: its settings are the profile's
    /// until the game closes (the Tauri tab does the same).
    fn locked_game(&self) -> Option<String> {
        let key = &self.devices.tiles.get(self.devices.selected)?.key;
        self.profile_overrides.iter().find(|o| &o.device == key).map(|o| o.game_name.clone())
    }

    /// Refuses an edit with a note when [`Self::locked_game`] applies.
    fn blocked_by_profile(&mut self, cx: &mut Context<Self>) -> bool {
        match self.locked_game() {
            Some(game) => {
                self.show_toast(format!("Locked while {game} is running — your game profile is in control."), true, cx);
                true
            }
            None => false,
        }
    }

    /// A DPI edit. Linked (the default) writes one number for both axes; with the
    /// axes unlinked, X and Y go together as a pair.
    fn edit_dpi(&mut self, x: f64, y: Option<f64>, cx: &mut Context<Self>) {
        if self.blocked_by_profile(cx) {
            return;
        }
        let unlinked = self.devices.unlinked_axes;
        if self.settings.instant_apply {
            let value = if unlinked { serde_json::json!({ "x": x, "y": y.unwrap_or(x) }) } else { serde_json::json!(x) };
            self.write_setting("dpi", value, cx);
            return;
        }
        self.devices.staged.dpi = Some(x);
        self.devices.staged.dpi_y = unlinked.then(|| y.unwrap_or(x));
        cx.notify();
    }

    /// Records an edit: written now with instant apply, staged otherwise.
    fn edit(&mut self, action: &'static str, value: serde_json::Value, cx: &mut Context<Self>) {
        if self.blocked_by_profile(cx) {
            return;
        }
        if self.settings.instant_apply {
            self.write_setting(action, value, cx);
            return;
        }
        let st = &mut self.devices.staged;
        match action {
            "dpi" => st.dpi = value.as_f64(),
            "pollingRate" => st.polling = value.as_u64().map(|n| n as u32),
            "liftOff" => st.lod = value.as_str().map(str::to_string),
            _ => st.surface = value.as_str().map(str::to_string),
        }
        cx.notify();
    }

    /// Opens one device, finds the driver that answers and reads its status.
    fn connect_tile(&mut self, key: &str, quiet: bool, cx: &mut Context<Self>) {
        let Some(index) = self.devices.tiles.iter().position(|t| t.key == key) else { return };
        if matches!(self.devices.tiles[index].status, TileStatus::Connecting) {
            return;
        }
        // A refresh after a write keeps showing the old readout until the new
        // one lands, so the page does not flash "Connecting…".
        if !quiet {
            self.devices.tiles[index].status = TileStatus::Connecting;
        }
        self.devices.selected = index;
        cx.notify();
        let service = self.hid_service();
        service.set_prefs(self.settings.link_left_right);
        let key = key.to_string();
        cx.spawn(async move |view, cx| {
            let connected = service.connect(&key).await;
            let mut new_name = None;
            let _ = view.update(cx, |v: &mut Self, cx| {
                let mut connected_now = false;
                let Some(tile) = v.devices.tiles.iter_mut().find(|t| t.key == key) else { return };
                match connected {
                    Ok(Ok(device)) => {
                        if let Some(name) = device
                            .status
                            .get("name")
                            .and_then(|n| n.as_str())
                            .filter(|n| !n.is_empty() && *n != tile.name)
                        {
                            tile.name = name.to_string();
                            new_name = Some(name.to_string());
                        }
                        tile.brand = device.brand.clone();
                        tile.display_brand = device.display_brand.clone().filter(|b| !b.is_empty()).unwrap_or_else(|| device.brand.clone());
                        connected_now = true;
                        tile.status = TileStatus::Connected(DeviceReadout::from_status(&device.status, device.controls.clone(), device.tabs.clone().unwrap_or_default()));
                    }
                    Ok(Err(error)) => {
                        if !(quiet && matches!(tile.status, TileStatus::Connected(_))) {
                            tile.status = TileStatus::Failed(error);
                        }
                    }
                    Err(_) => tile.status = TileStatus::Failed("device runtime stopped".into()),
                }
                if connected_now {
                    v.sync_running_profiles(cx);
                    v.after_status(&key, cx);
                }
                v.refresh_presence(cx);
                cx.notify();
            });
            // Receivers share product ids, so the artwork is only knowable
            // from the name the mouse itself reports.
            if let Some(name) = new_name {
                Self::load_art(&service, &key, &name, &view, cx).await;
            }
        })
        .detach();
    }

    pub(crate) fn apply_game_updates(
        &mut self,
        updates: &[crate::game_watcher::GameUpdate],
        cx: &mut Context<Self>,
    ) {
        // Edges, found by comparing with what was known before this batch.
        let mut started = Vec::new();
        let mut closed = Vec::new();
        for update in updates {
            let Some(running) = update.running else { continue };
            let was = self.games.iter().find(|g| g.id == update.id).map(|g| g.running);
            match (was, running) {
                (Some(false), true) => started.push(update.id.clone()),
                (Some(true), false) => closed.push(update.id.clone()),
                _ => {}
            }
        }
        if crate::game_watcher::apply_updates(&mut self.games, updates) {
            cx.notify();
            self.refresh_presence(cx);
        }
        for id in closed {
            self.on_game_closed(&id, cx);
        }
        for id in started {
            self.on_game_started(&id, cx);
        }
    }

    // ── toasts ───────────────────────────────────────────────────────────

    fn show_toast(&mut self, text: impl Into<String>, error: bool, cx: &mut Context<Self>) {
        self.toast_seq += 1;
        let id = self.toast_seq;
        self.toast = Some(Toast { id, text: text.into(), error });
        cx.notify();
        cx.spawn(async move |view, cx| {
            cx.background_executor().timer(std::time::Duration::from_secs(4)).await;
            let _ = view.update(cx, |v: &mut Self, cx| {
                if v.toast.as_ref().is_some_and(|t| t.id == id) {
                    v.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// A message about a game: on the screen overlay (the game has focus, so
    /// the main window is not where the player is looking) or, for something
    /// they did in this window, as an in-app toast.
    fn announce(&mut self, text: String, error: bool, overlay: bool, cx: &mut Context<Self>) {
        if overlay {
            // Off means off: nothing on the screen.
            if self.settings.overlay_enabled {
                let corner = self.settings.overlay_corner;
                self.overlay = crate::overlay::show(cx, self.overlay.take(), text, error, corner);
                if self.settings.overlay_sound {
                    crate::audio::play(if error { crate::audio::Chime::Error } else { crate::audio::Chime::Success });
                }
            }
        } else {
            self.show_toast(text, error, cx);
        }
    }

    // ── game profiles ────────────────────────────────────────────────────

    /// Every connected device with its readout, in list order.
    fn connected_devices(&self) -> Vec<(String, &DeviceReadout)> {
        self.devices
            .tiles
            .iter()
            .filter_map(|t| match &t.status {
                TileStatus::Connected(r) => Some((t.key.clone(), r)),
                _ => None,
            })
            .collect()
    }

    /// The device that inherits a profile saved before profiles were per-device:
    /// the first mouse-like (has a DPI) connected one.
    fn legacy_heir(&self) -> Option<String> {
        self.connected_devices().into_iter().find(|(_, r)| r.dpi.is_some()).map(|(k, _)| k)
    }

    /// The profile a game has for this device (its own, or the inherited legacy one).
    fn profile_for(&self, game_id: &str, device: &str) -> Option<crate::profiles::GameProfile> {
        let heir = self.legacy_heir().as_deref() == Some(device);
        self.profiles.effective(game_id, device, heir).cloned()
    }

    fn game_name(&self, id: &str) -> String {
        self.games.iter().find(|g| g.id == id).map(|g| g.name.clone()).unwrap_or_else(|| id.to_string())
    }

    /// A game launched: announce profiles that don't auto-apply, and apply the
    /// ones that do.
    fn on_game_started(&mut self, id: &str, cx: &mut Context<Self>) {
        let notice = self
            .connected_devices()
            .iter()
            .filter_map(|(key, _)| self.profile_for(id, key))
            .find(|p| p.is_meaningful() && !p.auto_apply)
            .map(|p| p.describe());
        if let Some(described) = notice {
            let text = format!("\"{}\" has a profile ({described}). Apply it from the Games page.", self.game_name(id));
            self.announce(text, false, true, cx);
        }
        self.sync_running_profiles(cx);
    }

    /// Applies every auto-apply profile that should be in force: for each game
    /// that is running and each connected device, unless that pair is already
    /// applied or being applied. Safe to call whenever a game starts or a
    /// device connects — a device that shows up after the game launched is
    /// picked up here too.
    fn sync_running_profiles(&mut self, cx: &mut Context<Self>) {
        let running: Vec<String> = self.games.iter().filter(|g| g.running).map(|g| g.id.clone()).collect();
        let devices: Vec<String> = self.connected_devices().into_iter().map(|(k, _)| k).collect();
        for game in running {
            for device in &devices {
                let Some(profile) = self.profile_for(&game, device) else { continue };
                if !profile.auto_apply || !profile.is_meaningful() {
                    continue;
                }
                let pair = (game.clone(), device.clone());
                let taken = self.profile_overrides.iter().any(|o| o.game_id == game && &o.device == device);
                if taken || self.profile_inflight.contains(&pair) {
                    continue;
                }
                self.apply_profile(&game, device, profile, true, cx);
            }
        }
    }

    /// The game closed: put back what each of its profiles replaced.
    fn on_game_closed(&mut self, id: &str, cx: &mut Context<Self>) {
        let (closing, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.profile_overrides).into_iter().partition(|o| o.game_id == id);
        self.profile_overrides = keep;
        for active in closing {
            if !active.restore.is_meaningful() {
                continue;
            }
            let (name, described) = (active.game_name.clone(), active.restore.describe());
            let text = format!("{name} closed — restored your default {described}.");
            self.apply_profile_inner(id, &name, &active.device, active.restore, None, Some(text), true, cx);
        }
    }

    /// Pushes a profile to one connected device. `auto` marks a launch-triggered
    /// apply, which remembers the pre-game settings to restore on close.
    fn apply_profile(&mut self, game_id: &str, device: &str, profile: crate::profiles::GameProfile, auto: bool, cx: &mut Context<Self>) {
        let Some(r) = self.connected_devices().into_iter().find(|(k, _)| k == device).map(|(_, r)| r) else {
            self.announce("Connect this device to apply a profile.".into(), true, auto, cx);
            return;
        };
        let name = self.game_name(game_id);
        let described = profile.describe();
        let override_ = if auto {
            // The first game to take over a device defines its "default"; a
            // second game taking over from a running one only changes who is in
            // control.
            let restore = match self.profile_overrides.iter().find(|o| o.device == device) {
                Some(active) => active.restore.clone(),
                None => crate::profiles::GameProfile {
                    dpi: profile.dpi.and(r.dpi),
                    polling: profile.polling.and(r.polling_hz.map(|hz| hz as u32)),
                    lod: profile.lod.as_ref().and(r.lod.clone()),
                    surface: profile.surface.as_ref().and(r.surface.clone()),
                    auto_apply: false,
                },
            };
            Some(ProfileOverride { game_id: game_id.to_string(), game_name: name.clone(), device: device.to_string(), restore })
        } else {
            None
        };
        if auto {
            self.profile_inflight.push((game_id.to_string(), device.to_string()));
        }
        self.apply_profile_inner(game_id, &name, device, profile, override_, Some(format!("Applied \"{name}\" profile — {described}.")), auto, cx);
    }

    fn apply_profile_inner(
        &mut self,
        game_id: &str,
        name: &str,
        device: &str,
        profile: crate::profiles::GameProfile,
        override_: Option<ProfileOverride>,
        success: Option<String>,
        overlay: bool,
        cx: &mut Context<Self>,
    ) {
        let (game_id, name, key) = (game_id.to_string(), name.to_string(), device.to_string());
        let service = self.hid_service();
        cx.spawn(async move |view, cx| {
            let result = service.apply_profile(&key, profile.to_js()).await;
            let _ = view.update(cx, |v: &mut Self, cx| {
                v.profile_inflight.retain(|(g, d)| !(g == &game_id && d == &key));
                match result {
                    Ok(Ok(applied)) => {
                        for (field, action) in [
                            ("dpi", "dpi"),
                            ("pollingRateHz", "pollingRate"),
                            ("liftOffDistance", "liftOff"),
                            ("gamingSurfaceMode", "surface"),
                        ] {
                            if let Some(value) = applied.get(field) {
                                v.patch_readout(&key, action, value);
                            }
                        }
                        // Only take over once the write is confirmed to have landed.
                        if let Some(o) = override_ {
                            v.profile_overrides.push(o);
                        }
                        if let Some(text) = success {
                            v.announce(text, false, overlay, cx);
                        }
                        v.refresh_presence(cx);
                        cx.notify();
                    }
                    Ok(Err(error)) => v.announce(format!("Couldn't apply \"{name}\" profile: {error}"), true, overlay, cx),
                    Err(_) => v.announce(format!("Couldn't apply \"{name}\" profile: device runtime stopped"), true, overlay, cx),
                }
            });
        })
        .detach();
    }

    /// Edits (and persists) one game's profile for one device.
    fn edit_profile(&mut self, game_id: &str, device: &str, change: impl FnOnce(&mut crate::profiles::GameProfile), cx: &mut Context<Self>) {
        let mut profile = self.profile_for(game_id, device).unwrap_or_default();
        change(&mut profile);
        self.profiles.set(game_id, device, profile);
        self.profiles.save();
        cx.notify();
    }

    /// Current page, for tests and owners that drive navigation.
    pub fn page(&self) -> Page {
        self.page
    }

    /// Switch page programmatically; mirrors sidebar `on_click`.
    pub fn set_page(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        cx.notify();
    }

    /// Current theme choice.
    pub fn theme(&self) -> ThemeChoice {
        self.settings.theme
    }

    /// Switch theme; mirrors the Settings theme switcher. Paints through
    /// [`crate::theme::apply_choice`] so every window refreshes. Switching
    /// to Dynamic resamples the wallpaper immediately (mirrors Settings'
    /// `force` refresh) instead of showing the fallback until the 2s watcher
    /// tick lands.
    pub fn set_theme(&mut self, theme: ThemeChoice, window: &mut Window, cx: &mut Context<Self>) {
        // Palette before the switch, for the crossfade below.
        let previous = cx.theme().colors;
        self.settings.theme = theme;
        self.settings.save();
        if theme == ThemeChoice::Dynamic {
            let vibrancy = self.settings.dynamic_vibrancy;
            // Instant path: paint the cached hex synchronously (no I/O —
            // mirrors the web app reading `DYNAMIC_HEX_KEY`), then verify
            // the signature off-thread and correct only if the wallpaper
            // actually changed. Same wallpaper → instant, zero subprocess.
            if let Some(accent) = crate::theme::cached_dynamic_accent() {
                crate::theme::apply_choice(
                    ThemeChoice::Dynamic,
                    Some(accent),
                    vibrancy,
                    self.settings.window_opacity / 100.0,
                    self.settings.accent,
                    Some(window),
                    cx,
                );
            }
            cx.spawn({
                async move |view, cx| {
                    let accent = cx
                        .background_executor()
                        .spawn(async { crate::theme::dynamic_accent() })
                        .await;
                    // Fallback green only if sampling truly fails.
                    let accent = accent.unwrap_or((93, 222, 137));
                    let _ = view.update(cx, |view: &mut Self, cx| {
                        if view.settings.theme != ThemeChoice::Dynamic {
                            return;
                        }
                        view.settings.dynamic_hex = Some(accent);
                        view.settings.save();
                        crate::theme::apply_choice(
                            ThemeChoice::Dynamic,
                            Some(accent),
                            vibrancy,
                            view.settings.window_opacity / 100.0,
                            view.settings.accent,
                            None,
                            cx,
                        );
                        cx.notify();
                    });
                }
            })
            .detach();
        } else {
            self.repaint_theme(Some(window), cx);
        }
        // The new palette is live; fade into it, and replay the art's
        // entrance so the field slides back in from the side.
        self.theme_seq += 1;
        crate::theme::crossfade_colors(previous, cx.theme().colors, cx);
        cx.notify();
    }

    /// Set (or clear) the user's accent override. The palette fades to it and
    /// the ASCII field replays its entrance, exactly like a theme switch.
    pub fn set_accent(
        &mut self,
        accent: Option<(u8, u8, u8)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.accent = accent;
        self.settings.save();
        let previous = cx.theme().colors;
        self.repaint_theme(Some(window), cx);
        self.theme_seq += 1;
        crate::theme::crossfade_colors(previous, cx.theme().colors, cx);
        cx.notify();
    }

    /// Wallpaper watcher callback: a fresh sample landed. Repaints only
    /// when Dynamic is the live choice (mirrors `refreshDynamicAccent`'s
    /// signature-moved path); other choices keep their palette.
    pub fn note_wallpaper_sample(
        &mut self,
        accent: (u8, u8, u8),
        _window: gpui_kit::AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        if self.settings.theme != ThemeChoice::Dynamic {
            return;
        }
        // Persist the sample so the next launch repaints it instantly
        // instead of flashing fallback green (mirrors DYNAMIC_HEX_KEY).
        self.settings.dynamic_hex = Some(accent);
        self.settings.save();
        let previous = cx.theme().colors;
        crate::theme::apply_choice(
            ThemeChoice::Dynamic,
            Some(accent),
            self.settings.dynamic_vibrancy,
            self.settings.window_opacity / 100.0,
            self.settings.accent,
            None,
            cx,
        );
        // A new wallpaper is a new palette: fade it in, and replay the art's
        // entrance like a theme switch.
        self.theme_seq += 1;
        crate::theme::crossfade_colors(previous, cx.theme().colors, cx);
        cx.notify();
    }

    /// Update watcher callback: a `latest.json` fetch landed, from either the
    /// background poll or the manual "Check for updates" button. Stores the
    /// result and, if it's newer than `APP_VERSION` and this version hasn't
    /// already been nagged about, toasts once (mirrors
    /// `runUpdateCheck`/`notifiedVersion` in `src/lib/update-check.ts`).
    pub fn note_update_available(&mut self, info: crate::update_check::UpdateInfo, cx: &mut Context<Self>) {
        let is_new = crate::update_check::is_newer(APP_VERSION, &info.version);
        let should_toast = is_new
            && self.settings.last_notified_update_version.as_deref() != Some(info.version.as_str());
        self.update_available = is_new.then(|| info.clone());
        if should_toast {
            self.settings.last_notified_update_version = Some(info.version.clone());
            self.settings.save();
        }
        if is_new && info.asset().is_some() && self.settings.auto_update {
            self.download_update(info, should_toast, cx);
        } else if should_toast {
            self.show_toast(format!("OpenMouse Desktop v{} is available — see Updates.", info.version), false, cx);
        }
        cx.notify();
    }

    /// Fetches and verifies a newer version's installer in the background.
    fn download_update(&mut self, info: crate::update_check::UpdateInfo, announce: bool, cx: &mut Context<Self>) {
        if self.update_downloading || self.update_ready.as_ref().is_some_and(|p| p.version == info.version) {
            return;
        }
        self.update_downloading = true;
        cx.spawn(async move |view, cx| {
            let result = cx.background_executor().spawn(async move { crate::update_check::download(&info) }).await;
            let _ = view.update(cx, |v: &mut Self, cx| {
                v.update_downloading = false;
                match result {
                    Ok(pending) => {
                        if announce {
                            v.show_toast(format!("Update v{} is ready. Restart OpenMouse to install it.", pending.version), false, cx);
                        }
                        v.update_ready = Some(pending);
                    }
                    Err(error) => {
                        if announce {
                            v.show_toast(format!("Couldn't download the update: {error}"), true, cx);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// "Restart to update": hands over to the downloaded installer and quits.
    fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.update_ready.clone() else { return };
        // The installer runs it now; next launch must not run it again.
        let _ = crate::update_check::take_pending("0");
        match crate::update_check::launch_installer(&pending, false) {
            Ok(()) => cx.quit(),
            Err(error) => self.show_toast(format!("Couldn't start the update: {error}"), true, cx),
        }
    }

    /// "Check for updates" button: runs a check right now (not waiting for
    /// the next background-poll tick) and always surfaces *something* —
    /// "up to date" included — since the user explicitly asked.
    fn check_for_updates_now(&mut self, cx: &mut Context<Self>) {
        if self.checking_for_updates {
            return;
        }
        self.checking_for_updates = true;
        cx.notify();
        cx.spawn(async move |view, cx| {
            let info = cx.background_executor().spawn(async { crate::update_check::fetch_latest() }).await;
            let _ = view.update(cx, |view, cx| {
                view.checking_for_updates = false;
                match info {
                    // A manual click always wants an answer, even "nothing to
                    // report" — `note_update_available` itself stays silent
                    // once a version's already been nagged about.
                    Some(info) if !crate::update_check::is_newer(APP_VERSION, &info.version) => {
                        view.note_update_available(info, cx);
                        view.show_toast("You're up to date.", false, cx);
                    }
                    Some(info) => view.note_update_available(info, cx),
                    None => view.show_toast("Couldn't check for updates — check your connection.", true, cx),
                }
            });
        })
        .detach();
    }

    /// Repaint the active theme from current settings (theme + vibrancy).
    fn repaint_theme(&self, window: Option<&mut Window>, cx: &mut App) {
        // Dynamic repaints from the cached sample — passing None would reset
        // the shell to the fallback green on every vibrancy tick.
        let wallpaper = if self.settings.theme == ThemeChoice::Dynamic {
            crate::theme::cached_dynamic_accent()
        } else {
            None
        };
        crate::theme::apply_choice(
            self.settings.theme,
            wallpaper,
            self.settings.dynamic_vibrancy,
            self.settings.window_opacity / 100.0,
            self.settings.accent,
            window,
            cx,
        );
    }

    /// Custom titlebar content mirroring `TitleBar.tsx`: app name plus a
    /// version·stage badge. Dragging, double-click zoom, and the
    /// min/max/close buttons stay owned by Kit's `TitleBar`; this only
    /// supplies the branded row inside it.
    /// Updates: this version, and the release notes as a timeline (version and
    /// date on the left, changes grouped under New / Improved / Fixed on the
    /// right). One virtualized list, so only the rows on screen are built — the
    /// page used to rebuild every release on each scroll tick and pinned a core.
    fn render_updates(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = PageColors {
            fg: cx.theme().foreground,
            muted: cx.theme().muted_foreground,
            accent: cx.theme().primary,
            border: Hsla { a: 0.7, ..cx.theme().border },
        };
        let checking = self.checking_for_updates;
        let on_click = cx.listener(|this, _, _, cx| this.check_for_updates_now(cx));
        let check_button = Button::new("check-updates")
            .compact()
            .label("Check for updates")
            .loading(checking)
            .disabled(checking)
            .on_click(on_click)
            .into_any_element();
        // The click handler lives on the card's button, built here once per render
        // of the page (cheap); the list rows are built lazily below.
        let mut check_button = Some(check_button);
        let ready = self.update_ready.is_some();
        let downloading = self.update_downloading;
        let mut update_banner = self.update_available.as_ref().map(|info| {
            let notes = info.notes.clone().unwrap_or_default();
            h_flex()
                .gap_3()
                .items_center()
                .justify_between()
                .p_4()
                .mt_3()
                .rounded_lg()
                .border_1()
                .border_color(colors.accent)
                .bg(Hsla { a: 0.08, ..colors.accent })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.fg)
                                .child(format!("v{} is available", info.version)),
                        )
                        .children((!notes.is_empty()).then(|| div().text_xs().text_color(colors.muted).child(notes))),
                )
                .child(if ready {
                    Button::new("restart-to-update")
                        .compact()
                        .label("Restart to update")
                        .on_click(cx.listener(|this, _, _, cx| this.restart_to_update(cx)))
                        .into_any_element()
                } else {
                    Button::new("view-release")
                        .compact()
                        .label(if downloading { "Downloading…" } else { "View release" })
                        .on_click(cx.listener(|_, _, _, cx| cx.open_url(crate::update_check::RELEASES_URL)))
                        .into_any_element()
                })
                .into_any_element()
        });

        let entries = crate::changelog::entries();
        let list_state = self.updates_list.clone();
        let stage = release_stage(APP_VERSION);

        list(list_state, move |ix, _window, _cx| {
            let c = colors;
            let body: AnyElement = match ix {
                0 => div()
                    .flex()
                    .flex_col()
                    .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).text_color(c.fg).child("Updates"))
                    .child(div().text_sm().text_color(c.muted).child("Your version and what's changed"))
                    .children(update_banner.take())
                    .into_any_element(),
                1 => h_flex()
                    .gap_4()
                    .items_center()
                    .justify_between()
                    .p_6()
                    .mt_5()
                    .rounded_xl()
                    .border_1()
                    .border_color(Hsla { a: 0.35, ..c.accent })
                    .bg(Hsla { a: 0.06, ..c.accent })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1p5()
                            .child(div().text_xs().text_color(c.muted).child("OpenMouse Desktop"))
                            .child(
                                h_flex()
                                    .gap_3()
                                    .items_center()
                                    .child(div().text_2xl().font_weight(FontWeight::BOLD).text_color(c.fg).child(format!("v{APP_VERSION}")))
                                    .child(accent_badge(stage.to_string(), c.accent, c.fg)),
                            )
                            .child(div().text_sm().text_color(c.muted).child("You're running this version.")),
                    )
                    .children(check_button.take())
                    .into_any_element(),
                2 => div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .pt_8()
                    .pb_1()
                    .child(div().text_base().font_weight(FontWeight::SEMIBOLD).text_color(c.fg).child("Release notes"))
                    .children(entries.is_empty().then(|| div().text_sm().text_color(c.muted).child("No release notes yet.")))
                    .into_any_element(),
                n => match entries.get(n - UPDATES_LEADING_ROWS) {
                    Some(entry) => release_row(entry, entry.version == APP_VERSION, c),
                    None => div().into_any_element(),
                },
            };
            div().w_full().px_4().pt_3().child(div().max_w(px(960.)).child(body)).into_any_element()
        })
        .size_full()
        .into_any_element()
    }

    /// One nav row: icon and label, a tinted fill and an accent bar when active.
    fn nav_row(&self, id: &'static str, icon: IconName, label: &str, active: bool, collapsed: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let accent = cx.theme().primary;
        div()
            .id(id)
            .relative()
            .cursor_pointer()
            .flex()
            .flex_row()
            .items_center()
            .when(collapsed, |row| row.justify_center())
            .gap_3()
            .h(px(40.))
            .px_3()
            .rounded_lg()
            .text_sm()
            .font_weight(if active { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM })
            .text_color(if active { fg } else { muted })
            .when(active, |row| row.bg(Hsla { a: 0.16, ..accent }))
            .when(!active, |row| row.hover(|h| h.bg(Hsla { a: 0.07, ..fg }).text_color(fg)))
            .when(active, |row| {
                row.child(
                    div()
                        .absolute()
                        .left(px(-6.))
                        .top(px(10.))
                        .w(px(3.))
                        .h(px(20.))
                        .rounded_full()
                        .bg(accent),
                )
            })
            .child(Icon::new(icon).size(px(18.)).text_color(if active { accent } else { muted }))
            .when(!collapsed, |row| row.child(label.to_string()))
    }

    /// A footer row (Help & Feedback, Settings): an icon, a title with an optional
    /// subtitle, and a chevron.
    fn footer_row(
        &self,
        id: &'static str,
        glyph: AnyElement,
        title: &str,
        subtitle: Option<&str>,
        active: bool,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        div()
            .id(id)
            .cursor_pointer()
            .flex()
            .flex_row()
            .items_center()
            .when(collapsed, |row| row.justify_center())
            .gap_3()
            .px_3()
            .py_2p5()
            .when(active, |row| row.bg(Hsla { a: 0.14, ..cx.theme().primary }))
            .when(!active, |row| row.hover(|h| h.bg(Hsla { a: 0.06, ..fg })))
            .child(div().w(px(22.)).flex_none().flex().items_center().justify_center().child(glyph))
            .when(!collapsed, |row| {
                row.child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(if active { fg } else { muted }).child(title.to_string()))
                        .children(subtitle.map(|s| div().text_xs().text_color(muted).truncate().child(s.to_string()))),
                )
                .child(Icon::new(IconName::ChevronRight).size(px(14.)).text_color(muted))
            })
    }

    /// Subtle ASCII field behind the whole shell. Rows are cached by grid
    /// size; the accent color is applied at paint time, so switching themes
    /// recolors the field without regenerating it. A theme change also
    /// replays the entrance — the art itself carries the transition, sliding
    /// back in from the right with the top row arriving first.
    fn render_ascii_background(&mut self, window: &Window, cx: &Context<Self>) -> AnyElement {
        // Kit's mono family resolves per platform (Menlo / Consolas /
        // DejaVu) and falls back to an installed family, so the grid always
        // has a real monospace face whose advance it can rely on.
        let font_size = cx.theme().mono_font_size;
        let line_height = font_size.as_f32();
        let viewport = window.viewport_size();
        let cols = ((viewport.width.as_f32() / (line_height * ASCII_CELL_RATIO)).ceil() as usize).max(1);
        let grid_rows = ((viewport.height.as_f32() / line_height).ceil() as usize).max(1);
        if self.ascii_dims != (cols, grid_rows) {
            self.ascii_rows = ascii_background_rows(cols, grid_rows);
            self.ascii_dims = (cols, grid_rows);
        }
        let font_family = cx.theme().mono_font_family.clone();
        let color = cx.theme().primary;
        // On true black the same ink reads far fainter than on the lifted
        // greys of the other themes, so it gets extra gain there.
        let gain = if cx.theme().background.l < 0.03 { ASCII_OLED_GAIN } else { 1.0 };
        let total = self.ascii_rows.len();
        let rows: Vec<(SharedString, f32)> = self
            .ascii_rows
            .iter()
            .enumerate()
            .map(|(index, line)| (line.clone(), (index as f32 + 0.5) / total as f32))
            .collect();
        div()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .w_full()
            .h_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .with_animation(
                // The id carries the theme generation: a new one restarts the
                // entrance instead of leaving the field parked mid-frame.
                format!("ascii-field-{}", self.theme_seq),
                Animation::new(Duration::from_millis(ASCII_SWEEP_MS)),
                move |field, delta| {
                    field.children(rows.iter().map(|(line, position)| {
                        // The wave reaches each row in turn; a row holds its
                        // start offset until then.
                        let local = ((delta - position * ASCII_SWEEP_STAGGER)
                            / (1.0 - ASCII_SWEEP_STAGGER))
                            .clamp(0.0, 1.0);
                        let eased = 1.0 - (1.0 - local).powi(3);
                        div()
                            .h(px(line_height))
                            .flex_none()
                            .left(px((1.0 - eased) * ASCII_SWEEP_OFFSET))
                            .font_family(font_family.clone())
                            .text_size(font_size)
                            .line_height(px(line_height))
                            .whitespace_nowrap()
                            .child(div().flex().flex_none().children(
                                (0..ASCII_BANDS).filter_map(|band| {
                                    let start = band * line.len() / ASCII_BANDS;
                                    let end = (band + 1) * line.len() / ASCII_BANDS;
                                    if start == end {
                                        return None;
                                    }
                                    let u = ((start + end) as f32 * 0.5) / line.len() as f32;
                                    Some(
                                        div()
                                            .flex_none()
                                            .text_color(Hsla {
                                                a: (ascii_cell_alpha(u, *position) * gain).min(1.0),
                                                ..color
                                            })
                                            .child(SharedString::from(line[start..end].to_string())),
                                    )
                                }),
                            ))
                    }))
                },
            )
            .into_any_element()
    }

    /// Accent selector: a row of basic color swatches. Each swatch is its own
    /// hit target; the one matching the stored accent gets a ring.
    fn render_accent_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let selected_accent = self.settings.accent;
        h_flex()
            .items_center()
            .gap_2()
            .children(ACCENT_SWATCHES.iter().enumerate().map(|(index, (_, hue))| {
                let hue = *hue;
                let value = crate::theme::picked_accent(hue);
                let selected = selected_accent == Some(value);
                div()
                    .id(("accent-swatch", index))
                    .flex_none()
                    .size(px(ACCENT_SWATCH_SIZE))
                    .rounded_full()
                    .bg(crate::theme::picked_accent_hsla(hue))
                    .border_2()
                    .border_color(if selected {
                        cx.theme().foreground
                    } else {
                        Hsla { a: 0.0, ..cx.theme().foreground }
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_accent(Some(value), window, cx);
                    }))
            }))
            .into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::web_style::{alpha, icon_el, Lucide, Tokens, R_SM};
        let t = Tokens::read(cx);
        let collapsed = self.sidebar_collapsed;

        // `.om-nav-item`: 36px rows, radius 6, 10px inline, 13/500.
        let nav_item = |id: &'static str, glyph: Lucide, label: &str, active: bool| {
            let icon_color = if active { t.accent } else { t.text3 };
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(11.))
                .w_full()
                .h(px(36.))
                .px(px(10.))
                .rounded(px(R_SM))
                .text_size(px(13.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(if active { t.text1 } else { t.text2 })
                .when(active, |row| row.bg(t.surface(2)).border_1().border_color(t.lit()))
                .when(!active, |row| row.border_1().border_color(alpha(t.lit(), 0.0)).hover(|row| row.bg(t.glint()).text_color(t.text1)))
                .cursor_pointer()
                .child(icon_el(glyph, icon_color, 17.))
                .when(!collapsed, |row| row.child(div().truncate().child(label.to_string())))
        };

        let mut nav = div().flex().flex_col().gap(px(2.));
        for (id, page, glyph, label) in [
            ("nav-overview", Page::Overview, Lucide::LayoutGrid, "Devices"),
            ("nav-games", Page::Games, Lucide::Gamepad2, "Games"),
            ("nav-supported", Page::Supported, Lucide::BookOpen, "Supported mice"),
        ] {
            nav = nav.child(nav_item(id, glyph, label, self.page == page).on_click(cx.listener(move |this, _, _, cx| {
                match page {
                    Page::Overview => this.devices.open = None,
                    Page::Games => this.profile_editor = None,
                    _ => {}
                }
                this.page = page;
                cx.notify();
            })));
        }

        // "Your devices": thumbnail, name, battery or connection state.
        let mut devices = div().flex().flex_col().gap(px(6.));
        if !self.devices.tiles.is_empty() {
            if !collapsed {
                devices = devices.child(
                    div().px(px(10.)).text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(t.text3).child("Your devices"),
                );
            }
            let mut list = div().flex().flex_col().gap(px(2.));
            for (index, tile) in self.visible_tiles().into_iter().enumerate() {
                let active = self.page == Page::Overview && self.devices.open.as_deref() == Some(tile.key.as_str());
                let live = matches!(tile.status, TileStatus::Connected(_));
                let sub: AnyElement = match &tile.status {
                    TileStatus::Connected(r) => match r.battery {
                        Some(b) => {
                            let percent = b.clamp(0.0, 100.0).round() as u8;
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .child(crate::web_style::battery_glyph(&t, percent, r.charging))
                                .child(format!("{percent}%"))
                                .into_any_element()
                        }
                        None => div().child("Connected").into_any_element(),
                    },
                    TileStatus::Connecting => div().child("Connecting…").into_any_element(),
                    _ => div().child("Not connected").into_any_element(),
                };
                let key = tile.key.clone();
                let art = div()
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    .flex_none()
                    .size(px(36.))
                    .rounded(px(R_SM))
                    .bg(t.surface(3))
                    .child(self.device_art(tile, 30., 1.0))
                    .when(live, |art| {
                        art.child(
                            div()
                                .absolute()
                                .right(px(-2.))
                                .bottom(px(-2.))
                                .size(px(9.))
                                .rounded_full()
                                .bg(t.ok)
                                .border_2()
                                .border_color(t.ink0),
                        )
                    });
                list = list.child(
                    div()
                        .id(("side-device", index))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .w_full()
                        .min_h(px(46.))
                        .pl(px(6.))
                        .pr(px(8.))
                        .py(px(5.))
                        .rounded(px(R_SM))
                        .cursor_pointer()
                        .when(active, |row| row.bg(t.surface(2)).border_1().border_color(t.lit()))
                        .when(!active, |row| row.border_1().border_color(alpha(t.lit(), 0.0)).hover(|row| row.bg(t.glint())))
                        .child(art)
                        .when(!collapsed, |row| {
                            row.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .min_w_0()
                                    .gap(px(1.))
                                    .child(div().truncate().text_size(px(12.5)).font_weight(FontWeight::SEMIBOLD).text_color(t.text1).child(tile.name.clone()))
                                    .child(div().text_size(px(11.)).text_color(t.text3).child(sub)),
                            )
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.page = Page::Overview;
                            this.open_device(&key, cx);
                        })),
                );
            }
            devices = devices.child(list);
        }

        // Footer: What's new, Help & feedback, Settings; then a status line.
        let foot_item = |id: &'static str, glyph: AnyElement, label: &str, sub: Option<&str>, external: bool, active: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(11.))
                .w_full()
                .min_h(px(36.))
                .py(px(4.))
                .px(px(10.))
                .rounded(px(R_SM))
                .cursor_pointer()
                .text_color(if active { t.text1 } else { t.text2 })
                .when(active, |row| row.bg(t.surface(2)).border_1().border_color(t.lit()))
                .when(!active, |row| row.border_1().border_color(alpha(t.lit(), 0.0)).hover(|row| row.bg(t.glint()).text_color(t.text1)))
                .child(glyph)
                .when(!collapsed, |row| {
                    row.child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().text_size(px(13.)).font_weight(FontWeight::MEDIUM).child(label.to_string()))
                            .children(sub.map(|s| div().truncate().text_size(px(11.)).text_color(t.text3).child(s.to_string()))),
                    )
                    .when(external, |row| row.child(icon_el(Lucide::ArrowUpRight, t.text4, 12.)))
                })
        };
        let on_updates = self.page == Page::Updates;
        let on_settings = self.page == Page::Settings;
        let discord = img(themed_discord(t.text3)).w(px(17.)).h(px(13.)).flex_none().into_any_element();
        let connected = self.visible_tiles().into_iter().filter(|t| matches!(t.status, TileStatus::Connected(_))).count();
        let status_text = match connected {
            0 => "No mouse connected".to_string(),
            1 => "1 device connected".to_string(),
            n => format!("{n} devices connected"),
        };
        let footer = div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .mt_auto()
            .pt(px(10.))
            .border_t_1()
            .border_color(t.line(1))
            .children(self.update_ready.as_ref().map(|pending| {
                div()
                    .id("footer-restart-update")
                    .flex()
                    .items_center()
                    .gap(px(11.))
                    .w_full()
                    .min_h(px(36.))
                    .py(px(4.))
                    .px(px(10.))
                    .mb(px(4.))
                    .rounded(px(R_SM))
                    .cursor_pointer()
                    .bg(alpha(t.accent, 0.14))
                    .border_1()
                    .border_color(alpha(t.accent, 0.35))
                    .hover(|row| row.bg(alpha(t.accent, 0.22)))
                    .child(div().flex_none().size(px(8.)).rounded_full().bg(t.accent))
                    .when(!collapsed, |row| {
                        row.child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .child(div().truncate().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(t.text1).child("Restart to update"))
                                .child(div().truncate().text_size(px(11.)).text_color(t.text3).child(format!("Version {} is ready", pending.version))),
                        )
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.restart_to_update(cx)))
            }))
            .child(
                foot_item("footer-updates", icon_el(Lucide::Megaphone, if on_updates { t.accent } else { t.text3 }, 17.).into_any_element(), "What's new", None, false, on_updates)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.page = Page::Updates;
                        cx.notify();
                    })),
            )
            .child(
                foot_item("footer-discord", discord, "Help & feedback", Some("Join the community server"), true, false)
                    .on_click(cx.listener(|_, _, _, cx| cx.open_url(DISCORD_INVITE_URL))),
            )
            .child(
                foot_item("footer-settings", icon_el(Lucide::Settings, if on_settings { t.accent } else { t.text3 }, 17.).into_any_element(), "Settings", None, false, on_settings)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.page = Page::Settings;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(9.))
                    .h(px(30.))
                    .px(px(12.))
                    .text_size(px(11.))
                    .text_color(if connected > 0 { t.text2 } else { t.text3 })
                    .child(div().flex_none().size(px(6.)).rounded_full().bg(if connected > 0 { t.ok } else { t.text4 }))
                    .when(!collapsed, |row| row.child(div().truncate().child(status_text))),
            );

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(if collapsed { SIDEBAR_COLLAPSED_W } else { SIDEBAR_W }))
            .h_full()
            .px(px(10.))
            .pt(px(4.))
            .pb(px(12.))
            .gap(px(18.))
            .child(nav)
            .child(devices)
            .child(footer)
    }

    /// The sidebar head (`.om-sidebar-head`): mark, name and build, and the
    /// collapse chevron. It sits in the title bar row so the window keeps its
    /// caption buttons on the right, as the web shell has no title bar.
    fn render_titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::web_style::{icon_el, Lucide, Tokens, R_SM};
        let t = Tokens::read(cx);
        let collapsed = self.sidebar_collapsed;
        let mark = img(brand_mark()).w(px(14.)).h(px(21.)).flex_none();
        let toggle = div()
            .id("toggle-sidebar")
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .flex_none()
            .size(px(28.))
            .rounded(px(R_SM))
            .cursor_pointer()
            .hover(|b| b.bg(t.glint()))
            .child(icon_el(if collapsed { Lucide::ChevronRight } else { Lucide::ChevronLeft }, t.text3, 16.))
            .on_click(cx.listener(|this, _, _, cx| {
                this.sidebar_collapsed = !this.sidebar_collapsed;
                cx.notify();
            }));
        let brand = h_flex()
            .items_center()
            .gap(px(10.))
            .flex_1()
            .min_w_0()
            .px(px(2.))
            .child(mark)
            .when(!collapsed, |row| {
                row.child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .child(div().text_size(px(15.)).line_height(px(18.)).font_weight(FontWeight(650.)).text_color(t.text1).child("OpenMouse"))
                        .child(
                            div()
                                .truncate()
                                .text_size(px(11.))
                                .line_height(px(14.))
                                .text_color(t.text3)
                                .child(format!("{} · v{APP_VERSION}", release_stage(APP_VERSION).to_uppercase())),
                        ),
                )
            });
        h_flex()
            .items_center()
            .gap(px(6.))
            .w(px((if collapsed { SIDEBAR_COLLAPSED_W } else { SIDEBAR_W }) - 12.))
            .pl(px(6.))
            .child(brand)
            .child(toggle)
    }

    fn render_page(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.page == Page::Settings {
            return self.render_settings(window, cx);
        }
        if self.page == Page::Updates {
            return self.render_updates(cx);
        }
        if self.page == Page::Games {
            if let Some(id) = self.profile_editor.clone() {
                return self.render_profile_editor(&id, cx);
            }
            return self.render_games(cx);
        }
        if self.page == Page::Overview {
            let size = window.viewport_size();
            return self.render_overview(size.width.into(), size.height.into(), cx);
        }
        let (title, body): (&str, &str) = match self.page {
            Page::Supported => ("Supported mice", "Supported device catalog lives here."),
            _ => unreachable!(),
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .p_4()
            .gap_2()
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().foreground)
                    .child(title),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(body),
            )
            .into_any_element()
    }

    /// Mouse artwork in a `side` x `side` box. Placement is in fractions of
    /// the box (the Tauri app's `deviceArtStyle` normalisation, so every
    /// model's subject comes out a comparable size); `zoom` scales that up
    /// where the art should dominate.
    fn device_art(&self, tile: &Tile, side: f32, zoom: f32) -> Div {
        let frame = div().relative().flex_none().w(px(side)).h(px(side)).overflow_hidden();
        let Some(art) = &tile.art else { return frame };
        let image = img(art.path.clone());
        match &art.placement {
            Some(p) => {
                let h = p.height / 100.0 * zoom;
                let w = h * p.aspect;
                let left = 0.5 - w / 2.0 + p.offset_x / 100.0 * w;
                let top = 0.5 - h / 2.0 + p.offset_y / 100.0 * h;
                frame.child(
                    image
                        .absolute()
                        .left(relative(left))
                        .top(relative(top))
                        .w(relative(w))
                        .h(relative(h))
                        .object_fit(ObjectFit::Fill),
                )
            }
            None => frame.child(image.size_full().object_fit(ObjectFit::Contain)),
        }
    }

    /// Selectable pill used for the page's option lists.
    fn chip(
        &self,
        id: String,
        label: String,
        selected: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let primary = cx.theme().primary;
        div()
            .id(SharedString::from(id))
            .cursor_pointer()
            .px_3()
            .py_1p5()
            .rounded_md()
            .border_1()
            .text_sm()
            .border_color(if selected { primary } else { cx.theme().border })
            .when(selected, |this| this.bg(Hsla { a: 0.14, ..primary }))
            .text_color(if selected { cx.theme().foreground } else { cx.theme().muted_foreground })
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
    }

    /// Creates the sliders the selected device's number controls need. The write
    /// waits until the drag pauses, so a sweep sends one command, not hundreds.
    fn ensure_control_sliders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tile) = self.devices.tiles.get(self.devices.selected) else { return };
        let TileStatus::Connected(r) = &tile.status else { return };
        let device = tile.key.clone();
        let specs: Vec<crate::hid::ControlSpec> = r
            .tabs
            .performance
            .iter()
            .chain(&r.tabs.buttons)
            .chain(&r.tabs.advanced)
            .flat_map(|s| s.controls.iter())
            .filter(|c| c.kind == "number" || c.kind == "stepped")
            .cloned()
            .collect();
        // One slider per on-device DPI stage, written (debounced) as that
        // stage's value through `perf.dpiStageValue`.
        let (dpi_min, dpi_max, dpi_step) = r.controls.as_ref().map(|c| (c.dpi.min, c.dpi.max, c.dpi.step.max(1.0))).unwrap_or((50.0, 22000.0, 50.0));
        let stages = r.stages.clone();
        for (stage, value) in stages.iter().copied().enumerate() {
            let key = format!("{device}/stage/{stage}");
            // Keep the box and the slider on the mouse's value unless they
            // are being edited (the mouse's own button, or a re-read, moved it).
            if !self.slider_live.contains_key(&key) {
                if let Some(input) = self.stage_inputs.get(&key).cloned() {
                    let text = format!("{}", value.round() as i64);
                    let editing = input.read(cx).focus_handle(cx).is_focused(window);
                    if !editing && input.read(cx).value().as_ref() != text {
                        input.update(cx, |s, cx| s.set_value(text, window, cx));
                    }
                }
                if let Some(slider) = self.control_sliders.get(&key).cloned() {
                    let at = (value.clamp(dpi_min, dpi_max) - dpi_min) as f32;
                    if (slider.read(cx).value().end() - at).abs() > 0.5 {
                        slider.update(cx, |s, cx| s.set_value(at, window, cx));
                    }
                }
            }
            if !self.stage_inputs.contains_key(&key) {
                let input = cx.new(|cx| {
                    let mut state = InputState::new(window, cx)
                        .default_value(format!("{}", value.round() as i64))
                        .validate(|text, _| text.len() <= 5 && text.chars().all(|c| c.is_ascii_digit()));
                    state.set_text_align(TextAlign::Right, cx);
                    state
                });
                let commit_device = device.clone();
                let sub = cx.subscribe_in(&input, window, move |this: &mut Self, state, event: &InputEvent, window, cx| {
                    if !matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                        return;
                    }
                    let typed = state.read(cx).value().trim().parse::<f64>().ok();
                    match typed {
                        Some(dpi) => this.stage_edit(&commit_device, stage, dpi, false, window, cx),
                        // Nothing usable typed: put the stage's value back.
                        None => {
                            this.slider_live.remove(&format!("{commit_device}/stage/{stage}"));
                        }
                    }
                    cx.notify();
                });
                self._subscriptions.push(sub);
                self.stage_inputs.insert(key.clone(), input);
            }
            if self.control_sliders.contains_key(&key) {
                continue;
            }
            let slider = cx.new(|_| {
                SliderState::new()
                    .min(0.0)
                    .max((dpi_max - dpi_min) as f32)
                    .step(dpi_step as f32)
                    .default_value((value.clamp(dpi_min, dpi_max) - dpi_min) as f32)
            });
            let live_key = key.clone();
            let sub = cx.subscribe_in(&slider, window, move |this: &mut Self, _s, event, window, cx| {
                let SliderEvent::Change(v) = event else { return };
                let dpi = ((v.end() as f64 + dpi_min) / dpi_step).round() * dpi_step;
                if let Some(input) = this.stage_inputs.get(&live_key).cloned() {
                    input.update(cx, |s, cx| s.set_value(format!("{}", dpi.round() as i64), window, cx));
                }
                this.slider_live.insert(live_key.clone(), dpi);
                this.slider_seq += 1;
                let seq = this.slider_seq;
                cx.notify();
                cx.spawn(async move |view, cx| {
                    cx.background_executor().timer(Duration::from_millis(400)).await;
                    let _ = view.update(cx, |this: &mut Self, cx| {
                        if this.slider_seq == seq {
                            this.set_control("perf.dpiStageValue".into(), serde_json::json!({ "stage": stage, "dpi": dpi }), cx);
                        }
                    });
                })
                .detach();
            });
            self._subscriptions.push(sub);
            self.control_sliders.insert(key, slider);
        }
        for c in specs {
            let key = format!("{device}/{}", c.id);
            if self.control_sliders.contains_key(&key) {
                continue;
            }
            // A stepped control slides over the positions of its option list
            // (the stops need not be evenly spaced); the write sends the
            // option's own value.
            let stepped: Option<Vec<serde_json::Value>> =
                (c.kind == "stepped").then(|| c.options.iter().map(|o| o.value.clone()).collect());
            let (min, max) = match &stepped {
                Some(values) => (0.0, values.len().saturating_sub(1) as f64),
                None => (c.min.unwrap_or(0.0), c.max.unwrap_or(255.0)),
            };
            let step = if stepped.is_some() { 1.0 } else { c.step.unwrap_or(1.0) };
            let start = match &stepped {
                Some(values) => values.iter().position(|v| *v == c.current).unwrap_or(0) as f64,
                None => c.current.as_f64().unwrap_or(min).clamp(min, max),
            };
            let slider = cx.new(|_| SliderState::new().min(0.0).max((max - min) as f32).step(step as f32).default_value((start - min) as f32));
            let id = c.id.clone();
            let places = c.decimals.unwrap_or(0);
            let live_key = key.clone();
            let sub = cx.subscribe_in(&slider, window, move |this: &mut Self, _s, event, _w, cx| {
                let SliderEvent::Change(value) = event else { return };
                let scale = 10f64.powi(places as i32);
                let v = ((value.end() as f64 + min) * scale).round() / scale;
                this.slider_live.insert(live_key.clone(), v);
                this.slider_seq += 1;
                let seq = this.slider_seq;
                let id = id.clone();
                let stepped = stepped.clone();
                cx.notify();
                cx.spawn(async move |view, cx| {
                    cx.background_executor().timer(std::time::Duration::from_millis(350)).await;
                    let _ = view.update(cx, |this: &mut Self, cx| {
                        if this.slider_seq == seq {
                            let sent = match &stepped {
                                Some(values) => values.get(v as usize).cloned().unwrap_or(serde_json::Value::Null),
                                None => serde_json::json!(v),
                            };
                            this.set_control(id, sent, cx);
                        }
                    });
                })
                .detach();
            });
            self._subscriptions.push(sub);
            self.control_sliders.insert(key, slider);
        }
    }

    /// One DPI stage's new value, from its box, the arrow keys or the scroll
    /// wheel: snapped to the mouse's step and range, shown on the box and
    /// slider at once, and written (after a pause, when `debounce`, so a run
    /// of key presses sends one command).
    fn stage_edit(&mut self, device: &str, stage: usize, dpi: f64, debounce: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Tile { status: TileStatus::Connected(r), .. }) = self.devices.tiles.iter().find(|t| t.key == device) else { return };
        let (min, max, step) = r.controls.as_ref().map(|c| (c.dpi.min, c.dpi.max, c.dpi.step.max(1.0))).unwrap_or((50.0, 22000.0, 50.0));
        let current = r.stages.get(stage).copied();
        let dpi = ((dpi.clamp(min, max) - min) / step).round() * step + min;
        let key = format!("{device}/stage/{stage}");
        if let Some(input) = self.stage_inputs.get(&key).cloned() {
            input.update(cx, |s, cx| s.set_value(format!("{}", dpi.round() as i64), window, cx));
        }
        if let Some(slider) = self.control_sliders.get(&key).cloned() {
            slider.update(cx, |s, cx| s.set_value((dpi - min) as f32, window, cx));
        }
        if current.is_some_and(|c| (c - dpi).abs() < 0.5) && !self.slider_live.contains_key(&key) {
            return;
        }
        self.slider_live.insert(key, dpi);
        self.slider_seq += 1;
        let seq = self.slider_seq;
        let value = serde_json::json!({ "stage": stage, "dpi": dpi });
        if !debounce {
            self.set_control("perf.dpiStageValue".into(), value, cx);
            return;
        }
        cx.spawn(async move |view, cx| {
            cx.background_executor().timer(Duration::from_millis(450)).await;
            let _ = view.update(cx, |this: &mut Self, cx| {
                if this.slider_seq == seq {
                    this.set_control("perf.dpiStageValue".into(), value, cx);
                }
            });
        })
        .detach();
    }

    /// A slider for a Buttons/Advanced/Performance control: accent-filled track
    /// and thumb, with its value in a fixed-width pill so rows line up.
    fn control_slider_row(slider: Option<Entity<SliderState>>, value: String, cx: &mut Context<Self>) -> AnyElement {
        let accent = cx.theme().primary;
        let fg = cx.theme().foreground;
        let border = cx.theme().border;
        h_flex()
            .gap_3()
            .items_center()
            .children(slider.map(|s| div().w(px(220.)).child(Slider::new(&s).bg(accent).text_color(fg))))
            .child(
                div()
                    .min_w(px(84.))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(Hsla { a: 0.7, ..border })
                    .bg(Hsla { a: 0.05, ..fg })
                    .text_sm()
                    .text_center()
                    .text_color(fg)
                    .child(value),
            )
            .into_any_element()
    }

    /// One Buttons/Advanced control: label on the left, the control on the right.
    fn render_control(&self, c: &crate::hid::ControlSpec, cx: &mut Context<Self>) -> AnyElement {
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let id = c.id.clone();
        let label_block = div()
            .flex()
            .flex_col()
            .gap_0p5()
            .flex_1()
            .min_w_0()
            .child(div().text_sm().text_color(fg).child(c.label.clone()))
            .children(c.description.clone().map(|d| div().text_xs().text_color(muted).child(d)));

        let mut unfolded: Option<AnyElement> = None;
        let control: AnyElement = match c.kind.as_str() {
            "toggle" => {
                let on = c.current.as_bool().unwrap_or(false);
                Switch::new(SharedString::from(format!("sw-{id}")))
                    .checked(on)
                    .on_change(cx.listener(move |this, enabled: &bool, _, cx| {
                        this.set_control(id.clone(), serde_json::json!(*enabled), cx)
                    }))
                    .into_any_element()
            }
            "number" => {
                let places = c.decimals.unwrap_or(0);
                let unit = c.unit.as_deref().map(|u| format!(" {u}")).unwrap_or_default();
                let slider_key = self.devices.tiles.get(self.devices.selected).map(|t| format!("{}/{}", t.key, c.id));
                let live = slider_key.as_ref().and_then(|k| self.slider_live.get(k)).copied();
                let now = live.or(c.current.as_f64()).unwrap_or(c.min.unwrap_or(0.0));
                let slider = slider_key.and_then(|k| self.control_sliders.get(&k).cloned());
                let shown = match (&c.zero_label, now <= c.min.unwrap_or(0.0)) {
                    (Some(label), true) => label.clone(),
                    _ => format!("{now:.places$}{unit}"),
                };
                Self::control_slider_row(slider, shown, cx)
            }
            "stepped" => {
                let slider_key = self.devices.tiles.get(self.devices.selected).map(|t| format!("{}/{}", t.key, c.id));
                let live = slider_key.as_ref().and_then(|k| self.slider_live.get(k)).copied();
                let index = live
                    .map(|v| v as usize)
                    .or_else(|| c.options.iter().position(|o| o.value == c.current))
                    .unwrap_or(0);
                let slider = slider_key.and_then(|k| self.control_sliders.get(&k).cloned());
                Self::control_slider_row(slider, c.options.get(index).map(|o| o.label.clone()).unwrap_or_default(), cx)
            }
            "choice" if c.options.len() <= 8 => {
                let mut row = h_flex().gap_2().flex_wrap().when(c.options.len() <= 3, |row| row.justify_end());
                for option in &c.options {
                    let value = option.value.clone();
                    let sent = value.clone();
                    let cid = id.clone();
                    row = row.child(self.chip(
                        format!("{id}-{}", option.label),
                        option.label.clone(),
                        c.current == value,
                        cx,
                        move |this, cx| this.set_control(cid.clone(), sent.clone(), cx),
                    ));
                }
                row.into_any_element()
            }
            // Long lists (18 Logitech button actions, nine on Endgame Gear): a
            // button showing the current value that unfolds the options below.
            "choice" => {
                let open = self.devices.expanded.as_deref() == Some(id.as_str());
                let shown = c
                    .options
                    .iter()
                    .find(|o| o.value == c.current)
                    .map(|o| o.label.clone())
                    .unwrap_or_else(|| "Custom".to_string());
                if open {
                    let mut grid = h_flex().gap_2().flex_wrap().justify_end();
                    for option in &c.options {
                        let (cid, sent) = (id.clone(), option.value.clone());
                        grid = grid.child(self.chip(
                            format!("{id}-{}", option.label),
                            option.label.clone(),
                            c.current == option.value,
                            cx,
                            move |this, cx| {
                                this.devices.expanded = None;
                                this.set_control(cid.clone(), sent.clone(), cx)
                            },
                        ));
                    }
                    unfolded = Some(grid.into_any_element());
                }
                let toggle_id = id.clone();
                Button::new(SharedString::from(format!("choose-{id}")))
                    .compact()
                    .label(format!("{shown}  {}", if open { "▴" } else { "▾" }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.devices.expanded =
                            if this.devices.expanded.as_deref() == Some(toggle_id.as_str()) { None } else { Some(toggle_id.clone()) };
                        cx.notify();
                    }))
                    .into_any_element()
            }
            _ => div()
                .text_sm()
                .text_color(muted)
                .child(c.current.as_str().map(str::to_string).unwrap_or_else(|| c.current.to_string()))
                .into_any_element(),
        };
        // A row of many option chips takes the full width under its label: side
        // by side, the chips' one-line width squeezed the label to nothing.
        let stacked = c.kind == "choice" && c.options.len() > 3;
        let row = if stacked {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .w_full()
                .child(label_block)
                .child(div().w_full().flex().flex_row().flex_wrap().gap_2().child(control))
        } else {
            h_flex().gap_4().items_center().justify_between().child(label_block).child(control)
        };
        match unfolded {
            Some(options) => div().flex().flex_col().gap_2().child(row).child(options).into_any_element(),
            None => row.into_any_element(),
        }
    }

    /// Logitech button remapping: the buttons of the profile the mouse is
    /// running, each assignable to any action the protocol knows. Changes are
    /// written to that profile in the mouse's own memory (the library verifies
    /// the write and restores the old sector if it fails), so the mouse has to
    /// be running its stored profiles, i.e. in Onboard mode.
    fn render_logitech_buttons(&self, r: &DeviceReadout, cx: &mut Context<Self>) -> AnyElement {
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let onboard_mode = r.mode.as_deref() == Some("Onboard");
        let mut col = div().flex().flex_col().gap_3().flex_1().min_w_0().max_w(px(720.));
        let note = |text: &str| div().text_sm().text_color(muted).child(text.to_string());

        if !onboard_mode {
            return col
                .child(note(
                    "Button changes are saved into the mouse's stored profile, which only runs in Onboard memory mode. The mouse is currently under software control.",
                ))
                .child(
                    Button::new("buttons-onboard")
                        .label("Switch to Onboard memory mode")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.onboard_action("mode", serde_json::json!("Onboard"), cx)
                        })),
                )
                .into_any_element();
        }
        let Some(read) = &self.devices.onboard else {
            return col
                .child(note(if self.devices.onboard_loading {
                    "Reading the mouse's buttons…"
                } else {
                    "Buttons haven't been read yet."
                }))
                .into_any_element();
        };
        let Some(running) = read.profiles.iter().find(|p| p.is_current) else {
            return col.child(note("No stored profile is running.")).into_any_element();
        };

        const NAMES: [&str; 5] = ["Left button", "Right button", "Middle button", "Back", "Forward"];
        let options: Vec<crate::hid::OptionSpec> = read
            .actions
            .iter()
            .map(|a| crate::hid::OptionSpec { value: serde_json::json!(a), label: a.clone() })
            .collect();
        let mut card = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(border)
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .child(format!("Profile {} — buttons", running.sector)),
            );
        for (n, assignment) in running.button_assignments.iter().enumerate() {
            let label = NAMES.get(n).map(|s| s.to_string()).unwrap_or_else(|| format!("Button {}", n + 1));
            // A macro or custom binding the protocol can't name shows as such.
            let current = assignment.get("action").cloned().unwrap_or(serde_json::Value::Null);
            let spec = crate::hid::ControlSpec {
                id: format!("logi.button.primary.{n}"),
                label,
                description: None,
                kind: "choice".to_string(),
                options: options.clone(),
                min: None,
                max: None,
                step: None,
                decimals: None,
                unit: None,
                zero_label: None,
                current,
            };
            card = card.child(self.render_control(&spec, cx));
        }
        col = col.child(card).child(note(
            "Saved to the mouse's memory. Choose a different action for a button to remap it.",
        ));
        if self.devices.writing.is_some() {
            col = col.child(note("Saving…"));
        }
        if let Some(error) = &self.devices.write_error {
            col = col.child(div().text_sm().text_color(cx.theme().danger).child(format!("Could not save: {error}")));
        }
        col.into_any_element()
    }

    /// The mouse's own stored profiles (Logitech): which one runs, and whether
    /// the mouse runs them at all. Read-only apart from switching — editing a
    /// profile writes flash, and the library flags that sequence as unproven.
    fn render_onboard_profiles(&self, r: &DeviceReadout, cx: &mut Context<Self>) -> AnyElement {
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let accent = cx.theme().primary;
        let ink = cx.theme().foreground;
        let onboard_mode = r.mode.as_deref() == Some("Onboard");
        let busy = self.devices.writing.is_some();
        let card = || {
            div().flex().flex_col().gap_2().p_4().rounded_lg().border_1().border_color(border)
        };
        let mut col = div().flex().flex_col().gap_3().flex_1().min_w_0().max_w(px(720.));

        col = col.child(
            card().child(
                h_flex()
                    .gap_4()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(fg).child("Onboard memory mode"))
                            .child(div().text_xs().text_color(muted).child(if onboard_mode {
                                "The mouse is running its own stored profile."
                            } else {
                                "Software is controlling the mouse, so its stored profiles are bypassed. Changing DPI or polling rate in this app puts the mouse in this mode."
                            })),
                    )
                    .child(
                        Switch::new("onboard-mode")
                            .checked(onboard_mode)
                            .on_change(cx.listener(|this, on: &bool, _, cx| {
                                this.onboard_action("mode", serde_json::json!(if *on { "Onboard" } else { "Host" }), cx)
                            })),
                    ),
            ),
        );

        match &self.devices.onboard {
            None => {
                col = col.child(div().text_sm().text_color(muted).child(if self.devices.onboard_loading {
                    "Reading the mouse's profiles…"
                } else {
                    "Profiles haven't been read yet."
                }));
            }
            Some(list) => {
                let mut rows = div().flex().flex_col().gap_2();
                for p in &list.profiles {
                    let sector = p.sector;
                    let title = p.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| format!("Profile {}", p.sector));
                    let running = p.is_current && onboard_mode;
                    let dpis: Vec<String> = p
                        .dpi_stages
                        .iter()
                        .filter_map(|s| s.get("x").and_then(|v| v.as_f64()))
                        .filter(|d| *d > 0.0)
                        .map(|d| format!("{d:.0}"))
                        .collect();
                    let mut facts = Vec::new();
                    if !dpis.is_empty() {
                        facts.push(format!("DPI {}", dpis.join(" · ")));
                    }
                    if let Some(hz) = p.report_rate_wireless.or(p.report_rate_wired) {
                        facts.push(format!("{hz} Hz"));
                    }
                    if !p.button_assignments.is_empty() {
                        facts.push(format!("{} buttons", p.button_assignments.len()));
                    }
                    if !p.crc_valid {
                        facts.push("checksum mismatch".to_string());
                    }
                    rows = rows.child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .justify_between()
                            .p_3()
                            .rounded_md()
                            .border_1()
                            .border_color(if running { cx.theme().primary } else { border })
                            .when(running, |this| this.bg(Hsla { a: 0.10, ..cx.theme().primary }))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .min_w_0()
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .items_center()
                                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(fg).child(title))
                                            .when(running, |row| row.child(accent_badge("Running", accent, ink)))
                                            .when(!p.enabled, |row| row.child(Tag::secondary().small().child("Disabled"))),
                                    )
                                    .child(div().text_xs().text_color(muted).child(facts.join("  ·  "))),
                            )
                            .when(p.enabled && !running, |row| {
                                row.child(
                                    Button::new(SharedString::from(format!("use-profile-{sector}")))
                                        .compact()
                                        .label(if busy { "Switching…" } else { "Use" })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.onboard_action("useProfile", serde_json::json!(sector), cx)
                                        })),
                                )
                            }),
                    );
                }
                col = col.child(card().child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(fg).child("Stored profiles")).child(rows));
                col = col.child(
                    div().text_xs().text_color(muted).child(
                        "Editing a stored profile, or enabling and disabling slots, rewrites the mouse's flash memory. That isn't offered here yet.",
                    ),
                );
            }
        }
        if let Some(error) = &self.devices.write_error {
            col = col.child(div().text_sm().text_color(cx.theme().danger).child(error.clone()));
        }
        col.into_any_element()
    }

    /// The Buttons, Lighting and Advanced tabs. Controls come from the shared
    /// JS (`js/controls.ts`), so any brand's extras appear without Rust knowing them.
    fn render_extra_tab(&self, r: &DeviceReadout, cx: &mut Context<Self>) -> AnyElement {
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let busy = self.devices.writing.is_some();
        if self.devices.tab == DeviceTab::Profiles {
            return self.render_onboard_profiles(r, cx);
        }
        if self.devices.tab == DeviceTab::Buttons && r.onboard_profiles && r.tabs.buttons.is_empty() {
            return self.render_logitech_buttons(r, cx);
        }
        let mut col = div().flex().flex_col().gap_3().flex_1().min_w_0().max_w(px(720.));
        let placeholder = |text: &str| div().text_sm().text_color(muted).child(text.to_string()).into_any_element();

        let sections: Option<&Vec<crate::hid::Section>> = match self.devices.tab {
            DeviceTab::Buttons => Some(&r.tabs.buttons),
            DeviceTab::Advanced => Some(&r.tabs.advanced),
            _ => None,
        };
        if let Some(sections) = sections {
            if sections.is_empty() {
                return placeholder(if self.devices.tab == DeviceTab::Buttons {
                    "No reconfigurable buttons are exposed by this device's driver."
                } else {
                    "No brand-specific settings are exposed by this device's driver."
                });
            }
            for section in sections {
                let mut card = div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(border)
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(fg).child(section.title.clone()));
                for control in &section.controls {
                    card = card.child(self.render_control(control, cx));
                }
                col = col.child(card);
            }
        } else if let Some(spec) = r.tabs.lighting.clone() {
            let e = &self.devices.staged.light;
            let mode = e.mode.clone().or_else(|| spec.mode.clone());
            let in_mode = |list: &[String]| mode.as_ref().is_some_and(|m| list.contains(m));
            let card = |title: &str| {
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(border)
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(fg).child(title.to_string()))
            };
            if spec.write_only {
                col = col.child(div().text_xs().text_color(muted).child(
                    "Lighting is write-only on this device. The current effect is cached from the last change and may not reflect the hardware state.",
                ));
            }
            let mut effects = h_flex().gap_2().flex_wrap();
            for m in spec.modes.iter().cloned() {
                let spec2 = spec.clone();
                let picked = m.clone();
                effects = effects.child(self.chip(
                    format!("light-mode-{m}"),
                    m.clone(),
                    mode.as_deref() == Some(m.as_str()),
                    cx,
                    move |this, cx| this.edit_lighting(|l| l.mode = Some(picked.clone()), &spec2, cx),
                ));
            }
            col = col.child(card("Effect").child(effects));

            let swatches = |cx: &mut Context<Self>, which: u8, current: Option<String>| {
                let mut row = h_flex().gap_2().flex_wrap();
                for hex in LIGHT_SWATCHES {
                    let spec2 = spec.clone();
                    let picked = hex.to_string();
                    let selected = current.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(hex));
                    row = row.child(
                        div()
                            .id(SharedString::from(format!("swatch-{which}-{hex}")))
                            .cursor_pointer()
                            .size(px(28.))
                            .rounded_full()
                            .border_2()
                            .border_color(if selected { cx.theme().foreground } else { Hsla { a: 0.0, ..hex_color(hex) } })
                            .bg(hex_color(hex))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let picked = picked.clone();
                                this.edit_lighting(
                                    move |l| if which == 1 { l.color = Some(picked) } else { l.color2 = Some(picked) },
                                    &spec2,
                                    cx,
                                )
                            })),
                    );
                }
                row
            };
            if in_mode(&spec.color_modes) {
                let current = e.color.clone().or_else(|| spec.color.clone());
                col = col.child(card("Color").child(swatches(cx, 1, current)));
            }
            if in_mode(&spec.dual_color_modes) {
                let current = e.color2.clone().or_else(|| spec.color2.clone());
                col = col.child(card("Second color").child(swatches(cx, 2, current)));
            }
            if in_mode(&spec.reactive_modes) && !spec.speeds.is_empty() {
                let current = e.speed.or(spec.speed);
                let mut row = h_flex().gap_2().flex_wrap();
                for sp in spec.speeds.iter().copied() {
                    let spec2 = spec.clone();
                    row = row.child(self.chip(
                        format!("light-speed-{sp}"),
                        format!("{sp:.0}"),
                        current.is_some_and(|c| (c - sp).abs() < 0.01),
                        cx,
                        move |this, cx| this.edit_lighting(|l| l.speed = Some(sp), &spec2, cx),
                    ));
                }
                col = col.child(card("Speed").child(row));
            }
        } else {
            return placeholder("No lighting data available for this device.");
        }

        if busy && self.settings.instant_apply {
            col = col.child(div().text_sm().text_color(muted).child("Applying…"));
        }
        if let Some(error) = &self.devices.write_error {
            col = col.child(div().text_sm().text_color(cx.theme().danger).child(format!("Could not apply: {error}")));
        }
        col.into_any_element()
    }

    /// Per-device settings: sensitivity, polling rate, lift-off distance and
    /// gaming-surface mode, written through the shared protocol drivers.
    /// With instant apply off, edits are staged and the bottom bar applies them.
    fn render_device_page(&self, tile: &Tile, view_width: f32, view_height: f32, cx: &mut Context<Self>) -> AnyElement {
        let _ = view_height;
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let busy = self.devices.writing.is_some();
        let sidebar = if self.sidebar_collapsed { SIDEBAR_COLLAPSED_W } else { SIDEBAR_W };
        let page_w = (view_width - sidebar - 2.0 * PAGE_PAD_X).max(240.0);
        let stacked = page_w < RAIL_W + 16.0 + 600.0;
        let content_w = if stacked { page_w } else { page_w - RAIL_W - 16.0 };
        let staged = &self.devices.staged;
        let instant = self.settings.instant_apply;

        // Quiet one-line summary under the name: brand, link and battery.
        let mut facts = vec![tile.display_brand.clone()];
        if let TileStatus::Connected(r) = &tile.status {
            if let Some(c) = &r.connection {
                facts.push(c.clone());
            }
            if let Some(b) = r.battery {
                facts.push(format!("{b:.0}%{}", if r.charging { " charging" } else { "" }));
            }
        }
        let header = h_flex()
            .gap_3()
            .items_center()
            .child(
                Button::new("device-back")
                    .ghost()
                    .icon(IconName::ArrowLeft)
                    .tooltip("Back to devices")
                    .accessibility_label("Back to devices")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.devices.open = None;
                        this.devices.write_error = None;
                        this.devices.staged = Staged::default();
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(fg)
                            .child(if tile.name.is_empty() { "Unknown device".to_string() } else { tile.name.clone() }),
                    )
                    .child(div().text_sm().text_color(muted).child(facts.join("  ·  "))),
            );

        let web = crate::web_style::Tokens::read(cx);
        let _ = border;
        let section = |title: &str| crate::web_style::panel(&web).child(crate::web_style::panel_head(&web, None, title, None));

        let content: AnyElement = match &tile.status {
            TileStatus::Connected(r) if self.devices.tab == DeviceTab::Overview => self.render_device_overview(tile, r, content_w, cx),
            TileStatus::Connected(r) if self.devices.tab == DeviceTab::Advanced => self.render_device_advanced(tile, r, cx),
            TileStatus::Connected(r) if self.devices.tab != DeviceTab::Performance => self.render_extra_tab(r, cx),
            TileStatus::Connected(r) => {
                let mut col = div().flex().flex_col().gap(px(16.)).flex_1().min_w_0();

                // Locked while a game profile is in control of this device.
                if let Some(game) = self.locked_game() {
                    col = col.child(div().text_sm().text_color(muted).child(format!(
                        "Locked while {game} is running — these are your game profile's settings, and they're editable again once the game closes."
                    )));
                }

                // Sensitivity. Hidden when the driver reports no real DPI (a
                // control whose Apply would fail), as the Tauri tab does.
                let stage_editor = r.stages.len() > 1;
                if stage_editor {
                    col = col.child(self.render_stage_editor(tile, r, cx));
                } else if let (Some(current), Some(c)) = (r.dpi.filter(|d| *d > 0.0), &r.controls) {
                    let unlinked = r.separate_axes && self.devices.unlinked_axes;
                    let cur_y = r.dpi_y.unwrap_or(current);
                    let x_now = staged.dpi.unwrap_or(current);
                    let y_now = staged.dpi_y.unwrap_or(cur_y);
                    let (min, max, step) = (c.dpi.min, c.dpi.max, c.dpi.step.max(1.0));
                    let mut presets = h_flex().gap_2().flex_wrap();
                    for preset in c.dpi_presets.iter().copied().filter(|p| *p >= min && *p <= max) {
                        presets = presets.child(self.chip(
                            format!("dpi-preset-{preset}"),
                            format!("{preset:.0}"),
                            (x_now - preset).abs() < 0.5,
                            cx,
                            move |this, cx| {
                                let y = this.devices.unlinked_axes.then(|| this.devices.staged.dpi_y.unwrap_or(cur_y));
                                this.edit_dpi(preset, y, cx)
                            },
                        ));
                    }
                    // One stepper per axis: `axis_row(label, value, is_y)`.
                    let axis_row = |label: Option<&str>, value: f64, is_y: bool, cx: &mut Context<Self>| {
                        let (down_id, up_id) = (format!("dpi-down-{is_y}"), format!("dpi-up-{is_y}"));
                        let apply = move |this: &mut Self, delta: f64, cx: &mut Context<Self>| {
                            let x_cur = this.devices.staged.dpi.unwrap_or(current);
                            let y_cur = this.devices.staged.dpi_y.unwrap_or(cur_y);
                            if is_y {
                                this.edit_dpi(x_cur, Some((y_cur + delta).clamp(min, max)), cx);
                            } else {
                                let y = this.devices.unlinked_axes.then_some(y_cur);
                                this.edit_dpi((x_cur + delta).clamp(min, max), y, cx);
                            }
                        };
                        h_flex()
                            .gap_3()
                            .items_center()
                            .children(label.map(|l| div().w(px(14.)).text_sm().text_color(muted).child(l.to_string())))
                            .child(Button::new(SharedString::from(down_id)).compact().label("−").on_click(
                                cx.listener(move |this, _, _, cx| apply(this, -step, cx)),
                            ))
                            .child(
                                div()
                                    .min_w(px(96.))
                                    .text_center()
                                    .text_2xl()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(fg)
                                    .child(format!("{value:.0}")),
                            )
                            .child(Button::new(SharedString::from(up_id)).compact().label("+").on_click(
                                cx.listener(move |this, _, _, cx| apply(this, step, cx)),
                            ))
                    };
                    let mut card = section("Sensitivity (DPI)").child(axis_row(unlinked.then_some("X"), x_now, false, cx));
                    if unlinked {
                        card = card.child(axis_row(Some("Y"), y_now, true, cx));
                    }
                    card = card.child(presets);
                    if r.separate_axes {
                        card = card.child(
                            h_flex()
                                .gap_3()
                                .items_center()
                                .child(
                                    Switch::new("dpi-link").checked(!unlinked).on_change(cx.listener(move |this, linked: &bool, _, cx| {
                                        if *linked {
                                            // Re-linking puts Y back on X, so what is written is what is shown.
                                            let x = this.devices.staged.dpi.unwrap_or(current);
                                            this.devices.unlinked_axes = false;
                                            this.devices.staged.dpi_y = None;
                                            if (cur_y - x).abs() > 0.5 {
                                                this.edit_dpi(x, None, cx);
                                            }
                                        } else {
                                            this.devices.unlinked_axes = true;
                                        }
                                        cx.notify();
                                    })),
                                )
                                .child(div().text_sm().text_color(fg).child("Link X and Y"))
                                .child(div().text_xs().text_color(muted).child("Move both axes together")),
                        );
                    }
                    let now = if (cur_y - current).abs() > 0.5 { format!("now {current:.0} (Y {cur_y:.0})") } else { format!("now {current:.0}") };
                    col = col.child(card.child(div().text_xs().text_color(muted).child(now)));
                }

                if !r.polling_rates.is_empty() {
                    col = col.child(self.render_report_frequency(r, cx));
                }

                let lod_options: Vec<String> =
                    r.lod_options.iter().filter(|o| !(r.hide_lod_low && o.as_str() == "Low")).cloned().collect();
                if !lod_options.is_empty() {
                    let shown = staged.lod.clone().or_else(|| r.lod.clone());
                    // Lift-off only applies while a gaming surface mode is selected.
                    let surface_now = staged.surface.as_deref().or(r.surface.as_deref());
                    let disabled = r.lod_requires_surface && surface_now == Some("Off");
                    let mut row = h_flex().gap_2().flex_wrap();
                    for option in lod_options {
                        let picked = option.clone();
                        row = row.child(self.chip(
                            format!("lod-{option}"),
                            option.clone(),
                            shown.as_deref() == Some(option.as_str()),
                            cx,
                            move |this, cx| {
                                if !disabled {
                                    this.edit("liftOff", serde_json::json!(picked), cx)
                                }
                            },
                        ));
                    }
                    col = col.child(section("Lift-off distance").child(row.when(disabled, |r| r.opacity(0.45))).child(div().text_xs().text_color(muted).child(
                        if disabled {
                            "Available with a gaming surface mode selected — lift-off distance is ignored while surface tracking is off."
                        } else {
                            "How far you can lift the mouse before tracking stops. Higher values keep tracking a little longer."
                        },
                    )));
                }

                if let (Some(current), Some(c)) = (&r.surface, &r.controls) {
                    let shown = staged.surface.clone().unwrap_or_else(|| current.clone());
                    let mut row = h_flex().gap_2().flex_wrap();
                    for mode in c.surface_modes.iter().cloned() {
                        row = row.child(self.chip(
                            format!("surface-{mode}"),
                            mode.clone(),
                            shown == mode,
                            cx,
                            move |this, cx| this.edit("surface", serde_json::json!(mode), cx),
                        ));
                    }
                    col = col.child(section("Gaming surface mode").child(row).child(div().text_xs().text_color(muted).child(
                        "Tunes the sensor for gaming mouse pads. Auto lets the mouse decide; turn it off if tracking misbehaves on a non-gaming surface.",
                    )));
                }

                // Everything else the device reports (sleep, motion sync, DPI stages,
                // debounce, low-power threshold, USB speed, primary button…), from the
                // shared JS with the Tauri tab's own gating. These apply immediately.
                for group in &r.tabs.performance {
                    let controls: Vec<_> = group.controls.iter().filter(|c| !(stage_editor && c.id == "perf.dpiStage")).collect();
                    if controls.is_empty() {
                        continue;
                    }
                    let mut card = section(&group.title);
                    for control in controls {
                        card = card.child(self.render_control(control, cx));
                    }
                    col = col.child(card);
                }

                if busy && instant {
                    col = col.child(div().text_sm().text_color(muted).child("Applying…"));
                }
                if let Some(error) = &self.devices.write_error {
                    col = col.child(div().text_sm().text_color(cx.theme().danger).child(format!("Could not apply: {error}")));
                }
                col.into_any_element()
            }
            TileStatus::Failed(error) => div()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(format!("Could not read this device: {error}"))
                .into_any_element(),
            _ => div().text_sm().text_color(muted).child("Connecting…").into_any_element(),
        };

        let firmware = match &tile.status {
            TileStatus::Connected(r) if !r.firmware.is_empty() => {
                Some(format!("Firmware {}", r.firmware.join(" · ")))
            }
            _ => None,
        };
        let _ = (firmware, view_width, fg, &header);
        use crate::web_style::{alpha, icon_el, Lucide, R_MD, R_SM};
        let t = web;
        // Only tabs this device has something for, as before; Overview and
        // Advanced (diagnostics) always.
        let (has_buttons, has_lighting, has_profiles) = match &tile.status {
            TileStatus::Connected(r) => (!r.tabs.buttons.is_empty() || r.onboard_profiles, r.tabs.lighting.is_some(), r.onboard_profiles),
            _ => (false, false, false),
        };
        let tabs: Vec<(DeviceTab, Lucide)> = [
            (DeviceTab::Overview, Lucide::Gauge, true),
            (DeviceTab::Performance, Lucide::ChartColumn, true),
            (DeviceTab::Buttons, Lucide::MousePointer, has_buttons),
            (DeviceTab::Lighting, Lucide::Activity, has_lighting),
            (DeviceTab::Profiles, Lucide::LayoutGrid, has_profiles),
            (DeviceTab::Advanced, Lucide::Sliders, true),
        ]
        .into_iter()
        .filter_map(|(tab, glyph, shown)| shown.then_some((tab, glyph)))
        .collect();
        // `.om-tabs`: a 40px well (4px padding and gap, radius 10) of 32px keys.
        let mut tab_bar = h_flex().flex_none().p(px(4.)).gap(px(4.)).h(px(40.)).rounded(px(R_MD)).bg(hsla(0., 0., 0., 0.26)).border_1().border_color(t.line(1));
        // Icons only when the labels would not fit on one line.
        let labels = content_w >= tabs.len() as f32 * 132.0;
        for (tab, glyph) in tabs {
            let active = self.devices.tab == tab;
            tab_bar = tab_bar.child(
                div()
                    .id(SharedString::from(format!("tab-{}", tab.label())))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(32.))
                    .px(px(12.))
                    .rounded(px(R_SM))
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .text_color(if active { t.text1 } else { t.text2 })
                    .when(active, |k| k.bg(t.surface(4)).border_1().border_color(t.lit()))
                    .when(!active, |k| k.hover(|k| k.text_color(t.text1).bg(t.glint())))
                    .flex_none()
                    .whitespace_nowrap()
                    .child(icon_el(glyph, if active { t.accent } else { t.text3 }, 15.))
                    .when(labels || active, |k| k.child(tab.label()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.devices.tab = tab;
                        if has_profiles && matches!(tab, DeviceTab::Profiles | DeviceTab::Buttons) && this.devices.onboard.is_none() {
                            this.load_onboard_profiles(cx);
                        }
                        cx.notify();
                    })),
            );
        }

        // The rail: breadcrumb, name, connection, then the render alone.
        let (live, connection) = match &tile.status {
            TileStatus::Connected(r) => (true, r.connection.clone()),
            _ => (false, None),
        };
        let crumb = h_flex()
            .items_center()
            .gap(px(8.))
            .text_size(px(11.))
            .text_color(t.text3)
            .child(
                div()
                    .id("crumb-devices")
                    .cursor_pointer()
                    .hover(|c| c.text_color(t.text1))
                    .child("Devices")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.devices.open = None;
                        this.devices.write_error = None;
                        this.devices.staged = Staged::default();
                        cx.notify();
                    })),
            )
            .child(icon_el(Lucide::ChevronRight, t.text4, 11.))
            .child(div().truncate().child(tile.name.clone()));
        let status_row = h_flex()
            .items_center()
            .gap(px(14.))
            .text_size(px(12.))
            .text_color(t.text2)
            .child(
                h_flex()
                    .items_center()
                    .gap(px(7.))
                    .child(div().size(px(7.)).rounded_full().bg(if live { t.ok } else { t.text4 }))
                    .child(match &tile.status {
                        TileStatus::Connected(_) => "Connected",
                        TileStatus::Connecting => "Connecting…",
                        _ => "Not connected",
                    }),
            )
            .children(connection.map(|c| {
                let wired = c == "Wired";
                h_flex().items_center().gap(px(6.)).child(icon_el(if wired { Lucide::Plug } else { Lucide::Wifi }, t.text3, 14.)).child(c)
            }));
        let title = div()
            .text_size(px(22.))
            .font_weight(FontWeight(650.))
            .text_color(t.text1)
            .line_height(px(28.))
            .truncate()
            .child(tile.name.clone());
        // Narrow windows: the rail becomes a header (render beside the name)
        // above the tabs, instead of a column that squeezes the panels.
        let rail = if stacked {
            h_flex()
                .items_center()
                .gap(px(18.))
                .w_full()
                .child(self.device_art(tile, 120., 1.15))
                .child(div().flex().flex_col().gap(px(8.)).flex_1().min_w_0().child(crumb).child(title).child(status_row))
        } else {
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .flex_none()
                .w(px(RAIL_W))
                .child(crumb)
                .child(title)
                .child(status_row)
                .child(div().pt(px(8.)).child(self.device_art(tile, RAIL_W, 1.15)))
        };

        let show_bar = !instant && !staged.is_empty();
        // `.om-pending`: the floating pending-changes bar.
        let apply_bar = div()
            .absolute()
            .left(px(32.))
            .right(px(32.))
            .bottom(px(20.))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .px(px(16.))
            .py(px(10.))
            .rounded(px(14.))
            .border_1()
            .border_color(t.line(2))
            .bg(alpha(t.ink1, 0.97))
            .shadow(vec![BoxShadow { color: hsla(0., 0., 0., 0.6), offset: point(px(0.), px(18.)), blur_radius: px(44.), spread_radius: px(-14.), inset: false }])
            .child(
                h_flex()
                    .items_center()
                    .gap(px(10.))
                    .child(div().size(px(8.)).rounded_full().bg(t.warn))
                    .child(div().text_size(px(13.)).text_color(t.text1).child("Unsaved changes")),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .id("revert")
                            .flex()
                            .items_center()
                            .h(px(32.))
                            .px(px(12.))
                            .rounded(px(R_SM))
                            .cursor_pointer()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.text2)
                            .hover(|b| b.bg(t.glint()).text_color(t.text1))
                            .child("Revert")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.devices.staged = Staged::default();
                                this.devices.write_error = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("apply-changes")
                            .flex()
                            .items_center()
                            .h(px(32.))
                            .px(px(14.))
                            .rounded(px(R_SM))
                            .cursor_pointer()
                            .bg(t.accent)
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.on_accent)
                            .child(if busy { "Applying…" } else { "Apply changes" })
                            .on_click(cx.listener(|this, _, _, cx| this.apply_staged(cx))),
                    ),
            );

        let tab_key = self.devices.tab as u64;
        div()
            .relative()
            .size_full()
            .child(
                div()
                    .id("device-page")
                    .size_full()
                    .overflow_y_scrollbar()
                    .child(
                        div()
                            .flex()
                            .when(stacked, |d| d.flex_col().gap(px(20.)))
                            .when(!stacked, |d| d.flex_row().items_start().gap(px(16.)))
                            .pt(px(28.))
                            .px(px(PAGE_PAD_X))
                            .pb(px(if show_bar { 96. } else { 40. }))
                            .child(rail)
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(16.))
                                    .flex_1()
                                    .min_w_0()
                                    .child(h_flex().child(tab_bar))
                                    .child(
                                        div()
                                            .relative()
                                            .child(content)
                                            .with_animation(
                                                ElementId::NamedInteger("tab-enter".into(), tab_key),
                                                Animation::new(Duration::from_millis(220)).with_easing(gpui_kit::base::animation::ease_out_cubic),
                                                |el, delta| el.opacity(delta).top(px(6.0 * (1.0 - delta))),
                                            ),
                                    ),
                            ),
                    ),
            )
            .when(show_bar, |this| this.child(apply_bar))
            .into_any_element()
    }

    /// Overview tab (`DeviceOverview` in the web app): current setup figures,
    /// the DPI stage chips, and battery over time.
    fn render_device_overview(&self, tile: &Tile, r: &DeviceReadout, content_w: f32, cx: &mut Context<Self>) -> AnyElement {
        use crate::web_style::{figure, group, panel, panel_head, Tokens, R_SM};
        let t = Tokens::read(cx);
        // Below this the three figures go one under another, and the battery
        // and device panels stack.
        let narrow = content_w < 560.0;
        let column = |label: &str, value: AnyElement, meta: Option<String>, first: bool| {
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .when(!narrow, |c| c.flex_1().min_w(px(120.)))
                .when(!first && !narrow, |c| c.pl(px(20.)).border_l_1().border_color(t.line(1)))
                .when(!first && narrow, |c| c.pt(px(14.)).border_t_1().border_color(t.line(1)))
                .child(div().whitespace_nowrap().text_size(px(12.)).text_color(t.text3).child(label.to_string()))
                .child(value)
                .children(meta.map(|m| div().whitespace_nowrap().text_size(px(12.)).text_color(t.text3).child(m)))
        };
        let dash = || div().text_size(px(30.)).text_color(t.text4).child("–").into_any_element();
        let sensitivity = match r.dpi.filter(|d| *d > 0.0) {
            Some(dpi) => figure(&t, group(dpi), "DPI", 30.).into_any_element(),
            None => dash(),
        };
        let in_use = r.stage_count.unwrap_or(r.stages.len()).clamp(1, r.stages.len().max(1));
        let stage_meta = r.active_stage.filter(|_| in_use > 1).map(|a| format!("Stage {} of {}", a + 1, in_use));
        let polling = match r.polling_hz {
            Some(hz) => figure(&t, group(hz), "Hz", 30.).into_any_element(),
            None => dash(),
        };
        let battery = match r.battery {
            Some(b) => figure(&t, format!("{}", b.round() as i64), "%", 30.).into_any_element(),
            None => dash(),
        };
        let left = self.monitor.battery_log.get(&tile.name).and_then(crate::battery_history::time_left);
        let battery_meta = r.battery.map(|_| match (r.charging, left) {
            (true, _) => "Charging".to_string(),
            (false, Some(left)) => format!("Discharging · {}", crate::battery_history::describe_left(left)),
            (false, None) => "Discharging".to_string(),
        });
        let mut setup = panel(&t)
            .gap(px(0.))
            .p(px(0.))
            .child(div().p(px(16.)).child(panel_head(&t, None, "Current setup", Some("Read from the device"))))
            .child(
                div()
                    .flex()
                    .when(narrow, |d| d.flex_col().gap(px(14.)))
                    .when(!narrow, |d| d.flex_row().items_start())
                    .px(px(16.))
                    .pb(px(18.))
                    .child(column("Sensitivity", sensitivity, stage_meta, true))
                    .child(column("Polling rate", polling, None, false))
                    .child(column("Battery", battery, battery_meta, false)),
            );
        if in_use > 1 {
            let mut chips = h_flex().items_center().gap(px(6.)).flex_wrap();
            for (i, dpi) in r.stages.iter().enumerate().take(in_use) {
                let active = r.active_stage == Some(i);
                let light = r.stage_colors.get(i).map(|hex| hex_color(hex));
                chips = chips.child(
                    div()
                        .id(("overview-stage", i))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .h(px(30.))
                        .px(px(14.))
                        .rounded(px(R_SM))
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .cursor_pointer()
                        .bg(if active { t.surface(3) } else { t.surface(1) })
                        .border_1()
                        .border_color(t.lit())
                        .text_color(if active { t.text1 } else { t.text2 })
                        .hover(|c| c.bg(t.surface(2)))
                        .when(active || light.is_some(), |c| {
                            c.child(div().size(px(7.)).rounded_full().bg(light.unwrap_or(t.ok)).when(!active, |d| d.opacity(0.5)))
                        })
                        .child(group(*dpi))
                        .on_click(cx.listener(move |this, _, _, cx| this.set_control("perf.dpiStage".into(), serde_json::json!(i), cx))),
                );
            }
            setup = setup.child(
                div()
                    .flex()
                    .when(narrow, |d| d.flex_col().items_start().gap(px(10.)))
                    .when(!narrow, |d| d.flex_row().items_center().gap(px(16.)))
                    .px(px(16.))
                    .py(px(12.))
                    .border_t_1()
                    .border_color(t.line(1))
                    .child(div().flex_none().whitespace_nowrap().text_size(px(13.)).text_color(t.text2).child("DPI stages"))
                    .child(chips.flex_1().min_w_0()),
            );
        }

        // Battery over time: the past week's readings, as bars.
        let log = self.monitor.battery_log.get(&tile.name).cloned().unwrap_or_default();
        let bars = ((content_w - 40.0) / 9.0).clamp(12.0, 96.0) as usize;
        let mut chart = h_flex().items_end().gap(px(2.)).h(px(130.)).w_full().border_b_1().border_color(t.line(1));
        if log.len() >= 2 {
            let samples = crate::battery_history::sample(&log, bars);
            let last = samples.len().saturating_sub(1);
            for (i, percent) in samples.into_iter().enumerate() {
                let tone = if percent <= self.settings.low_battery_percent { t.bad } else { t.accent };
                chart = chart.child(
                    div()
                        .flex_1()
                        .max_w(px(10.))
                        .h(relative(f32::from(percent).max(2.0) / 100.0))
                        .rounded_t(px(2.))
                        .bg(crate::web_style::alpha(tone, if i == last { 0.95 } else { 0.55 })),
                );
            }
        }
        let since = log.first().map(|(at, _)| crate::battery_history::ago(*at));
        let battery_panel = panel(&t)
            .when(!narrow, |p| p.flex_1().min_w_0())
            .child(panel_head(&t, None, "Battery over time", Some("Past 7 days")))
            .child(if log.len() >= 2 {
                div().flex().flex_col().gap(px(8.)).child(chart).child(
                    h_flex()
                        .justify_between()
                        .text_size(px(11.))
                        .text_color(t.text3)
                        .child(since.unwrap_or_default())
                        .child("Now"),
                ).into_any_element()
            } else {
                div().h(px(130.)).flex().items_center().justify_center().text_size(px(12.)).text_color(t.text3).child(if r.battery.is_some() {
                    "Readings appear here as the battery changes."
                } else {
                    "This mouse does not report its battery over this connection."
                }).into_any_element()
            });
        let fact = |label: &str, value: String| {
            h_flex()
                .justify_between()
                .gap(px(12.))
                .child(div().flex_none().whitespace_nowrap().text_color(t.text3).child(label.to_string()))
                .child(div().min_w_0().truncate().text_color(t.text1).child(value))
        };
        let mut facts = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .text_size(px(13.))
            .child(fact("Model", tile.name.clone()))
            .child(fact("Connection", r.connection.clone().unwrap_or_else(|| "–".into())))
            .child(fact("Polling rate", r.polling_hz.map(|hz| format!("{} Hz", group(hz))).unwrap_or_else(|| "–".into())));
        if r.stages.len() > 1 {
            facts = facts.child(fact("DPI stages", r.stages.len().to_string()));
        }
        if !r.firmware.is_empty() {
            facts = facts.child(fact("Firmware", r.firmware.join(" · ")));
        }
        let device_panel = panel(&t).when(!narrow, |p| p.flex_1().min_w_0()).child(panel_head(&t, None, "Device", None)).child(facts);
        div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(setup)
            .child(
                div()
                    .flex()
                    .gap(px(16.))
                    .when(narrow, |d| d.flex_col())
                    .when(!narrow, |d| d.flex_row().items_start())
                    .child(battery_panel)
                    .child(device_panel),
            )
            .into_any_element()
    }

    /// Performance › Sensitivity in stages (`DpiStagesEditor`): Stages or
    /// Single, how many stages are in use, then one row per stage with its tick
    /// (in use), number (click to make it active), light colour, a value box you
    /// can type into (arrow keys and the scroll wheel step it) and a slider.
    fn render_stage_editor(&self, tile: &Tile, r: &DeviceReadout, cx: &mut Context<Self>) -> AnyElement {
        use crate::web_style::{alpha, figure, group, icon_el, panel, panel_head, Lucide, Tokens, R_SM};
        let t = Tokens::read(cx);
        let total = r.stages.len();
        let count = r.stage_count.unwrap_or(total).clamp(1, total.max(1));
        let editable = r.stage_count_editable;
        let current = r.active_stage.and_then(|a| r.stages.get(a).copied()).or(r.dpi).unwrap_or(0.0);
        let step = r.controls.as_ref().map(|c| c.dpi.step.max(1.0)).unwrap_or(50.0);
        let set_count = move |n: usize| serde_json::json!(n.clamp(1, total.max(1)));

        // `.om-segment`: Stages | Single.
        let segment = |id: &'static str, label: &'static str, on: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .h(px(28.))
                .px(px(12.))
                .rounded(px(R_SM))
                .text_size(px(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .whitespace_nowrap()
                .cursor_pointer()
                .text_color(if on { t.text1 } else { t.text2 })
                .when(on, |k| k.bg(t.surface(4)).border_1().border_color(t.lit()))
                .when(!on, |k| k.hover(|k| k.text_color(t.text1).bg(t.glint())))
                .child(label)
        };
        let single = count == 1;
        let last_multi = if self.devices.last_stage_count < 2 { total } else { self.devices.last_stage_count.min(total) };
        let mode = h_flex()
            .flex_none()
            .p(px(3.))
            .gap(px(3.))
            .rounded(px(8.))
            .bg(hsla(0., 0., 0., 0.26))
            .border_1()
            .border_color(t.line(1))
            .child(segment("stage-mode-multi", "Stages", !single).on_click(cx.listener(move |this, _, _, cx| {
                this.set_control("perf.dpiStageCount".into(), serde_json::json!(last_multi), cx)
            })))
            .child(segment("stage-mode-single", "Single", single).on_click(cx.listener(move |this, _, _, cx| {
                if count > 1 {
                    this.devices.last_stage_count = count;
                }
                this.set_control("perf.dpiStageCount".into(), serde_json::json!(1), cx)
            })));
        let stepper_key = |id: &'static str, glyph: &'static str, enabled: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .size(px(26.))
                .rounded(px(R_SM))
                .text_size(px(15.))
                .text_color(if enabled { t.text1 } else { t.text4 })
                .bg(t.surface(2))
                .border_1()
                .border_color(t.lit())
                .when(enabled, |k| k.cursor_pointer().hover(|k| k.bg(t.surface(3))))
                .child(glyph)
        };
        let in_use = h_flex()
            .items_center()
            .gap(px(8.))
            .text_size(px(13.))
            .text_color(t.text2)
            .child(div().whitespace_nowrap().child("Stages in use"))
            .when(editable, |row| {
                row.child(stepper_key("stages-fewer", "−", count > 1).on_click(cx.listener(move |this, _, _, cx| {
                    if count > 1 {
                        this.set_control("perf.dpiStageCount".into(), set_count(count - 1), cx)
                    }
                })))
            })
            .child(div().whitespace_nowrap().min_w(px(42.)).text_center().font_weight(FontWeight::SEMIBOLD).text_color(t.text1).child(format!("{count} / {total}")))
            .when(editable, |row| {
                row.child(stepper_key("stages-more", "+", count < total).on_click(cx.listener(move |this, _, _, cx| {
                    if count < total {
                        this.set_control("perf.dpiStageCount".into(), set_count(count + 1), cx)
                    }
                })))
            });

        let mut rows = div().flex().flex_col().border_t_1().border_color(t.line(1));
        for (i, dpi) in r.stages.iter().enumerate() {
            let active = r.active_stage == Some(i);
            let used = i < count;
            let key = format!("{}/stage/{i}", tile.key);
            let live = self.slider_live.get(&key).copied().unwrap_or(*dpi);
            let slider = self.control_sliders.get(&key).cloned();
            let input = self.stage_inputs.get(&key).cloned();
            let color = r.stage_colors.get(i).cloned();
            let picking = self.devices.expanded.as_deref() == Some(format!("stage-color-{i}").as_str());
            let device = tile.key.clone();
            let device_keys = device.clone();
            let value_box = div()
                .id(("stage-value", i))
                .flex()
                .items_center()
                .flex_none()
                .w(px(104.))
                .h(px(36.))
                .px(px(10.))
                .rounded(px(R_SM))
                .bg(hsla(0., 0., 0., 0.26))
                .border_1()
                .border_color(t.line(1))
                .text_size(px(14.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.text1)
                .map(|b| match input {
                    Some(input) => b.child(Input::new(&input).appearance(false).w_full()),
                    None => b.justify_end().child(format!("{}", live.round() as i64)),
                })
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    let delta = match event.keystroke.key.as_str() {
                        "up" => step,
                        "down" => -step,
                        _ => return,
                    };
                    cx.stop_propagation();
                    this.stage_edit(&device_keys, i, live + delta, true, window, cx);
                }))
                .on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, window, cx| {
                    let dy = event.delta.pixel_delta(px(20.)).y;
                    if dy == px(0.) {
                        return;
                    }
                    cx.stop_propagation();
                    let now = this.slider_live.get(&format!("{device}/stage/{i}")).copied().unwrap_or(live);
                    this.stage_edit(&device, i, now + if dy > px(0.) { step } else { -step }, true, window, cx);
                }));
            let tick = div()
                .id(("stage-used", i))
                .flex()
                .items_center()
                .justify_center()
                .flex_none()
                .size(px(20.))
                .rounded(px(4.))
                .border_1()
                .when(used, |c| c.bg(t.accent).border_color(t.accent).child(icon_el(Lucide::Check, t.on_accent, 13.)))
                .when(!used, |c| c.border_color(t.line(3)))
                .when(editable, |c| {
                    c.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                        // Unticking a stage stops the cycle before it; ticking
                        // one extends the cycle to it.
                        let next = if used { i.max(1) } else { i + 1 };
                        this.set_control("perf.dpiStageCount".into(), serde_json::json!(next), cx)
                    }))
                });
            let swatch = color.as_ref().map(|hex| {
                div()
                    .id(("stage-color", i))
                    .flex_none()
                    .size(px(18.))
                    .rounded_full()
                    .bg(hex_color(hex))
                    .border_2()
                    .border_color(if picking { t.text1 } else { t.line(3) })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let id = format!("stage-color-{i}");
                        this.devices.expanded = if this.devices.expanded.as_deref() == Some(id.as_str()) { None } else { Some(id) };
                        cx.notify();
                    }))
            });
            rows = rows.child(
                h_flex()
                    .items_center()
                    .gap(px(12.))
                    .h(px(55.))
                    .px(px(4.))
                    .border_b_1()
                    .border_color(t.line(1))
                    .when(active, |row| row.bg(alpha(t.accent, 0.07)))
                    .child(tick)
                    .child(
                        div()
                            .id(("stage-number", i))
                            .flex()
                            .items_center()
                            .justify_center()
                            .flex_none()
                            .size(px(36.))
                            .rounded(px(R_SM))
                            .cursor_pointer()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if active { t.accent } else if used { t.text2 } else { t.text4 })
                            .when(active, |n| n.bg(alpha(t.accent, 0.16)))
                            .when(!active, |n| n.hover(|n| n.bg(t.glint())))
                            .child(format!("{}", i + 1))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if used {
                                    this.set_control("perf.dpiStage".into(), serde_json::json!(i), cx)
                                } else {
                                    this.show_toast(format!("Stage {} isn't in use. Tick it first.", i + 1), false, cx);
                                }
                            })),
                    )
                    .children(swatch)
                    .child(value_box)
                    .child(div().flex_1().min_w(px(60.)).when(!used, |d| d.opacity(0.45)).children(slider.map(|s| Slider::new(&s).bg(t.accent).text_color(t.text1)))),
            );
            if picking {
                let mut swatches = h_flex().flex_wrap().gap(px(8.)).px(px(60.)).py(px(10.)).border_b_1().border_color(t.line(1));
                for hex in LIGHT_SWATCHES.iter().filter(|h| **h != "#000000") {
                    let chosen = color.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(hex));
                    let hex_value = hex.to_string();
                    swatches = swatches.child(
                        div()
                            .id(SharedString::from(format!("stage-{i}-swatch-{hex}")))
                            .size(px(22.))
                            .rounded_full()
                            .bg(hex_color(hex))
                            .border_2()
                            .border_color(if chosen { t.text1 } else { t.line(2) })
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.devices.expanded = None;
                                this.set_control("perf.dpiStageColor".into(), serde_json::json!({ "stage": i, "color": hex_value }), cx)
                            })),
                    );
                }
                rows = rows.child(swatches);
            }
        }
        let note = if r.stage_colors.is_empty() {
            "Type a value or drag its slider; arrow keys and the scroll wheel step it. Click a number to make that stage active."
        } else {
            "Type a value or drag its slider; arrow keys and the scroll wheel step it. Click a number to make that stage active, and a colour dot to change that stage's light."
        };
        panel(&t)
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.))
                    .child(panel_head(&t, Some(Lucide::Crosshair), "Sensitivity", None))
                    .child(figure(&t, group(current), "DPI", 30.)),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap(px(10.))
                    .when(editable, |row| row.child(mode))
                    .child(in_use),
            )
            .child(rows)
            .child(div().text_size(px(13.)).text_color(t.text3).child(note))
            .into_any_element()
    }

    /// Performance › Report frequency: a well of keys with growing bars.
    fn render_report_frequency(&self, r: &DeviceReadout, cx: &mut Context<Self>) -> AnyElement {
        use crate::web_style::{alpha, figure, group, panel, panel_head, Lucide, Tokens, R_SM};
        let t = Tokens::read(cx);
        let shown = self.devices.staged.polling.or(r.polling_hz.map(|hz| hz as u32));
        let read_only = r.polling_read_only;
        let count = r.polling_rates.len().max(1);
        let mut well = h_flex().gap(px(4.)).p(px(4.)).rounded(px(10.)).bg(hsla(0., 0., 0., 0.26)).border_1().border_color(t.line(1));
        for (i, rate) in r.polling_rates.iter().copied().enumerate() {
            let active = shown == Some(rate);
            let label = if rate >= 1000 && rate % 1000 == 0 { format!("{}K", rate / 1000) } else { rate.to_string() };
            let bar_h = 10.0 + 14.0 * (i as f32 + 1.0) / count as f32;
            let tone = if active { t.accent } else { alpha(t.accent, 0.3 + 0.45 * (i as f32 + 1.0) / count as f32) };
            well = well.child(
                div()
                    .id(("rate-key", i))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_end()
                    .gap(px(10.))
                    .flex_1()
                    .h(px(88.))
                    .pb(px(12.))
                    .rounded(px(R_SM))
                    .cursor_pointer()
                    .when(active, |k| k.bg(t.surface(4)).border_1().border_color(t.lit()))
                    .when(!active, |k| k.hover(|k| k.bg(t.glint())))
                    .child(div().w(px(42.)).h(px(bar_h)).rounded(px(3.)).bg(tone))
                    .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(if active { t.text1 } else { t.text2 }).child(label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !read_only {
                            this.edit("pollingRate", serde_json::json!(rate), cx)
                        }
                    })),
            );
        }
        panel(&t)
            .max_w(px(560.))
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .child(panel_head(&t, Some(Lucide::Activity), "Report frequency", None))
                    .child(figure(&t, shown.map(|hz| group(hz as f64)).unwrap_or_else(|| "–".into()), "Hz", 30.)),
            )
            .child(div().text_size(px(13.)).text_color(t.text2).child(if read_only {
                "This mouse reports its polling rate but doesn't let it be changed."
            } else {
                "Higher rates update cursor movement more often, but use more battery."
            }))
            .child(well.when(read_only, |w| w.opacity(0.45)))
            .into_any_element()
    }

    /// Advanced tab: the device's own advanced controls, then Diagnostics.
    fn render_device_advanced(&self, tile: &Tile, r: &DeviceReadout, cx: &mut Context<Self>) -> AnyElement {
        use crate::web_style::{icon_el, panel, Lucide, Tokens};
        let t = Tokens::read(cx);
        let open = self.devices.expanded.as_deref() == Some("diagnostics");
        let mut col = div().flex().flex_col().gap(px(16.));
        if !r.tabs.advanced.is_empty() {
            col = col.child(self.render_extra_tab(r, cx));
        }
        col = col.child(div().text_size(px(13.)).text_color(t.text2).child(
            "Record the device identifier, protocol version, and any failing setting in the issue or pull request. Do not use factory reset during initial testing.",
        ));
        let fact = |label: &str, value: String| {
            h_flex().justify_between().gap(px(16.)).text_size(px(13.)).child(div().text_color(t.text3).child(label.to_string())).child(div().text_color(t.text1).child(value))
        };
        let mut diag = panel(&t).p(px(0.)).gap(px(0.)).child(
            h_flex()
                .id("diagnostics-toggle")
                .items_center()
                .justify_between()
                .px(px(20.))
                .h(px(62.))
                .cursor_pointer()
                .child(div().text_size(px(17.)).font_weight(FontWeight::SEMIBOLD).text_color(t.text1).child("Diagnostics"))
                .child(icon_el(Lucide::ChevronDown, t.text3, 16.))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.devices.expanded = if this.devices.expanded.as_deref() == Some("diagnostics") { None } else { Some("diagnostics".into()) };
                    cx.notify();
                })),
        );
        let report = format!(
            "OpenMouse Desktop v{APP_VERSION} ({})\nDevice: {}\nIdentifier: {}\nDriver: {}\n\n{}",
            std::env::consts::OS,
            tile.name,
            tile.key,
            tile.brand,
            serde_json::to_string_pretty(&r.raw).unwrap_or_default()
        );
        let copy = div()
            .id("copy-diagnostics")
            .flex()
            .items_center()
            .justify_center()
            .h(px(34.))
            .px(px(14.))
            .rounded(px(crate::web_style::R_SM))
            .cursor_pointer()
            .bg(t.surface(2))
            .border_1()
            .border_color(t.lit())
            .text_size(px(13.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(t.text1)
            .hover(|b| b.bg(t.surface(3)))
            .child("Copy diagnostics")
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(report.clone()));
                this.show_toast("Diagnostics copied. Paste them into your message.", false, cx);
            }));
        col = col.child(
            h_flex()
                .items_center()
                .justify_between()
                .gap(px(12.))
                .child(div().flex_1().min_w_0().text_size(px(13.)).text_color(t.text3).child("Something not working? Copy this mouse's details and send them along."))
                .child(copy),
        );
        if open {
            diag = diag.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .px(px(20.))
                    .pb(px(18.))
                    .pt(px(4.))
                    .border_t_1()
                    .border_color(t.line(1))
                    .child(fact("Device", tile.name.clone()))
                    .child(fact("Identifier", tile.key.clone()))
                    .child(fact("Driver", tile.brand.clone()))
                    .child(fact("Connection", r.connection.clone().unwrap_or_else(|| "–".into())))
                    .child(fact("Firmware", if r.firmware.is_empty() { "Not reported".into() } else { r.firmware.join(" · ") }))
                    .child(fact("Battery", r.battery.map(|b| format!("{b:.0}%")).unwrap_or_else(|| "Not reported".into())))
                    .child(fact("DPI stages", if r.stages.is_empty() { "–".into() } else { r.stages.iter().map(|d| format!("{d:.0}")).collect::<Vec<_>>().join(", ") })),
            );
        }
        col.child(diag).into_any_element()
    }

    /// Overview: the connected mouse and its artwork, read through the shared
    /// protocol drivers (`crate::hid`).
    fn render_overview(&self, view_width: f32, view_height: f32, cx: &mut Context<Self>) -> AnyElement {
        if let Some(tile) = self
            .devices
            .open
            .as_ref()
            .and_then(|key| self.devices.tiles.iter().find(|t| &t.key == key))
        {
            return self.render_device_page(tile, view_width, view_height, cx);
        }
        self.render_devices_home(view_width, view_height, cx)
    }

    /// The device the profile screen is editing: the one picked, else the first
    /// connected, else the first listed.
    /// The tiles to show: one card per mouse. A receiver with no mouse is
    /// hidden while a mouse of the same maker is connected another way, and the
    /// same mouse by cable and through its receiver shows once (the cable,
    /// which is the link in use).
    fn visible_tiles(&self) -> Vec<&Tile> {
        let tiles = &self.devices.tiles;
        let vendor = |t: &Tile| t.key.split(':').next().unwrap_or_default().to_string();
        let named = |t: &Tile| matches!(t.status, TileStatus::Connected(_)) && !t.name.contains("receiver");
        let wired = |t: &Tile| matches!(&t.status, TileStatus::Connected(r) if r.connection.as_deref() == Some("Wired"));
        let mut out: Vec<&Tile> = Vec::new();
        for t in tiles {
            if t.name.contains("receiver") && tiles.iter().any(|o| o.key != t.key && vendor(o) == vendor(t) && named(o)) {
                continue;
            }
            if let Some(pos) = out.iter().position(|o| !t.name.is_empty() && o.name == t.name) {
                if wired(t) && !wired(out[pos]) {
                    out[pos] = t;
                }
                continue;
            }
            out.push(t);
        }
        out
    }

    /// The web Devices page (`features/devices/DevicesHome.tsx`, `home.css`):
    /// title and line, then the shelf of device blocks ending in Add device.
    fn render_devices_home(&self, view_width: f32, view_height: f32, cx: &mut Context<Self>) -> AnyElement {
        use crate::web_style::{alpha, icon_el, Lucide, Tokens, R_LG, R_SM};
        let t = Tokens::read(cx);
        let _ = view_width;
        let block_h = (view_height * 0.58).clamp(440.0, 560.0);
        let block_w = 330.0;

        // `.om-btn` / `.om-btn.is-primary` at the block's full width.
        let button = |id: SharedString, label: String, primary: bool, enabled: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .w_full()
                .h(px(38.))
                .rounded(px(R_SM))
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .when(primary, |b| b.bg(t.accent).text_color(t.on_accent))
                .when(!primary, |b| b.bg(t.surface(2)).border_1().border_color(t.lit()).text_color(t.text1).hover(|b| b.bg(t.surface(3))))
                .when(enabled, |b| b.cursor_pointer())
                .when(!enabled, |b| b.opacity(0.45))
                .child(label)
        };

        let mut shelf = h_flex().flex_wrap().justify_center().items_start().gap(px(16.));
        for tile in self.visible_tiles() {
            let live = matches!(tile.status, TileStatus::Connected(_));
            let key = tile.key.clone();
            let brand = if tile.display_brand.is_empty() { tile.brand.clone() } else { tile.display_brand.clone() };
            let meta = if brand.is_empty() { "Mouse".to_string() } else { format!("{brand} · Mouse") };

            let mut top = h_flex()
                .items_center()
                .gap(px(8.))
                .min_h(px(28.))
                .child(div().flex_1().min_w_0().truncate().text_size(px(16.)).font_weight(FontWeight(650.)).text_color(t.text1).child(tile.name.clone()));
            if live {
                top = top.child(
                    h_flex()
                        .flex_none()
                        .items_center()
                        .gap(px(6.))
                        .px(px(8.))
                        .py(px(2.))
                        .rounded(px(R_SM))
                        .bg(alpha(t.ok, 0.14))
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.ok)
                        .child(div().size(px(6.)).rounded_full().bg(t.ok))
                        .child("Connected"),
                );
            }

            let mut stats = h_flex().items_center().gap(px(14.)).min_h(px(20.)).text_size(px(13.)).text_color(t.text2);
            match &tile.status {
                TileStatus::Connected(r) => {
                    let wired = r.connection.as_deref() == Some("Wired");
                    if let Some(connection) = &r.connection {
                        stats = stats.child(
                            h_flex()
                                .items_center()
                                .gap(px(6.))
                                .child(icon_el(if wired { Lucide::Plug } else { Lucide::Wifi }, t.text3, 15.))
                                .child(connection.clone()),
                        );
                    }
                    if let Some(b) = r.battery {
                        let percent = b.clamp(0.0, 100.0).round() as u8;
                        stats = stats.child(
                            h_flex()
                                .items_center()
                                .gap(px(6.))
                                .child(crate::web_style::battery_glyph(&t, percent, r.charging))
                                .child(format!("{percent}%")),
                        );
                    }
                }
                TileStatus::Failed(error) => {
                    stats = stats.child(div().text_size(px(12.)).text_color(t.text3).truncate().child(error.lines().next().unwrap_or_default().to_string()));
                }
                _ => {}
            }

            // `.om-device-block-art`: the stage light, the render and its floor shadow.
            let mut art = div().relative().flex_1().min_h(px(210.)).mx(px(-4.)).flex().items_center().justify_center();
            if live {
                art = art.child(
                    div()
                        .absolute()
                        .size(px(170.))
                        .rounded_full()
                        .bg(alpha(t.accent, 0.07))
                        .shadow(vec![BoxShadow {
                            color: alpha(t.accent, 0.13),
                            offset: point(px(0.), px(0.)),
                            blur_radius: px(70.),
                            spread_radius: px(24.),
                            inset: false,
                        }]),
                );
            }
            art = art
                .child(
                    div()
                        .absolute()
                        .bottom(relative(0.08))
                        .w(relative(0.56))
                        .h(px(14.))
                        .rounded_full()
                        .shadow(vec![BoxShadow {
                            color: hsla(0., 0., 0., 0.5),
                            offset: point(px(0.), px(0.)),
                            blur_radius: px(10.),
                            spread_radius: px(0.),
                            inset: false,
                        }]),
                )
                .child({
                    let hovered = self.monitor.hovered.as_deref() == Some(tile.key.as_str());
                    div().relative().child(self.device_art(tile, 250., 1.0)).with_animation(
                        ElementId::Name(format!("lift-{}-{}", tile.key, hovered as u8).into()),
                        Animation::new(Duration::from_millis(260)).with_easing(gpui_kit::base::animation::ease_out_cubic),
                        move |el, delta| {
                            let lift = if hovered { delta } else { 1.0 - delta };
                            el.top(px(-6.0 * lift))
                        },
                    )
                });

            let action = match &tile.status {
                TileStatus::Connected(_) => button(SharedString::from(format!("configure-{}", tile.key)), "Configure".into(), true, true),
                TileStatus::Connecting => button(SharedString::from(format!("configure-{}", tile.key)), "Connecting…".into(), false, false),
                TileStatus::Failed(_) => button(SharedString::from(format!("configure-{}", tile.key)), "Try again".into(), false, true),
                TileStatus::Idle => button(SharedString::from(format!("configure-{}", tile.key)), "Connect".into(), false, true),
            };
            let open_key = key.clone();
            let action = action.on_click(cx.listener(move |this, _, _, cx| this.open_device(&open_key, cx)));

            let block = div()
                .id(SharedString::from(format!("block-{}", tile.key)))
                .relative()
                .flex()
                .flex_col()
                .gap(px(8.))
                .w(px(block_w))
                .min_h(px(block_h))
                .pt(px(16.))
                .px(px(16.))
                .pb(px(14.))
                .rounded(px(R_LG))
                .bg(t.surface(1))
                .border_1()
                .border_color(if live { alpha(t.accent, 0.24) } else { t.lit() })
                .when(!live, |b| b.hover(|b| b.bg(t.surface(2))))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.open_device(&key, cx)))
                .on_hover({
                    let hover_key = tile.key.clone();
                    cx.listener(move |this, hovered: &bool, _, cx| {
                        let next = if *hovered { Some(hover_key.clone()) } else { None };
                        if this.monitor.hovered != next && (*hovered || this.monitor.hovered.as_deref() == Some(hover_key.as_str())) {
                            this.monitor.hovered = next;
                            cx.notify();
                        }
                    })
                })
                .child(top)
                .child(div().text_size(px(12.)).text_color(t.text3).child(meta))
                .child(stats)
                .child(art)
                .child(action);
            shelf = shelf.child(block);
        }

        // The Add device block: plug-in hint and a rescan.
        let add = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .w(px(block_w))
            .min_h(px(block_h))
            .pt(px(16.))
            .px(px(16.))
            .pb(px(14.))
            .rounded(px(R_LG))
            .bg(t.surface(1))
            .border_1()
            .border_color(t.line(1))
            .child(h_flex().min_h(px(28.)).items_center().child(div().text_size(px(16.)).font_weight(FontWeight(650.)).text_color(t.text1).child("Add device")))
            .child(
                div()
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .text_color(t.text3)
                    .child("Plug in your mouse or its receiver. OpenMouse finds it on its own; rescan if it does not show up."),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(210.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child({
                        let index = (self.monitor.ticks / 2) as usize;
                        img(add_device_art(index)).w(px(190.)).h(px(190.)).object_fit(ObjectFit::Contain).with_animation(
                            ElementId::NamedInteger("showcase".into(), index as u64),
                            Animation::new(Duration::from_millis(900)).with_easing(gpui_kit::base::animation::ease_out_cubic),
                            |el, delta| el.opacity(0.55 * delta),
                        )
                    }),
            )
            .child(
                button("rescan".into(), if self.devices.scanning { "Scanning…".into() } else { "Add device".into() }, false, !self.devices.scanning)
                    .child(icon_el(Lucide::Plus, t.text1, 15.))
                    .on_click(cx.listener(|this, _, _, cx| this.scan_devices(cx))),
            );
        shelf = shelf.child(add);

        div()
            .id("devices-home")
            .size_full()
            .overflow_y_scrollbar()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(22.))
                    .min_h(relative(1.))
                    .pt(px(28.))
                    .px(px(32.))
                    .pb(px(48.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(div().text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).text_color(t.text1).child("Devices"))
                            .child(div().text_size(px(13.)).text_color(t.text2).child("Devices this app can open. Pick one to change its settings.")),
                    )
                    .children(self.devices.error.clone().map(|e| div().text_size(px(12.)).text_color(t.bad).child(e)))
                    .child(div().flex_1().flex().items_center().justify_center().child(shelf.with_animation(
                        "home-enter",
                        Animation::new(Duration::from_millis(320)).with_easing(gpui_kit::base::animation::ease_out_cubic),
                        |el, delta| el.opacity(delta).mt(px(10.0 * (1.0 - delta))),
                    ))),
            )
            .into_any_element()
    }

    fn profile_target(&self) -> Option<usize> {
        let tiles = &self.devices.tiles;
        self.profile_device
            .as_ref()
            .and_then(|key| tiles.iter().position(|t| &t.key == key))
            .or_else(|| tiles.iter().position(|t| matches!(t.status, TileStatus::Connected(_))))
            .or(if tiles.is_empty() { None } else { Some(0) })
    }

    /// One game's device profile: pick a device, then which of its settings the
    /// profile overrides, whether it applies on launch, and Apply now / Clear.
    fn render_profile_editor(&self, game_id: &str, cx: &mut Context<Self>) -> AnyElement {
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let name = self.game_name(game_id);
        let target = self.profile_target();

        let header = h_flex()
            .gap_3()
            .items_center()
            .child(
                Button::new("profile-back")
                    .ghost()
                    .icon(IconName::ArrowLeft)
                    .tooltip("Back to games")
                    .accessibility_label("Back to games")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.profile_editor = None;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).text_color(fg).child(name))
                    .child(div().text_sm().text_color(muted).child("Device profile")),
            );

        // Device picker: one chip per device. A device that isn't connected yet is
        // connected when picked, since its options come from the device itself.
        let picker: AnyElement = if self.devices.tiles.is_empty() {
            div().into_any_element()
        } else {
            let mut row = h_flex().gap_2().flex_wrap();
            for (i, tile) in self.devices.tiles.iter().enumerate() {
                let key = tile.key.clone();
                let has_profile = self.profiles.get(game_id, &tile.key).is_some_and(|p| p.is_meaningful());
                let label = if tile.name.is_empty() { "Unknown device".to_string() } else { tile.name.clone() };
                row = row.child(self.chip(
                    format!("pf-device-{}", tile.key),
                    if has_profile { format!("{label} ●") } else { label },
                    target == Some(i),
                    cx,
                    move |this, cx| {
                        this.profile_device = Some(key.clone());
                        if let Some(t) = this.devices.tiles.iter().find(|t| t.key == key) {
                            if matches!(t.status, TileStatus::Idle | TileStatus::Failed(_)) {
                                this.connect_tile(&key, false, cx);
                            }
                        }
                        cx.notify();
                    },
                ));
            }
            div()
                .flex()
                .flex_col()
                .gap_2()
                .max_w(px(720.))
                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(fg).child("Device"))
                .child(div().text_xs().text_color(muted).child("This profile is saved for the device you pick. ● marks devices that already have one."))
                .child(row)
                .into_any_element()
        };

        let body: AnyElement = match target.and_then(|i| self.devices.tiles.get(i)) {
            None => div()
                .text_sm()
                .text_color(muted)
                .child("No device found. Connect a mouse and press Rescan on the Overview page.")
                .into_any_element(),
            Some(tile) => match &tile.status {
                TileStatus::Connected(r) => {
                    let device = tile.key.clone();
                    let profile = self.profile_for(game_id, &device).unwrap_or_default();
                    let section = |title: &str, hint: &str| {
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .p_4()
                            .rounded_lg()
                            .border_1()
                            .border_color(border)
                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(fg).child(title.to_string()))
                            .child(div().text_xs().text_color(muted).child(hint.to_string()))
                    };
                    let id = game_id.to_string();
                    let mut col = div().flex().flex_col().gap_3().max_w(px(720.));
                    let has_options = r.dpi.is_some() || !r.polling_rates.is_empty() || !r.lod_options.is_empty() || r.surface.is_some();
                    if !has_options {
                        col = col.child(div().text_sm().text_color(muted).child(
                            "This device has no settings a game profile can change.",
                        ));
                    }

                    if let Some(c) = &r.controls {
                        if r.dpi.is_some() {
                            let (gid, dev) = (id.clone(), device.clone());
                            let mut row = h_flex().gap_2().flex_wrap().child(self.chip(
                                "pf-dpi-none".into(),
                                "Unchanged".into(),
                                profile.dpi.is_none(),
                                cx,
                                move |this, cx| this.edit_profile(&gid, &dev, |p| p.dpi = None, cx),
                            ));
                            for preset in c.dpi_presets.iter().copied().filter(|p| *p >= c.dpi.min && *p <= c.dpi.max) {
                                let (gid, dev) = (id.clone(), device.clone());
                                row = row.child(self.chip(
                                    format!("pf-dpi-{preset}"),
                                    format!("{preset:.0}"),
                                    profile.dpi.is_some_and(|d| (d - preset).abs() < 0.5),
                                    cx,
                                    move |this, cx| this.edit_profile(&gid, &dev, |p| p.dpi = Some(preset), cx),
                                ));
                            }
                            col = col.child(section("Sensitivity (DPI)", "Set when the profile applies.").child(row));
                        }
                    }
                    if !r.polling_rates.is_empty() {
                        let (gid, dev) = (id.clone(), device.clone());
                        let mut row = h_flex().gap_2().flex_wrap().child(self.chip(
                            "pf-poll-none".into(),
                            "Unchanged".into(),
                            profile.polling.is_none(),
                            cx,
                            move |this, cx| this.edit_profile(&gid, &dev, |p| p.polling = None, cx),
                        ));
                        for rate in r.polling_rates.iter().copied() {
                            let (gid, dev) = (id.clone(), device.clone());
                            row = row.child(self.chip(
                                format!("pf-poll-{rate}"),
                                format!("{rate} Hz"),
                                profile.polling == Some(rate),
                                cx,
                                move |this, cx| this.edit_profile(&gid, &dev, |p| p.polling = Some(rate), cx),
                            ));
                        }
                        col = col.child(section("Polling rate", "Leave on Unchanged to keep the mouse's rate.").child(row));
                    }
                    if !r.lod_options.is_empty() {
                        let (gid, dev) = (id.clone(), device.clone());
                        let mut row = h_flex().gap_2().flex_wrap().child(self.chip(
                            "pf-lod-none".into(),
                            "Unchanged".into(),
                            profile.lod.is_none(),
                            cx,
                            move |this, cx| this.edit_profile(&gid, &dev, |p| p.lod = None, cx),
                        ));
                        for option in r.lod_options.iter().cloned() {
                            let (gid, dev, picked) = (id.clone(), device.clone(), option.clone());
                            row = row.child(self.chip(
                                format!("pf-lod-{option}"),
                                option.clone(),
                                profile.lod.as_deref() == Some(option.as_str()),
                                cx,
                                move |this, cx| this.edit_profile(&gid, &dev, |p| p.lod = Some(picked.clone()), cx),
                            ));
                        }
                        col = col.child(section("Lift-off distance", "").child(row));
                    }
                    if let (Some(_), Some(c)) = (&r.surface, &r.controls) {
                        let (gid, dev) = (id.clone(), device.clone());
                        let mut row = h_flex().gap_2().flex_wrap().child(self.chip(
                            "pf-surface-none".into(),
                            "Unchanged".into(),
                            profile.surface.is_none(),
                            cx,
                            move |this, cx| this.edit_profile(&gid, &dev, |p| p.surface = None, cx),
                        ));
                        for mode in c.surface_modes.iter().cloned() {
                            let (gid, dev, picked) = (id.clone(), device.clone(), mode.clone());
                            row = row.child(self.chip(
                                format!("pf-surface-{mode}"),
                                mode.clone(),
                                profile.surface.as_deref() == Some(mode.as_str()),
                                cx,
                                move |this, cx| this.edit_profile(&gid, &dev, |p| p.surface = Some(picked.clone()), cx),
                            ));
                        }
                        col = col.child(section("Gaming surface mode", "").child(row));
                    }

                    if has_options {
                        let (gid_auto, dev_auto) = (id.clone(), device.clone());
                        let (gid_apply, dev_apply, applying) = (id.clone(), device.clone(), profile.clone());
                        let (gid_clear, dev_clear) = (id.clone(), device.clone());
                        col = col
                            .child(
                                h_flex()
                                    .gap_4()
                                    .items_center()
                                    .justify_between()
                                    .p_4()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(border)
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_0p5()
                                            .child(div().text_sm().text_color(fg).child("Apply when the game launches"))
                                            .child(div().text_xs().text_color(muted).child("Your previous settings come back when the game closes.")),
                                    )
                                    .child(
                                        Switch::new("profile-auto")
                                            .checked(profile.auto_apply)
                                            .on_change(cx.listener(move |this, on: &bool, _, cx| {
                                                let on = *on;
                                                this.edit_profile(&gid_auto, &dev_auto, |p| p.auto_apply = on, cx)
                                            })),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("profile-apply").label("Apply now").on_click(cx.listener(move |this, _, _, cx| {
                                            if applying.is_meaningful() {
                                                this.apply_profile(&gid_apply, &dev_apply, applying.clone(), false, cx);
                                            } else {
                                                this.show_toast("This profile doesn't change anything yet.", true, cx);
                                            }
                                        })),
                                    )
                                    .child(
                                        Button::new("profile-clear").ghost().label("Clear profile").on_click(cx.listener(move |this, _, _, cx| {
                                            this.profiles.clear(&gid_clear, &dev_clear);
                                            this.profiles.save();
                                            cx.notify();
                                        })),
                                    ),
                            );
                    }
                    col.into_any_element()
                }
                TileStatus::Failed(error) => div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(format!("Could not read this device: {error}"))
                    .into_any_element(),
                _ => div().text_sm().text_color(muted).child("Connecting…").into_any_element(),
            },
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .p_4()
            .gap_4()
            .overflow_y_scrollbar()
            .child(header)
            .child(picker)
            .child(body)
            .into_any_element()
    }

    /// The bottom-of-window message, if one is showing.
    fn render_toast(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let toast = self.toast.as_ref()?;
        let accent = if toast.error { cx.theme().danger } else { cx.theme().primary };
        Some(
            div()
                .absolute()
                .bottom(px(24.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .max_w(px(560.))
                        .px_4()
                        .py_3()
                        .rounded_lg()
                        .border_1()
                        .border_color(accent)
                        .bg(cx.theme().sidebar)
                        .shadow_lg()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(toast.text.clone()),
                )
                .into_any_element(),
        )
    }

    /// Games page: a header with live counts, then the installed/running
    /// games and the rest of the catalog as grids of poster cards. Cached art
    /// falls back to a name-initial tile until prefetch completes.
    fn render_games(&self, cx: &mut Context<Self>) -> AnyElement {
        let (installed, rest): (Vec<&Game>, Vec<&Game>) =
            self.games.iter().partition(|g| g.installed || g.running);
        let running = self.games.iter().filter(|g| g.running).count();
        let profiles = self.games.iter().filter(|g| self.profiles.has_any(&g.id)).count();
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let subtitle = {
            let mut parts = vec![format!("{} installed", installed.len())];
            if running > 0 {
                parts.push(format!("{running} running"));
            }
            parts.push(format!("{profiles} with a profile"));
            parts.join(" · ")
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .px_6()
            .py_5()
            .gap_6()
            .overflow_y_scrollbar()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).text_color(fg).child("Games"))
                    .child(div().text_sm().text_color(muted).child(subtitle)),
            )
            .child(self.render_game_section(cx, "Installed & running", "Pick a game to give it its own mouse profile.", &installed, false))
            .child(self.render_game_section(cx, "More games", "Not found on this PC. Profiles you set up still apply once they are installed.", &rest, true))
            .into_any_element()
    }

    fn render_game_section(
        &self,
        cx: &mut Context<Self>,
        title: &str,
        blurb: &str,
        games: &[&Game],
        muted: bool,
    ) -> AnyElement {
        if games.is_empty() {
            return div().into_any_element();
        }
        let fg = cx.theme().foreground;
        let subtle = cx.theme().muted_foreground;
        let border = cx.theme().border;
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_baseline()
                            .gap_2()
                            .child(div().text_base().font_weight(FontWeight::SEMIBOLD).text_color(fg).child(title.to_string()))
                            .child(
                                div()
                                    .text_xs()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(Hsla { a: 0.08, ..fg })
                                    .text_color(subtle)
                                    .child(games.len().to_string()),
                            ),
                    )
                    .child(div().text_sm().text_color(subtle).child(blurb.to_string())),
            )
            .child(div().h(px(1.)).w_full().bg(Hsla { a: 0.7, ..border }))
            .child(
                div().flex().flex_row().flex_wrap().gap_4().children(
                    games
                        .iter()
                        .map(|game| self.render_game_card(cx, game, muted)),
                ),
            )
            .into_any_element()
    }

    /// One poster card: art on top, then the name and status badges. A running
    /// game gets an accent outline; games not installed are dimmed.
    fn render_game_card(&self, cx: &mut Context<Self>, game: &Game, muted: bool) -> AnyElement {
        const CARD_W: f32 = 148.;
        const ART_H: f32 = 200.;
        let fg = cx.theme().foreground;
        let accent = cx.theme().primary;
        let border = cx.theme().border;
        // Cached file art when prefetched, else the name-initial tile.
        // Remote `img(url)` is not used: desktop GPUI ships no HTTP client
        // (`with_http_client` is web-backend-only), so URLs would stay blank.
        let art = match crate::games::cached_art_path(game) {
            Some(path) => div()
                .w_full()
                .h(px(ART_H))
                .overflow_hidden()
                .child(
                    // PathBuf (not String): String routes to the embedded
                    // asset bundle and misses; PathBuf reads the file.
                    img(path).w_full().h_full().object_fit(ObjectFit::Cover),
                )
                .into_any_element(),
            None => div()
                .w_full()
                .h(px(ART_H))
                .flex()
                .items_center()
                .justify_center()
                .bg(Hsla { a: 0.10, ..accent })
                .text_size(px(40.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(Hsla { a: 0.7, ..accent })
                .child(game.mark().to_string())
                .into_any_element(),
        };
        let game_id = game.id.clone();
        let has_profile = self.profiles.has_any(&game.id);
        div()
            .id(SharedString::from(format!("game-{}", game.id)))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.profile_editor = Some(game_id.clone());
                this.profile_device = None;
                cx.notify();
            }))
            .flex()
            .flex_col()
            .w(px(CARD_W))
            .flex_shrink_0()
            .rounded_xl()
            .overflow_hidden()
            .border_1()
            .border_color(if game.running { Hsla { a: 0.8, ..accent } } else { Hsla { a: 0.7, ..border } })
            .bg(Hsla { a: 0.04, ..fg })
            .hover(move |card| card.bg(Hsla { a: 0.08, ..fg }).border_color(Hsla { a: 0.55, ..accent }))
            .opacity(if muted && !game.running { 0.6 } else { 1.0 })
            .child(art)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .p_2p5()
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .h(px(32.))
                            .overflow_hidden()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(fg)
                            .child(game.name.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap_1()
                            .h(px(20.))
                            .when(game.running, |row| row.child(accent_badge("Running", accent, fg)))
                            .when(game.installed && !game.running, |row| {
                                row.child(Tag::secondary().small().child("Installed"))
                            })
                            .when(has_profile, |row| row.child(accent_badge("Profile", accent, fg))),
                    ),
            )
            .into_any_element()
    }

    /// Shared section surface and heading keep settings on one alignment spine.
    fn render_settings_section(
        &self,
        cx: &mut Context<Self>,
        title: &'static str,
        description: &'static str,
    ) -> Div {
        let t = crate::web_style::Tokens::read(cx);
        crate::web_style::panel(&t)
            .gap(px(14.))
            .p(px(20.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .pb(px(4.))
                    .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(t.text1).child(title))
                    .child(div().text_size(px(12.5)).text_color(t.text3).child(description)),
            )
    }

    /// The theme picker: a card per theme, each a small preview of its page,
    /// sidebar and accent.
    fn render_theme_grid(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::theme::WEB_THEMES;
        let t = crate::web_style::Tokens::read(cx);
        let hex = |v: u32| -> Hsla { rgb(v).into() };
        let mut grid = h_flex().flex_wrap().gap(px(10.));
        for choice in THEME_CHOICES {
            // (sidebar, page, text, accent) for the preview.
            let (side, page, ink, accent) = match choice {
                ThemeChoice::Web(i) => {
                    let w = &WEB_THEMES[i as usize];
                    (hex(w.ink0), hex(w.ink1), hex(w.text1), hex(w.accent))
                }
                ThemeChoice::Oled => (hex(0x000000), hex(0x000000), hex(0xf2f2f2), hex(0x5dde89)),
                ThemeChoice::Dynamic => {
                    let (r, g, b) = crate::theme::cached_dynamic_accent().unwrap_or((93, 222, 137));
                    (hex(0x0d0d0f), hex(0x111114), hex(0xf2f2f2), rgb(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b)).into())
                }
                _ => (hex(0x08090a), hex(0xf2f2ef), hex(0x17181a), hex(0x5dde89)),
            };
            let system = choice == ThemeChoice::System;
            let on = self.settings.theme == choice || (choice == ThemeChoice::Web(0) && self.settings.theme == ThemeChoice::Dark);
            let sub = match choice {
                ThemeChoice::System => if cfg!(target_os = "windows") { "Follow Windows" } else { "Follow system" },
                ThemeChoice::Oled => "True black",
                ThemeChoice::Dynamic => "From wallpaper",
                _ => "",
            };
            let preview = div()
                .relative()
                .flex()
                .h(px(54.))
                .w_full()
                .rounded(px(7.))
                .overflow_hidden()
                .border_1()
                .border_color(t.line(1))
                .child(div().w(px(22.)).h_full().bg(side))
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .bg(page)
                        .flex()
                        .flex_col()
                        .gap(px(5.))
                        .p(px(8.))
                        .child(div().w(px(34.)).h(px(4.)).rounded_full().bg(Hsla { a: 0.9, ..ink }))
                        .child(div().w(px(22.)).h(px(4.)).rounded_full().bg(Hsla { a: 0.4, ..ink }))
                        .child(div().w(px(18.)).h(px(7.)).mt_auto().rounded(px(3.)).bg(accent)),
                )
                .when(system, |p| {
                    // Half dark, half light.
                    p.child(div().absolute().right(px(0.)).top(px(0.)).bottom(px(0.)).w(relative(0.5)).bg(hex(0xf2f2ef)))
                });
            grid = grid.child(
                div()
                    .id(SharedString::from(format!("theme-{}", choice.key())))
                    .flex()
                    .flex_col()
                    .gap(px(7.))
                    .w(px(132.))
                    .p(px(7.))
                    .rounded(px(10.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if on { t.accent } else { t.lit() })
                    .bg(if on { crate::web_style::alpha(t.accent, 0.10) } else { t.surface(1) })
                    .when(!on, |c| c.hover(|c| c.bg(t.surface(2))))
                    .child(preview)
                    .child(
                        div()
                            .px(px(2.))
                            .flex()
                            .flex_col()
                            .child(div().truncate().text_size(px(12.5)).font_weight(FontWeight::SEMIBOLD).text_color(t.text1).child(choice.label()))
                            .when(!sub.is_empty(), |c| c.child(div().truncate().text_size(px(11.)).text_color(t.text3).child(sub))),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| this.set_theme(choice, window, cx))),
            );
        }
        grid.into_any_element()
    }

    /// Mouse alerts: Bridge's banner and chime, and the low-battery level.
    fn render_alert_settings(&self, cx: &mut Context<Self>) -> Div {
        let t = crate::web_style::Tokens::read(cx);
        let mut levels = h_flex().gap_2().flex_wrap();
        for level in [10u8, 15, 20, 25, 30] {
            levels = levels.child(self.chip(
                format!("low-battery-{level}"),
                format!("{level}%"),
                self.settings.low_battery_percent == level,
                cx,
                move |this, cx| {
                    this.settings.low_battery_percent = level;
                    this.settings.save();
                    this.monitor.low_alerted.clear();
                    this.update_tray();
                    cx.notify();
                },
            ));
        }
        self.render_settings_section(cx, "Mouse alerts", "When a mouse connects, runs low or finishes charging.")
            .child(self.render_switch_row(
                "device-banner",
                "Show a banner",
                "A small notice in the corner of the screen, like OpenMouse Bridge's.",
                self.settings.device_banner,
                cx,
            ))
            .child(self.render_switch_row(
                "device-sound",
                "Play a sound",
                "Bridge's two-note chime with each notice.",
                self.settings.device_sound,
                cx,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .py_1()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).text_color(t.text1).child("Low battery warning"))
                    .child(div().text_xs().text_color(t.text3).child("Warn at this level. The tray icon turns red too."))
                    .child(levels),
            )
    }

    /// Updates: automatic install, the version, and a check now.
    fn render_update_settings(&self, cx: &mut Context<Self>) -> Div {
        let t = crate::web_style::Tokens::read(cx);
        let status = if let Some(ready) = &self.update_ready {
            format!("Version {} is downloaded and installs when you restart.", ready.version)
        } else if self.update_downloading {
            "Downloading an update…".to_string()
        } else if let Some(info) = &self.update_available {
            format!("Version {} is available.", info.version)
        } else {
            "You're on the newest version.".to_string()
        };
        self.render_settings_section(cx, "Updates", "New versions download in the background and install when OpenMouse restarts.")
            .child(self.render_switch_row(
                "auto-update",
                "Update automatically",
                "Download new versions on their own and install them on the next start.",
                self.settings.auto_update,
                cx,
            ))
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .py_1()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .min_w_0()
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).text_color(t.text1).child(format!("OpenMouse Desktop v{APP_VERSION}")))
                            .child(div().text_xs().text_color(t.text3).child(status)),
                    )
                    .child(if self.update_ready.is_some() {
                        Button::new("settings-restart-update")
                            .compact()
                            .label("Restart to update")
                            .on_click(cx.listener(|this, _, _, cx| this.restart_to_update(cx)))
                            .into_any_element()
                    } else {
                        Button::new("settings-check-updates")
                            .compact()
                            .label(if self.checking_for_updates { "Checking…" } else { "Check now" })
                            .disabled(self.checking_for_updates)
                            .on_click(cx.listener(|this, _, _, cx| this.check_for_updates_now(cx)))
                            .into_any_element()
                    }),
            )
    }

    /// Labeled switch row: title + description on the left, [`Switch`] on the
    /// right. `switch_id` selects which placeholder setting it drives.
    fn render_switch_row(
        &self,
        switch_id: &'static str,
        title: &'static str,
        description: &'static str,
        checked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_4()
            .py_1()
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(cx.theme().foreground)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(description),
                    )
                    .children(
                        match switch_id {
                            "startup-switch" => self.startup_error.clone(),
                            "discord-switch" => self.discord_error.clone(),
                            _ => None,
                        }
                        .map(|error| div().text_xs().text_color(cx.theme().danger).child(error)),
                    ),
            )
            .child(
                Switch::new(switch_id)
                    .checked(checked)
                    .on_change(cx.listener(move |this, enabled: &bool, _, cx| {
                        if switch_id == "startup-switch" {
                            this.set_launch_on_startup(*enabled);
                        } else if switch_id == "discord-device" {
                            this.settings.discord_show_device = *enabled;
                            this.refresh_presence(cx);
                        } else if switch_id == "discord-game" {
                            this.settings.discord_show_game = *enabled;
                            this.refresh_presence(cx);
                        } else if switch_id == "discord-stats" {
                            this.settings.discord_show_stats = *enabled;
                            this.refresh_presence(cx);
                        } else if switch_id == "overlay-sound" {
                            this.settings.overlay_sound = *enabled;
                            if *enabled {
                                crate::audio::play(crate::audio::Chime::Success);
                            }
                        } else if switch_id == "overlay-switch" {
                            this.settings.overlay_enabled = *enabled;
                            if *enabled {
                                this.announce("Game notifications are on.".into(), false, true, cx);
                            } else if let Some(open) = this.overlay.take() {
                                let _ = open.update(cx, |_, window, _| window.remove_window());
                            }
                        } else if switch_id == "device-banner" {
                            this.settings.device_banner = *enabled;
                        } else if switch_id == "device-sound" {
                            this.settings.device_sound = *enabled;
                            if *enabled {
                                crate::audio::play_notification();
                            }
                        } else if switch_id == "auto-update" {
                            this.settings.auto_update = *enabled;
                            if *enabled {
                                if let Some(info) = this.update_available.clone() {
                                    this.download_update(info, true, cx);
                                }
                            }
                        } else if switch_id == "instant-apply-switch" {
                            this.settings.instant_apply = *enabled;
                            // Nothing may stay staged once edits stop being staged.
                            this.devices.staged = Staged::default();
                        } else {
                            this.settings.discord_presence = *enabled;
                            this.set_discord(*enabled, cx);
                        }
                        this.settings.save();
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// Appearance controls retain their entities and immediate persistence.
    fn render_settings(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = crate::web_style::Tokens::read(cx);
        let show_vibrancy = self.settings.theme == ThemeChoice::Dynamic;
        div()
            .id("settings-page")
            .size_full()
            .overflow_y_scrollbar()
            .child(
                div()
                    .w_full()
                    .max_w(px(820.))
                    .pt(px(28.))
                    .px(px(32.))
                    .pb(px(48.))
                    .flex()
                    .flex_col()
                    .gap(px(16.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .pb(px(6.))
                            .child(div().text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).text_color(t.text1).child("Settings"))
                            .child(div().text_size(px(13.)).text_color(t.text2).child("Appearance, alerts, updates and app preferences.")),
                    )
                    .child(
                        self.render_settings_section(
                            cx,
                            "Appearance",
                            "Pick a theme (the same set as the web app) and adjust the window backdrop.",
                        )
                        .child(self.render_theme_grid(cx))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .items_start()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child("Accent color"),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("Override the theme's accent, or reset to use the theme's own."),
                                )
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap_3()
                                        .child(self.render_accent_picker(cx))
                                        .when(self.settings.accent.is_some(), |row| {
                                            row.child(
                                                Button::new("accent-reset")
                                                    .compact()
                                                    .label("Reset")
                                                    .on_click(cx.listener(|this, _, window, cx| {
                                                        this.set_accent(None, window, cx);
                                                    })),
                                            )
                                        }),
                                ),
                        )
                        .child(div().h(px(1.)).w_full().bg(cx.theme().border))
                        .child(self.render_effect_slider(
                            "Background opacity",
                            "Lower the tint to reveal more of the desktop behind the window.",
                            &self.opacity_slider,
                            format!("{:.0}%", self.settings.window_opacity),
                            cx,
                        ))
                        .child(
                            h_flex()
                                .items_center()
                                .justify_between()
                                .gap_4()
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .flex_1()
                                        .min_w_0()
                                        .gap_1()
                                        .child(div().text_sm().child("Background blur"))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child("Soften the desktop behind the window. macOS controls blur strength."),
                                        ),
                                )
                                .child(
                                    Switch::new("background-blur")
                                        .checked(self.settings.panel_blur > 0.0)
                                        .on_change(cx.listener(|this, enabled: &bool, window, cx| {
                                            this.settings.panel_blur = if *enabled { MAX_BLUR } else { 0.0 };
                                            window.set_background_appearance(if *enabled {
                                                WindowBackgroundAppearance::Blurred
                                            } else {
                                                WindowBackgroundAppearance::Transparent
                                            });
                                            this.settings.save();
                                            cx.notify();
                                        })),
                                ),
                        )
                        .when(show_vibrancy, |this| {
                            this.child(self.render_effect_slider(
                                "Vibrancy",
                                "Adjust the intensity of your wallpaper-derived colors.",
                                &self.vibrancy_slider,
                                format!("{:.0}%", self.settings.dynamic_vibrancy),
                                cx,
                            ))
                        }),
                    )
                    .child(self.render_alert_settings(cx))
                    .child(self.render_update_settings(cx))
                    .child(
                        self.render_settings_section(
                            cx,
                            "General",
                            "Startup and integration preferences.",
                        )
                        .child(self.render_switch_row(
                            "startup-switch",
                            "Launch on startup",
                            "Open OpenMouse automatically when you log in.",
                            self.settings.launch_on_startup,
                            cx,
                        ))
                        .child(div().h(px(1.)).w_full().bg(cx.theme().border))
                        .child(self.render_switch_row(
                            "discord-switch",
                            "Discord Rich Presence",
                            "Show OpenMouse in your Discord status. Requires Discord to be running.",
                            self.settings.discord_presence,
                            cx,
                        ))
                        .when(self.settings.discord_presence, |section| {
                            let (details, state) = self.presence_text();
                            section
                                .child(div().h(px(1.)).w_full().bg(cx.theme().border))
                                .child(div().text_sm().font_weight(FontWeight::MEDIUM).text_color(cx.theme().foreground).child("What to show"))
                                .child(self.render_switch_row(
                                    "discord-device",
                                    "Connected mouse",
                                    "Your mouse's name, e.g. PRO X SUPERLIGHT 2c.",
                                    self.settings.discord_show_device,
                                    cx,
                                ))
                                .child(self.render_switch_row(
                                    "discord-game",
                                    "Current game",
                                    "\"Playing …\" while a game from your library is running.",
                                    self.settings.discord_show_game,
                                    cx,
                                ))
                                .child(self.render_switch_row(
                                    "discord-stats",
                                    "Live stats",
                                    "Your mouse's current DPI and polling rate.",
                                    self.settings.discord_show_stats,
                                    cx,
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_0p5()
                                        .p_3()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(cx.theme().border)
                                        .child(div().text_xs().text_color(cx.theme().muted_foreground).child("Your status will read"))
                                        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(cx.theme().foreground).child("OpenMouse Desktop"))
                                        .child(div().text_sm().text_color(cx.theme().foreground).child(details))
                                        .children((!state.is_empty()).then(|| div().text_sm().text_color(cx.theme().foreground).child(state))),
                                )
                        }),
                    )
                    .child(
                        self.render_settings_section(
                            cx,
                            "Devices",
                            "How changes reach your mouse.",
                        )
                        .child(self.render_switch_row(
                            "instant-apply-switch",
                            "Apply changes instantly",
                            "On: every change is written to the mouse as you make it. Off: changes are held until you press Apply Changes in the bar at the bottom.",
                            self.settings.instant_apply,
                            cx,
                        ))
                        .child(div().h(px(1.)).w_full().bg(cx.theme().border))
                        .child(self.render_switch_row(
                            "overlay-switch",
                            "Game notifications",
                            "Show a message on screen when a game with a profile launches or closes. Appears over the game without taking focus.",
                            self.settings.overlay_enabled,
                            cx,
                        ))
                        .when(self.settings.overlay_enabled, |section| {
                            section
                                .child(div().h(px(1.)).w_full().bg(cx.theme().border))
                                .child(self.render_switch_row(
                                    "overlay-sound",
                                    "Play a sound",
                                    "A short, quiet chime with each game notification.",
                                    self.settings.overlay_sound,
                                    cx,
                                ))
                                .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .py_1()
                                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).text_color(cx.theme().foreground).child("Notification position"))
                                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child(
                                        "Which corner of the screen the message appears in.",
                                    ))
                                    .child({
                                        let mut row = h_flex().gap_2().flex_wrap();
                                        for corner in crate::native_window::OverlayCorner::ALL {
                                            row = row.child(self.chip(
                                                format!("corner-{}", corner.as_str()),
                                                corner.label().to_string(),
                                                self.settings.overlay_corner == corner,
                                                cx,
                                                move |this, cx| {
                                                    this.settings.overlay_corner = corner;
                                                    this.settings.save();
                                                    // Show it where it will appear.
                                                    this.announce("Notifications will appear here.".into(), false, true, cx);
                                                },
                                            ));
                                        }
                                        row
                                    }),
                            )
                        }),
                    ),
            )
            .into_any_element()
    }

    /// One labeled slider row (label + slider + value readout).
    fn render_effect_slider(
        &self,
        label: &'static str,
        description: &'static str,
        state: &Option<Entity<SliderState>>,
        value_text: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let slider = match state {
            Some(state) => div().child(Slider::new(state)).into_any_element(),
            None => div().into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .font_weight(FontWeight::MEDIUM)
                            .child(label),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .child(value_text),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(description),
            )
            .child(slider)
            .into_any_element()
    }
}

/// Colours a list row needs, copied into the list's `'static` render closure.
#[derive(Clone, Copy)]
struct PageColors {
    fg: Hsla,
    muted: Hsla,
    accent: Hsla,
    border: Hsla,
}

/// One release on the Updates page, as a card; the running version is tinted.
/// The list is virtualized, so only the rows in view are ever laid out.
fn release_row(entry: &crate::changelog::Entry, current: bool, c: PageColors) -> AnyElement {
    use crate::changelog::{display_date, grouped, Kind};
    let mut changes = div().flex().flex_col().gap_4().flex_1().min_w_0();
    for (kind, lines) in grouped(entry) {
        let (label, tone) = match kind {
            Some(Kind::New) => ("New", c.accent),
            Some(Kind::Improved) => ("Improved", c.accent),
            Some(Kind::Fixed) => ("Fixed", c.muted),
            None => ("Changes", c.muted),
        };
        // The badge's dot and fill take the tone; its text stays readable.
        let mut group = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().flex().child(accent_badge(label, tone, c.fg)));
        for line in lines {
            group = group.child(
                h_flex()
                    .gap_3()
                    .items_start()
                    .child(div().mt(px(7.)).size(px(5.)).flex_none().rounded_full().bg(Hsla { a: 0.7, ..tone }))
                    .child(div().flex_1().min_w_0().text_sm().line_height(relative(1.5)).text_color(c.fg).child(line.to_string())),
            );
        }
        changes = changes.child(group);
    }
    h_flex()
        .gap_8()
        .items_start()
        .p_5()
        .rounded_xl()
        .border_1()
        .border_color(if current { Hsla { a: 0.5, ..c.accent } } else { c.border })
        .bg(if current { Hsla { a: 0.05, ..c.accent } } else { Hsla { a: 0.03, ..c.fg } })
        .child(
            div()
                .w(px(132.))
                .flex_none()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::BOLD)
                        .text_color(if current { c.accent } else { c.fg })
                        .child(format!("v{}", entry.version)),
                )
                .child(div().text_xs().text_color(c.muted).child(display_date(&entry.date)))
                .children(current.then(|| div().text_xs().font_weight(FontWeight::MEDIUM).text_color(c.accent).child("Current"))),
        )
        .child(changes)
        .into_any_element()
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_control_sliders(window, cx);
        let pages = [Page::Overview, Page::Games, Page::Supported];
        // Kit's Root paints the translucent semantic background once.
        // Keep the app shell clear so overlapping layers do not compound alpha.
        let clear = Hsla {
            a: 0.0,
            ..cx.theme().background
        };
        let toast = self.render_toast(cx);
        let ascii = self.render_ascii_background(window, cx);
        // The sidebar's tint and divider run the window's full height, behind
        // the title bar too; the sidebar itself is transparent over it.
        let sidebar_backdrop = div()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .h_full()
            .w(px(if self.sidebar_collapsed { SIDEBAR_COLLAPSED_W } else { SIDEBAR_W }))
            .border_r_1()
            .border_color(crate::web_style::Tokens::read(cx).line(1))
            .bg(Hsla { a: 0.92, ..cx.theme().sidebar });
        div().relative().size_full().children(toast).child(ascii).child(sidebar_backdrop).child(
            div()
                .flex()
                .flex_col()
                .size_full()
                .text_color(cx.theme().foreground)
                .child(TitleBar::new().bg(clear).border_b_0().h(px(TITLEBAR_H)).child(self.render_titlebar(cx)))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_1()
                        .min_h_0()
                        .child(self.render_sidebar(cx))
                        .child(
                            div()
                                .flex()
                                .flex_1()
                                .min_w_0()
                                .child(self.render_page(window, cx)),
                        ),
                ),
        )
    }
}
