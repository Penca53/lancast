//! GPU colour conversion + scaling (BGRA -> NV12 BT.709 limited range) via the D3D11 video processor.

use super::gpu::Gpu;
use anyhow::{Context, Result};
use std::mem::ManuallyDrop;
use windows::{
    Win32::Graphics::{
        Direct3D11::*,
        Dxgi::Common::{
            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_B8G8R8A8_UNORM,
            DXGI_FORMAT_NV12, DXGI_RATIONAL,
        },
    },
    core::Interface,
};

/// Output textures in flight; the encoder may still read one while the next is written.
const RING: usize = 4;

pub struct Converter {
    context: ID3D11DeviceContext,
    video: ID3D11VideoContext1,
    processor: ID3D11VideoProcessor,
    staging: ID3D11Texture2D,
    input: ID3D11VideoProcessorInputView,
    ring: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
}

impl Converter {
    pub fn new(gpu: &Gpu, (in_w, in_h): (u32, u32), (out_w, out_h): (u32, u32), fps: u32) -> Result<Converter> {
        unsafe {
            let vdev: ID3D11VideoDevice = gpu.device.cast()?;
            let video: ID3D11VideoContext1 = gpu.context.cast()?;
            let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: in_w,
                InputHeight: in_h,
                OutputFrameRate: rate,
                OutputWidth: out_w,
                OutputHeight: out_h,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
            };
            let en = vdev.CreateVideoProcessorEnumerator(&desc)?;
            let processor = vdev.CreateVideoProcessor(&en, 0)?;
            video.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            video.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            video.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);

            // Capture-pool textures are recycled by Windows, so copy into a texture we own.
            let staging = gpu.texture(in_w, in_h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE)?;
            let iv = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
            };
            let mut input = None;
            vdev.CreateVideoProcessorInputView(&staging, &en, &iv, Some(&mut input))?;

            let ov = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let ring = (0..RING)
                .map(|_| {
                    let tex = gpu.texture(out_w, out_h, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET)?;
                    let mut view = None;
                    vdev.CreateVideoProcessorOutputView(&tex, &en, &ov, Some(&mut view))?;
                    Ok((tex, view.context("no output view")?))
                })
                .collect::<Result<_>>()?;
            Ok(Converter { context: gpu.context.clone(), video, processor, staging, input: input.context("no input view")?, ring, next: 0 })
        }
    }

    /// Converts `src` into the next NV12 ring texture and returns it.
    pub fn convert(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let (tex, view) = &self.ring[self.next];
        self.next = (self.next + 1) % RING;
        unsafe {
            self.context.CopyResource(&self.staging, src);
            let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
                ..Default::default()
            }];
            let r = self.video.VideoProcessorBlt(&self.processor, view, 0, &streams);
            ManuallyDrop::drop(&mut streams[0].pInputSurface);
            r?;
            self.context.Flush();
        }
        Ok(tex.clone())
    }
}
