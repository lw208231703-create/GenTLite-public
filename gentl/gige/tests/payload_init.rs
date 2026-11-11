//! Pre-start `STREAM_INFO_PAYLOAD_SIZE` regression: the producer resolves the
//! mandatory GenICam `PayloadSize` feature through the device XML and reads
//! its backing register when the data stream opens. A consumer that checks
//! the value before `DSStartAcquisition` (the LDVision pattern) must see the
//! real payload, not 0 waiting for the first GVSP leader.
//!
//! The fake device is a plain `GvcpServer`: First URL -> `local:` XML blob ->
//! `PayloadSize` -> an `IntReg` at 0x1000_0050. No stream is started; the
//! assertion is purely the control-plane value.

#![allow(non_snake_case)]

use std::ffi::{c_char, c_void};
use std::net::{Ipv4Addr, SocketAddr};
use std::ptr;

use gentl_core::types::*;
use gentl_gige::*;
use gentl_gvcp::registers as reg;
use gentl_gvcp::wire::{DiscoveryInfo, GEV_STATUS_INVALID_ADDRESS};
use gentl_gvcp::{DeviceHandler, GvcpServer};

const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
const DEVICE_ID: &str = "00:11:22:33:44:55";
const XML_BASE: u32 = 0x0020_0000;
const PAYLOAD_ADDR: u32 = 0x1000_0050;
const PAYLOAD: u32 = 1_310_720; // 1280 * 1024 * 1 (Mono8)

struct FakeDevice {
    xml: Vec<u8>,
    url: Vec<u8>,
}

impl FakeDevice {
    fn new() -> Self {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<RegisterDescription xmlns="http://www.genicam.org/GenApi/Version_1_1" ModelName="Fake" VendorName="GenTLite">
  <Integer Name="Width" NameSpace="Standard"><pValue>WidthReg</pValue></Integer>
  <IntReg Name="WidthReg" NameSpace="Custom">
    <Address>0x00100000</Address><Length>4</Length>
    <AccessMode>RW</AccessMode><pPort>Device</pPort>
    <Endianess>BigEndian</Endianess>
  </IntReg>
  <Integer Name="PayloadSize" NameSpace="Standard"><pValue>PayloadSizeReg</pValue></Integer>
  <IntReg Name="PayloadSizeReg" NameSpace="Custom">
    <Address>0x10000050</Address><Length>4</Length>
    <AccessMode>RO</AccessMode><pPort>Device</pPort>
    <Endianess>BigEndian</Endianess>
  </IntReg>
</RegisterDescription>
"#
        .to_vec();
        let mut url = format!("local:fake.xml;{XML_BASE:x};{:x}", xml.len()).into_bytes();
        url.resize(512, 0);
        Self { xml, url }
    }
}

impl DeviceHandler for FakeDevice {
    fn discovery_info(&self) -> DiscoveryInfo {
        DiscoveryInfo {
            spec_version_major: 2,
            spec_version_minor: 2,
            device_mode: 0,
            mac: MAC,
            ip_config_options: 0,
            ip_config_current: 0,
            current_ip: u32::from(Ipv4Addr::LOCALHOST),
            current_subnet_mask: u32::from(Ipv4Addr::new(255, 0, 0, 0)),
            default_gateway: 0,
            manufacturer_name: "GenTLite".into(),
            model_name: "Fake".into(),
            device_version: "1.0.0".into(),
            manufacturer_info: String::new(),
            serial_number: "F001".into(),
            user_defined_name: String::new(),
        }
    }

    fn read_reg(&self, addr: u32) -> Result<u32, u16> {
        match addr {
            reg::VERSION => Ok(0x0002_0002),
            reg::STREAMING_CAPABILITY => Ok(0),
            reg::HEARTBEAT_TIMEOUT => Ok(3000),
            reg::GVCP_CAPABILITY => Ok(0),
            reg::TIMESTAMP_TICK_FREQ_HIGH => Ok(0),
            reg::TIMESTAMP_TICK_FREQ_LOW => Ok(125_000_000),
            reg::SCP0 => Ok(0),
            PAYLOAD_ADDR => Ok(PAYLOAD),
            _ => Err(GEV_STATUS_INVALID_ADDRESS),
        }
    }

    fn write_reg(&self, _addr: u32, _value: u32) -> Result<(), u16> {
        Ok(())
    }

