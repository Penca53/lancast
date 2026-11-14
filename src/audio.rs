//! Platform-independent audio sample conversion.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    F32,
    I16,
}

/// Converts interleaved samples with `channels` channels to interleaved s16le stereo.
/// Mono is duplicated; extra channels beyond front-left/right are dropped.
pub fn to_s16_stereo(data: &[u8], format: SampleFormat, channels: usize) -> Vec<u8> {
    let channels = channels.max(1);
    let sample = |i: usize| -> i16 {
        match format {
            SampleFormat::F32 => {
                let b = &data[i * 4..i * 4 + 4];
                (f32::from_le_bytes([b[0], b[1], b[2], b[3]]).clamp(-1.0, 1.0) * 32767.0) as i16
            }
            SampleFormat::I16 => i16::from_le_bytes([data[i * 2], data[i * 2 + 1]]),
        }
    };
    let width = match format {
        SampleFormat::F32 => 4,
        SampleFormat::I16 => 2,
    };
    let frames = data.len() / (width * channels);
    let mut out = Vec::with_capacity(frames * 4);
    for f in 0..frames {
        let l = sample(f * channels);
        let r = if channels > 1 { sample(f * channels + 1) } else { l };
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f32s(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    fn i16s(b: &[u8]) -> Vec<i16> {
        b.chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
    }

    #[test]
    fn float_stereo_with_clipping() {
        let out = to_s16_stereo(&f32s(&[0.0, 1.0, -1.0, 2.0]), SampleFormat::F32, 2);
        assert_eq!(i16s(&out), [0, 32767, -32767, 32767]);
    }

    #[test]
    fn mono_is_duplicated_and_surround_downmixed_to_front() {
        assert_eq!(i16s(&to_s16_stereo(&f32s(&[0.5]), SampleFormat::F32, 1)), [16383, 16383]);
        let six = f32s(&[0.1, 0.2, 0.9, 0.9, 0.9, 0.9]);
        assert_eq!(i16s(&to_s16_stereo(&six, SampleFormat::F32, 6)), [3276, 6553]);
    }

    #[test]
    fn i16_passthrough_and_partial_frames_ignored() {
        let data: Vec<u8> = [100i16, -100, 7].iter().flat_map(|x| x.to_le_bytes()).collect();
        assert_eq!(i16s(&to_s16_stereo(&data, SampleFormat::I16, 2)), [100, -100]);
    }
}
