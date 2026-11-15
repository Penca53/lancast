//! Hardware H.264/HEVC/AV1 encoding through the GPU vendor's async Media Foundation transform
//! (NVENC, AMF or Quick Sync), fed directly with D3D11 textures.

use super::gpu::Gpu;
use crate::config::{Codec, RateControl};
use anyhow::{Context, Result, bail};
use std::{mem::ManuallyDrop, ptr, slice};
use windows::{
    Win32::{
        Graphics::Direct3D11::ID3D11Texture2D,
        Media::MediaFoundation::*,
        System::{Com::CoTaskMemFree, Variant::VARIANT},
    },
    core::{GUID, Interface, PWSTR},
};

pub struct Encoder {
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: ICodecAPI,
    _manager: IMFDXGIDeviceManager,
    provides_samples: bool,
    out_size: u32,
    duration: i64,
    pub name: String,
}

// SAFETY: async hardware MFTs and multithread-protected D3D11 devices are free-threaded;
// the encoder is used from one thread at a time.
unsafe impl Send for Encoder {}

pub enum Event {
    NeedInput,
    Output { data: Vec<u8>, pts_us: u64, key: bool },
    Other,
}

fn find_encoder(gpu: &Gpu, subtype: GUID) -> Result<(IMFActivate, String)> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype };
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.context("no attributes")?;
        let luid = gpu.luid;
        let luid_bytes = [luid.LowPart.to_le_bytes(), luid.HighPart.to_le_bytes()].concat();
        attrs.SetBlob(&MFT_ENUM_ADAPTER_LUID, &luid_bytes)?;
        let mut list: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count = 0u32;
        MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &attrs,
            &mut list,
            &mut count,
        )?;
        let items = slice::from_raw_parts_mut(list, count as usize);
        let first = items.first_mut().and_then(Option::take);
        items.iter_mut().for_each(|a| drop(a.take()));
        CoTaskMemFree(Some(list as *const _));
        let activate = first.with_context(|| format!("no hardware encoder for this codec on {}", gpu.name))?;
        let mut name = PWSTR::null();
        let mut len = 0;
        let label = if activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len).is_ok() {
            let s = name.to_string().unwrap_or_default();
            CoTaskMemFree(Some(name.0 as *const _));
            s
        } else {
            "hardware encoder".into()
        };
        Ok((activate, label))
    }
}

fn subtype(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => MFVideoFormat_H264,
        Codec::Hevc => MFVideoFormat_HEVC,
        Codec::Av1 => MFVideoFormat_AV1,
    }
}

/// Whether `gpu` has a hardware encoder for `codec`.
pub fn available(gpu: &Gpu, codec: Codec) -> bool {
    find_encoder(gpu, subtype(codec)).is_ok()
}

fn set(api: &ICodecAPI, key: &GUID, value: VARIANT) {
    if let Err(e) = unsafe { api.SetValue(key, &value) } {
        log::debug!("encoder ignores {key:?}: {e}");
    }
}

fn media_type(subtype: &GUID, w: u32, h: u32, fps: u32) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, (w as u64) << 32 | h as u64)?;
        t.SetUINT64(&MF_MT_FRAME_RATE, (fps as u64) << 32 | 1)?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, 1 << 32 | 1)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        // Matches the converter's BT.709 limited-range output so TVs show correct colours.
        t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        Ok(t)
    }
}

