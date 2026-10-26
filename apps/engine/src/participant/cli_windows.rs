//! Process input only. Native reads are cancellable and never own audio/control.

use std::os::windows::io::AsRawHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::ReadFile;
use windows::Win32::System::Console::{
    CONSOLE_MODE, CONSOLE_READCONSOLE_CONTROL, CTRL_BREAK_EVENT, CTRL_C_EVENT, GetConsoleMode,
    GetStdHandle, ReadConsoleW, STD_INPUT_HANDLE, SetConsoleCtrlHandler,
};
use windows::Win32::System::IO::CancelSynchronousIo;

use super::InputEvent;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static REGISTERED: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn interrupt(kind: u32) -> windows::core::BOOL {
    if kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT {
        INTERRUPTED.store(true, Ordering::Release);
        true.into()
    } else {
        false.into()
    }
}

pub(super) struct Interrupt;

impl Interrupt {
    pub fn install() -> Result<Self, String> {
        if REGISTERED.swap(true, Ordering::AcqRel) {
            return Err("A CLI interrupt owner is already active in this process.".into());
        }
        INTERRUPTED.store(false, Ordering::Release);
        // SAFETY: a static function and atomic flag outlive the registration.
        if let Err(error) = unsafe { SetConsoleCtrlHandler(Some(interrupt), true) } {
            REGISTERED.store(false, Ordering::Release);
            return Err(format!("Console interrupt registration failed: {error}"));
        }
        Ok(Self)
    }

    pub fn requested(&self) -> bool {
        INTERRUPTED.load(Ordering::Acquire)
    }
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        // SAFETY: remove exactly this static handler before releasing its owner.
        let _ = unsafe { SetConsoleCtrlHandler(Some(interrupt), false) };
        REGISTERED.store(false, Ordering::Release);
    }
}

pub(super) struct Input {
    receiver: Option<Receiver<InputEvent>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Input {
    pub fn start() -> Result<Self, String> {
        // SAFETY: borrow the process stdin handle; its owner does not change here.
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }
            .map_err(|error| format!("Standard input is unavailable: {error}"))?;
        Self::from_handle(handle)
    }

    fn from_handle(handle: HANDLE) -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel(16);
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop);
        // Pass the borrowed value across the thread boundary. The input guard
        // joins before the caller may close/replace this handle.
        let raw = handle.0 as usize;
        let worker = thread::Builder::new()
            .name("rlb-cli-input".into())
            .spawn(move || {
                let handle = HANDLE(raw as *mut core::ffi::c_void);
                let mut mode = CONSOLE_MODE::default();
                // SAFETY: valid borrowed stdin handle and writable mode output.
                let console = unsafe { GetConsoleMode(handle, &mut mode) }.is_ok();
                let result = if console {
                    read_console(handle, &reader_stop, &sender)
                } else {
                    read_bytes(handle, &reader_stop, &sender)
                };
                if !reader_stop.load(Ordering::Acquire) {
                    let _ = sender.send(match result {
                        Ok(()) => InputEvent::Eof,
                        Err(error) => InputEvent::Error(error),
                    });
                }
            })
            .map_err(|error| format!("CLI input reader did not start: {error}"))?;
        Ok(Self {
            receiver: Some(receiver),
            stop,
            worker: Some(worker),
        })
    }

    pub fn receiver(&self) -> &Receiver<InputEvent> {
        self.receiver.as_ref().expect("live input")
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Also wake a reader blocked sending into a full bounded queue.
        self.receiver.take();
        if let Some(worker) = self.worker.take() {
            while !worker.is_finished() {
                // SAFETY: JoinHandle keeps this real thread handle valid until
                // join. Repeat to close the stop-check/read-start race.
                let _ = unsafe { CancelSynchronousIo(HANDLE(worker.as_raw_handle())) };
                thread::sleep(Duration::from_millis(2));
            }
            let _ = worker.join();
        }
    }
}

fn deliver(sender: &SyncSender<InputEvent>, line: Result<String, &'static str>) -> bool {
    let event = match line {
        Ok(line) => InputEvent::Line(line.trim_end_matches('\r').to_owned()),
        Err(message) => InputEvent::BadLine(message.to_owned()),
    };
    sender.send(event).is_ok()
}

