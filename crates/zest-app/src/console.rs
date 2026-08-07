//! Console attachment for a GUI-subsystem binary.
//!
//! # The problem this solves
//!
//! A terminal emulator must not be a console application. Built as
//! `WINDOWS_CUI`, launching zesterm from Explorer, a shortcut or the Start menu
//! creates a console window that flashes up and then sits behind the terminal
//! — visibly wrong for this program in particular.
//!
//! But building as `WINDOWS_GUI` means the process starts with no standard
//! handles at all, so `--help`, `--themes` and every log line vanish silently.
//! That is worse than the console window: a CLI flag that prints nothing looks
//! like a broken build.
//!
//! So: GUI subsystem, and *attach* to the parent's console when there is one.
//! Launched from a shell, output appears exactly as expected. Launched from
//! Explorer, there is no console and nothing to attach to, so nothing flashes.

/// Attach to the parent process's console, if it has one.
///
/// Returns true if standard output is now usable. Safe to call unconditionally;
/// on a non-Windows platform, or when there is no parent console, it does
/// nothing.
pub fn attach_to_parent() -> bool {
    #[cfg(all(windows, not(debug_assertions)))]
    {
        attach_windows()
    }
    #[cfg(not(all(windows, not(debug_assertions))))]
    {
        // Debug builds keep the console subsystem, so the handles already work.
        true
    }
}

#[cfg(all(windows, not(debug_assertions)))]
fn attach_windows() -> bool {
    use std::ptr;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    // SAFETY: ordinary Win32 calls with no preconditions beyond valid
    // arguments. Failure is expected and handled -- there is simply no parent
    // console when launched from Explorer.
    unsafe {
        let usable = |h: *mut core::ffi::c_void| !h.is_null() && h != INVALID_HANDLE_VALUE;

        // If the parent redirected our output -- `zesterm --themes > file`, or a
        // pipe -- Windows passes those handles through even to a GUI-subsystem
        // process, and they are exactly what the caller wants written to.
        // Overwriting them with CONOUT$ would send the output to the console
        // instead and silently break every redirect and pipe.
        if usable(GetStdHandle(STD_OUTPUT_HANDLE)) {
            return true;
        }

        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return false;
        }

        // Attaching gives the process a console but does NOT repoint its
        // standard handles, which a GUI-subsystem process starts without. They
        // have to be opened against the console device explicitly, or `println!`
        // still writes to nothing.
        let open = |name: &str, access: u32| {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            CreateFileW(
                wide.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        };

        let out = open("CONOUT$", GENERIC_READ | GENERIC_WRITE);
        if usable(out) {
            SetStdHandle(STD_OUTPUT_HANDLE, out);
            if !usable(GetStdHandle(STD_ERROR_HANDLE)) {
                SetStdHandle(STD_ERROR_HANDLE, out);
            }
        }
        let inp = open("CONIN$", GENERIC_READ | GENERIC_WRITE);
        if usable(inp) && !usable(GetStdHandle(STD_INPUT_HANDLE)) {
            SetStdHandle(STD_INPUT_HANDLE, inp);
        }

        usable(out)
    }
}
