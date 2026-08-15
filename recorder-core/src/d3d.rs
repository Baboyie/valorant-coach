//! D3D11 device setup for the capture pipeline.
//!
//! One device is shared by capture, the texture ring, and (later) the encoder,
//! so that a captured frame never has to cross a device boundary — crossing one
//! would mean a copy through system memory, which is exactly what ADR §8 forbids.

use windows::core::{Interface, Result};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;

pub struct Device {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
}

impl Device {
    pub fn new() -> Result<Device> {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;

        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                // BGRA_SUPPORT is mandatory for the WinRT Direct3D interop that
                // Windows.Graphics.Capture requires.
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }

        let device = device.expect("D3D11CreateDevice returned success with no device");
        let context = context.expect("D3D11CreateDevice returned success with no context");

        // WGC delivers frames on threadpool threads while we may be submitting
        // work from the encoder thread. Without multithread protection that is a
        // data race on the immediate context, and it manifests as rare corrupt
        // frames or device-removed — the kind of bug that only shows up under
        // load, i.e. exactly when someone is playing.
        let mt: ID3D11Multithread = context.cast()?;
        // Returns the *previous* protection state, which we have no use for.
        let _was_protected = unsafe { mt.SetMultithreadProtected(true) };

        Ok(Device { device, context })
    }

    /// The DXGI description of the adapter this device came up on.
    ///
    /// Printed with every measurement on purpose. ADR §6 warns that conflating
    /// the dev laptop with the benchmark rig would invalidate every number we
    /// produce, and the cheapest defence is for each run to state which silicon
    /// it ran on rather than trusting whoever pastes the output to remember.
    pub fn adapter_name(&self) -> Result<String> {
        let dxgi: IDXGIDevice = self.device.cast()?;
        let adapter = unsafe { dxgi.GetAdapter()? };
        let desc = unsafe { adapter.GetDesc()? };
        let end = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        Ok(String::from_utf16_lossy(&desc.Description[..end]))
    }

    /// The adapter this device runs on, for VRAM queries (§17).
    pub fn adapter3(&self) -> Result<windows::Win32::Graphics::Dxgi::IDXGIAdapter3> {
        let dxgi: IDXGIDevice = self.device.cast()?;
        let adapter = unsafe { dxgi.GetAdapter()? };
        adapter.cast()
    }

    /// The WinRT-flavoured handle to the same device, which is what
    /// `Direct3D11CaptureFramePool` wants.
    pub fn winrt_device(&self) -> Result<IDirect3DDevice> {
        let dxgi: IDXGIDevice = self.device.cast()?;
        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
        inspectable.cast()
    }
}

/// Allocate a GPU-resident texture matching a captured frame.
///
/// These are allocated once at session start and reused for the lifetime of the
/// recording. Steady-state allocation is what produces frame-time spikes, so the
/// ring owns every buffer it will ever need before the first frame arrives.
pub fn create_ring_texture(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        // DEFAULT usage with no CPU access flags keeps this texture entirely in
        // VRAM. We never map it, never read it back — the encoder consumes it
        // directly as a DXGI surface.
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    Ok(tex.expect("CreateTexture2D returned success with no texture"))
}
