//! Shared PCM values and speaker positions. Network packetization and device
//! lifetime belong to their respective owners.

use core::fmt;

pub const MAX_CHANNELS: usize = 8;
pub const KNOWN_SPEAKERS: u32 = 0x0003_ffff;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PcmEncoding { F32Le = 1, Signed32Le = 2, F64Le = 3 }

impl PcmEncoding {
    pub const fn wire_tag(self) -> u8 { self as u8 }
    pub const fn sample_bytes(self) -> u8 {
        match self { Self::F32Le | Self::Signed32Le => 4, Self::F64Le => 8 }
    }
}

/// Interleaved channels follow ascending WAVEFORMATEXTENSIBLE speaker bits.
/// Preserve side/back positions rather than inferring them from channel count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelLayout { mask: u32 }

#[allow(non_upper_case_globals)]
impl ChannelLayout {
    pub const Mono: Self = Self { mask: 0x4 };
    pub const StereoLeftRight: Self = Self { mask: 0x3 };
    pub const Surround51: Self = Self { mask: 0x3f };
    pub const Surround51Side: Self = Self { mask: 0x60f };
    pub const Surround71: Self = Self { mask: 0x63f };
    pub const Surround71Wide: Self = Self { mask: 0xff };

    pub fn from_mask(channels: u16, mask: u32) -> Result<Self, PcmError> {
        let mask = match (channels, mask) { (1, 0) => 0x4, (2, 0) => 0x3, _ => mask };
        if channels == 0 || usize::from(channels) > MAX_CHANNELS
            || mask & !KNOWN_SPEAKERS != 0 || mask.count_ones() != u32::from(channels) {
            return Err(PcmError::InvalidFormat);
        }
        Ok(Self { mask })
    }
    pub const fn channels(self) -> u16 { self.mask.count_ones() as u16 }
    pub const fn channel_mask(self) -> u32 { self.mask }
    pub fn speaker(self, channel: usize) -> Option<u32> {
        let mut bits = self.mask;
        for _ in 0..channel { bits &= bits.wrapping_sub(1); }
        (bits != 0).then(|| 1_u32 << bits.trailing_zeros())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PcmFormat {
    rate: u32,
    layout: ChannelLayout,
    encoding: PcmEncoding,
    valid_bits: u8,
}

impl PcmFormat {
    pub fn new(rate: u32, layout: ChannelLayout, encoding: PcmEncoding, valid_bits: u8) -> Result<Self, PcmError> {
        let valid = match encoding {
            PcmEncoding::F32Le => valid_bits == 32,
            PcmEncoding::Signed32Le => (1..=32).contains(&valid_bits),
            PcmEncoding::F64Le => valid_bits == 64,
        };
        if !(8_000..=384_000).contains(&rate) || !valid { return Err(PcmError::InvalidFormat); }
        Ok(Self { rate, layout, encoding, valid_bits })
    }
    pub const fn sample_rate_hz(self) -> u32 { self.rate }
    pub const fn layout(self) -> ChannelLayout { self.layout }
    pub const fn encoding(self) -> PcmEncoding { self.encoding }
    pub const fn valid_bits(self) -> u8 { self.valid_bits }
    pub const fn channels(self) -> u16 { self.layout.channels() }
    pub const fn sample_bytes(self) -> u8 { self.encoding.sample_bytes() }
    pub const fn frame_bytes(self) -> usize { self.channels() as usize * self.sample_bytes() as usize }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PcmError { InvalidFormat, InvalidSamples, StorageTooSmall, NonFiniteSample, PrecisionLoss }

impl fmt::Display for PcmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidFormat => "invalid PCM format or speaker layout",
            Self::InvalidSamples => "PCM sample count or value does not match its format",
            Self::StorageTooSmall => "PCM storage is too small",
            Self::NonFiniteSample => "non-finite PCM samples are not supported",
            Self::PrecisionLoss => "sample cannot be represented without losing original precision",
        })
    }
}
impl std::error::Error for PcmError {}

/// Canonical numeric storage is F64: all F32 and signed32 values convert
/// exactly. The declared source encoding remains unchanged on the wire.
pub fn decode_into(format: PcmFormat, input: &[u8], output: &mut [f64]) -> Result<usize, PcmError> {
    if input.is_empty() || !input.len().is_multiple_of(format.frame_bytes()) { return Err(PcmError::InvalidSamples); }
    let width = usize::from(format.sample_bytes());
    let count = input.len() / width;
    if output.len() < count { return Err(PcmError::StorageTooSmall); }
    for (bytes, sample) in input.chunks_exact(width).zip(output.iter_mut()) {
        *sample = match format.encoding() {
            PcmEncoding::F32Le => f64::from(f32::from_le_bytes(bytes.try_into().unwrap())),
            PcmEncoding::F64Le => f64::from_le_bytes(bytes.try_into().unwrap()),
            PcmEncoding::Signed32Le => {
                let integer = i32::from_le_bytes(bytes.try_into().unwrap());
                if !valid_integer(integer, format.valid_bits()) { return Err(PcmError::InvalidSamples); }
                f64::from(integer) / 2_147_483_648.0
            }
        };
        if !sample.is_finite() { return Err(PcmError::NonFiniteSample); }
    }
    Ok(count)
}

pub fn encode_into(format: PcmFormat, input: &[f64], output: &mut [u8]) -> Result<usize, PcmError> {
    if input.is_empty() || !input.len().is_multiple_of(usize::from(format.channels())) { return Err(PcmError::InvalidSamples); }
    let width = usize::from(format.sample_bytes());
    let bytes = input.len().checked_mul(width).ok_or(PcmError::StorageTooSmall)?;
    if output.len() < bytes { return Err(PcmError::StorageTooSmall); }
    // Validate first so an invalid block never leaves partially encoded output.
    for &sample in input {
        if !sample.is_finite() { return Err(PcmError::NonFiniteSample); }
        match format.encoding() {
            PcmEncoding::F32Le if f64::from(sample as f32) != sample => return Err(PcmError::PrecisionLoss),
            PcmEncoding::Signed32Le => { integer_sample(sample, format.valid_bits())?; }
            _ => {}
        }
    }
    for (&sample, target) in input.iter().zip(output[..bytes].chunks_exact_mut(width)) {
        match format.encoding() {
            PcmEncoding::F32Le => target.copy_from_slice(&(sample as f32).to_le_bytes()),
            PcmEncoding::F64Le => target.copy_from_slice(&sample.to_le_bytes()),
            PcmEncoding::Signed32Le => target.copy_from_slice(&integer_sample(sample, format.valid_bits())?.to_le_bytes()),
        }
    }
    Ok(bytes)
}

fn integer_sample(sample: f64, valid_bits: u8) -> Result<i32, PcmError> {
    let value = sample * 2_147_483_648.0;
    if value < f64::from(i32::MIN) || value > f64::from(i32::MAX) || value.fract() != 0.0 {
        return Err(PcmError::PrecisionLoss);
    }
    let integer = value as i32;
    if !valid_integer(integer, valid_bits) { return Err(PcmError::PrecisionLoss); }
    Ok(integer)
}
fn valid_integer(value: i32, valid_bits: u8) -> bool {
    let unused = 32 - u32::from(valid_bits);
    unused == 0 || value as u32 & ((1_u32 << unused) - 1) == 0
}
