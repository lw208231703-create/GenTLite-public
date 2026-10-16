//! End-to-end control-plane test: `gentl_simulator::SimCamera` behind the
//! real producer entry points, over actual GVCP/UDP on loopback.
//!
//! Loopback is not enumerated by GetAdaptersAddresses, so the producer's
//! documented test hooks aim it at the sim: `GENTLITE_GVCP_PORT` (device
//! connect port) and `GENTLITE_DISCOVERY_TARGETS` (extra unicast discovery
//! targets, attached without subnet filtering).

use std::ffi::{c_char, c_void};
use std::net::Ipv4Addr;
use std::ptr;

use gentl_core::types::*;
use gentl_gige::*;
use gentl_simulator::{SimCamera, SimConfig, StreamingProtocol, REG_EXPOSURE_TIME};

const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
const DEVICE_ID: &str = "00:11:22:33:44:55";

fn read_port(h: PortHandle, addr: u64, buf: &mut [u8]) -> GCError {
    let mut size = buf.len();
    GCReadPort(h, addr, buf.as_mut_ptr().cast(), &mut size)
}

fn write_port(h: PortHandle, addr: u64, data: &[u8]) -> GCError {
    let mut size = data.len();
    GCWritePort(h, addr, data.as_ptr().cast(), &mut size)
}

fn get_string(mut f: impl FnMut(*mut c_char, *mut usize) -> GCError) -> (GCError, String) {
    let mut buf = [0u8; 1024];
    let mut size = buf.len();
    let r = f(buf.as_mut_ptr() as *mut c_char, &mut size);
    let s = if r == GC_ERR_SUCCESS {
        String::from_utf8_lossy(&buf[..size.saturating_sub(1)]).into_owned()
    } else {
        String::new()
    };
    (r, s)
}

