use rlb_audio::EndpointFormatRange;
use wasapi::{SampleType, WaveFormat};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};

use crate::WindowsAudioError;

/// Sample representation reported by a WASAPI mix or stream format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleEncoding {
    /// IEEE-754 floating point.
    Float,
    /// Signed integer PCM.
    IntegerPcm,
    /// A subformat this adapter cannot convert.
    Unknown,
}

/// Owned projection of a `WAVEFORMATEX`/`WAVEFORMATEXTENSIBLE`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WasapiFormat {
    /// Frames per second.
    pub sample_rate_hz: u32,
    /// Interleaved channels per frame.
    pub channels: u16,
    /// Container width for one channel sample.
    pub bits_per_sample: u16,
    /// Significant bits for one channel sample.
    pub valid_bits_per_sample: u16,
    /// Bytes in one interleaved frame.
    pub block_align: u16,
    /// Windows speaker-position mask.
    pub channel_mask: u32,
    /// PCM representation.
    pub encoding: SampleEncoding,
}

impl WasapiFormat {
    pub(crate) fn from_wave_format(format: &WaveFormat) -> Self {
        let encoding = match format.get_subformat() {
            Ok(SampleType::Float) => SampleEncoding::Float,
            Ok(SampleType::Int) => SampleEncoding::IntegerPcm,
            Err(_) => SampleEncoding::Unknown,
        };
        Self {
            sample_rate_hz: format.get_samplespersec(),
            channels: format.get_nchannels(),
            bits_per_sample: format.get_bitspersample(),
            valid_bits_per_sample: format.get_validbitspersample(),
            block_align: format.get_blockalign() as u16,
            channel_mask: format.get_dwchannelmask(),
            encoding,
        }
    }

    pub(crate) fn engine_wave_format(channels: u8) -> Result<WaveFormat, WindowsAudioError> {
        if !matches!(channels, 1 | 2) {
            return Err(rlb_audio::AudioError::UnsupportedChannels { channels }.into());
        }
        Ok(WaveFormat::new(
            32,
            32,
            &SampleType::Float,
            48_000,
            usize::from(channels),
            None,
        ))
    }

    /// Returns whether this is the exact callback format requested from WASAPI.
    #[must_use]
    pub const fn is_engine_f32(self, channels: u8) -> bool {
        self.sample_rate_hz == 48_000
            && self.channels == channels as u16
            && self.bits_per_sample == 32
            && self.valid_bits_per_sample == 32
            && self.block_align == channels as u16 * 4
            && matches!(self.encoding, SampleEncoding::Float)
    }

    /// Projects the known current mix point into the common endpoint contract.
    #[must_use]
    pub fn current_mix_range(self, engine_channels: u8) -> EndpointFormatRange {
        let channels = u8::try_from(self.channels).unwrap_or(u8::MAX);
        EndpointFormatRange {
            minimum_sample_rate_hz: self.sample_rate_hz,
            maximum_sample_rate_hz: self.sample_rate_hz,
            minimum_channels: channels.max(1),
            maximum_channels: channels.max(1),
            supports_engine_f32: matches!(engine_channels, 1 | 2),
        }
    }
}

/// Decoded capture flags from one WASAPI packet.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CapturePacketFlags {
    /// The engine reported that data preceding this packet was lost.
    pub data_discontinuity: bool,
    /// The complete packet is valid digital silence.
    pub silent: bool,
    /// Device-position and QPC fields are unreliable.
    pub timestamp_error: bool,
}

impl CapturePacketFlags {
    /// Decodes the documented `AUDCLNT_BUFFERFLAGS_*` bits.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self {
            data_discontinuity: raw & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0,
            silent: raw & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0,
            timestamp_error: raw & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0,
        }
    }
}

pub(crate) fn hns_to_frames(period_hns: i64, sample_rate_hz: u32) -> u32 {
    if period_hns <= 0 || sample_rate_hz == 0 {
        return 0;
    }
    let numerator = period_hns as u128 * u128::from(sample_rate_hz);
    let rounded = (numerator + 5_000_000) / 10_000_000;
    u32::try_from(rounded).unwrap_or(u32::MAX)
}

pub(crate) unsafe fn parse_mix_format(
    ptr: *const WAVEFORMATEX,
) -> Result<WasapiFormat, WindowsAudioError> {
    if ptr.is_null() {
        return Err(WindowsAudioError::Hresult {
            operation: "IAudioClient::GetMixFormat",
            code: 0x8000_4003,
        });
    }
    let format = unsafe { &*ptr };
    let mut encoding = match u32::from(format.wFormatTag) {
        tag if tag == WAVE_FORMAT_IEEE_FLOAT => SampleEncoding::Float,
        tag if tag == WAVE_FORMAT_PCM => SampleEncoding::IntegerPcm,
        _ => SampleEncoding::Unknown,
    };
    let mut valid_bits = format.wBitsPerSample;
    let mut channel_mask = 0_u32;
    if u32::from(format.wFormatTag) == WAVE_FORMAT_EXTENSIBLE && format.cbSize >= 22 {
        let extensible = unsafe { core::ptr::read_unaligned(ptr.cast::<WAVEFORMATEXTENSIBLE>()) };
        let samples = unsafe { core::ptr::addr_of!(extensible.Samples).read_unaligned() };
        valid_bits = unsafe { samples.wValidBitsPerSample };
        channel_mask = unsafe { core::ptr::addr_of!(extensible.dwChannelMask).read_unaligned() };
        let subformat = unsafe { core::ptr::addr_of!(extensible.SubFormat).read_unaligned() };
        encoding = if subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            SampleEncoding::Float
        } else if subformat == KSDATAFORMAT_SUBTYPE_PCM {
            SampleEncoding::IntegerPcm
        } else {
            SampleEncoding::Unknown
        };
    }
    Ok(WasapiFormat {
        sample_rate_hz: format.nSamplesPerSec,
        channels: format.nChannels,
        bits_per_sample: format.wBitsPerSample,
        valid_bits_per_sample: valid_bits,
        block_align: format.nBlockAlign,
        channel_mask,
        encoding,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_format_is_exact_float_and_interleaved() {
        for channels in [1, 2] {
            let wave = WasapiFormat::engine_wave_format(channels).expect("engine format");
            let format = WasapiFormat::from_wave_format(&wave);
            assert!(format.is_engine_f32(channels));
            assert_eq!(format.block_align, u16::from(channels) * 4);
        }
    }

    #[test]
    fn flags_decode_independently() {
        let all = AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32
            | AUDCLNT_BUFFERFLAGS_SILENT.0 as u32
            | AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32;
        assert_eq!(
            CapturePacketFlags::from_raw(all),
            CapturePacketFlags {
                data_discontinuity: true,
                silent: true,
                timestamp_error: true,
            }
        );
        assert_eq!(
            CapturePacketFlags::from_raw(AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32),
            CapturePacketFlags {
                data_discontinuity: true,
                ..CapturePacketFlags::default()
            }
        );
    }

    #[test]
    fn period_rounding_uses_100ns_units() {
        assert_eq!(hns_to_frames(100_000, 48_000), 480);
        assert_eq!(hns_to_frames(53_333, 48_000), 256);
        assert_eq!(hns_to_frames(0, 48_000), 0);
    }
}
