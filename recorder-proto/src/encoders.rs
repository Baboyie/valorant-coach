//! Hardware video-encoder discovery (requirement §3).
//!
//! Enumerates *hardware* Media Foundation encoder MFTs. We deliberately pass
//! MFT_ENUM_FLAG_HARDWARE without MFT_ENUM_FLAG_SOFTWARE: if the machine has no
//! hardware encoder we want to find that out and say so, not silently fall back
//! to x264 on the CPU cores Valorant is using (§2).

use windows::core::{GUID, Result};
use windows::Win32::Media::MediaFoundation::*;

/// A codec we are willing to encode with, in the order we prefer it.
///
/// H.264 first is a deliberate choice, not an oversight — see ADR §2. It has the
/// broadest hardware support and the lowest encoder cost. HEVC is a size win for
/// users who ask for it. AV1 is listed so the probe can *report* it, but note the
/// target rig (RTX 2060 / Turing) cannot encode AV1 at all.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub const PREFERENCE_ORDER: [Codec; 3] = [Codec::H264, Codec::Hevc, Codec::Av1];

    fn subtype(self) -> GUID {
        match self {
            Codec::H264 => MFVideoFormat_H264,
            Codec::Hevc => MFVideoFormat_HEVC,
            Codec::Av1 => MFVideoFormat_AV1,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
        }
    }
}

/// Which silicon is doing the encoding. Inferred from the MFT's friendly name,
/// which is the only vendor hint Media Foundation actually gives us.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Unknown,
}

impl Vendor {
    fn from_friendly_name(name: &str) -> Vendor {
        let n = name.to_ascii_lowercase();
        // Match on vendor words rather than the marketing name of the block:
        // NVIDIA's MFT has been called both "NVIDIA H.264 Encoder" and
        // "NVIDIA GPU accelerated video encoder" across driver generations.
        if n.contains("nvidia") || n.contains("nvenc") {
            Vendor::Nvidia
        } else if n.contains("amd") || n.contains("radeon") || n.contains("amf") {
            Vendor::Amd
        } else if n.contains("intel") || n.contains("quick sync") || n.contains("quicksync") {
            Vendor::Intel
        } else {
            Vendor::Unknown
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Vendor::Nvidia => "NVIDIA (NVENC)",
            Vendor::Amd => "AMD (AMF)",
            Vendor::Intel => "Intel (Quick Sync)",
            Vendor::Unknown => "unknown vendor",
        }
    }
}

#[derive(Clone, Debug)]
pub struct HardwareEncoder {
    pub friendly_name: String,
    pub codec: Codec,
    pub vendor: Vendor,
}

/// Enumerate every hardware encoder MFT for the codecs we care about.
///
/// Caller must have called `MFStartup` first.
pub fn probe() -> Result<Vec<HardwareEncoder>> {
    let mut found: Vec<HardwareEncoder> = Vec::new();
    for codec in Codec::PREFERENCE_ORDER {
        for e in probe_codec(codec)? {
            // MFTEnumEx reports the same physical encoder more than once (observed
            // on Intel: two identical entries per codec). Offering a user the same
            // encoder twice in a settings dropdown is a defect, so collapse by
            // (codec, name). Note this would also collapse the same-named encoder
            // on two GPUs — irrelevant for picking *an* encoder, but revisit if we
            // ever let the user choose which adapter encodes.
            let dup = found
                .iter()
                .any(|f| f.codec == e.codec && f.friendly_name == e.friendly_name);
            if !dup {
                found.push(e);
            }
        }
    }
    Ok(found)
}

fn probe_codec(codec: Codec) -> Result<Vec<HardwareEncoder>> {
    // We constrain the *output* type (the compressed codec) and leave the input
    // type open, so we see every encoder that can produce this codec regardless
    // of which raw formats it accepts.
    let output_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: codec.subtype(),
    };

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;

    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            // SORTANDFILTER puts the preferred/most capable MFT first, which is
            // what we want when several are registered for one codec.
            MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0),
            None,
            Some(&output_type),
            &mut activates,
            &mut count,
        )?;
    }

    let mut out = Vec::with_capacity(count as usize);

    // MFTEnumEx hands back a CoTaskMemAlloc'd array of AddRef'd interfaces. We
    // must release each one and free the array, on every path.
    for i in 0..count as usize {
        let slot = unsafe { &*activates.add(i) };
        if let Some(activate) = slot.as_ref() {
            if let Ok(name) = friendly_name(activate) {
                out.push(HardwareEncoder {
                    vendor: Vendor::from_friendly_name(&name),
                    friendly_name: name,
                    codec,
                });
            }
        }
        // Drop the interface reference this slot owns.
        unsafe { std::ptr::drop_in_place(activates.add(i)) };
    }

    if !activates.is_null() {
        unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _)) };
    }

    Ok(out)
}

fn friendly_name(activate: &IMFActivate) -> Result<String> {
    let mut buf: windows::core::PWSTR = windows::core::PWSTR::null();
    let mut len: u32 = 0;
    unsafe {
        activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, &mut len)?;
    }
    let s = unsafe { buf.to_string() }.unwrap_or_default();
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(buf.as_ptr() as *const _)) };
    Ok(s)
}

/// Pick the encoder we should actually record with.
///
/// Preference is by codec order (H.264 first, per ADR §2), and within a codec we
/// take the first MFT — MFT_ENUM_FLAG_SORTANDFILTER has already ordered them.
pub fn select_best(encoders: &[HardwareEncoder]) -> Option<&HardwareEncoder> {
    Codec::PREFERENCE_ORDER
        .iter()
        .find_map(|want| encoders.iter().find(|e| e.codec == *want))
}
