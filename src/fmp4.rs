//! Minimal fragmented-MP4 muxer (one H.264 / HEVC / AV1 sample per fragment) for the MSE
//! player path.

use crate::{
    codec,
    config::Codec,
    stream::{Kind, Packet},
};

const TIMESCALE: u32 = 1_000_000; // microseconds, so pts maps directly

fn bx(kind: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
    let len = 8 + parts.iter().map(|p| p.len()).sum::<usize>();
    let mut v = Vec::with_capacity(len);
    v.extend_from_slice(&(len as u32).to_be_bytes());
    v.extend_from_slice(kind);
    parts.iter().for_each(|p| v.extend_from_slice(p));
    v
}

fn full(kind: &[u8; 4], version: u8, flags: u32, parts: &[&[u8]]) -> Vec<u8> {
    let mut vf = flags.to_be_bytes();
    vf[0] = version;
    let mut all: Vec<&[u8]> = vec![&vf];
    all.extend_from_slice(parts);
    bx(kind, &all)
}

const MATRIX: [u8; 36] = [0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x40, 0, 0, 0];

/// Initialization segment (ftyp + moov) for one video track; `config` is the codec's decoder
/// configuration record (see [`codec::config_record`]).
pub fn init_segment(codec: Codec, config: &[u8], width: u16, height: u16) -> Vec<u8> {
    let (w, h) = (width.to_be_bytes(), height.to_be_bytes());
    let ftyp = bx(b"ftyp", &[b"isom", &0x200u32.to_be_bytes(), b"isomiso6mp41"]);
    let mvhd = full(
        b"mvhd",
        0,
        0,
        &[&[0; 8], &TIMESCALE.to_be_bytes(), &[0; 4], &0x00010000u32.to_be_bytes(), &[1, 0], &[0; 10], &MATRIX, &[0; 24], &2u32.to_be_bytes()],
    );
    let tkhd = full(b"tkhd", 0, 3, &[&[0; 8], &1u32.to_be_bytes(), &[0; 4], &[0; 4], &[0; 8], &[0; 4], &[0; 4], &MATRIX, &w, &[0, 0], &h, &[0, 0]]);
    let mdhd = full(b"mdhd", 0, 0, &[&[0; 8], &TIMESCALE.to_be_bytes(), &[0; 4], &[0x55, 0xc4, 0, 0]]);
    let hdlr = full(b"hdlr", 0, 0, &[&[0; 4], b"vide", &[0; 12], b"video\0"]);
    let vmhd = full(b"vmhd", 0, 1, &[&[0; 8]]);
    let dinf = bx(b"dinf", &[&full(b"dref", 0, 0, &[&1u32.to_be_bytes(), &full(b"url ", 0, 1, &[])])]);
    let (entry, config_box) = match codec {
        Codec::H264 => (b"avc1", b"avcC"),
        Codec::Hevc => (b"hvc1", b"hvcC"),
        Codec::Av1 => (b"av01", b"av1C"),
    };
    #[rustfmt::skip] // fields grouped as in the ISO BMFF VisualSampleEntry layout
    let sample_entry = bx(
        entry,
        &[
            &[0; 6], &1u16.to_be_bytes(), &[0; 16], &w, &h,
            &0x00480000u32.to_be_bytes(), &0x00480000u32.to_be_bytes(), &[0; 4], &1u16.to_be_bytes(),
            &[0; 32], &0x0018u16.to_be_bytes(), &0xffffu16.to_be_bytes(), &bx(config_box, &[config]),
        ],
    );
    let stbl = bx(
        b"stbl",
        &[
            &full(b"stsd", 0, 0, &[&1u32.to_be_bytes(), &sample_entry]),
            &full(b"stts", 0, 0, &[&[0; 4]]),
            &full(b"stsc", 0, 0, &[&[0; 4]]),
            &full(b"stsz", 0, 0, &[&[0; 8]]),
            &full(b"stco", 0, 0, &[&[0; 4]]),
        ],
    );
    let minf = bx(b"minf", &[&vmhd, &dinf, &stbl]);
    let mdia = bx(b"mdia", &[&mdhd, &hdlr, &minf]);
    let trak = bx(b"trak", &[&tkhd, &mdia]);
    let trex = full(b"trex", 0, 0, &[&1u32.to_be_bytes(), &1u32.to_be_bytes(), &[0; 12]]);
    let moov = bx(b"moov", &[&mvhd, &trak, &bx(b"mvex", &[&trex])]);
    [ftyp, moov].concat()
}