#[test]
fn control_plane_end_to_end() {
    let cam = SimCamera::new(SimConfig {
        mac: MAC,
        ip: Ipv4Addr::LOCALHOST,
        subnet_mask: Ipv4Addr::new(255, 0, 0, 0),
        gateway: Ipv4Addr::UNSPECIFIED,
        manufacturer_name: "GenTLite".into(),
        model_name: "SimCam".into(),
        device_version: "1.0.0".into(),
        serial_number: "SIM-0001".into(),
        streaming: StreamingProtocol::GvspOnly,
    });
    let server = cam.start(0).expect("start sim camera");
    let port = server.local_port().expect("server port");

    // Producer-side hooks: connect + discovery target for the loopback cam.
    std::env::set_var("GENTLITE_GVCP_PORT", port.to_string());
    std::env::set_var("GENTLITE_DISCOVERY_TARGETS", format!("127.0.0.1:{port}"));

    {
        assert_eq!(GCInitLib(), GC_ERR_SUCCESS);

        let mut tl: TLHandle = ptr::null_mut();
        assert_eq!(TLOpen(&mut tl), GC_ERR_SUCCESS, "TLOpen");
        let mut num_ifaces = 0u32;
        assert_eq!(TLGetNumInterfaces(tl, &mut num_ifaces), GC_ERR_SUCCESS);
        if num_ifaces == 0 {
            eprintln!("no network adapters enumerated; skipping interface-level checks");
            assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
            server.stop();
            return;
        }

        let (r, iface_id) = get_string(|s, n| TLGetInterfaceID(tl, 0, s, n));
        assert_eq!(r, GC_ERR_SUCCESS, "TLGetInterfaceID");
        assert_eq!(
            iface_id,
            "GenTLite GEV GenTL Interface Module",
            "Basler-style aggregate interface"
        );

        let mut iface: IFHandle = ptr::null_mut();
        let c_id = std::ffi::CString::new(iface_id.clone()).unwrap();
        assert_eq!(TLOpenInterface(tl, c_id.as_ptr(), &mut iface), GC_ERR_SUCCESS, "TLOpenInterface");

        // Opening the same interface twice -> RESOURCE_IN_USE.
        let mut iface2: IFHandle = ptr::null_mut();
        assert_eq!(
            TLOpenInterface(tl, c_id.as_ptr(), &mut iface2),
            GC_ERR_RESOURCE_IN_USE,
            "second TLOpenInterface"
        );

        let mut changed = 0u8;
        assert_eq!(
            IFUpdateDeviceList(iface, &mut changed, 200),
            GC_ERR_SUCCESS,
            "IFUpdateDeviceList"
        );

        let mut num_devs = 0u32;
        assert_eq!(IFGetNumDevices(iface, &mut num_devs), GC_ERR_SUCCESS);
        assert!(num_devs >= 1, "discovery via GENTLITE_DISCOVERY_TARGETS found no device");

        let (r, dev_id) = get_string(|s, n| IFGetDeviceID(iface, 0, s, n));
        assert_eq!(r, GC_ERR_SUCCESS, "IFGetDeviceID");
        assert_eq!(dev_id, DEVICE_ID, "device id is the MAC string (Q4)");

        let (r, vendor) = get_string(|s, n| {
            let c = std::ffi::CString::new(DEVICE_ID).unwrap();
            let mut dtype: InfoDataType = 0;
            IFGetDeviceInfo(iface, c.as_ptr(), 1, &mut dtype, s as *mut c_void, n)
        });
        assert_eq!(r, GC_ERR_SUCCESS, "IFGetDeviceInfo VENDOR");
        assert_eq!(vendor, "GenTLite");

        // Open the device exclusively.
        let c_dev = std::ffi::CString::new(DEVICE_ID).unwrap();
        let mut dev: DevHandle = ptr::null_mut();
        assert_eq!(
            IFOpenDevice(iface, c_dev.as_ptr(), 4 /* DEVICE_ACCESS_EXCLUSIVE */, &mut dev),
            GC_ERR_SUCCESS,
            "IFOpenDevice"
        );
        assert!(!dev.is_null());

        // Opening the same device twice -> RESOURCE_IN_USE.
        let mut dev2: DevHandle = ptr::null_mut();
        assert_eq!(
            IFOpenDevice(iface, c_dev.as_ptr(), 4, &mut dev2),
            GC_ERR_RESOURCE_IN_USE,
            "second IFOpenDevice"
        );

        let mut port_h: PortHandle = ptr::null_mut();
        assert_eq!(DevGetPort(dev, &mut port_h), GC_ERR_SUCCESS, "DevGetPort");

        // Version register: 0x00020002 in big-endian register order.
        let mut v = [0u8; 4];
        assert_eq!(read_port(port_h, 0x0000, &mut v), GC_ERR_SUCCESS, "GCReadPort VERSION");
        assert_eq!(v, [0x00, 0x02, 0x00, 0x02], "version 0x00020002 BE");

        // Exposure register (f32 per the sim's GenApi map) write/read round-trip.
        let exposure = 33_000.0f32;
        assert_eq!(
            write_port(port_h, u64::from(REG_EXPOSURE_TIME), &exposure.to_bits().to_be_bytes()),
            GC_ERR_SUCCESS,
            "GCWritePort exposure"
        );
        let mut v = [0u8; 4];
        assert_eq!(read_port(port_h, u64::from(REG_EXPOSURE_TIME), &mut v), GC_ERR_SUCCESS);
        assert_eq!(f32::from_bits(u32::from_be_bytes(v)), exposure);

        // Port info: big-endian register space, "Device" module string.
        let mut dtype: InfoDataType = 0;
        let mut big = 0u8;
        let mut size = 1usize;
        assert_eq!(
            GCGetPortInfo(port_h, 6, &mut dtype, (&mut big as *mut u8).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert_eq!(dtype, 11 /* INFO_DATATYPE_BOOL8 */);
        assert_eq!(big, 1);
        let (r, module) = get_string(|s, n| {
            GCGetPortInfo(port_h, 4, &mut dtype, s as *mut c_void, n)
        });
        assert_eq!(r, GC_ERR_SUCCESS);
        assert_eq!(module, "Device");

        // XML URL from the bootstrap register, then read the XML header
        // straight from the register map at the URL address.
        let (r, url) = get_string(|s, n| GCGetPortURL(port_h, s, n));
        assert_eq!(r, GC_ERR_SUCCESS, "GCGetPortURL");
        assert!(url.to_lowercase().starts_with("local:"), "local URL: {url}");
        let xml_addr = u32::from_str_radix(url.split(';').nth(1).unwrap(), 16).unwrap();

        let mut head = [0u8; 5];
        assert_eq!(read_port(port_h, u64::from(xml_addr), &mut head), GC_ERR_SUCCESS);
        assert_eq!(&head, b"<?xml");

        // Unaligned read exercises the alignment adaptation (§6.3.6.5).
        let mut unaligned = [0u8; 4];
        assert_eq!(read_port(port_h, u64::from(xml_addr) + 1, &mut unaligned), GC_ERR_SUCCESS);
        assert_eq!(&unaligned, b"?xml");

        // Timestamp frequency via DevGetInfo (GVCP read on the channel).
        let mut freq: u64 = 0;
        let mut size = std::mem::size_of::<u64>();
        assert_eq!(
            DevGetInfo(dev, 9, &mut dtype, (&mut freq as *mut u64).cast(), &mut size),
            GC_ERR_SUCCESS,
            "DevGetInfo TIMESTAMP_FREQUENCY"
        );
        assert_eq!(dtype, 8 /* INFO_DATATYPE_UINT64 */);
        assert_eq!(freq, 1_000_000);

        // Close the device: the port handle becomes implicitly invalid.
        assert_eq!(DevClose(dev), GC_ERR_SUCCESS, "DevClose");
        let mut v = [0u8; 4];
        assert_eq!(
            read_port(port_h, 0x0000, &mut v),
            GC_ERR_INVALID_HANDLE,
            "port invalid after DevClose"
        );
        let mut err_code: GCError = 0;
        let mut needed = 0usize;
        assert_eq!(GCGetLastError(&mut err_code, ptr::null_mut(), &mut needed), GC_ERR_SUCCESS);
        assert_eq!(err_code, GC_ERR_INVALID_HANDLE);
        assert!(needed > 1);

        assert_eq!(IFClose(iface), GC_ERR_SUCCESS, "IFClose");
        assert_eq!(TLClose(tl), GC_ERR_SUCCESS, "TLClose");
    }

    server.stop();
}
