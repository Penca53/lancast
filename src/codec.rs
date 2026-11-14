//! Bitstream helpers for H.264 / HEVC (Annex-B NAL units) and AV1 (low-overhead OBUs):
//! keyframe detection, RFC 6381 codec strings, and MP4 sample / decoder-config conversion.

use crate::config::Codec;

/// Iterates NAL unit payloads (start codes stripped) of an Annex-B stream.
pub fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts.iter().skip(1).map(|&s| s - 3).chain([data.len()]).collect();
    starts.into_iter().zip(ends).map(move |(s, e)| {
        // A 4-byte start code leaves a trailing zero on the previous unit.
        let mut e = e;
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        &data[s..e]
    })
}

pub fn h264_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

pub fn hevc_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| (b >> 1) & 0x3f)
}

const HEVC_VPS: u8 = 32;
const HEVC_SPS: u8 = 33;
const HEVC_PPS: u8 = 34;
const HEVC_AUD: u8 = 35;

/// Removes emulation-prevention bytes (00 00 03 -> 00 00).
pub fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

// ---- AV1 ---------------------------------------------------------------------------------

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
const OBU_FRAME_HEADER: u8 = 3;
const OBU_FRAME: u8 = 6;
const OBU_PADDING: u8 = 15;

/// One AV1 OBU: its type, the whole OBU (header included) and its payload.
pub struct Obu<'a> {
    pub kind: u8,
    pub raw: &'a [u8],
    pub payload: &'a [u8],
}

/// Iterates the OBUs of a low-overhead bitstream (stops at the first malformed one).
pub fn obus(mut data: &[u8]) -> impl Iterator<Item = Obu<'_>> {
    std::iter::from_fn(move || {
        let header = *data.first()?;
        let mut pos = 1 + usize::from(header & 0x04 != 0); // extension byte
        let size = if header & 0x02 != 0 {
            let (size, n) = leb128(data.get(pos..)?)?;
            pos += n;
            usize::try_from(size).ok()?
        } else {
            data.len().checked_sub(pos)?
        };
        let end = pos.checked_add(size).filter(|&e| e <= data.len())?;
        let obu = Obu { kind: (header >> 3) & 0x0f, raw: &data[..end], payload: &data[pos..end] };
        data = &data[end..];
        Some(obu)
    })
}

/// Unsigned LEB128 value and its length in bytes.
fn leb128(data: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (i, &b) in data.iter().take(8).enumerate() {
        v |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// MSB-first bit reader for AV1 headers.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            v = v << 1 | u32::from(byte >> (7 - self.pos % 8) & 1);
            self.pos += 1;
        }
        Some(v)
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.bits(1)? == 1)
    }

    fn uvlc(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros >= 32 {
                return None;
            }
        }
        Some(self.bits(zeros)? + ((1u64 << zeros) - 1) as u32)
    }
}

/// Profile, level index and tier of operating point 0, from an AV1 sequence header payload.
fn av1_profile_level_tier(seq: &[u8]) -> Option<(u32, u32, u32)> {
    let mut b = Bits { data: seq, pos: 0 };
    let profile = b.bits(3)?;
    b.bits(1)?; // still_picture
    if b.flag()? {
        // reduced_still_picture_header
        return Some((profile, b.bits(5)?, 0));
    }
    if b.flag()? {
        // timing_info: num_units_in_display_tick, time_scale, equal_picture_interval
        b.bits(32)?;
        b.bits(32)?;
        if b.flag()? {
            b.uvlc()?;
        }
        if b.flag()? {
            // decoder_model_info (its per-operating-point fields come after the level/tier)
            b.bits(5 + 32 + 5 + 5)?;
        }
    }
    b.bits(1)?; // initial_display_delay_present_flag
    b.bits(5)?; // operating_points_cnt_minus_1
    b.bits(12)?; // operating_point_idc[0]
    let level = b.bits(5)?;
    let tier = if level > 7 { b.bits(1)? } else { 0 };
    Some((profile, level, tier))
}