fn read_bytes(
    handle: HANDLE,
    stop: &AtomicBool,
    sender: &SyncSender<InputEvent>,
) -> Result<(), String> {
    let mut pending = Vec::new();
    let mut chunk = [0_u8; 4096];
    while !stop.load(Ordering::Acquire) {
        let mut count = 0;
        // SAFETY: live borrowed handle and correctly sized writable buffer.
        let read = unsafe { ReadFile(handle, Some(&mut chunk), Some(&mut count), None) };
        if let Err(error) = read {
            if error.code() == windows::Win32::Foundation::ERROR_BROKEN_PIPE.to_hresult() {
                break;
            }
            return Err(format!("Standard input read failed: {error}"));
        }
        if count == 0 {
            break;
        }
        pending.extend_from_slice(&chunk[..count as usize]);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let bytes: Vec<_> = pending.drain(..=end).collect();
            let line = String::from_utf8(bytes[..end].to_vec())
                .map_err(|_| "Command input must be UTF-8.");
            if !deliver(sender, line) {
                return Ok(());
            }
        }
    }
    if !stop.load(Ordering::Acquire) && !pending.is_empty() {
        let line = String::from_utf8(pending).map_err(|_| "Command input must be UTF-8.");
        let _ = deliver(sender, line);
    }
    Ok(())
}

fn read_console(
    handle: HANDLE,
    stop: &AtomicBool,
    sender: &SyncSender<InputEvent>,
) -> Result<(), String> {
    let mut pending = Vec::new();
    let mut chunk = [0_u16; 1024];
    let control = CONSOLE_READCONSOLE_CONTROL {
        nLength: std::mem::size_of::<CONSOLE_READCONSOLE_CONTROL>() as u32,
        dwCtrlWakeupMask: 1 << 0x1a,
        ..Default::default()
    };
    while !stop.load(Ordering::Acquire) {
        let mut count = 0;
        // SAFETY: live console handle, writable UTF-16 buffer and count output.
        unsafe {
            ReadConsoleW(
                handle,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &mut count,
                Some(&control),
            )
        }
        .map_err(|error| format!("Console input read failed: {error}"))?;
        if count == 0 {
            break;
        }
        let received = &chunk[..count as usize];
        let eof = received.iter().position(|unit| *unit == 0x1a);
        pending.extend_from_slice(&received[..eof.unwrap_or(received.len())]);
        while let Some(end) = pending.iter().position(|unit| *unit == u16::from(b'\n')) {
            let units: Vec<_> = pending.drain(..=end).collect();
            let line = String::from_utf16(&units[..end])
                .map_err(|_| "Console command is not valid Unicode.");
            if !deliver(sender, line) {
                return Ok(());
            }
        }
        if eof.is_some() {
            break;
        }
    }
    if !stop.load(Ordering::Acquire) && !pending.is_empty() {
        let line =
            String::from_utf16(&pending).map_err(|_| "Console command is not valid Unicode.");
        let _ = deliver(sender, line);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::WriteFile;
    use windows::Win32::System::Pipes::CreatePipe;

    fn pipe() -> (HANDLE, HANDLE) {
        let mut read = HANDLE::default();
        let mut write = HANDLE::default();
        unsafe { CreatePipe(&mut read, &mut write, None, 0) }.unwrap();
        (read, write)
    }

    #[test]
    fn native_input_frames_split_unicode_and_final_line_then_eof() {
        let (read, write) = pipe();
        let input = Input::from_handle(read).unwrap();
        unsafe { WriteFile(write, Some(&[0xff, b'\n']), None, None) }.unwrap();
        let bytes = "status\r\n房间🌊\nquit".as_bytes();
        for chunk in bytes.chunks(1) {
            unsafe { WriteFile(write, Some(chunk), None, None) }.unwrap();
        }
        unsafe { CloseHandle(write) }.unwrap();
        assert!(matches!(
            input
                .receiver()
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            InputEvent::BadLine(_)
        ));
        for expected in ["status", "房间🌊", "quit"] {
            let InputEvent::Line(line) = input
                .receiver()
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
            else {
                panic!("line expected")
            };
            assert_eq!(line, expected);
        }
        assert!(matches!(
            input
                .receiver()
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            InputEvent::Eof
        ));
        drop(input);
        unsafe { CloseHandle(read) }.unwrap();
    }

    #[test]
    fn native_input_shutdown_joins_idle_and_backpressured_readers() {
        for full in [false, true] {
            let (read, write) = pipe();
            let input = Input::from_handle(read).unwrap();
            if full {
                let bytes = "status\n".repeat(64);
                unsafe { WriteFile(write, Some(bytes.as_bytes()), None, None) }.unwrap();
            }
            thread::sleep(Duration::from_millis(15));
            let start = std::time::Instant::now();
            drop(input);
            assert!(start.elapsed() < Duration::from_secs(2));
            unsafe { CloseHandle(read) }.unwrap();
            unsafe { CloseHandle(write) }.unwrap();
        }
    }
}
