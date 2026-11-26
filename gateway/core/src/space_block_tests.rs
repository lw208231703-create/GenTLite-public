//! Width-aware register-space block conversion (review 2026-11-26 follow-up).
//!
//! A raw block has no type information: an eight-byte block is either one
//! 64-bit register or two adjacent 32-bit registers, and the correct byte
//! order differs. These tests drive the real `RegisterSpace` data path with a
//! little-endian backend and the served XML as the width source.

use super::*;
use crate::back::{BackIdentity, CameraBack};
use crate::frame::{CanonicalFrame, FrameSession, FrameSource};
use crate::relay::Relay;
use crate::xml::XmlMerger;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};

struct NoopSession;

impl FrameSession for NoopSession {
    fn open(&mut self, _payload_hint: usize) -> Result<(), String> {
        Ok(())
    }
    fn send_frame(&mut self, _frame: CanonicalFrame) {}
    fn close(&mut self) {}
}

/// Little-endian backend (the U3V / Null direction) with canned blocks.
#[derive(Default)]
struct LeBack {
    reads: Arc<Mutex<Vec<(u32, Vec<u8>)>>>,
    writes: Arc<Mutex<Vec<(u32, Vec<u8>)>>>,
}

impl CameraBack for LeBack {
    fn register_endian(&self) -> RegisterEndian {
        RegisterEndian::Little
    }
    fn read_reg(&mut self, _addr: u32) -> Result<u32, u16> {
        Err(0x8001)
    }
    fn write_reg(&mut self, _addr: u32, _value: u32) -> Result<(), u16> {
        Ok(())
    }
    fn read_block(&mut self, addr: u32, len: u32) -> Result<Vec<u8>, u16> {
        if let Some((_, data)) = self
            .reads
            .lock()
            .unwrap()
            .iter()
            .find(|(a, d)| *a == addr && d.len() == len as usize)
        {
            return Ok(data.clone());
        }
        Ok(vec![0u8; len as usize])
    }
    fn write_block(&mut self, addr: u32, data: &[u8]) -> Result<(), u16> {
        self.writes.lock().unwrap().push((addr, data.to_vec()));
        Ok(())
    }
    fn fetch_xml(&mut self) -> Result<Vec<u8>, u16> {
        Ok(Vec::new())
    }
    fn fetch_identity(&mut self) -> Result<BackIdentity, u16> {
        Ok(BackIdentity {
            manufacturer: "m".into(),
            model: "m".into(),
            device_version: "1".into(),
            serial_number: "s".into(),
            user_defined_name: String::new(),
            spec_version_major: 2,
            spec_version_minor: 0,
        })
    }
    fn open_stream(&mut self) -> Result<Box<dyn FrameSource>, u16> {
        Err(0x8001)
    }
    fn close_stream(&mut self) -> Result<(), u16> {
        Ok(())
    }
}

/// Served XML: Width/Height are 32-bit registers at 0x80000/0x80004; a
/// 64-bit PayloadSize-like register lives at 0x81000.
const XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<RegisterDescription Name="Test" ModelName="Test" VendorName="T"
  StandardNameSpace="None" SchemaMajorVersion="1" SchemaMinorVersion="1"
  SchemaSubMinorVersion="0" MajorVersion="1" MinorVersion="0" SubMinorVersion="0"
  ToolTip="test" ProductGuid="00000000-0000-0000-0000-000000000000"
  VersionGuid="00000000-0000-0000-0000-000000000000">
  <Category Name="Root"><pFeature>WidthReg</pFeature><pFeature>HeightReg</pFeature><pFeature>PayloadSizeReg</pFeature></Category>
  <IntReg Name="WidthReg">
    <Address>0x00080000</Address>
    <Length>4</Length>
    <AccessMode>RW</AccessMode>
  </IntReg>
  <IntReg Name="HeightReg">
    <Address>0x00080004</Address>
    <Length>4</Length>
    <AccessMode>RW</AccessMode>
  </IntReg>
  <IntReg Name="PayloadSizeReg">
    <Address>0x00081000</Address>
    <Length>8</Length>
    <AccessMode>RO</AccessMode>
  </IntReg>
</RegisterDescription>
"#;

fn make_space(back: Arc<Mutex<LeBack>>) -> RegisterSpace {
    let shared: SharedBack = back;
    let xml = Arc::new(XmlMerger::new(XML.as_bytes(), None).expect("served XML"));
    let channel = Relay::new(
        Arc::clone(&shared),
        false,
        Box::new(|_regs| Box::new(NoopSession)),
    );
    RegisterSpace::new(shared, xml, Ipv4Addr::LOCALHOST, [0u8; 6], channel)
}

/// The reviewer's reproduction through the real path: an eight-byte READMEM
/// covering two 32-bit registers must keep register order and swap each word.
#[test]
fn eight_byte_read_keeps_two_32_bit_registers_in_order() {
    let back = Arc::new(Mutex::new(LeBack::default()));
    back.lock().unwrap().reads.lock().unwrap().push((
        0x0008_0000,
        // Width = 1280 (0x500), Height = 1024 (0x400), little-endian words.
        vec![0x00, 0x05, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00],
    ));
    let space = make_space(Arc::clone(&back));

    let bytes = space.read_block(0x0008_0000, 8).expect("READMEM");

    assert_eq!(bytes, vec![0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x04, 0x00]);
    assert_eq!(u32::from_be_bytes(bytes[0..4].try_into().unwrap()), 1280);
    assert_eq!(u32::from_be_bytes(bytes[4..8].try_into().unwrap()), 1024);
}

/// A register the served XML declares as 64-bit is still one value: an
/// eight-byte read reverses it whole (PayloadSize semantics).
#[test]
fn declared_64_bit_register_is_reversed_as_one_value() {
    let back = Arc::new(Mutex::new(LeBack::default()));
    back.lock().unwrap().reads.lock().unwrap().push((
        0x0008_1000,
        0x0026_2140u64.to_le_bytes().to_vec(), // 2,621,440
    ));
    let space = make_space(Arc::clone(&back));

    let bytes = space.read_block(0x0008_1000, 8).expect("READMEM");

    assert_eq!(u64::from_be_bytes(bytes.try_into().unwrap()), 0x0026_2140);
}

/// Same width rule on the write side: two 32-bit registers are swapped word
/// by word, so Width and Height do not land on each other's address.
#[test]
fn eight_byte_write_keeps_two_32_bit_registers_in_order() {
    let back = Arc::new(Mutex::new(LeBack::default()));
    let space = make_space(Arc::clone(&back));

    space
        .write_block(
            0x0008_0000,
            &[0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x04, 0x00],
        )
        .expect("WRITEMEM");

    let writes = back.lock().unwrap().writes.lock().unwrap().clone();
    assert_eq!(
        writes,
        vec![(0x0008_0000, vec![0x00, 0x05, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00])],
        "Width/Height must stay at their own addresses"
    );
}