fn av1_sequence_header(au: &[u8]) -> Option<Obu<'_>> {
    obus(au).find(|o| o.kind == OBU_SEQUENCE_HEADER)
}

// ---- codec-generic ---------------------------------------------------------------------

/// Whether the access unit starts a decodable sequence (IDR / IRAP / AV1 key frame).
pub fn is_keyframe(codec: Codec, au: &[u8]) -> bool {
    match codec {
        Codec::H264 => nal_units(au).any(|n| h264_type(n) == 5),
        Codec::Hevc => nal_units(au).any(|n| (16..=21).contains(&hevc_type(n))),
        // Frame header (never a reduced still-picture header in video): show_existing_frame
        // must be 0 and frame_type KEY_FRAME (0).
        Codec::Av1 => obus(au).any(|o| matches!(o.kind, OBU_FRAME | OBU_FRAME_HEADER) && o.payload.first().is_some_and(|b| b & 0xe0 == 0)),
    }
}

/// RFC 6381 codec string (e.g. `avc1.640033`) derived from the parameter sets in `au`.
pub fn codec_string(codec: Codec, au: &[u8]) -> Option<String> {
    match codec {
        Codec::H264 => {
            let sps = nal_units(au).find(|n| h264_type(n) == 7 && n.len() >= 4)?;
            Some(format!("avc1.{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3]))
        }
        Codec::Hevc => {
            let ptl = hevc_profile_tier_level(au)?;
            let space = ["", "A", "B", "C"][(ptl[0] >> 6) as usize];
            let tier = if ptl[0] & 0x20 != 0 { 'H' } else { 'L' };
            let compat = u32::from_be_bytes([ptl[1], ptl[2], ptl[3], ptl[4]]).reverse_bits();
            let mut s = format!("hvc1.{space}{}.{compat:X}.{tier}{}", ptl[0] & 0x1f, ptl[11]);
            let constraints = &ptl[5..11];
            let used = constraints.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
            for b in &constraints[..used] {
                s += &format!(".{b:X}");
            }
            Some(s)
        }
        Codec::Av1 => {
            let (profile, level, tier) = av1_profile_level_tier(av1_sequence_header(au)?.payload)?;
            Some(format!("av01.{profile}.{level:02}{}.08", if tier == 1 { 'H' } else { 'M' }))
        }
    }
}

/// The 12-byte general profile_tier_level of the SPS in `au` (same layout as in `hvcC`).
fn hevc_profile_tier_level(au: &[u8]) -> Option<[u8; 12]> {
    let r = unescape(hevc_sps(au)?.get(2..)?);
    r.get(1..13)?.try_into().ok()
}

fn hevc_sps(au: &[u8]) -> Option<&[u8]> {
    nal_units(au).find(|n| hevc_type(n) == HEVC_SPS)
}

/// Converts an access unit to an MP4 sample: length-prefixed NAL units without parameter
/// sets or delimiters (they live in the sample entry's config record), or AV1 OBUs without
/// temporal delimiters and padding.
pub fn mp4_sample(codec: Codec, au: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(au.len() + 16);
    if codec == Codec::Av1 {
        for o in obus(au).filter(|o| !matches!(o.kind, OBU_TEMPORAL_DELIMITER | OBU_PADDING)) {
            out.extend_from_slice(o.raw);
        }
        return out;
    }
    let keep = |n: &&[u8]| match codec {
        Codec::H264 => !matches!(h264_type(n), 7..=9),
        _ => !matches!(hevc_type(n), HEVC_VPS..=HEVC_AUD),
    };
    for n in nal_units(au).filter(keep) {
        out.extend_from_slice(&(n.len() as u32).to_be_bytes());
        out.extend_from_slice(n);
    }
    out
}

/// Decoder configuration record (`avcC`, `hvcC` or `av1C` box payload) from a keyframe.
pub fn config_record(codec: Codec, au: &[u8]) -> Option<Vec<u8>> {
    match codec {
        Codec::H264 => avcc_record(au),
        Codec::Hevc => hvcc_record(au),
        Codec::Av1 => av1c_record(au),
    }
}

/// AVCDecoderConfigurationRecord built from the SPS/PPS of an H.264 keyframe.
fn avcc_record(au: &[u8]) -> Option<Vec<u8>> {
    let sps = nal_units(au).find(|n| h264_type(n) == 7 && n.len() >= 4)?;
    let pps = nal_units(au).find(|n| h264_type(n) == 8)?;
    let mut r = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    r.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    r.extend_from_slice(sps);
    r.push(1);
    r.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    r.extend_from_slice(pps);
    Some(r)
}

/// HEVCDecoderConfigurationRecord (ISO/IEC 14496-15 §8.3.3) from the VPS/SPS/PPS of a
/// keyframe. Chroma format and bit depth are the encoder's fixed 4:2:0 8-bit.
fn hvcc_record(au: &[u8]) -> Option<Vec<u8>> {
    let ptl = hevc_profile_tier_level(au)?;
    let sub_layers = unescape(hevc_sps(au)?.get(2..3)?)[0];
    let (layers, nested) = ((sub_layers >> 1 & 7) + 1, sub_layers & 1);
    let mut r = vec![1];
    r.extend_from_slice(&ptl);
    // min_spatial_segmentation 0, parallelism 0, chroma 4:2:0, 8-bit luma/chroma, avg rate 0.
    r.extend_from_slice(&[0xf0, 0x00, 0xfc, 0xfd, 0xf8, 0xf8, 0, 0]);
    r.push(layers << 3 | nested << 2 | 3); // 4-byte NAL lengths
    let arrays: Vec<(u8, Vec<&[u8]>)> =
        [HEVC_VPS, HEVC_SPS, HEVC_PPS].into_iter().map(|t| (t, nal_units(au).filter(|n| hevc_type(n) == t).collect())).collect();
    if arrays.iter().any(|(_, nals)| nals.is_empty()) {
        return None;
    }
    r.push(arrays.len() as u8);
    for (t, nals) in arrays {
        r.push(0x80 | t); // array_completeness: parameter sets only appear here
        r.extend_from_slice(&(nals.len() as u16).to_be_bytes());
        for n in nals {
            r.extend_from_slice(&(n.len() as u16).to_be_bytes());
            r.extend_from_slice(n);
        }
    }
    Some(r)
}

/// AV1CodecConfigurationRecord (AV1-ISOBMFF §2.3) with the sequence header OBU.
/// Colour fields are the encoder's fixed 4:2:0 8-bit.
fn av1c_record(au: &[u8]) -> Option<Vec<u8>> {
    let seq = av1_sequence_header(au)?;
    let (profile, level, tier) = av1_profile_level_tier(seq.payload)?;
    let mut r = vec![0x81, (profile << 5 | level) as u8, (tier << 7) as u8 | 0b1100, 0];
    r.extend_from_slice(seq.raw);
    Some(r)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A tiny synthetic H.264 IDR access unit: AUD, SPS (High@5.1), PPS, IDR slice.
    pub fn h264_idr() -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, 0x09, 0xf0];
        v.extend([0, 0, 0, 1, 0x67, 0x64, 0x00, 0x33, 0xac, 0x00, 0x00, 0x03, 0x00, 0x10]);
        v.extend([0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80]);
        // IDR slice followed by trailing_zero_8bits padding, which must be dropped.
        v.extend([0, 0, 1, 0x65, 0x88, 0x84, 0x21, 0x00, 0x00]);
        v
    }

    /// A synthetic HEVC IDR access unit: AUD, VPS, SPS (Main@5.1, one sub-layer), PPS, IDR_W_RADL.
    pub fn hevc_idr() -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, 0x46, 0x01, 0x10];
        v.extend([0, 0, 0, 1, 0x40, 0x01, 0x0c, 0x01, 0xff, 0xff]);
        // SPS: NAL header, vps_id/max_sub_layers/nesting byte (0x01), then the general PTL:
        // Main profile, compat flags 0x60000000, progressive+frame-only, level 5.1 (153),
        // with an emulation-prevention byte inside the constraint flags.
        v.extend([0, 0, 0, 1, 0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0, 0, 0, 0, 0, 153, 0xa0]);
        v.extend([0, 0, 0, 1, 0x44, 0x01, 0xc1, 0x72]);
        v.extend([0, 0, 1, 0x26, 0x01, 0xaf, 0x09]);
        v
    }

    /// A synthetic AV1 key frame temporal unit: temporal delimiter, sequence header
    /// (profile 0, level 13 = 5.1, tier 0, no timing info), padding, key frame OBU.
    pub fn av1_key() -> Vec<u8> {
        // seq_profile 000, still 0, reduced 0, timing 0, initial_display_delay 0,
        // op_cnt 00000, idc 0x000 (24 bits), then level 01101.
        let seq = [0x00, 0x00, 0x00, 0x68];
        let mut v = vec![0x12, 0x00]; // temporal delimiter with size 0
        v.extend([0x0a, seq.len() as u8]);
        v.extend(seq);
        v.extend([0x7a, 1, 0xff]); // padding
        v.extend([0x32, 3, 0x10, 0xaa, 0xbb]); // OBU_FRAME: show_existing 0, KEY_FRAME, show_frame 1
        v
    }

    #[test]
    fn splits_nals_with_3_and_4_byte_start_codes() {
        let au = h264_idr();
        let types: Vec<u8> = nal_units(&au).map(h264_type).collect();
        assert_eq!(types, [9, 7, 8, 5]);
        // Trailing zeros inside the last NAL are trimmed only as start-code padding.
        assert_eq!(nal_units(&au).nth(2).unwrap(), &[0x68, 0xee, 0x3c, 0x80]);
    }

    #[test]
    fn h264_codec_string_and_keyframe() {
        let au = h264_idr();
        assert_eq!(codec_string(Codec::H264, &au).as_deref(), Some("avc1.640033"));
        assert!(is_keyframe(Codec::H264, &au));
        assert!(!is_keyframe(Codec::H264, &[0, 0, 1, 0x41, 0x9a]));
    }

    #[test]
    fn unescapes_emulation_prevention() {
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3, 0]), [0, 0, 1, 0, 0, 0]);
        assert_eq!(unescape(&[1, 3, 0, 3]), [1, 3, 0, 3]);
    }

    #[test]
    fn hevc_codec_string_and_keyframe() {
        let au = hevc_idr();
        assert_eq!(codec_string(Codec::Hevc, &au).as_deref(), Some("hvc1.1.6.L153.90"));
        assert!(is_keyframe(Codec::Hevc, &au));
        assert!(!is_keyframe(Codec::Hevc, &[0, 0, 1, 0x02, 0x01, 0xd0])); // TRAIL_R
    }

    #[test]
    fn hvcc_record_layout() {
        let au = hevc_idr();
        let r = config_record(Codec::Hevc, &au).unwrap();
        assert_eq!(&r[..2], &[1, 0x01]);
        assert_eq!(r[12], 153, "general_level_idc");
        assert_eq!(r[21], 0b0000_1111, "one temporal layer, nested, 4-byte lengths");
        assert_eq!(r[22], 3, "VPS, SPS and PPS arrays");
        // Arrays hold the NAL units verbatim (escaped), each with a 16-bit length.
        let sps = nal_units(&au).nth(2).unwrap();
        assert!(r.windows(sps.len()).any(|w| w == sps));
        let vps_len = nal_units(&au).nth(1).unwrap().len();
        assert_eq!(r[23..26], [0x80 | HEVC_VPS, 0, 1]);
        assert_eq!(u16::from_be_bytes([r[26], r[27]]) as usize, vps_len);
        assert!(config_record(Codec::Hevc, &[0, 0, 1, 0x26, 0x01]).is_none());
    }

    #[test]
    fn hevc_sample_drops_parameter_sets() {
        assert_eq!(mp4_sample(Codec::Hevc, &hevc_idr()), [0, 0, 0, 4, 0x26, 0x01, 0xaf, 0x09]);
    }

    #[test]
    fn av1_obus_codec_string_and_keyframe() {
        let tu = av1_key();
        let kinds: Vec<u8> = obus(&tu).map(|o| o.kind).collect();
        assert_eq!(kinds, [OBU_TEMPORAL_DELIMITER, OBU_SEQUENCE_HEADER, OBU_PADDING, OBU_FRAME]);
        assert_eq!(codec_string(Codec::Av1, &tu).as_deref(), Some("av01.0.13M.08"));
        assert!(is_keyframe(Codec::Av1, &tu));
        // Inter frame (frame_type 1) and a frame header repeating an existing frame.
        assert!(!is_keyframe(Codec::Av1, &[0x12, 0, 0x32, 2, 0x30, 0]));
        assert!(!is_keyframe(Codec::Av1, &[0x1a, 1, 0x80]));
    }

    #[test]
    fn av1_sample_and_config() {
        let tu = av1_key();
        let sample = mp4_sample(Codec::Av1, &tu);
        assert_eq!(sample, tu[2..8].iter().chain(&tu[11..]).copied().collect::<Vec<_>>());
        let r = config_record(Codec::Av1, &tu).unwrap();
        assert_eq!(&r[..4], &[0x81, 13, 0b0000_1100, 0]);
        assert_eq!(&r[4..], &tu[2..8], "carries the sequence header OBU");
    }

    #[test]
    fn av1_sequence_header_with_timing_and_tier() {
        // Profile 0, timing info (equal interval, uvlc 0), no decoder model, level 12 tier 1.
        let mut bits = String::from("000" /*profile*/);
        bits += "0" /*still*/;
        bits += "0" /*reduced*/;
        bits += "1" /*timing*/;
        bits += &"0".repeat(31);
        bits += "1" /*display tick*/;
        bits += &format!("{:032b}", 60);
        bits += "1" /*equal interval*/;
        bits += "1" /*uvlc 0*/;
        bits += "0" /*decoder model*/;
        bits += "0" /*initial display delay*/;
        bits += "00000" /*op count*/;
        bits += &"0".repeat(12);
        bits += "01100" /*level 12*/;
        bits += "1" /*tier*/;
        while bits.len() % 8 != 0 {
            bits.push('0');
        }
        let payload: Vec<u8> = bits.as_bytes().chunks(8).map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 2).unwrap()).collect();
        assert_eq!(av1_profile_level_tier(&payload), Some((0, 12, 1)));
    }

    #[test]
    fn obu_parsing_survives_garbage() {
        assert_eq!(obus(&[0x0a, 0xff]).count(), 0, "size runs past the end");
        assert_eq!(obus(&[0x0a, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80]).count(), 0);
        assert!(codec_string(Codec::Av1, &[0x0a, 1, 0]).is_none(), "truncated sequence header");
    }

    #[test]
    fn avcc_conversion() {
        let au = h264_idr();
        let avcc = mp4_sample(Codec::H264, &au);
        assert_eq!(avcc, [0, 0, 0, 4, 0x65, 0x88, 0x84, 0x21]);
        let rec = config_record(Codec::H264, &au).unwrap();
        assert_eq!(&rec[..6], &[1, 0x64, 0x00, 0x33, 0xff, 0xe1]);
        assert_eq!(rec.len(), 6 + 2 + 10 + 1 + 2 + 4);
    }
}
