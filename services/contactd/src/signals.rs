//! Stop requests from the console or the process supervisor.
//!
//! The handler only stores an atomic flag. The process thread calls
//! [`ContactService::stop`](rlb_contact::ContactService::stop) after it observes the flag.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

static STOP: AtomicBool = AtomicBool::new(false);

/// Installs the platform stop handler.
pub fn install() -> io::Result<()> {
    platform::install()
}

/// Reports whether a stop was requested.
#[must_use]
pub fn requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

fn request_stop() {
    STOP.store(true, Ordering::SeqCst);
}

#[cfg(windows)]
mod platform {
    use std::io;

    unsafe extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }

    pub fn install() -> io::Result<()> {
        unsafe extern "system" fn handle(control: u32) -> i32 {
            if matches!(control, 0..=2) {
                super::request_stop();
                1
            } else {
                0
            }
        }
        // SAFETY: `handle` stores one atomic flag and returns. It does not
        // allocate, lock, or stop the service on the handler stack.
        let installed = unsafe { SetConsoleCtrlHandler(Some(handle), 1) };
        if installed == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(unix)]
mod platform {
    use std::io;

    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    unsafe extern "C" {
        fn signal(signum: i32, handler: unsafe extern "C" fn(i32)) -> usize;
    }

    pub fn install() -> io::Result<()> {
        unsafe extern "C" fn handle_stop(_signal: i32) {
            super::request_stop();
        }
        // SAFETY: `handle_stop` stores one atomic flag. That store is permitted
        // in an asynchronous signal handler. The values are SIGINT and SIGTERM.
        unsafe {
            let _previous = signal(SIGINT, handle_stop);
            let _previous = signal(SIGTERM, handle_stop);
        }
        Ok(())
    }
}
