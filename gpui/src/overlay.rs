//! The on-screen game toast: a small always-on-top, click-through pill shown in
//! a corner of the screen the game is on, never under the menu bar, Dock or
//! taskbar. Used for profile apply/restore messages while a game has focus, so
//! it lives in its own window (the main window may be minimized or hidden).
//!
//! It must not take focus from the game: the window is opened without focus and
//! made click-through (see `native_window::configure_overlay`).

use std::time::Duration;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;

use crate::native_window::{configure_overlay, OverlayCorner};

/// Same pill as the Tauri overlay (`OVERLAY_SIZE`, `OVERLAY_MARGIN`).
// OpenMouse Bridge's banner (`src/overlay/render.rs`): 60 high, radius 14,
// 18 padding, 240-460 wide to fit, title 15 over detail 12.5, no status dot.
const WIDTH: f32 = 420.0;
const HEIGHT: f32 = 60.0;
const MARGIN: f32 = 24.0;
const MIN_WIDTH: f32 = 240.0;
const MAX_WIDTH: f32 = 460.0;
const PADDING: f32 = 18.0;
/// Bridge holds its banner 2.6 s between 180 ms fades.
const FADE: Duration = Duration::from_millis(180);
const VISIBLE_FOR: Duration = Duration::from_millis(2600 + 2 * 180);

struct OverlayView {
    title: String,
    detail: Option<String>,
    leaving: bool,
}

impl Render for OverlayView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let leaving = self.leaving;
        div()
            .size_full()
            .flex()
            .flex_col()
            .justify_center()
            .gap(px(2.))
            .px(px(PADDING))
            .rounded(px(14.))
            .bg(rgba(0x0c0d0ff5))
            .border_1()
            .border_color(rgba(0xffffff14))
            .child(div().text_size(px(15.)).text_color(rgb(0xeef0f2)).truncate().child(self.title.clone()))
            .children(self.detail.clone().map(|detail| {
                div().text_size(px(12.5)).text_color(rgb(0xaab0b7)).truncate().child(detail)
            }))
            .with_animation(
                ElementId::NamedInteger("overlay-fade".into(), leaving as u64),
                Animation::new(FADE),
                move |this, delta| this.opacity(if leaving { 1.0 - delta } else { delta }),
            )
    }
}

/// The banner's width for its text, as Bridge fits it.
fn banner_width(title: &str, detail: Option<&str>) -> f32 {
    let title = title.chars().count() as f32 * 8.2;
    let detail = detail.map_or(0.0, |d| d.chars().count() as f32 * 6.6);
    (title.max(detail) + 2.0 * PADDING + 8.0).clamp(MIN_WIDTH, MAX_WIDTH)
}

/// Bridge's banner: `title` over `detail`.
pub fn show_banner(
    cx: &mut App,
    previous: Option<AnyWindowHandle>,
    title: String,
    detail: Option<String>,
    corner: OverlayCorner,
) -> Option<AnyWindowHandle> {
    open(cx, previous, title, detail, corner)
}

pub fn show(
    cx: &mut App,
    previous: Option<AnyWindowHandle>,
    text: String,
    _error: bool,
    corner: OverlayCorner,
) -> Option<AnyWindowHandle> {
    open(cx, previous, text, None, corner)
}

fn open(
    cx: &mut App,
    previous: Option<AnyWindowHandle>,
    title: String,
    detail: Option<String>,
    corner: OverlayCorner,
) -> Option<AnyWindowHandle> {
    let width = banner_width(&title, detail.as_deref());
    if let Some(old) = previous {
        let _ = old.update(cx, |_, window, _| window.remove_window());
    }

    // Off macOS the position comes from the display's visible bounds (which
    // exclude the taskbar); on macOS `configure_overlay` places it after opening.
    let origin = cx
        .primary_display()
        .map(|display| {
            let area = display.visible_bounds();
            let (w, h) = (px(width), px(HEIGHT));
            let m = px(MARGIN);
            let x = match corner {
                OverlayCorner::TopLeft | OverlayCorner::BottomLeft => area.origin.x + m,
                _ => area.origin.x + area.size.width - w - m,
            };
            let y = match corner {
                OverlayCorner::TopLeft | OverlayCorner::TopRight => area.origin.y + m,
                _ => area.origin.y + area.size.height - h - m,
            };
            point(x, y)
        })
        .unwrap_or_else(|| point(px(MARGIN), px(MARGIN)));

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin,
            size: size(px(width), px(HEIGHT)),
        })),
        titlebar: None,
        focus: false,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    };
    let handle = cx
        .open_window(options, move |_, cx| cx.new(|_| OverlayView { title, detail, leaving: false }))
        .ok()?;
    configure_overlay(width as f64, HEIGHT as f64, corner, MARGIN as f64);
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let hwnd = handle
            .update(cx, |_, window, _| match HasWindowHandle::window_handle(&*window).map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(h)) => Some(h.hwnd.get()),
                _ => None,
            })
            .ok()
            .flatten();
        if let Some(hwnd) = hwnd {
            crate::native_window::configure_overlay_hwnd(hwnd, width as f64, HEIGHT as f64, corner, MARGIN as f64);
        }
    }

    let any: AnyWindowHandle = handle.into();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(VISIBLE_FOR - FADE).await;
        let _ = handle.update(cx, |view, _, cx| {
            view.leaving = true;
            cx.notify();
        });
        cx.background_executor().timer(FADE).await;
        let _ = any.update(cx, |_, window, _| window.remove_window());
    })
    .detach();
    Some(any)
}
