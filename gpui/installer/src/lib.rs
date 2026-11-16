pub mod colors;
pub mod devices;
pub mod paths;
pub mod payload;
pub mod registry;
pub mod shortcuts;

/// Passed to the copy of this binary left in the install directory, so the
/// Add/Remove Programs `UninstallString` can run the exact same .exe in its
/// other mode instead of needing a second embedded binary.
pub const UNINSTALL_ARG: &str = "--uninstall";

/// Passed by the app when it hands over to a downloaded update: install
/// without a window, then start the app again. Arguments after it are given
/// to the restarted app (`--autostart` keeps a login start in the tray).
pub const UPDATE_ARG: &str = "--update";

/// One step of the install, reported back to the UI as it runs.
#[derive(Debug, Clone)]
pub struct StepUpdate {
    pub label: &'static str,
    pub done: usize,
    pub total: usize,
}

/// Copies the embedded app payload and this running .exe (as the
/// uninstaller) into the install directory, then writes the Start Menu
/// shortcut, the optional Desktop shortcut, and the Add/Remove Programs
/// registry entry. `report` is called after each step for progress UI.
pub fn install(desktop_shortcut: bool, mut report: impl FnMut(StepUpdate)) -> Result<(), String> {
    let total = if desktop_shortcut { 5 } else { 4 };
    let mut done = 0;
    let mut step = |label: &'static str, result: Result<(), String>, report: &mut dyn FnMut(StepUpdate)| {
        done += 1;
        report(StepUpdate { label, done, total });
        result
    };

    let dir = paths::install_dir();
    step(
        "Creating install folder",
        std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {}: {e}", dir.display())),
        &mut report,
    )?;

    step(
        "Copying OpenMouse Desktop",
        std::fs::write(paths::app_exe_path(), payload::APP_EXE).map_err(|e| e.to_string()),
        &mut report,
    )?;

    step(
        "Installing the uninstaller",
        std::env::current_exe()
            .map_err(|e| e.to_string())
            .and_then(|self_exe| std::fs::copy(self_exe, paths::uninstaller_exe_path()).map_err(|e| e.to_string()).map(|_| ())),
        &mut report,
    )?;

    step("Adding Start Menu shortcut", shortcuts::create_start_menu(), &mut report)?;

    if desktop_shortcut {
        step("Adding Desktop shortcut", shortcuts::create_desktop(), &mut report)?;
    }

    // Registry last: if anything above failed, Add/Remove Programs never
    // learns about a half-written install.
    registry::write()
}

/// The silent update [`UPDATE_ARG`] asks for. The app quits right after
/// starting this, so its .exe stays locked for a moment: the new one is
/// written beside it and swapped in once the old process has let go. The
/// app is started again either way, so a failed update leaves the old
/// version running rather than nothing.
pub fn update(relaunch_args: &[String]) -> Result<(), String> {
    let exe = paths::app_exe_path();
    let staged = exe.with_extension("new");
    let result = (|| {
        std::fs::create_dir_all(paths::install_dir()).map_err(|e| e.to_string())?;
        std::fs::write(&staged, payload::APP_EXE).map_err(|e| e.to_string())?;
        let mut last = String::new();
        for _ in 0..60 {
            match std::fs::rename(&staged, &exe) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last = e.to_string();
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
        let _ = std::fs::remove_file(&staged);
        Err(format!("the app did not close in time: {last}"))
    })();
    if result.is_ok() {
        if let Ok(self_exe) = std::env::current_exe() {
            let _ = std::fs::copy(self_exe, paths::uninstaller_exe_path());
        }
        let _ = shortcuts::create_start_menu();
        if paths::desktop_shortcut_path().is_some_and(|p| p.exists()) {
            let _ = shortcuts::create_desktop();
        }
        let _ = registry::write();
    }
    let _ = std::process::Command::new(&exe).args(relaunch_args).spawn();
    result
}

/// Removes everything `install` created except the install directory
/// itself, which the caller must remove via [`spawn_self_delete`] once this
/// process (and its lock on the directory/its own .exe) has exited.
pub fn uninstall() -> Result<(), String> {
    shortcuts::remove_start_menu();
    shortcuts::remove_desktop();
    registry::remove()
}

/// Windows won't let a running .exe delete its own containing directory.
/// Spawns a detached helper that waits briefly for this process to exit,
/// then removes the install directory — the standard self-delete trick
/// NSIS-style uninstallers use internally.
#[cfg(windows)]
pub fn spawn_self_delete(install_dir: &std::path::Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let dir = install_dir.to_string_lossy().into_owned();
    let command = format!("ping 127.0.0.1 -n 2 >nul & rmdir /s /q \"{dir}\"");
    let _ = std::process::Command::new("cmd")
        .args(["/C", &command])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

#[cfg(not(windows))]
pub fn spawn_self_delete(_install_dir: &std::path::Path) {}
