use block2::RcBlock;
use objc2::{
    rc::{Retained, autoreleasepool},
    runtime::ProtocolObject,
};
use objc2_app_kit::{
    NSApplicationActivationOptions, NSApplicationWillTerminateNotification, NSRunningApplication,
    NSWorkspace,
};
use objc2_core_foundation::{
    CFDictionary, CFNumber, CFRetained, CFString, CFType, CGPoint, CGRect,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGPreflightListenEventAccess, CGPreflightPostEventAccess,
    CGRequestListenEventAccess, CGRequestPostEventAccess, CGWindowListCopyWindowInfo,
    CGWindowListOption, kCGWindowAlpha, kCGWindowBounds, kCGWindowOwnerPID,
};
use objc2_foundation::{NSNotificationCenter, NSObjectProtocol, NSString};
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io,
    os::unix::ffi::OsStrExt,
    path::Path,
};

pub const PERMISSIONS_HELP: &str = "Use Help → Input permissions to grant Accessibility and Input Monitoring, then reopen Afterbeat.";

pub fn has_permissions() -> bool {
    CGPreflightListenEventAccess() && CGPreflightPostEventAccess()
}

pub fn request_permissions() {
    if !CGPreflightPostEventAccess() {
        CGRequestPostEventAccess();
    }
    if !CGPreflightListenEventAccess() {
        CGRequestListenEventAccess();
    }
}

pub fn secure_input_enabled() -> bool {
    unsafe { IsSecureEventInputEnabled() != 0 }
}

pub struct QuitObserver(Retained<ProtocolObject<dyn NSObjectProtocol>>);

impl QuitObserver {
    pub fn install() -> Self {
        let on_quit = RcBlock::new(|_| {
            // AppKit delivers this notification on the main thread.
            if let Some(controller) =
                crate::ACTIVE_CONTROLLER.with(|local| local.borrow().upgrade())
            {
                controller.shutdown();
            }
        });
        let observer = unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                Some(NSApplicationWillTerminateNotification),
                None,
                None,
                &on_quit,
            )
        };
        Self(observer)
    }
}

impl Drop for QuitObserver {
    fn drop(&mut self) {
        unsafe { NSNotificationCenter::defaultCenter().removeObserver((*self.0).as_ref()) };
    }
}

pub fn acquire_instance() -> io::Result<Option<File>> {
    let path = crate::settings::path()
        .ok_or_else(|| io::Error::other("No local data folder is available."))?;
    fs::create_dir_all(path.parent().unwrap())?;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_file_name(".lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => {
            let id = NSString::from_str("io.github.xeintdm.afterbeat");
            for app in NSRunningApplication::runningApplicationsWithBundleIdentifier(&id).iter() {
                if app.processIdentifier() != own_pid() {
                    app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
                    break;
                }
            }
            Ok(None)
        }
        Err(TryLockError::Error(error)) => Err(error),
    }
}

pub fn own_pid() -> i32 {
    std::process::id() as i32
}

pub fn foreground_pid() -> u32 {
    autoreleasepool(|_| {
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .and_then(|app| u32::try_from(app.processIdentifier()).ok())
            .unwrap_or_default()
    })
}

pub fn process_name(pid: u32) -> Option<String> {
    let pid = i32::try_from(pid).ok().filter(|&pid| pid > 0)?;
    let mut buffer = [0; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // proc_pidpath writes a NUL-terminated path to the supplied buffer.
    let len = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if len <= 0 {
        return None;
    }
    let path = std::ffi::CStr::from_bytes_until_nul(&buffer).ok()?;
    Path::new(std::ffi::OsStr::from_bytes(path.to_bytes()))
        .file_name()?
        .to_str()
        .map(str::to_owned)
}

pub fn window_pid(point: CGPoint) -> u32 {
    let Some(windows) = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
        0,
    ) else {
        return 0;
    };
    // Quartz supplies an array of dictionaries with string keys and CF values.
    let windows = unsafe { windows.cast_unchecked::<CFDictionary<CFString, CFType>>() };
    for window in windows.iter() {
        let Some(bounds) = window_value::<CFDictionary>(&window, unsafe { kCGWindowBounds }) else {
            continue;
        };
        let mut rect = CGRect::ZERO;
        if !unsafe { CGRectMakeWithDictionaryRepresentation(&bounds, &mut rect) }
            || !(rect.min().x..rect.max().x).contains(&point.x)
            || !(rect.min().y..rect.max().y).contains(&point.y)
            || window_value::<CFNumber>(&window, unsafe { kCGWindowAlpha })
                .and_then(|number| number.as_f64())
                == Some(0.0)
        {
            continue;
        }
        return window_value::<CFNumber>(&window, unsafe { kCGWindowOwnerPID })
            .and_then(|number| u32::try_from(number.as_i32()?).ok())
            .unwrap_or_default();
    }
    0
}

fn window_value<T: objc2_core_foundation::ConcreteType>(
    window: &CFDictionary<CFString, CFType>,
    key: &CFString,
) -> Option<CFRetained<T>> {
    window.get(key)?.downcast().ok()
}

pub fn event_tag() -> i64 {
    0x4166_7472_0000_0000 | i64::from(own_pid())
}

pub fn is_ours(event: &CGEvent) -> bool {
    CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUserData) == event_tag()
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGRectMakeWithDictionaryRepresentation(dict: &CFDictionary, rect: &mut CGRect) -> bool;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}