/// One movie fragment (moof + mdat) holding a single sample.
pub fn fragment(seq: u32, dts: u64, duration: u32, key: bool, sample: &[u8]) -> Vec<u8> {
    let flags: u32 = if key { 0x0200_0000 } else { 0x0101_0000 };
    let moof = |offset: u32| {
        let trun = full(
            b"trun",
            0,
            0x000701,
            &[&1u32.to_be_bytes(), &offset.to_be_bytes(), &duration.to_be_bytes(), &(sample.len() as u32).to_be_bytes(), &flags.to_be_bytes()],
        );
        let traf = bx(b"traf", &[&full(b"tfhd", 0, 0x020000, &[&1u32.to_be_bytes()]), &full(b"tfdt", 1, 0, &[&dts.to_be_bytes()]), &trun]);
        bx(b"moof", &[&full(b"mfhd", 0, 0, &[&seq.to_be_bytes()]), &traf])
    };
    let size = moof(0).len() as u32;
    [moof(size + 8), bx(b"mdat", &[sample])].concat()
}

/// Per-viewer converter from Annex-B video packets to fMP4 packets.
#[derive(Default)]
pub struct Remuxer {
    /// Codec and frame size, once the video config has arrived.
    format: Option<(Codec, u16, u16)>,
    /// Nominal frame duration in µs; the stream is constant-frame-rate.
    duration: u32,
    started: bool,
    seq: u32,
}

impl Remuxer {
    /// Returns the packets to send for `p` (possibly none, while waiting for a keyframe).
    pub fn push(&mut self, p: &Packet) -> Vec<Packet> {
        match p.kind {
            Kind::VideoConfig => {
                let cfg: serde_json::Value = serde_json::from_slice(&p.data).unwrap_or_default();
                let dim = |k: &str| cfg[k].as_u64().unwrap_or(0) as u16;
                let codec = cfg["codec"].as_str().and_then(Codec::from_codec_string);
                self.format = codec.map(|c| (c, dim("width"), dim("height")));
                // Two frame intervals: MSE drops everything after a gap longer than twice the
                // previous sample's duration, so this tolerates a late or skipped capture slot.
                // Overlap is harmless for video: each frame shows until the next one starts.
                self.duration = 2 * 1_000_000 / cfg["fps"].as_u64().unwrap_or(60).max(1) as u32;
                self.started = false;
                vec![p.clone()]
            }
            Kind::Video => self.video(p),
            _ => vec![p.clone()],
        }
    }

