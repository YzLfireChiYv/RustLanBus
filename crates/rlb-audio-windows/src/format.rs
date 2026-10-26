use std::fmt;

use rlb_audio::EndpointFormatRange;
use rlb_model::pcm::{ChannelLayout, PcmEncoding, PcmFormat};
use wasapi::{SampleType, WaveFormat};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};

use crate::WindowsAudioError;

/// Inclusive sample-rate bounds already accepted by the original-PCM descriptor
/// and by direct playout configuration. This crate keeps the same numeric window
/// for its native entry.
pub const NATIVE_F32_MIN_SAMPLE_RATE_HZ: u32 = 8_000;

/// Inclusive upper sample-rate bound paired with [`NATIVE_F32_MIN_SAMPLE_RATE_HZ`].
pub const NATIVE_F32_MAX_SAMPLE_RATE_HZ: u32 = 384_000;

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

/// Endpoint PCM format used without Windows rate/channel conversion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativePcmFormat {
    /// Frames per second from the endpoint mix format.
    pub sample_rate_hz: u32,
    /// Interleaved channels per frame. This is the mix-format count.
    pub channels: u8,
    /// Container width: signed PCM16/24/32 or IEEE float32/64.
    pub bits_per_sample: u16,
    /// Significant bits, left aligned in an integer container.
    pub valid_bits_per_sample: u16,
    /// Bytes in one interleaved frame.
    pub block_align: u16,
    /// Validated speaker mask. Missing mono/stereo masks are normalized.
    pub channel_mask: u32,
    /// Actual representation of the native endpoint samples.
    pub encoding: SampleEncoding,
}

/// Compatibility name for callers that still use the original F32 methods.
pub type NativeF32Format = NativePcmFormat;

impl NativePcmFormat {
    /// Validates the native sample container, precision and speaker positions.
    pub fn from_mix(format: WasapiFormat) -> Result<Self, NativeFormatReject> {
        match format.encoding {
            SampleEncoding::IntegerPcm => {
                if !matches!(format.bits_per_sample, 16 | 24 | 32)
                    || format.valid_bits_per_sample == 0
                    || format.valid_bits_per_sample > format.bits_per_sample
                {
                    return Err(NativeFormatReject::IntegerPcm {
                        bits_per_sample: format.bits_per_sample,
                        valid_bits_per_sample: format.valid_bits_per_sample,
                    });
                }
            }
            SampleEncoding::Unknown => {
                return Err(NativeFormatReject::UnknownEncoding {
                    bits_per_sample: format.bits_per_sample,
                    valid_bits_per_sample: format.valid_bits_per_sample,
                });
            }
            SampleEncoding::Float => {
                if !matches!(format.bits_per_sample, 32 | 64)
                    || format.valid_bits_per_sample != format.bits_per_sample
                {
                    return Err(NativeFormatReject::NonF32Float {
                        bits_per_sample: format.bits_per_sample,
                        valid_bits_per_sample: format.valid_bits_per_sample,
                    });
                }
            }
        }
        let channels = match format.channels {
            1..=8 => format.channels as u8,
            _ => {
                return Err(NativeFormatReject::UnsupportedChannelCount {
                    channels: format.channels,
                });
            }
        };
        let layout =
            ChannelLayout::from_mask(format.channels, format.channel_mask).map_err(|_| {
                NativeFormatReject::InvalidChannelMask {
                    channels: format.channels,
                    channel_mask: format.channel_mask,
                }
            })?;
        if format.sample_rate_hz < NATIVE_F32_MIN_SAMPLE_RATE_HZ
            || format.sample_rate_hz > NATIVE_F32_MAX_SAMPLE_RATE_HZ
        {
            return Err(NativeFormatReject::UnsupportedSampleRate {
                sample_rate_hz: format.sample_rate_hz,
            });
        }
        let expected_align = format.channels.saturating_mul(format.bits_per_sample / 8);
        if format.block_align != expected_align {
            return Err(NativeFormatReject::InconsistentFrameSize {
                channels: format.channels,
                bits_per_sample: format.bits_per_sample,
                block_align: format.block_align,
            });
        }
        Ok(Self {
            sample_rate_hz: format.sample_rate_hz,
            channels,
            bits_per_sample: format.bits_per_sample,
            valid_bits_per_sample: format.valid_bits_per_sample,
            block_align: format.block_align,
            channel_mask: layout.channel_mask(),
            encoding: format.encoding,
        })
    }

