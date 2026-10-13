//! Windows DXGI Desktop Duplication capture.
//!
//! The compositor hands us the desktop as a GPU texture plus dirty/move
//! rectangles. We copy it into a single reusable staging texture and map it
//! for CPU readback; no per-frame allocations after the first frame.

use std::time::Duration;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_DENIED, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_MOVE_RECT, IDXGIAdapter, IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use super::{CaptureError, CaptureStatus, ScreenCapturer};
use crate::frame::{Frame, Rect};

pub struct DxgiCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
    rect_buf: Vec<RECT>,
    move_buf: Vec<DXGI_OUTDUPL_MOVE_RECT>,
}

// COM pointers are used from the single capture thread that owns this value.
unsafe impl Send for DxgiCapturer {}

fn os(e: windows::core::Error) -> CaptureError {
    if e.code() == DXGI_ERROR_ACCESS_LOST {
        CaptureError::AccessLost
    } else if e.code() == DXGI_ERROR_ACCESS_DENIED {
        CaptureError::PermissionDenied("desktop duplication denied (secure desktop active?)")
    } else {
        CaptureError::Os(e.to_string())
    }
}

/// Releases the acquired frame on every exit path.
struct FrameGuard<'a>(&'a IDXGIOutputDuplication);
impl Drop for FrameGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: called exactly once per successful AcquireNextFrame.
        if let Err(e) = unsafe { self.0.ReleaseFrame() } {
            tracing::debug!(error = %e, "ReleaseFrame failed");
        }
    }
}

impl DxgiCapturer {
    pub fn new(output_index: u32) -> Result<Self, CaptureError> {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        // SAFETY: out-pointers reference live locals; arguments follow the D3D11 contract.
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(os)?;
        }
        let device = device.ok_or_else(|| CaptureError::Os("D3D11CreateDevice returned no device".into()))?;
        let context = context.ok_or_else(|| CaptureError::Os("D3D11CreateDevice returned no context".into()))?;

        // SAFETY: plain COM calls on valid interfaces.
        let (duplication, width, height) = unsafe {
            let dxgi_device: IDXGIDevice = device.cast().map_err(os)?;
            let adapter = dxgi_device.GetAdapter().map_err(os)?;
            let output = adapter.EnumOutputs(output_index).map_err(os)?;
            let output1: IDXGIOutput1 = output.cast().map_err(os)?;
            let duplication = output1.DuplicateOutput(&device).map_err(os)?;
            let desc = duplication.GetDesc();
            (duplication, desc.ModeDesc.Width, desc.ModeDesc.Height)
        };

        Ok(Self {
            device,
            context,
            duplication,
            staging: None,
            width,
            height,
            rect_buf: Vec::with_capacity(64),
            move_buf: Vec::with_capacity(16),
        })
    }

    fn staging_for(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11Texture2D, CaptureError> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: desc is a valid out-pointer.
        unsafe { src.GetDesc(&mut desc) };
        if let Some(existing) = &self.staging {
            let mut cur = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: as above.
            unsafe { existing.GetDesc(&mut cur) };
            if cur.Width == desc.Width && cur.Height == desc.Height {
                return Ok(existing.clone());
            }
        }
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(CaptureError::Os(format!("unsupported desktop format {:?}", desc.Format)));
        }
        desc.Usage = D3D11_USAGE_STAGING;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY: desc describes a valid staging texture; out-pointer is live.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.map_err(os)?;
        let tex = tex.ok_or_else(|| CaptureError::Os("CreateTexture2D returned nothing".into()))?;
        self.width = desc.Width;
        self.height = desc.Height;
        self.staging = Some(tex.clone());
        Ok(tex)
    }

    /// Reads dirty rectangles; returns `None` (full diff) when the desktop
    /// moved content around or metadata is unavailable.
    fn dirty_rects(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO) -> Option<Vec<Rect>> {
        let total = info.TotalMetadataBufferSize as usize;
        if total == 0 {
            return None;
        }
        let rect_size = std::mem::size_of::<RECT>();
        let move_size = std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
        self.move_buf.resize(total / move_size + 1, DXGI_OUTDUPL_MOVE_RECT::default());
        let mut used = 0u32;
        // SAFETY: buffer length in bytes matches the allocation.
        let moved = unsafe {
            self.duplication.GetFrameMoveRects(
                (self.move_buf.len() * move_size) as u32,
                self.move_buf.as_mut_ptr(),
                &mut used,
            )
        };
        if moved.is_err() || used > 0 {
            return None;
        }
        self.rect_buf.resize(total / rect_size + 1, RECT::default());
        // SAFETY: as above.
        let dirty = unsafe {
            self.duplication.GetFrameDirtyRects(
                (self.rect_buf.len() * rect_size) as u32,
                self.rect_buf.as_mut_ptr(),
                &mut used,
            )
        };
        if dirty.is_err() {
            return None;
        }
        let count = used as usize / rect_size;
        Some(
            self.rect_buf[..count.min(self.rect_buf.len())]
                .iter()
                .filter(|r| r.right > r.left && r.bottom > r.top && r.left >= 0 && r.top >= 0)
                .map(|r| Rect {
                    x: r.left as u32,
                    y: r.top as u32,
                    width: (r.right - r.left) as u32,
                    height: (r.bottom - r.top) as u32,
                })
                .collect(),
        )
    }
}

impl ScreenCapturer for DxgiCapturer {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn capture(&mut self, timeout: Duration, frame: &mut Frame) -> Result<CaptureStatus, CaptureError> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let ms = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
        // SAFETY: out-pointers reference live locals.
        match unsafe { self.duplication.AcquireNextFrame(ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(CaptureStatus::Unchanged),
            Err(e) => return Err(os(e)),
        }
        let duplication = self.duplication.clone();
        let _guard = FrameGuard(&duplication);

        // Pointer-only updates carry no new desktop image.
        if info.LastPresentTime == 0 {
            return Ok(CaptureStatus::Unchanged);
        }
        let resource = resource.ok_or_else(|| CaptureError::Os("no desktop resource".into()))?;
        let texture: ID3D11Texture2D = resource.cast().map_err(os)?;
        let staging = self.staging_for(&texture)?;
        let dirty = self.dirty_rects(&info);

        // SAFETY: both textures share size/format; Map/Unmap are paired below.
        unsafe {
            self.context.CopyResource(&staging, &texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).map_err(os)?;
            let pitch = mapped.RowPitch as usize;
            let len = pitch * self.height as usize;
            let ok = if mapped.pData.is_null() {
                false
            } else {
                let src = std::slice::from_raw_parts(mapped.pData as *const u8, len);
                frame.copy_from_strided(src, self.width, self.height, pitch)
            };
            self.context.Unmap(&staging, 0);
            if !ok {
                return Err(CaptureError::Os("mapped desktop texture smaller than expected".into()));
            }
        }
        Ok(CaptureStatus::Updated { dirty })
    }
}
