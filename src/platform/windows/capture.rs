//! Windows.Graphics.Capture of a whole monitor, delivering GPU textures (no CPU copies).

use super::gpu::Gpu;
use anyhow::{Result, bail};
use std::{
    sync::mpsc::{Receiver, sync_channel},
    time::Duration,
};
use windows::{
    Foundation::TypedEventHandler,
    Graphics::{
        Capture::{Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Graphics::{Direct3D11::ID3D11Texture2D, Dxgi::IDXGIDevice, Gdi::HMONITOR},
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
        },
    },
    core::{Interface, factory},
};

pub struct Capture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    arrived: Receiver<()>,
    pub width: u32,
    pub height: u32,
}

impl Capture {
    pub fn new(gpu: &Gpu, monitor: HMONITOR, cursor: bool) -> Result<Capture> {
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor)? };
        let size = item.Size()?;
        let dxgi: IDXGIDevice = gpu.device.cast()?;
        let device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)?;
        let (tx, arrived) = sync_channel(1);
        pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
            let _ = tx.try_send(());
            Ok(())
        }))?;
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsCursorCaptureEnabled(cursor);
        let _ = session.SetIsBorderRequired(false); // Windows 11: no yellow border
        session.StartCapture()?;
        Ok(Capture { pool, session, arrived, width: size.Width as u32, height: size.Height as u32 })
    }

    /// Waits up to `timeout` for new content; returns the newest frame, discarding older ones.
    pub fn next(&self, timeout: Duration) -> Result<Option<Direct3D11CaptureFrame>> {
        if self.arrived.recv_timeout(timeout).is_err() {
            return Ok(None);
        }
        self.newest()
    }

    /// Newest already-queued frame, without waiting.
    pub fn newest(&self) -> Result<Option<Direct3D11CaptureFrame>> {
        let mut latest: Option<Direct3D11CaptureFrame> = None;
        while let Ok(f) = self.pool.TryGetNextFrame() {
            if let Some(old) = latest.replace(f) {
                let _ = old.Close();
            }
        }
        if let Some(f) = &latest {
            let s = f.ContentSize()?;
            if (s.Width as u32, s.Height as u32) != (self.width, self.height) {
                bail!("display resolution changed");
            }
        }
        Ok(latest)
    }
}

pub fn texture(frame: &Direct3D11CaptureFrame) -> Result<ID3D11Texture2D> {
    let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
    Ok(unsafe { access.GetInterface()? })
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