    /// Original representation for publication. Integer containers widen exactly
    /// into a signed32 wire word; their original valid-bit count is preserved.
    #[must_use]
    pub fn pcm_format(self) -> PcmFormat {
        let layout = ChannelLayout::from_mask(u16::from(self.channels), self.channel_mask)
            .expect("native format has a validated speaker mask");
        let encoding = match (self.encoding, self.bits_per_sample) {
            (SampleEncoding::IntegerPcm, _) => PcmEncoding::Signed32Le,
            (SampleEncoding::Float, 64) => PcmEncoding::F64Le,
            _ => PcmEncoding::F32Le,
        };
        PcmFormat::new(
            self.sample_rate_hz,
            layout,
            encoding,
            self.valid_bits_per_sample as u8,
        )
        .expect("native format has validated precision and rate")
    }

    /// Whether the compatibility F32 entry can copy this format directly.
    #[must_use]
    pub const fn is_f32(self) -> bool {
        matches!(self.encoding, SampleEncoding::Float) && self.bits_per_sample == 32
    }

    /// Reports the opened rate and channel count on the common endpoint descriptor.
    ///
    /// `supports_engine_f32` stays false. The legacy planar 48 kHz callback is a
    /// separate open path.
    #[must_use]
    pub const fn format_range(self) -> EndpointFormatRange {
        EndpointFormatRange {
            minimum_sample_rate_hz: self.sample_rate_hz,
            maximum_sample_rate_hz: self.sample_rate_hz,
            minimum_channels: self.channels,
            maximum_channels: self.channels,
            supports_engine_f32: false,
        }
    }
}

/// Why a mix format was refused by [`NativeF32Format::from_mix`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeFormatReject {
    /// `GetMixFormat` failed or returned no readable description.
    MixFormatUnavailable,
    /// Unsupported integer container or invalid significant-bit width.
    IntegerPcm {
        /// Container width.
        bits_per_sample: u16,
        /// Significant bits.
        valid_bits_per_sample: u16,
    },
    /// Floating point whose container/significant width is outside F32/F64.
    NonF32Float {
        /// Container width.
        bits_per_sample: u16,
        /// Significant bits.
        valid_bits_per_sample: u16,
    },
    /// A subformat this adapter does not classify as float or integer PCM.
    UnknownEncoding {
        /// Container width.
        bits_per_sample: u16,
        /// Significant bits.
        valid_bits_per_sample: u16,
    },
    /// Channel count was outside one through eight.
    UnsupportedChannelCount {
        /// Mix-format channel count.
        channels: u16,
    },
    /// Speaker bits were missing or inconsistent with the channel count.
    InvalidChannelMask {
        /// Mix-format channel count.
        channels: u16,
        /// Reported speaker-position mask.
        channel_mask: u32,
    },
    /// Sample rate was outside the original-PCM window.
    UnsupportedSampleRate {
        /// Mix-format sample rate.
        sample_rate_hz: u32,
    },
    /// Byte width of one frame did not match its interleaved sample container.
    InconsistentFrameSize {
        /// Mix-format channel count.
        channels: u16,
        /// Container width.
        bits_per_sample: u16,
        /// Reported byte width of one frame.
        block_align: u16,
    },
    /// Shared-mode initialization refused the endpoint mix format.
    DeviceRejectedMixFormat,
    /// The device reported a zero-length buffer after initialization.
    EmptyDeviceBuffer,
}

