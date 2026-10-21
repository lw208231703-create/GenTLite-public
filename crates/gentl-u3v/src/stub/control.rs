//! Phase A Linux stub for the GenCP control channel (notes/PENDING.md
//! "Linux 可移植"): the transaction engine needs the USB backend (Phase
//! B: rusb/libusb). The bootstrap snapshot type and the `ControlChannel`
//! impl exist so the ungated GenTL surface in lib.rs type-checks; every
//! operation returns GC_ERR_NOT_IMPLEMENTED. Unreachable in practice: the
//! stub discovery enumerates no devices, so no `U3vDevice` can be created
//! and no port can be opened.

#![allow(dead_code)]

use gentl_core::channel::ControlChannel;
use gentl_core::{GentlError, GentlResult};

use crate::cti::{DeviceAccess, U3vDevice};

fn not_supported<T>() -> GentlResult<T> {
    Err(GentlError::NotImplemented(
        "U3V USB backend on this platform (Phase B: rusb, notes/PENDING.md)",
    ))
}

/// SBRM snapshot (§4.2.2) — the fields the GenTL surface reads off an
/// opened device. Never populated on this backend.
#[derive(Debug, Clone, Default)]
pub struct Bootstrap {
    pub sbrm_address: u64,
    pub number_of_stream_channels: u32,
    pub manifest_table: u64,
    pub model: String,
}

impl ControlChannel for U3vDevice {
    fn read_mem(&self, _addr: u64, _buf: &mut [u8]) -> GentlResult<()> {
        not_supported()
    }

    fn write_mem(&self, _addr: u64, _buf: &[u8]) -> GentlResult<()> {
        if self.access == DeviceAccess::Monitor {
            return Err(GentlError::AccessDenied);
        }
        not_supported()
    }

    fn xml_url(&self) -> GentlResult<String> {
        not_supported()
    }
}

impl U3vDevice {
    /// Download the device description XML (manifest first Device XML
    /// entry, §4.1.5). Not reachable without a USB backend.
    pub fn device_xml(&self) -> GentlResult<Vec<u8>> {
        not_supported()
    }
}
