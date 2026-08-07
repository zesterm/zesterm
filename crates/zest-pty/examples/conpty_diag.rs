//! Step-by-step diagnostics for the ConPTY spawn sequence.
//!
//! Every Win32 call in the chain is reported with its return value and
//! `GetLastError`, plus the pointer alignments involved, so a silent failure
//! (the attribute list being ignored, say) can be localized instead of guessed
//! at. Throwaway if the spawn path ever stops being interesting.

#[cfg(not(windows))]
fn main() {
    eprintln!("windows only");
}

#[cfg(windows)]
fn main() {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, FALSE, GetLastError, HANDLE};
    use windows_sys::Win32::System::Console::{CreatePseudoConsole, COORD, HPCON};
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
        UpdateProcThreadAttribute, EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST,
        PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTUPINFOEXW,
        WaitForSingleObject,
    };

    unsafe {
        println!("PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE = {PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE} (0x{PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE:x})");
        println!("size_of::<HPCON>() = {}", std::mem::size_of::<HPCON>());
        println!("size_of::<STARTUPINFOEXW>() = {}", std::mem::size_of::<STARTUPINFOEXW>());

        let mut child_read: HANDLE = ptr::null_mut();
        let mut our_write: HANDLE = ptr::null_mut();
        let mut our_read: HANDLE = ptr::null_mut();
        let mut child_write: HANDLE = ptr::null_mut();

        println!("\nCreatePipe #1 -> {}", CreatePipe(&mut child_read, &mut our_write, ptr::null(), 0));
        println!("CreatePipe #2 -> {}", CreatePipe(&mut our_read, &mut child_write, ptr::null(), 0));

        let mut hpcon: HPCON = 0;
        let hr = CreatePseudoConsole(COORD { X: 120, Y: 30 }, child_read, child_write, 0, &mut hpcon);
        println!("CreatePseudoConsole -> hr=0x{hr:08x}  hpcon={hpcon:#x}");

        // --- attribute list ---
        let mut bytes: usize = 0;
        let r0 = InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut bytes);
        println!("\nInitialize(size query) -> {r0}, last_error={} (122 = INSUFFICIENT_BUFFER), needs {bytes} bytes",
                 GetLastError());

        let words = bytes.div_ceil(std::mem::size_of::<usize>());
        let mut attr_buf: Vec<usize> = vec![0usize; words];
        let attr_list = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        println!("attr_list ptr = {attr_list:p}, addr % 8 = {}", (attr_list as usize) % 8);

        let r1 = InitializeProcThreadAttributeList(attr_list, 1, 0, &mut bytes);
        println!("Initialize(real)      -> {r1}, last_error={}", GetLastError());

        let r2 = UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            hpcon as *mut core::ffi::c_void,
            std::mem::size_of::<HPCON>(),
            ptr::null_mut(),
            ptr::null_mut(),
        );
        println!("UpdateProcThreadAttribute -> {r2}, last_error={}", GetLastError());

        // --- spawn ---
        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attr_list;

        // Set DIAG_OMIT_USESTDHANDLES=1 to reproduce the original bug: without
        // STARTF_USESTDHANDLES, Windows propagates the parent's std handles into
        // the child and they win over the pseudoconsole, so the child's output
        // vanishes from the pty while every API call still reports success.
        // Keeping both paths here makes the failure reproducible on demand.
        if std::env::var_os("DIAG_OMIT_USESTDHANDLES").is_some() {
            println!("(omitting STARTF_USESTDHANDLES -- expect the marker to be MISSING)");
        } else {
            use windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
            si.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
            si.StartupInfo.hStdInput = ptr::null_mut();
            si.StartupInfo.hStdOutput = ptr::null_mut();
            si.StartupInfo.hStdError = ptr::null_mut();
            println!("(using STARTF_USESTDHANDLES with null handles -- the fix)");
        }

        let mut cmdline: Vec<u16> = OsStr::new("cmd.exe /c echo zesterm-probe")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();

        let r3 = CreateProcessW(
            ptr::null(),
            cmdline.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            FALSE,
            EXTENDED_STARTUPINFO_PRESENT,
            ptr::null(),
            ptr::null(),
            &mut si as *mut STARTUPINFOEXW as *mut _,
            &mut pi,
        );
        println!("CreateProcessW -> {r3}, last_error={}, pid={}", GetLastError(), pi.dwProcessId);

        // Close our copies of the child ends so EOF can eventually arrive.
        CloseHandle(child_read);
        CloseHandle(child_write);

        // Read whatever ConPTY produces, on a thread, while we wait.
        let read_handle = our_read as usize;
        let t = std::thread::spawn(move || {
            use windows_sys::Win32::Storage::FileSystem::ReadFile;
            let h = read_handle as HANDLE;
            let mut out: Vec<u8> = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let mut n = 0u32;
                let ok = ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut n, ptr::null_mut());
                if ok == 0 || n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n as usize]);
            }
            out
        });

        WaitForSingleObject(pi.hProcess, 5000);
        std::thread::sleep(std::time::Duration::from_millis(400));

        windows_sys::Win32::System::Console::ClosePseudoConsole(hpcon);
        let out = t.join().unwrap();

        let text = String::from_utf8_lossy(&out);
        println!("\n--- pty produced {} bytes ---", out.len());
        println!("{}", visible(&out));
        println!("\ncontains marker: {}", text.contains("zesterm-probe"));
        println!("=> if false, the child was NOT attached to the pseudoconsole");

        CloseHandle(our_read);
        CloseHandle(our_write);
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
        DeleteProcThreadAttributeList(attr_list);
    }
}

#[cfg(windows)]
fn visible(bytes: &[u8]) -> String {
    let mut s = String::new();
    for &b in bytes {
        match b {
            0x1b => s.push_str("<ESC>"),
            0x07 => s.push_str("<BEL>"),
            b'\n' => s.push_str("<LF>"),
            b'\r' => s.push_str("<CR>"),
            0x00..=0x1f | 0x7f => s.push_str(&format!("<{b:02x}>")),
            _ => s.push(b as char),
        }
    }
    s
}
