//! WASAPI loopback capture of whatever the PC is playing.

use crate::{
    audio::{SampleFormat, to_s16_stereo},
    clock,
    stream::{Hub, Kind, Packet},
};
use anyhow::{Result, bail};
use std::{
    slice,
    sync::atomic::{AtomicBool, Ordering},
};
use windows::Win32::{
    Foundation::{CloseHandle, WAIT_OBJECT_0},
    Media::Audio::*,
    System::{
        Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree},
        Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
        Threading::{CreateEventW, WaitForSingleObject},
    },
};

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;

pub fn run_audio(hub: &Hub, stop: &AtomicBool) -> Result<()> {
    super::com_init();
    unsafe {
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = devices.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let fmt_ptr = client.GetMixFormat()?;
        let fmt = std::ptr::read_unaligned(fmt_ptr);
        let (tag, bits) = (fmt.wFormatTag, fmt.wBitsPerSample);
        let format = match (tag, bits) {
            (WAVE_FORMAT_IEEE_FLOAT | WAVE_FORMAT_EXTENSIBLE, 32) => SampleFormat::F32,
            (WAVE_FORMAT_PCM | WAVE_FORMAT_EXTENSIBLE, 16) => SampleFormat::I16,
            _ => {
                CoTaskMemFree(Some(fmt_ptr as *const _));
                bail!("unsupported mix format (tag {tag:#x}, {bits} bits)");
            }
        };
        let (rate, channels, block) = (fmt.nSamplesPerSec, fmt.nChannels as usize, fmt.nBlockAlign as usize);
        let init = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            200_000, // 20 ms buffer
            0,
            fmt_ptr,
            None,
        );
        CoTaskMemFree(Some(fmt_ptr as *const _));
        init?;
        let event = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        let config = serde_json::json!({ "rate": rate, "channels": 2 }).to_string();
        hub.publish(Packet::new(Kind::AudioConfig, false, clock::now_us(), config.into_bytes()));
        log::info!("audio: {rate} Hz, {channels} ch, {format:?}");
        client.Start()?;
        let result = (|| -> Result<()> {
            while !stop.load(Ordering::Relaxed) {
                if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
                    continue;
                }
                while capture.GetNextPacketSize()? > 0 {
                    let (mut data, mut frames, mut flags, mut qpc) = (std::ptr::null_mut(), 0u32, 0u32, 0u64);
                    capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                    let timestamp = (flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0).then_some(qpc);
                    let pts = clock::now_us().saturating_add_signed(playout_offset_us(qpc_now(), timestamp));
                    let bytes = frames as usize * block;
                    let pcm = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                        vec![0u8; frames as usize * 4]
                    } else {
                        to_s16_stereo(slice::from_raw_parts(data, bytes), format, channels)
                    };
                    capture.ReleaseBuffer(frames)?;
                    if hub.viewers() > 0 {
                        hub.publish(Packet::new(Kind::Audio, false, pts, pcm));
                    }
                }
            }
            Ok(())
        })();
        let _ = client.Stop();
        let _ = CloseHandle(event);
        result
    }
}

/// Performance-counter time now, in the 100 ns units WASAPI timestamps packets with.
fn qpc_now() -> u64 {
    let (mut count, mut freq) = (0i64, 0i64);
    unsafe {
        let _ = QueryPerformanceCounter(&mut count);
        let _ = QueryPerformanceFrequency(&mut freq);
    }
    (count as u128 * 10_000_000 / freq.max(1) as u128) as u64
}

/// When the PC plays a packet's first sample, in µs from now (positive: in the future).
/// Video pts are when the PC shows a frame, so audio pts must be when it plays a sound for
/// the player to line them up. Loopback packets carry that playout time, about one period
/// (~10 ms) after they can be read. Without a plausible timestamp: now.
fn playout_offset_us(now: u64, timestamp: Option<u64>) -> i64 {
    let offset = timestamp.filter(|&t| t != 0).map(|t| (t as i64 - now as i64) / 10);
    offset.filter(|o| o.abs() < 1_000_000).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playout_offset_from_timestamp() {
        assert_eq!(playout_offset_us(1_000_000, Some(1_106_000)), 10_600, "loopback: plays ~10 ms after reading");
        assert_eq!(playout_offset_us(1_000_000, Some(800_000)), -20_000);
        assert_eq!(playout_offset_us(1_000_000, None), 0, "timestamp error flagged");
        assert_eq!(playout_offset_us(1_000_000, Some(0)), 0, "no timestamp");
        assert_eq!(playout_offset_us(100_000_000, Some(1)), 0, "implausible");
        assert!(qpc_now() > 0);
    }
}