impl fmt::Display for NativeFormatReject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MixFormatUnavailable => formatter.write_str("mix format is unavailable"),
            Self::IntegerPcm {
                bits_per_sample,
                valid_bits_per_sample,
            } => write!(
                formatter,
                "integer PCM container {bits_per_sample} significant {valid_bits_per_sample}"
            ),
            Self::NonF32Float {
                bits_per_sample,
                valid_bits_per_sample,
            } => write!(
                formatter,
                "float container {bits_per_sample} significant {valid_bits_per_sample}"
            ),
            Self::UnknownEncoding {
                bits_per_sample,
                valid_bits_per_sample,
            } => write!(
                formatter,
                "unclassified encoding container {bits_per_sample} significant {valid_bits_per_sample}"
            ),
            Self::UnsupportedChannelCount { channels } => {
                write!(formatter, "channel count {channels}")
            }
            Self::InvalidChannelMask {
                channels,
                channel_mask,
            } => {
                write!(
                    formatter,
                    "speaker mask 0x{channel_mask:x} for {channels} channels"
                )
            }
            Self::UnsupportedSampleRate { sample_rate_hz } => {
                write!(
                    formatter,
                    "sample rate {sample_rate_hz} Hz outside {NATIVE_F32_MIN_SAMPLE_RATE_HZ}..={NATIVE_F32_MAX_SAMPLE_RATE_HZ}"
                )
            }
            Self::InconsistentFrameSize {
                channels,
                bits_per_sample,
                block_align,
            } => write!(
                formatter,
                "frame size {block_align} bytes for {channels} channels of {bits_per_sample}-bit samples"
            ),
            Self::DeviceRejectedMixFormat => {
                formatter.write_str("the endpoint refused its mix format for shared mode")
            }
            Self::EmptyDeviceBuffer => formatter.write_str("the device reported an empty buffer"),
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

    fn float_mix(rate: u32, channels: u16, mask: u32) -> WasapiFormat {
        WasapiFormat {
            sample_rate_hz: rate,
            channels,
            bits_per_sample: 32,
            valid_bits_per_sample: 32,
            block_align: channels.saturating_mul(4),
            channel_mask: mask,
            encoding: SampleEncoding::Float,
        }
    }

    #[test]
    fn native_pcm_accepts_real_rates_and_normalizes_missing_mono_stereo_masks() {
        for (rate, channels, mask) in [
            (8_000, 1, 0),
            (44_100, 2, 0),
            (48_000, 2, 3),
            (384_000, 1, 4),
        ] {
            let native =
                NativeF32Format::from_mix(float_mix(rate, channels, mask)).expect("accept");
            assert_eq!(native.sample_rate_hz, rate);
            assert_eq!(native.channels, u8::try_from(channels).unwrap());
            let normalized = match (channels, mask) {
                (1, 0) => 4,
                (2, 0) => 3,
                _ => mask,
            };
            assert_eq!(native.channel_mask, normalized);
            let range = native.format_range();
            assert_eq!(range.minimum_sample_rate_hz, rate);
            assert_eq!(range.maximum_sample_rate_hz, rate);
            assert_eq!(range.minimum_channels, native.channels);
            assert_eq!(range.maximum_channels, native.channels);
            assert!(!range.supports_engine_f32);
        }
        assert!(matches!(
            NativeF32Format::from_mix(float_mix(96_000, 1, 3)),
            Err(NativeFormatReject::InvalidChannelMask { .. })
        ));
    }

    #[test]
    fn native_pcm_preserves_integer_wide_float_layout_and_rejects_bad_rate() {
        let integer = WasapiFormat {
            encoding: SampleEncoding::IntegerPcm,
            ..float_mix(48_000, 2, 3)
        };
        assert_eq!(
            NativeF32Format::from_mix(integer)
                .unwrap()
                .pcm_format()
                .encoding(),
            PcmEncoding::Signed32Le
        );
        let wide = WasapiFormat {
            bits_per_sample: 64,
            valid_bits_per_sample: 64,
            block_align: 16,
            ..float_mix(48_000, 2, 3)
        };
        assert_eq!(
            NativeF32Format::from_mix(wide)
                .unwrap()
                .pcm_format()
                .encoding(),
            PcmEncoding::F64Le
        );
        assert_eq!(
            NativeF32Format::from_mix(float_mix(48_000, 6, 0x3F))
                .unwrap()
                .channels,
            6
        );
        assert_eq!(
            NativeF32Format::from_mix(float_mix(7_999, 2, 0)),
            Err(NativeFormatReject::UnsupportedSampleRate {
                sample_rate_hz: 7_999
            })
        );
        assert_eq!(
            NativeF32Format::from_mix(float_mix(384_001, 1, 0)),
            Err(NativeFormatReject::UnsupportedSampleRate {
                sample_rate_hz: 384_001
            })
        );
        let broken = WasapiFormat {
            block_align: 4,
            ..float_mix(48_000, 2, 0)
        };
        assert_eq!(
            NativeF32Format::from_mix(broken),
            Err(NativeFormatReject::InconsistentFrameSize {
                channels: 2,
                bits_per_sample: 32,
                block_align: 4,
            })
        );
        let unknown = WasapiFormat {
            encoding: SampleEncoding::Unknown,
            ..float_mix(48_000, 2, 0)
        };
        assert!(matches!(
            NativeF32Format::from_mix(unknown),
            Err(NativeFormatReject::UnknownEncoding { .. })
        ));
    }

    #[test]
    fn parsed_extensible_pcm_keeps_native_precision_and_layout() {
        let parsed =
            parse_aligned_extensible(2, 96_000, 32, 32, 0, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
        let native = NativeF32Format::from_mix(parsed).expect("96 kHz stereo");
        assert_eq!(native.channel_mask, 3);
        assert_eq!(native.channels, 2);
        assert_eq!(native.sample_rate_hz, 96_000);

        let wide = parse_aligned_extensible(1, 48_000, 64, 64, 4, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
        assert_eq!(
            NativeF32Format::from_mix(wide)
                .unwrap()
                .pcm_format()
                .encoding(),
            PcmEncoding::F64Le
        );
        let integer = parse_aligned_extensible(2, 48_000, 32, 32, 3, KSDATAFORMAT_SUBTYPE_PCM);
        assert_eq!(
            NativeF32Format::from_mix(integer)
                .unwrap()
                .pcm_format()
                .encoding(),
            PcmEncoding::Signed32Le
        );
    }

    fn parse_aligned_extensible(
        channels: u16,
        rate: u32,
        bits: u16,
        valid: u16,
        mask: u32,
        subtype: windows::core::GUID,
    ) -> WasapiFormat {
        #[repr(C, align(8))]
        struct Aligned([u8; 40]);
        let block = channels.saturating_mul(bits / 8);
        let average = rate.saturating_mul(u32::from(block));
        let mut bytes = Aligned([0_u8; 40]);
        bytes.0[0..2]
            .copy_from_slice(&u16::try_from(WAVE_FORMAT_EXTENSIBLE).unwrap().to_le_bytes());
        bytes.0[2..4].copy_from_slice(&channels.to_le_bytes());
        bytes.0[4..8].copy_from_slice(&rate.to_le_bytes());
        bytes.0[8..12].copy_from_slice(&average.to_le_bytes());
        bytes.0[12..14].copy_from_slice(&block.to_le_bytes());
        bytes.0[14..16].copy_from_slice(&bits.to_le_bytes());
        bytes.0[16..18].copy_from_slice(&22_u16.to_le_bytes());
        bytes.0[18..20].copy_from_slice(&valid.to_le_bytes());
        bytes.0[20..24].copy_from_slice(&mask.to_le_bytes());
        bytes.0[24..28].copy_from_slice(&subtype.data1.to_le_bytes());
        bytes.0[28..30].copy_from_slice(&subtype.data2.to_le_bytes());
        bytes.0[30..32].copy_from_slice(&subtype.data3.to_le_bytes());
        bytes.0[32..40].copy_from_slice(&subtype.data4);
        unsafe { parse_mix_format(bytes.0.as_ptr().cast()) }.expect("parse extensible format")
    }
}
