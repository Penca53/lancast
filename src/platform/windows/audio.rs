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
                    let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                    capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                    let bytes = frames as usize * block;
                    let pcm = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                        vec![0u8; frames as usize * 4]
                    } else {
                        to_s16_stereo(slice::from_raw_parts(data, bytes), format, channels)
                    };
                    capture.ReleaseBuffer(frames)?;
                    if hub.viewers() > 0 {
                        hub.publish(Packet::new(Kind::Audio, false, clock::now_us(), pcm));
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
