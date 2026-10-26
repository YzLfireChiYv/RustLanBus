//! Speaker-aware coefficients from the Microsoft Audio Resampler DSP tables.
//! https://learn.microsoft.com/en-us/windows/win32/medfound/audioresampler
//!
//! The published decimal coefficients are used verbatim. A larger layout adds
//! silence at new speaker positions. Unsupported reductions remain explicit.

use rlb_peer::ChannelLayout;

use super::contract::{ABSOLUTE_MAX_CHANNELS, ChannelMatrix, RenderError, TimelineId};

pub fn default_channel_matrix(
    timeline: TimelineId,
    source: ChannelLayout,
    output: ChannelLayout,
    pan: f64,
) -> Result<ChannelMatrix, RenderError> {
    if !pan.is_finite() || !(-1.0..=1.0).contains(&pan) {
        return Err(RenderError::InvalidConfig(
            "source pan must be finite and within -1..1",
        ));
    }
    let mut matrix = ChannelMatrix {
        timeline,
        source_layout: source,
        output_layout: output,
        gains: [[0.0; ABSOLUTE_MAX_CHANNELS]; ABSOLUTE_MAX_CHANNELS],
    };
    let left = 1.0 - pan.max(0.0);
    let right = 1.0 + pan.min(0.0);
    let source_mask = source.channel_mask();
    let output_mask = output.channel_mask();
    match (source_mask, output_mask) {
        // Existing mono duplication and stereo balance semantics.
        (0x4, 0x3) => {
            matrix.gains[0][0] = left;
            matrix.gains[1][0] = right;
            return Ok(matrix);
        }
        (0x3, 0x4) => {
            matrix.gains[0][0] = 0.5 * left;
            matrix.gains[0][1] = 0.5 * right;
            return Ok(matrix);
        }
        (0x3f, 0x3) => set_rows(
            &mut matrix,
            &[
                &[0.314, 0.0, 0.222, 0.031, 0.268, 0.164],
                &[0.0, 0.314, 0.222, 0.031, 0.164, 0.268],
            ],
        ),
        (0x60f, 0x3) => set_rows(
            &mut matrix,
            &[
                &[0.320, 0.0, 0.226, 0.032, 0.292, 0.130],
                &[0.0, 0.320, 0.226, 0.032, 0.130, 0.292],
            ],
        ),
        (0x3f | 0x60f, 0x4) => {
            set_rows(&mut matrix, &[&[0.192, 0.192, 0.192, 0.038, 0.192, 0.192]])
        }
        (0x63f, 0x3) => set_rows(
            &mut matrix,
            &[
                &[0.222, 0.0, 0.157, 0.022, 0.189, 0.116, 0.203, 0.090],
                &[0.0, 0.222, 0.157, 0.022, 0.116, 0.189, 0.090, 0.203],
            ],
        ),
        (0x63f, 0x4) => set_rows(
            &mut matrix,
            &[&[0.139, 0.139, 0.139, 0.028, 0.139, 0.139, 0.139, 0.139]],
        ),
        (0x63f, 0x3f) => set_rows(
            &mut matrix,
            &[
                &[0.518, 0.0, 0.0, 0.0, 0.0, 0.0, 0.189, 0.0],
                &[0.0, 0.518, 0.0, 0.0, 0.0, 0.0, 0.0, 0.189],
                &[0.0, 0.0, 0.518, 0.0, 0.0, 0.0, 0.0, 0.0],
                &[0.0, 0.0, 0.0, 0.518, 0.0, 0.0, 0.0, 0.0],
                &[0.0, 0.0, 0.0, 0.0, 0.518, 0.0, 0.482, 0.0],
                &[0.0, 0.0, 0.0, 0.0, 0.0, 0.518, 0.0, 0.482],
            ],
        ),
        (0x63f, 0x60f) => set_rows(
            &mut matrix,
            &[
                &[0.447, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                &[0.0, 0.447, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                &[0.0, 0.0, 0.447, 0.0, 0.0, 0.0, 0.0, 0.0],
                &[0.0, 0.0, 0.0, 0.447, 0.0, 0.0, 0.0, 0.0],
                &[0.0, 0.0, 0.0, 0.0, 0.429, 0.124, 0.447, 0.0],
                &[0.0, 0.0, 0.0, 0.0, 0.124, 0.429, 0.0, 0.447],
            ],
        ),
        // Preserve positions in the common 7.1 bed, then apply the published
        // 7.1-to-5.1 matrix. Composition is computed only on configuration.
        (0x3f, 0x60f) | (0x60f, 0x3f) => {
            let expanded = default_channel_matrix(timeline, source, ChannelLayout::Surround71, 0.0)?;
            let reduced = default_channel_matrix(timeline, ChannelLayout::Surround71, output, 0.0)?;
            for target in 0..usize::from(output.channels()) {
                for input in 0..usize::from(source.channels()) {
                    matrix.gains[target][input] = (0..8)
                        .map(|middle| reduced.gains[target][middle] * expanded.gains[middle][input])
                        .sum();
                }
            }
        }
        // Identity by actual speaker positions, including sparse upmixes.
        _ if source_mask & !output_mask == 0 => {
            for input in 0..usize::from(source.channels()) {
                let speaker = source.speaker(input).expect("a source speaker exists");
                let target = (0..usize::from(output.channels()))
                    .find(|channel| output.speaker(*channel) == Some(speaker))
                    .expect("the output contains every source speaker");
                matrix.gains[target][input] = 1.0;
            }
        }
        _ => return Err(RenderError::UnsupportedLayout { source, output }),
    }
    // Balance only the front left/right output pair. Surround, centre and LFE
    // retain their published mapping; no spatial pan algorithm is introduced.
    for channel in 0..usize::from(output.channels()) {
        let balance = match output.speaker(channel) {
            Some(0x1) => left,
            Some(0x2) => right,
            _ => continue,
        };
        for coefficient in &mut matrix.gains[channel] {
            *coefficient *= balance;
        }
    }
    Ok(matrix)
}

fn set_rows(matrix: &mut ChannelMatrix, rows: &[&[f64]]) {
    for (row, source) in matrix.gains.iter_mut().zip(rows) {
        row[..source.len()].copy_from_slice(source);
    }
}
