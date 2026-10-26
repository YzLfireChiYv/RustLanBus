//! Cross-thread control wake for same-thread WASAPI owners.

use std::sync::Arc;

use windows::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Threading::{
    CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};

use crate::{WindowsAudioError, hresult};

/// Reason an audio owner woke from its interruptible wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeWake {
    /// The stream's audio event is ready to service.
    Audio,
    /// The owner should inspect its current control state or command queue.
    Control,
    /// Neither event became ready before the timeout.
    Timeout,
}

#[derive(Debug)]
struct WakeEvent(HANDLE);

// SAFETY: this handle names a Win32 kernel event. SetEvent and the wait APIs
// support concurrent calls from arbitrary threads and have no COM affinity.
// Every operation borrows an Arc owner, so its final Drop cannot close the
// handle while one of those operations is using it. No raw handle is exported.
unsafe impl Send for WakeEvent {}
// SAFETY: the same kernel synchronization and Arc lifetime rules apply to
// shared references. The event's signaled state is maintained by Windows.
unsafe impl Sync for WakeEvent {}

impl Drop for WakeEvent {
    fn drop(&mut self) {
        // Exactly the final Arc owner closes the successfully created event.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Cloneable auto-reset event used to interrupt one audio owner's wait.
///
/// Signals may coalesce. This is a notification to inspect authoritative
/// control state or drain a command queue, rather than a count of commands.
/// Clones may signal from any thread; one owner consumes the notifications.
/// The event contains no COM objects and does not transfer a WASAPI stream.
#[derive(Clone, Debug)]
pub struct AudioWake(Arc<WakeEvent>);

impl AudioWake {
    /// Creates an initially unsignaled, unnamed auto-reset kernel event.
    pub fn new() -> Result<Self, WindowsAudioError> {
        unsafe { CreateEventW(None, false, false, None) }
            .map(|handle| Self(Arc::new(WakeEvent(handle))))
            .map_err(|error| hresult("CreateEventW(audio control)", error))
    }

    /// Wakes the owner, or leaves a signal ready for its next wait.
    pub fn signal(&self) -> Result<(), WindowsAudioError> {
        unsafe { SetEvent(self.0.0) }.map_err(|error| hresult("SetEvent(audio control)", error))
    }

    /// Waits for control while no native stream is open.
    ///
    /// Returns only [`NativeWake::Control`] or [`NativeWake::Timeout`].
    pub fn wait(&self, timeout_ms: u32) -> Result<NativeWake, WindowsAudioError> {
        let result = unsafe { WaitForSingleObject(self.0.0, timeout_ms) };
        if result == WAIT_OBJECT_0 {
            Ok(NativeWake::Control)
        } else if result == WAIT_TIMEOUT {
            Ok(NativeWake::Timeout)
        } else {
            Err(wait_failure(result.0))
        }
    }

    pub(crate) fn wait_for_audio(
        &self,
        audio_event: HANDLE,
        timeout_ms: u32,
    ) -> Result<NativeWake, WindowsAudioError> {
        // Wait-any chooses the lowest signaled index. Control wins when both
        // are signaled; the auto-reset audio signal remains for the next wait.
        let handles = [self.0.0, audio_event];
        let result = unsafe { WaitForMultipleObjects(&handles, false, timeout_ms) };
        if result == WAIT_OBJECT_0 {
            Ok(NativeWake::Control)
        } else if result.0 == WAIT_OBJECT_0.0 + 1 {
            Ok(NativeWake::Audio)
        } else if result == WAIT_TIMEOUT {
            Ok(NativeWake::Timeout)
        } else {
            Err(wait_failure(result.0))
        }
    }
}

fn wait_failure(result: u32) -> WindowsAudioError {
    WindowsAudioError::WaitFailed {
        code: if result == WAIT_FAILED.0 {
            unsafe { GetLastError().0 }
        } else {
            result
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_wake_crosses_threads_and_wins_before_audio() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AudioWake>();

        let control = AudioWake::new().expect("control event");
        let audio = AudioWake::new().expect("audio event");
        assert_eq!(control.wait(0).unwrap(), NativeWake::Timeout);

        let sender = control.clone();
        std::thread::spawn(move || sender.signal().unwrap())
            .join()
            .unwrap();
        audio.signal().unwrap();
        assert_eq!(
            control.wait_for_audio(audio.0.0, 0).unwrap(),
            NativeWake::Control
        );
        assert_eq!(
            control.wait_for_audio(audio.0.0, 0).unwrap(),
            NativeWake::Audio
        );
        assert_eq!(
            control.wait_for_audio(audio.0.0, 0).unwrap(),
            NativeWake::Timeout
        );

        control.signal().unwrap();
        control.signal().unwrap();
        assert_eq!(control.wait(0).unwrap(), NativeWake::Control);
        assert_eq!(control.wait(0).unwrap(), NativeWake::Timeout);
    }
}
