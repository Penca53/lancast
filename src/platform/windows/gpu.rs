//! D3D11 device selection (the adapter driving the captured monitor) and monitor enumeration.

use crate::platform::Monitor;
use anyhow::{Context, Result};
use windows::{
    Win32::{
        Foundation::{LPARAM, LUID, RECT},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1},
            Direct3D11::*,
            Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1},
            Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW},
        },
        UI::WindowsAndMessaging::MONITORINFOF_PRIMARY,
    },
    core::{BOOL, Interface},
};

pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub luid: LUID,
    pub name: String,
}

/// Monitors with their handles, primary first.
pub fn monitor_handles() -> Vec<(HMONITOR, Monitor)> {
    unsafe extern "system" fn collect(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        unsafe { (*(data.0 as *mut Vec<HMONITOR>)).push(h) };
        true.into()
    }
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut handles as *mut _ as isize));
    }
    let mut list: Vec<_> = handles
        .into_iter()
        .filter_map(|h| {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            unsafe { GetMonitorInfoW(h, &mut info as *mut _ as *mut MONITORINFO) }.ok().ok()?;
            let r = info.monitorInfo.rcMonitor;
            let len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
            let name = String::from_utf16_lossy(&info.szDevice[..len]).trim_start_matches(r"\\.\").to_owned();
            let primary = info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0;
            Some((h, Monitor { name, width: (r.right - r.left) as u32, height: (r.bottom - r.top) as u32, primary }))
        })
        .collect();
    list.sort_by_key(|(_, m)| !m.primary);
    list
}

pub fn monitors() -> Vec<Monitor> {
    monitor_handles().into_iter().map(|(_, m)| m).collect()
}

fn adapter_for(factory: &IDXGIFactory1, monitor: HMONITOR) -> Option<IDXGIAdapter1> {
    (0..).map_while(|i| unsafe { factory.EnumAdapters1(i) }.ok()).find(|a| {
        (0..)
            .map_while(|j| unsafe { a.EnumOutputs(j) }.ok())
            .any(|o| unsafe { o.GetDesc() }.is_ok_and(|d| d.Monitor == monitor))
    })
}

impl Gpu {
    /// Creates a device on the adapter that scans out `monitor`, so capture and encode
    /// never cross GPUs (matters on hybrid iGPU + dGPU systems).
    pub fn for_monitor(monitor: HMONITOR) -> Result<Gpu> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let adapter = adapter_for(&factory, monitor).or_else(|| factory.EnumAdapters1(0).ok()).context("no GPU adapter")?;
            let desc = adapter.GetDesc1()?;
            let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..len]);
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.context("no D3D11 device")?;
            // The encoder MFT uses the device from its own threads.
            let _ = device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
            Ok(Gpu { device, context: context.context("no D3D11 context")?, luid: desc.AdapterLuid, name })
        }
    }

    pub fn texture(&self, width: u32, height: u32, format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT, bind: D3D11_BIND_FLAG) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind.0 as u32,
            ..Default::default()
        };
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        tex.context("CreateTexture2D returned nothing")
    }
}
