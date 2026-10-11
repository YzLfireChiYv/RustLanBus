use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
};
use windows::core::w;

use crate::WindowsAudioError;

/// Guard that keeps the calling thread in the MMCSS `Pro Audio` task.
///
/// Register this on the WASAPI wait/process thread after the stream is opened
/// and before the first wait. Dropping the guard leaves MMCSS.
pub struct MmcssRegistration {
    handle: HANDLE,
}

impl MmcssRegistration {
    /// Joins the MMCSS `Pro Audio` task on the current thread.
    pub fn register_pro_audio() -> Result<Self, WindowsAudioError> {
        let mut task_index = 0_u32;
        let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) }
            .map_err(|error| WindowsAudioError::MmcssFailed {
                code: error.code().0 as u32,
            })?;
        if handle.is_invalid() {
            return Err(WindowsAudioError::MmcssFailed { code: 0 });
        }
        Ok(Self { handle })
    }
}

impl Drop for MmcssRegistration {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            let _ = unsafe { AvRevertMmThreadCharacteristics(self.handle) };
            self.handle = HANDLE::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pro_audio_registration_round_trips() {
        let registration = MmcssRegistration::register_pro_audio();
        assert!(
            registration.is_ok(),
            "MMCSS Pro Audio should be available on this Windows session"
        );
    }
}
