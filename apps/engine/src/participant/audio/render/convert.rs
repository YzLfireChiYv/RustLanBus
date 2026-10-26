//! One public-count F64 fixed-output resampler per original.
//!
//! Chunk size and ratio are set before each requirement query. The caller
//! closes the staged source span before `process`. A count mismatch or
//! processing error leaves that closure in place and resets the kernel.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Adjustable, Async, FixedAsync, Resampler, Resizable, SincInterpolationParameters,
    WindowFunction,
};

use super::contract::{RenderError, SINC_PHASES, SINC_TAPS};

#[derive(Debug)]
pub(crate) struct RateConverter {
    inner: Async<f64>,
    channels: usize,
    nominal: f64,
    ratio: f64,
    chunk: usize,
    max_chunk: usize,
    planned_input: usize,
    planned_output: usize,
    pub cutoff: Option<f32>,
    input: Vec<f64>,
    output: Vec<f64>,
}

impl RateConverter {
    pub(crate) fn prepare(
        input_rate: u32,
        output_rate: u32,
        channels: usize,
        max_chunk: usize,
        max_ratio_relative: f64,
    ) -> Result<Self, RenderError> {
        if input_rate == 0 || output_rate == 0 || channels == 0 || max_chunk == 0 {
            return Err(RenderError::InvalidConfig(
                "a converter needs positive rates, channels and a chunk",
            ));
        }
        let nominal = f64::from(output_rate) / f64::from(input_rate);
        let parameters =
            SincInterpolationParameters::new(SINC_TAPS, WindowFunction::BlackmanHarris2)
                .oversampling_factor(SINC_PHASES);
        let inner = Async::<f64>::new_sinc(
            nominal,
            max_ratio_relative,
            &parameters,
            max_chunk,
            channels,
            FixedAsync::Output,
        )
        .map_err(|error| RenderError::Resampler(error.to_string()))?;
        let cutoff = inner.cutoff();
        let max_input = inner.input_frames_max().max(1);
        let max_output = inner.output_frames_max().max(max_chunk);
        Ok(Self {
            inner,
            channels,
            nominal,
            ratio: nominal,
            chunk: max_chunk,
            max_chunk,
            planned_input: 0,
            planned_output: 0,
            cutoff,
            input: vec![0.0; max_input * channels],
            output: vec![0.0; max_output * channels],
        })
    }

    #[must_use]
    pub(crate) fn nominal(&self) -> f64 {
        self.nominal
    }

    #[must_use]
    pub(crate) fn ratio(&self) -> f64 {
        self.ratio
    }

    #[must_use]
    pub(crate) fn output_delay(&self) -> usize {
        self.inner.output_delay()
    }

    pub(crate) fn select(
        &mut self,
        chunk: usize,
        ratio: f64,
    ) -> Result<(usize, usize), RenderError> {
        if chunk == 0 || chunk > self.max_chunk {
            return Err(RenderError::Capacity(
                "the requested output chunk exceeds the prepared converter",
            ));
        }
        self.inner
            .set_chunk_size(chunk)
            .map_err(|error| RenderError::Resampler(error.to_string()))?;
        self.inner
            .set_resample_ratio(ratio, true)
            .map_err(|error| RenderError::Resampler(error.to_string()))?;
        self.chunk = chunk;
        self.ratio = ratio;
        self.planned_input = self.inner.input_frames_next();
        self.planned_output = self.inner.output_frames_next();
        if self.planned_output != chunk || self.planned_input * self.channels > self.input.len() {
            return Err(RenderError::Resampler(
                "the resampler reported an unexpected input or output count".into(),
            ));
        }
        Ok((self.planned_input, self.planned_output))
    }

    #[must_use]
    pub(crate) fn max_input_frames(&self) -> usize {
        self.input.len() / self.channels.max(1)
    }

    pub(crate) fn input_mut(&mut self) -> &mut [f64] {
        &mut self.input
    }

    #[must_use]
    pub(crate) fn rendered(&self, frames: usize) -> &[f64] {
        let length = frames.saturating_mul(self.channels).min(self.output.len());
        &self.output[..length]
    }

    pub(crate) fn process(&mut self) -> Result<(), RenderError> {
        let input_frames = self.planned_input;
        let output_frames = self.planned_output;
        let channels = self.channels;
        let input = InterleavedSlice::new(
            &self.input[..input_frames * channels],
            channels,
            input_frames,
        )
        .map_err(|_| RenderError::Capacity("the staged input does not match the converter"))?;
        let output_samples = output_frames * channels;
        let mut output =
            InterleavedSlice::new_mut(&mut self.output[..output_samples], channels, output_frames)
                .map_err(|_| {
                    RenderError::Capacity("the prepared output does not match the converter")
                })?;
        let (used, made) = self
            .inner
            .process_into_buffer(&input, &mut output, None)
            .map_err(|error| RenderError::Resampler(error.to_string()))?;
        if used != input_frames || made != output_frames {
            self.recover_nominal();
            return Err(RenderError::Resampler(
                "the resampler consumed or produced a different count than requested".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn recover_nominal(&mut self) {
        self.inner.reset();
        self.ratio = self.nominal;
        self.chunk = self.max_chunk.min(self.chunk.max(1));
        if self.inner.set_chunk_size(self.chunk).is_err() {
            return;
        }
        let _ = self.inner.set_resample_ratio(self.nominal, false);
        self.ratio = self.inner.resample_ratio();
    }
}
