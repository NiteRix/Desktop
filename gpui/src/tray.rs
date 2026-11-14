//! System tray: closing the main window hides it to a tray icon instead of
//! quitting, and the icon brings it back. Quitting is the tray menu's job.
//!
//! Windows and macOS (menu bar); elsewhere [`install`] does nothing and closing
//! the window quits as before.

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use futures::channel::mpsc;
    use futures::StreamExt as _;
    use gpui_kit::{AnyWindowHandle, App, AppContext as _, Window};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    use crate::activity::ACTIVITY;
    use crate::tray_battery::{self, TrayBattery};
    use std::cell::RefCell;

    thread_local! {
        /// The tray icon, owned by the main thread for the life of the app.
        static TRAY: RefCell<Option<tray_icon::TrayIcon>> = const { RefCell::new(None) };
        /// What the icon shows now, so an unchanged reading is not redrawn.
        static SHOWN: RefCell<Option<(TrayBattery, u8)>> = const { RefCell::new(None) };
    }

    const APP_TOOLTIP: &str = "OpenMouse Desktop";

    /// Shows `battery` in the tray (Bridge's battery icon and tooltip), or the
    /// app icon again when there is none. `threshold` is the low-battery level
    /// that turns the icon red.
    pub fn show_battery(battery: Option<&TrayBattery>, threshold: u8) {
        let next = battery.map(|b| (b.clone(), threshold));
        let unchanged = SHOWN.with(|shown| *shown.borrow() == next);
        if unchanged {
            return;
        }
        TRAY.with(|tray| {
            let tray = tray.borrow();
            let Some(tray) = tray.as_ref() else { return };
            match battery {
                Some(battery) => {
                    let ink = battery.ink(threshold);
                    let Ok((rgba, width, height)) = tray_battery::icon_rgba(battery, ink) else { return };
                    let Ok(icon) = tray_icon::Icon::from_rgba(rgba, width, height) else { return };
                    let _ = tray.set_icon(Some(icon));
                    let _ = tray.set_tooltip(Some(tray_battery::tooltip(battery)));
                }
                None => {
                    let _ = tray.set_icon(platform::icon());
                    let _ = tray.set_tooltip(Some(APP_TOOLTIP));
                }
            }
            SHOWN.with(|shown| *shown.borrow_mut() = next);
        });
    }

    enum Command {
        Show,
        Quit,
    }

    #[cfg(windows)]
    mod platform {
        use gpui_kit::Window;
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use tray_icon::Icon;

        #[link(name = "user32")]
        extern "system" {
            fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
            fn SetForegroundWindow(hwnd: isize) -> i32;
        }

        const SW_HIDE: i32 = 0;
        /// Shows a hidden window and un-minimizes a minimized one.
        const SW_RESTORE: i32 = 9;
        /// The app icon `build.rs` embeds as the exe's first icon resource.
        const APP_ICON_RESOURCE: u16 = 1;

        fn hwnd_of(window: &Window) -> Option<isize> {
            match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
                Ok(RawWindowHandle::Win32(handle)) => Some(handle.hwnd.get()),
                _ => None,
            }
        }

        pub fn icon() -> Option<Icon> {
            Icon::from_resource(APP_ICON_RESOURCE, Some((32, 32))).ok()
        }

        /// Hides the window and its taskbar button.
        pub fn hide_window(window: &Window) {
            if let Some(hwnd) = hwnd_of(window) {
                unsafe {
                    ShowWindow(hwnd, SW_HIDE);
                }
            }
        }

        pub fn show_window(window: &mut Window) {
            if let Some(hwnd) = hwnd_of(window) {
                unsafe {
                    ShowWindow(hwnd, SW_RESTORE);
                    SetForegroundWindow(hwnd);
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    mod platform {
        use cocoa::base::{id, nil};
        use gpui_kit::Window;
        use objc::{class, msg_send, sel, sel_impl};
        use tray_icon::Icon;

        pub fn icon() -> Option<Icon> {
            let image = image::load_from_memory(include_bytes!("../assets/logo.png")).ok()?.into_rgba8();
            let (width, height) = image.dimensions();
            Icon::from_rgba(image.into_raw(), width, height).ok()
        }

        /// Hides the whole app (like Cmd+H); the menu bar icon stays.
        pub fn hide_window(_window: &Window) {
            unsafe {
                let app: id = msg_send![class!(NSApplication), sharedApplication];
                let _: () = msg_send![app, hide: nil];
            }
        }

        pub fn show_window(_window: &mut Window) {
            unsafe {
                let app: id = msg_send![class!(NSApplication), sharedApplication];
                let _: () = msg_send![app, unhide: nil];
            }
        }
    }

    /// Hides the window (no taskbar button either) and tells the background
    /// loops nobody is looking.
    pub fn hide(window: &Window) {
        platform::hide_window(window);
        ACTIVITY.set_active(false);
    }

    fn show(window: &mut Window) {
        platform::show_window(window);
        window.activate_window();
        ACTIVITY.set_active(true);
    }

    /// Creates the tray icon and makes closing `window` hide it. Returns
    /// whether the tray exists; without one, closing still quits.
    pub fn install(cx: &mut App, window: AnyWindowHandle) -> bool {
        let Some(icon) = platform::icon() else {
            return false;
        };
        let open = MenuItem::with_id("open", "Open OpenMouse", true, None);
        let quit = MenuItem::with_id("quit", "Quit", true, None);
        let menu = Menu::new();
        if menu.append_items(&[&open, &PredefinedMenuItem::separator(), &quit]).is_err() {
            return false;
        }
        let Ok(tray) = TrayIconBuilder::new()
            .with_icon(icon)
            .with_tooltip(APP_TOOLTIP)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
        else {
            return false;
        };

        // The handlers run inside the tray's window procedure on the main
        // thread; they only queue a command for the task below.
        let (commands, mut queue) = mpsc::unbounded();
        let menu_commands = commands.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let command = match event.id.as_ref() {
                "open" => Command::Show,
                "quit" => Command::Quit,
                _ => return,
            };
            let _ = menu_commands.unbounded_send(command);
        }));
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            let clicked = matches!(
                event,
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                    | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
            );
            if clicked {
                let _ = commands.unbounded_send(Command::Show);
            }
        }));

        // The main thread owns the icon for the life of the app.
        TRAY.with(|slot| *slot.borrow_mut() = Some(tray));
        cx.spawn(async move |cx| {
            while let Some(command) = queue.next().await {
                match command {
                    Command::Show => {
                        let _ = cx.update_window(window, |_, window, _| show(window));
                    }
                    Command::Quit => {
                        let _ = cx.update(|cx| cx.quit());
                    }
                }
            }
        })
        .detach();

        let _ = cx.update_window(window, |_, window, cx| {
            window.on_window_should_close(cx, |window, _| {
                hide(window);
                false
            });
        });
        true
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub use imp::{hide, install, show_battery};

#[cfg(not(any(windows, target_os = "macos")))]
pub fn show_battery(_battery: Option<&crate::tray_battery::TrayBattery>, _threshold: u8) {}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn install(_cx: &mut gpui_kit::App, _window: gpui_kit::AnyWindowHandle) -> bool {
    false
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn hide(_window: &gpui_kit::Window) {}
