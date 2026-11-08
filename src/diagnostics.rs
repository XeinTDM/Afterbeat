use crate::settings;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

static WRITER: Mutex<()> = Mutex::new(());
const MAX_LOG: u64 = 64 * 1024;

pub fn folder() -> Option<PathBuf> {
    Some(settings::path()?.parent()?.to_owned())
}

pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record(&format!(
            "Panic on {}: {info}",
            std::thread::current().name().unwrap_or("unnamed thread")
        ));
        default(info);
    }));
}

pub fn record(message: &str) {
    let Ok(_guard) = WRITER.try_lock() else {
        return;
    };
    let Some(folder) = folder() else {
        return;
    };
    let _ = write(&folder, message);
}

fn write(folder: &std::path::Path, message: &str) -> io::Result<()> {
    fs::create_dir_all(folder)?;
    let path = folder.join("diagnostics.log");
    if fs::metadata(&path).is_ok_and(|meta| meta.len() >= MAX_LOG) {
        fs::rename(&path, folder.join("diagnostics.previous.log"))?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let message: String = message
        .chars()
        .take(2_000)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    writeln!(
        file,
        "{timestamp}  v{}  {message}",
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(windows)]
pub fn open_folder() -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
    let folder =
        folder().ok_or_else(|| io::Error::other("Windows did not provide a local data folder."))?;
    fs::create_dir_all(&folder)?;
    let path: Vec<u16> = folder.as_os_str().encode_wide().chain([0]).collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            std::ptr::null(),
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize <= 32 {
        return Err(io::Error::other(
            "Windows could not open the local data folder.",
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn open_folder() -> io::Result<()> {
    let folder =
        folder().ok_or_else(|| io::Error::other("No local configuration folder is available."))?;
    fs::create_dir_all(&folder)?;
    #[cfg(target_os = "linux")]
    let command = "xdg-open";
    #[cfg(target_os = "macos")]
    let command = "/usr/bin/open";
    let mut opener = std::process::Command::new(command).arg(folder).spawn()?;
    std::thread::spawn(move || {
        if let Ok(status) = opener.wait()
            && !status.success()
        {
            record(&format!(
                "Could not open the configuration folder ({status})."
            ));
        }
    });
    Ok(())
}
