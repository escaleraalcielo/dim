//! Windows console plumbing. The console input and output modes outlive the
//! process (SetConsoleMode state is not reset on exit), so both are saved and
//! put back on every exit path, including Ctrl+C/Ctrl+Break and window close.
//!
//! The code pages are console state of the same kind, so they are saved and
//! put back the same way.
//!
//! Compile-verified only (see the stage-2 acceptance notes), except for the
//! code-page exchange, which the ConPTY flow test at the end of this file
//! checks on a console that runs a legacy code page.

use std::io;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
    ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    ENABLE_WRAP_AT_EOL_OUTPUT, GetConsoleCP, GetConsoleMode, GetConsoleOutputCP,
    GetConsoleScreenBufferInfo, GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCP,
    SetConsoleCtrlHandler, SetConsoleMode, SetConsoleOutputCP,
};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

use super::modes::{self, MAX_RESTORE_LEN};

pub type RawFd = i32;
pub const STDIN: RawFd = 0;
pub const STDOUT: RawFd = 1;

static ORIGINAL: OnceLock<(u32, u32)> = OnceLock::new();
static RAW_ON: AtomicBool = AtomicBool::new(false);
static RESTORED: AtomicBool = AtomicBool::new(false);

/// The code page the frames and keys are exchanged in: UTF-8.
const UTF8_CODE_PAGE: u32 = 65001;

/// The code pages in force before the terminal was taken.
static ORIGINAL_CODE_PAGES: OnceLock<(u32, u32)> = OnceLock::new();

fn handle(fd: RawFd) -> HANDLE {
    // SAFETY: GetStdHandle has no preconditions.
    unsafe {
        GetStdHandle(if fd == STDIN {
            STD_INPUT_HANDLE
        } else {
            STD_OUTPUT_HANDLE
        })
    }
}

pub fn is_tty(fd: RawFd) -> bool {
    let mut mode = 0u32;
    // SAFETY: valid out pointer.
    unsafe { GetConsoleMode(handle(fd), &mut mode) != 0 }
}

pub fn enable_raw_mode() -> io::Result<()> {
    let (input, output) = (handle(STDIN), handle(STDOUT));
    let (mut in_mode, mut out_mode) = (0u32, 0u32);
    // SAFETY: valid out pointers.
    unsafe {
        if GetConsoleMode(input, &mut in_mode) == 0 || GetConsoleMode(output, &mut out_mode) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let _ = ORIGINAL.set((in_mode, out_mode));
    let raw_in = (in_mode & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT))
        | ENABLE_VIRTUAL_TERMINAL_INPUT;
    let raw_out = out_mode
        | ENABLE_PROCESSED_OUTPUT
        | ENABLE_WRAP_AT_EOL_OUTPUT
        | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
    // SAFETY: handles from GetStdHandle.
    unsafe {
        if SetConsoleMode(input, raw_in) == 0 || SetConsoleMode(output, raw_out) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    RAW_ON.store(true, Ordering::SeqCst);
    use_utf8_code_pages();
    Ok(())
}

/// Exchanges frames and keys as UTF-8.
///
/// A console decodes the bytes written to its output handle with the console
/// output code page and encodes the keys it hands over with the console input
/// code page, so a console that runs a legacy OEM page (CP850 on a Western
/// European Windows, CP437 on a US one) shows every multi-byte UTF-8 sequence
/// as mojibake and delivers typed characters as bytes the UTF-8 decoder drops.
/// The code pages are console state that outlive the process, so the originals
/// are kept and put back when the terminal is released.
fn use_utf8_code_pages() {
    // SAFETY: the console is attached, so the calls address it.
    unsafe {
        let (input, output) = (GetConsoleCP(), GetConsoleOutputCP());
        let _ = ORIGINAL_CODE_PAGES.set((input, output));
        SetConsoleCP(UTF8_CODE_PAGE);
        SetConsoleOutputCP(UTF8_CODE_PAGE);
    }
}

/// Puts the console code pages back.
fn restore_code_pages() {
    if let Some(&(input, output)) = ORIGINAL_CODE_PAGES.get() {
        // SAFETY: the console is attached, so the calls address it.
        unsafe {
            SetConsoleCP(input);
            SetConsoleOutputCP(output);
        }
    }
}

pub fn disable_raw_mode() {
    if RAW_ON.swap(false, Ordering::SeqCst)
        && let Some(&(in_mode, out_mode)) = ORIGINAL.get()
    {
        // SAFETY: restoring the modes read earlier.
        unsafe {
            SetConsoleMode(handle(STDIN), in_mode);
            SetConsoleMode(handle(STDOUT), out_mode);
        }
    }
    restore_code_pages();
}

pub fn write_all(fd: RawFd, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let mut written = 0u32;
        // SAFETY: pointer/length from a live slice.
        let ok = unsafe {
            WriteFile(
                handle(fd),
                bytes.as_ptr().cast(),
                bytes.len().min(u32::MAX as usize) as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

/// Writes all of `bytes` to the console, giving up at `deadline`.
///
/// A console write cannot be interrupted from the writing thread, so the
/// write runs on a helper thread and this returns at the deadline; a write
/// still blocked there ends with the process, as the writer thread's does on
/// Unix.
pub fn write_until(fd: RawFd, bytes: &[u8], deadline: Instant) {
    let bytes = bytes.to_vec();
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = write_all(fd, &bytes);
        let _ = done.send(());
    });
    let _ = finished.recv_timeout(deadline.saturating_duration_since(Instant::now()));
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    Data(usize),
    Timeout,
    Closed,
}

pub fn read_timeout(fd: RawFd, buf: &mut [u8], timeout: Duration) -> ReadOutcome {
    let input = handle(fd);
    let millis = timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
    // SAFETY: waiting on the console input handle.
    let wait = unsafe { WaitForSingleObject(input, millis) };
    if wait != WAIT_OBJECT_0 {
        return ReadOutcome::Timeout;
    }
    let mut read = 0u32;
    // SAFETY: pointer/length from a live slice.
    let ok = unsafe {
        ReadFile(
            input,
            buf.as_mut_ptr().cast(),
            buf.len().min(u32::MAX as usize) as u32,
            &mut read,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || read == 0 {
        ReadOutcome::Closed
    } else {
        ReadOutcome::Data(read as usize)
    }
}

pub fn window_size() -> Option<(u16, u16)> {
    // SAFETY: zeroed struct is a valid out-parameter.
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    if unsafe { GetConsoleScreenBufferInfo(handle(STDOUT), &mut info) } == 0 {
        return None;
    }
    let columns = (info.srWindow.Right - info.srWindow.Left + 1) as u16;
    let rows = (info.srWindow.Bottom - info.srWindow.Top + 1) as u16;
    (columns > 0 && rows > 0).then_some((columns, rows))
}

/// Lets the next exit path write the restore sequence again (see unix.rs).
pub fn rearm_restore_guard() {
    RESTORED.store(false, Ordering::SeqCst);
}

/// Writes the mode restore sequence once and drops raw mode, giving the write
/// the exit bound (see unix.rs).
pub fn restore_terminal_once() {
    restore_terminal_once_until(Instant::now() + super::EXIT_BUDGET);
}

/// [`restore_terminal_once`] sharing the exit's deadline.
pub fn restore_terminal_once_until(deadline: Instant) {
    if RESTORED.swap(true, Ordering::SeqCst) {
        return;
    }
    let mut buf = [0u8; MAX_RESTORE_LEN];
    let len = modes::restore_into(modes::live_bits(), &mut buf);
    write_until(STDOUT, &buf[..len], deadline);
    disable_raw_mode();
}

unsafe extern "system" fn on_ctrl(_kind: u32) -> i32 {
    restore_terminal_once();
    0
}

pub fn install_signal_guard() {
    // SAFETY: registering a plain handler.
    unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) };
}

pub fn remove_signal_guard() {
    // SAFETY: removing the handler registered above.
    unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 0) };
}