impl Encoder {
    pub fn new(gpu: &Gpu, codec: Codec, w: u32, h: u32, fps: u32, rate: RateControl) -> Result<Encoder> {
        let subtype = subtype(codec);
        let (activate, name) = find_encoder(gpu, subtype)?;
        unsafe {
            let mft: IMFTransform = activate.ActivateObject()?;
            let attrs = mft.GetAttributes()?;
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            let mut token = 0;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.context("no device manager")?;
            manager.ResetDevice(&gpu.device, token)?;
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

            let api: ICodecAPI = mft.cast()?;
            set(&api, &CODECAPI_AVLowLatencyMode, true.into());
            set(&api, &CODECAPI_AVEncMPVDefaultBPictureCount, 0u32.into());
            // Keyframes are requested on demand (new viewer / loss), so use a very long GOP.
            set(&api, &CODECAPI_AVEncMPVGOPSize, (fps * 600).into());
            let bitrate = match rate {
                RateControl::Cbr { kbps } => {
                    set(&api, &CODECAPI_AVEncCommonRateControlMode, (eAVEncCommonRateControlMode_CBR.0 as u32).into());
                    set(&api, &CODECAPI_AVEncCommonMeanBitRate, (kbps * 1000).into());
                    // Rate-control buffer of two frames, as game streamers use: without it NVENC
                    // bursts to ~1.5x the bitrate for a second, which starved an LG webOS TV's small
                    // playback buffer (measured: 37 -> 27 Mbps peak second at 25 Mbps).
                    set(&api, &CODECAPI_AVEncCommonBufferSize, (kbps * 1000 / fps.max(1) * 2).into());
                    kbps * 1000
                }
                RateControl::Quality { level } => {
                    set(&api, &CODECAPI_AVEncCommonRateControlMode, (eAVEncCommonRateControlMode_Quality.0 as u32).into());
                    set(&api, &CODECAPI_AVEncCommonQuality, level.min(100).into());
                    100_000_000
                }
            };
            set(&api, &CODECAPI_AVEncCommonQualityVsSpeed, 50u32.into());

            let out = media_type(&subtype, w, h, fps)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            let profile = match codec {
                Codec::H264 => eAVEncH264VProfile_High.0 as u32,
                Codec::Hevc => eAVEncH265VProfile_Main_420_8.0 as u32,
                Codec::Av1 => eAVEncAV1VProfile_Main_420_8.0 as u32,
            };
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
            mft.SetOutputType(0, &out, 0).context("encoder rejected output format")?;
            mft.SetInputType(0, &media_type(&MFVideoFormat_NV12, w, h, fps)?, 0).context("encoder rejected NV12 input")?;

            let info = mft.GetOutputStreamInfo(0)?;
            let provides = (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Encoder {
                events: mft.cast()?,
                mft,
                codec_api: api,
                _manager: manager,
                provides_samples: info.dwFlags & provides != 0,
                out_size: info.cbSize.max(w * h * 2),
                duration: 10_000_000 / fps.max(1) as i64,
                name,
            })
        }
    }

    /// Blocks until the encoder asks for input or has output ready.
    pub fn next_event(&mut self) -> Result<Event> {
        unsafe {
            let ev = self.events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))?;
            let kind = ev.GetType()?;
            if kind == METransformNeedInput.0 as u32 {
                Ok(Event::NeedInput)
            } else if kind == METransformHaveOutput.0 as u32 {
                self.output()
            } else if kind == MEError.0 as u32 {
                // An async MFT reports failures (e.g. a GPU reset) only as this event and then
                // stops asking for input: fail so the worker restarts the pipeline.
                let status = ev.GetStatus()?;
                bail!("encoder failed: {}", windows::core::Error::from_hresult(status))
            } else {
                Ok(Event::Other)
            }
        }
    }

    pub fn submit(&mut self, tex: &ID3D11Texture2D, pts_us: u64, key: bool) -> Result<()> {
        unsafe {
            if key {
                set(&self.codec_api, &CODECAPI_AVEncVideoForceKeyFrame, 1u32.into());
            }
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts_us as i64 * 10)?;
            sample.SetSampleDuration(self.duration)?;
            self.mft.ProcessInput(0, &sample, 0)?;
        }
        Ok(())
    }

    fn output(&mut self) -> Result<Event> {
        unsafe {
            let sample = if self.provides_samples {
                None
            } else {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
                Some(s)
            };
            let mut buf =
                [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(sample), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
            let mut status = 0;
            let r = self.mft.ProcessOutput(0, &mut buf, &mut status);
            let sample = ManuallyDrop::take(&mut buf[0].pSample);
            drop(ManuallyDrop::take(&mut buf[0].pEvents));
            if let Err(e) = r {
                if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                    let t = self.mft.GetOutputAvailableType(0, 0)?;
                    self.mft.SetOutputType(0, &t, 0)?;
                    return Ok(Event::Other);
                }
                bail!("ProcessOutput failed: {e}");
            }
            let sample = sample.context("encoder produced no sample")?;
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let pts_us = (sample.GetSampleTime()? / 10) as u64;
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut p = ptr::null_mut();
            let mut len = 0;
            buffer.Lock(&mut p, None, Some(&mut len))?;
            let data = slice::from_raw_parts(p, len as usize).to_vec();
            buffer.Unlock()?;
            Ok(Event::Output { data, pts_us, key })
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}