    fn read_mem(&self, addr: u32, count: u16) -> Result<Vec<u8>, u16> {
        let count = count as usize;
        if addr == reg::FIRST_URL && count == 512 {
            return Ok(self.url.clone());
        }
        if addr >= XML_BASE && addr < XML_BASE + self.xml.len() as u32 {
            let start = (addr - XML_BASE) as usize;
            let take = (self.xml.len() - start).min(count);
            let mut out = self.xml[start..start + take].to_vec();
            out.resize(count, 0);
            return Ok(out);
        }
        if addr == PAYLOAD_ADDR && count == 4 {
            return Ok(PAYLOAD.to_be_bytes().to_vec());
        }
        Err(GEV_STATUS_INVALID_ADDRESS)
    }

    fn write_mem(&self, _addr: u32, _data: &[u8]) -> Result<(), u16> {
        Err(GEV_STATUS_INVALID_ADDRESS)
    }

    fn write_reg_from(&self, _peer: SocketAddr, addr: u32, value: u32) -> Result<(), u16> {
        self.write_reg(addr, value)
    }
}

fn get_string(mut f: impl FnMut(*mut c_char, *mut usize) -> GCError) -> String {
    let mut buf = [0u8; 1024];
    let mut size = buf.len();
    let r = f(buf.as_mut_ptr() as *mut c_char, &mut size);
    assert_eq!(r, GC_ERR_SUCCESS);
    String::from_utf8_lossy(&buf[..size.saturating_sub(1)]).into_owned()
}

#[test]
fn payload_size_is_known_before_start() {
    let server = GvcpServer::start(0, FakeDevice::new()).expect("fake device");
    let port = server.local_port().expect("server port");
    std::env::set_var("GENTLITE_GVCP_PORT", port.to_string());
    std::env::set_var("GENTLITE_DISCOVERY_TARGETS", format!("127.0.0.1:{port}"));

    assert_eq!(GCInitLib(), GC_ERR_SUCCESS);
    let mut tl: TLHandle = ptr::null_mut();
    assert_eq!(TLOpen(&mut tl), GC_ERR_SUCCESS);
    let mut interfaces = 0u32;
    assert_eq!(TLGetNumInterfaces(tl, &mut interfaces), GC_ERR_SUCCESS);
    assert!(interfaces >= 1, "test needs at least one network adapter");

    let iface_id = get_string(|s, n| TLGetInterfaceID(tl, 0, s, n));
    let c_iface = std::ffi::CString::new(iface_id).unwrap();
    let mut iface: IFHandle = ptr::null_mut();
    assert_eq!(
        TLOpenInterface(tl, c_iface.as_ptr(), &mut iface),
        GC_ERR_SUCCESS
    );
    let mut changed = 0u8;
    assert_eq!(IFUpdateDeviceList(iface, &mut changed, 200), GC_ERR_SUCCESS);
    let c_dev = std::ffi::CString::new(DEVICE_ID).unwrap();
    let mut dev: DevHandle = ptr::null_mut();
    assert_eq!(
        IFOpenDevice(iface, c_dev.as_ptr(), 4 /* exclusive */, &mut dev),
        GC_ERR_SUCCESS
    );

    let stream_id = get_string(|s, n| DevGetDataStreamID(dev, 0, s, n));
    let c_sid = std::ffi::CString::new(stream_id).unwrap();
    let mut ds: DSHandle = ptr::null_mut();
    assert_eq!(
        DevOpenDataStream(dev, c_sid.as_ptr(), &mut ds),
        GC_ERR_SUCCESS
    );

    // STREAM_INFO_PAYLOAD_SIZE = 7, INFO_DATATYPE_SIZET = 12.
    let mut dtype: InfoDataType = 0;
    let mut payload = 0usize;
    let mut size = std::mem::size_of::<usize>();
    assert_eq!(
        DSGetInfo(
            ds,
            7,
            &mut dtype,
            (&mut payload as *mut usize).cast::<c_void>(),
            &mut size
        ),
        GC_ERR_SUCCESS
    );
    assert_eq!(dtype, 12);
    assert_eq!(payload, PAYLOAD as usize);

    assert_eq!(DSClose(ds), GC_ERR_SUCCESS);
    assert_eq!(DevClose(dev), GC_ERR_SUCCESS);
    assert_eq!(IFClose(iface), GC_ERR_SUCCESS);
    assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
    server.stop();
}