#[cfg(all(test, windows))]
mod windows_flow {
    use super::*;
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::{
        io::Read,
        sync::mpsc,
        time::{Duration, Instant},
    };

    /// The code page a Windows console runs unless something sets UTF-8: CP437
    /// on a US Windows, CP850 on a Western European one.
    const LEGACY_CODE_PAGE: u32 = 437;

    /// A middot, the character the interface puts between labels.
    const MIDDOT: &str = "\u{b7}";

    /// A frame painted on a console that runs a legacy code page reaches the
    /// terminal as UTF-8, and the console keeps the code page it had.
    #[test]
    fn a_frame_reaches_a_legacy_code_page_console_as_utf8() {
        if std::env::var_os("DIM_CLI_CONSOLE_CODE_PAGE_CHILD").is_some() {
            child();
            return;
        }
        parent();
    }

    /// The child: a console on a legacy code page, painting one frame.
    fn child() {
        // SAFETY: the console is attached, so the call addresses it.
        unsafe {
            SetConsoleOutputCP(LEGACY_CODE_PAGE);
        }
        enable_raw_mode().expect("raw mode");
        write_all(STDOUT, MIDDOT.as_bytes()).expect("frame");
        disable_raw_mode();
        // SAFETY: the console is attached, so the call addresses it.
        let restored = unsafe { GetConsoleOutputCP() };
        write_all(STDOUT, restored.to_string().as_bytes()).expect("code page");
    }

    /// The parent: the console the child painted on.
    fn parent() {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
        command.env("DIM_CLI_CONSOLE_CODE_PAGE_CHILD", "1");
        command.args([
            "--exact",
            "ui::terminal::windows::windows_flow::a_frame_reaches_a_legacy_code_page_console_as_utf8",
            "--nocapture",
        ]);
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(size) => {
                        if tx.send(buf[..size].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let mut output = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        let restored = LEGACY_CODE_PAGE.to_string();
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(bytes) => {
                    output.extend_from_slice(&bytes);
                    if output
                        .windows(restored.len())
                        .any(|w| w == restored.as_bytes())
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            output.windows(MIDDOT.len()).any(|w| w == MIDDOT.as_bytes()),
            "the frame did not reach the terminal as UTF-8: {}",
            String::from_utf8_lossy(&output)
        );
        assert!(
            output
                .windows(LEGACY_CODE_PAGE.to_string().len())
                .any(|w| w == LEGACY_CODE_PAGE.to_string().as_bytes()),
            "the console kept the code page the terminal took: {}",
            String::from_utf8_lossy(&output)
        );
    }
}