    fn video(&mut self, p: &Packet) -> Vec<Packet> {
        let mut out = Vec::new();
        let Some((codec, w, h)) = self.format else { return out };
        if !self.started {
            let Some(config) = p.key.then(|| codec::config_record(codec, &p.data)).flatten() else {
                return out;
            };
            out.push(Packet::new(Kind::VideoInit, true, p.pts_us, init_segment(codec, &config, w, h)));
            self.started = true;
        }
        self.seq += 1;
        let frag = fragment(self.seq, p.pts_us, self.duration, p.key, &codec::mp4_sample(codec, &p.data));
        out.push(Packet::new(Kind::Video, p.key, p.pts_us, frag));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::tests::{av1_key, h264_idr, hevc_idr};

    /// Walks boxes recursively, checking that every size fits its parent; returns box paths.
    fn walk(data: &[u8], prefix: &str, out: &mut Vec<String>) {
        const CONTAINERS: [&[u8; 4]; 8] = [b"moov", b"trak", b"mdia", b"minf", b"stbl", b"mvex", b"moof", b"traf"];
        let mut i = 0;
        while i < data.len() {
            let size = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
            assert!(size >= 8 && i + size <= data.len(), "bad box size at {prefix}");
            let kind: &[u8; 4] = data[i + 4..i + 8].try_into().unwrap();
            let path = format!("{prefix}/{}", String::from_utf8_lossy(kind));
            out.push(path.clone());
            if CONTAINERS.contains(&kind) {
                walk(&data[i + 8..i + size], &path, out);
            }
            i += size;
        }
    }

    #[test]
    fn init_segment_structure() {
        for (codec, au, entry) in
            [(Codec::H264, h264_idr(), b"avc1avcC"), (Codec::Hevc, hevc_idr(), b"hvc1hvcC"), (Codec::Av1, av1_key(), b"av01av1C")]
        {
            let config = codec::config_record(codec, &au).unwrap();
            let init = init_segment(codec, &config, 1920, 1080);
            let mut boxes = Vec::new();
            walk(&init, "", &mut boxes);
            for want in ["/ftyp", "/moov/mvhd", "/moov/trak/tkhd", "/moov/trak/mdia/minf/stbl/stsd", "/moov/mvex/trex"] {
                assert!(boxes.iter().any(|b| b == want), "missing {want}: {boxes:?}");
            }
            // The sample entry's 78 bytes of visual fields are followed by its config box.
            let at = init.windows(4).position(|w| w == &entry[..4]).expect("sample entry") + 4;
            let config_box = &init[at + 78..];
            let config_len = u32::from_be_bytes(config_box[..4].try_into().unwrap()) as usize;
            assert_eq!(&config_box[4..8], &entry[4..], "{codec:?}");
            assert_eq!(&config_box[8..config_len], &config[..]);
        }
    }

    #[test]
    fn fragment_data_offset_points_at_sample() {
        let sample = [9u8, 8, 7, 6, 5];
        let f = fragment(1, 1234, 16_667, true, &sample);
        let mut boxes = Vec::new();
        walk(&f, "", &mut boxes);
        assert_eq!(boxes.first().map(String::as_str), Some("/moof"));
        let moof_len = u32::from_be_bytes(f[0..4].try_into().unwrap()) as usize;
        let trun = f.windows(4).position(|w| w == b"trun").unwrap() - 4;
        let offset = u32::from_be_bytes(f[trun + 16..trun + 20].try_into().unwrap()) as usize;
        assert_eq!(offset, moof_len + 8);
        assert_eq!(&f[offset..], &sample);
    }

    #[test]
    fn remuxer_waits_for_config_and_keyframe() {
        let mut r = Remuxer::default();
        let delta = Packet::new(Kind::Video, false, 0, vec![0, 0, 1, 0x41, 1]);
        assert!(r.push(&delta).is_empty());
        let cfg = Packet::new(Kind::VideoConfig, false, 0, br#"{"codec":"avc1.640033","width":1920,"height":1080,"fps":50}"#.to_vec());
        assert_eq!(r.push(&cfg).len(), 1);
        let out = r.push(&Packet::new(Kind::Video, true, 1000, h264_idr()));
        let kinds: Vec<_> = out.iter().map(|p| p.kind).collect();
        assert_eq!(kinds, [Kind::VideoInit, Kind::Video]);
        // Sample duration is two frame intervals (40 ms at 50 fps), read from the trun box.
        let frag = &out[1].data;
        let trun = frag.windows(4).position(|w| w == b"trun").unwrap() - 4;
        assert_eq!(u32::from_be_bytes(frag[trun + 20..trun + 24].try_into().unwrap()), 40_000);
        assert_eq!(r.push(&Packet::new(Kind::Video, false, 17_000, delta.data.to_vec())).len(), 1);
    }

    #[test]
    fn remuxer_follows_codec_changes() {
        let mut r = Remuxer::default();
        for (cs, au, entry) in
            [("hvc1.1.6.L153.90", hevc_idr(), b"hvc1"), ("av01.0.13M.08", av1_key(), b"av01"), ("avc1.640033", h264_idr(), b"avc1")]
        {
            let cfg = format!(r#"{{"codec":"{cs}","width":1280,"height":720,"fps":60}}"#);
            r.push(&Packet::new(Kind::VideoConfig, false, 0, cfg.into_bytes()));
            let out = r.push(&Packet::new(Kind::Video, true, 1000, au));
            assert_eq!(out[0].kind, Kind::VideoInit);
            assert!(out[0].data.windows(4).any(|w| w == entry), "{cs}");
        }
        let unknown = br#"{"codec":"vp09.00.10.08","width":1280,"height":720}"#.to_vec();
        r.push(&Packet::new(Kind::VideoConfig, false, 0, unknown));
        assert!(r.push(&Packet::new(Kind::Video, true, 2000, h264_idr())).is_empty(), "unknown codec: nothing to mux");
    }
}
