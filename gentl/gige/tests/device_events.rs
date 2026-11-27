//! GigE Vision message-channel device events end-to-end (GenTL 1.6
//! `EVENT_REMOTE_DEVICE`; GigE Vision 3.0 §12/§17):
//!
//! - the producer opens the message channel (MCDA then MCP, §12.2) when the
//!   device reports one (§36.22);
//! - EVENT_CMD / EVENTDATA_CMD datagrams (§17.1/§17.2) are delivered through
//!   EventGetData in the wire record layout;
//! - EventGetDataInfo (§6.3.7.3) exposes the event id and EVENTDATA payload.

#![allow(non_snake_case)]

use std::ffi::{c_char, c_void};
use std::net::Ipv4Addr;
use std::ptr;

use gentl_core::types::*;
use gentl_gige::*;
use gentl_gvcp::wire::GvcpEvent;
use gentl_simulator::{
    SimCamera, SimConfig, StreamingProtocol, REG_TEST_EVENT_DATA_LEN, REG_TEST_EVENT_ID,
    REG_TEST_EVENT_TRIGGER,
};

const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
const DEVICE_ID: &str = "00:11:22:33:44:55";
const EVENT_REMOTE_DEVICE: EventType = 4;
const EVENT_EVENT_TYPE: EventInfoCmd = 0;
const EVENT_SIZE_MAX: EventInfoCmd = 3;
const EVENT_DATA_ID: EventDataInfoCmd = 0;
const EVENT_DATA_VALUE: EventDataInfoCmd = 1;
const EVENT_DATA_NUMID: EventDataInfoCmd = 2;

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

fn write_reg(h: PortHandle, addr: u32, value: u32) {
    let buf = value.to_be_bytes();
    let mut size = buf.len();
    assert_eq!(
        GCWritePort(h, u64::from(addr), buf.as_ptr().cast(), &mut size),
        GC_ERR_SUCCESS,
        "GCWritePort 0x{addr:04X}"
    );
}

#[test]
fn gige_device_events_flow_over_the_message_channel() {
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
    let port_num = server.local_port().expect("server port");
    std::env::set_var("GENTLITE_GVCP_PORT", port_num.to_string());
    std::env::set_var(
        "GENTLITE_DISCOVERY_TARGETS",
        format!("127.0.0.1:{port_num}"),
    );

    assert_eq!(GCInitLib(), GC_ERR_SUCCESS);
    let mut tl: TLHandle = ptr::null_mut();
    assert_eq!(TLOpen(&mut tl), GC_ERR_SUCCESS);
    let (r, iface_id) = get_string(|s, n| TLGetInterfaceID(tl, 0, s, n));
    assert_eq!(r, GC_ERR_SUCCESS);
    let mut iface: IFHandle = ptr::null_mut();
    let c_iface = std::ffi::CString::new(iface_id).unwrap();
    assert_eq!(
        TLOpenInterface(tl, c_iface.as_ptr(), &mut iface),
        GC_ERR_SUCCESS
    );
    let mut changed = 0u8;
    assert_eq!(IFUpdateDeviceList(iface, &mut changed, 200), GC_ERR_SUCCESS);
    let mut dev: DevHandle = ptr::null_mut();
    let c_dev = std::ffi::CString::new(DEVICE_ID).unwrap();
    assert_eq!(
        IFOpenDevice(iface, c_dev.as_ptr(), 4 /* exclusive */, &mut dev),
        GC_ERR_SUCCESS
    );
    let mut port: PortHandle = ptr::null_mut();
    assert_eq!(DevGetPort(dev, &mut port), GC_ERR_SUCCESS);

    // -- Register the device event module (opens the message channel) ------
    let mut ev: EventHandle = ptr::null_mut();
    assert_eq!(
        GCRegisterEvent(dev, EVENT_REMOTE_DEVICE, &mut ev),
        GC_ERR_SUCCESS,
        "the sim advertises one message channel"
    );
    assert!(!ev.is_null());
    let mut ev2: EventHandle = ptr::null_mut();
    assert_eq!(
        GCRegisterEvent(dev, EVENT_REMOTE_DEVICE, &mut ev2),
        GC_ERR_RESOURCE_IN_USE,
        "one module per event type (§6.3.7.1)"
    );
    let mut dtype: InfoDataType = 0;
    let mut event_type = -1i32;
    let mut size = std::mem::size_of::<i32>();
    assert_eq!(
        EventGetInfo(
            ev,
            EVENT_EVENT_TYPE,
            &mut dtype,
            (&mut event_type as *mut i32).cast(),
            &mut size
        ),
        GC_ERR_SUCCESS
    );
    assert_eq!(event_type, EVENT_REMOTE_DEVICE);
    let mut size_max = 0usize;
    let mut size = std::mem::size_of::<usize>();
    assert_eq!(
        EventGetInfo(
            ev,
            EVENT_SIZE_MAX,
            &mut dtype,
            (&mut size_max as *mut usize).cast(),
            &mut size
        ),
        GC_ERR_SUCCESS
    );
    assert!(size_max >= 24, "at least one wire record fits");

    // -- EVENT_CMD (no payload) --------------------------------------------
    write_reg(port, REG_TEST_EVENT_ID, 7);
    write_reg(port, REG_TEST_EVENT_DATA_LEN, 0);
    write_reg(port, REG_TEST_EVENT_TRIGGER, 1);
    let mut raw = [0u8; 576];
    let mut n = raw.len();
    assert_eq!(
        EventGetData(ev, raw.as_mut_ptr().cast(), &mut n, 2_000),
        GC_ERR_SUCCESS,
        "EVENT_CMD within the timeout"
    );
    assert_eq!(n, 24, "one 24-byte event record (§17.1.1)");
    let event = GvcpEvent::decode(&raw[..n]).expect("event record decodes");
    assert_eq!(event.event_id, 7);
    assert_eq!(event.stream_channel, 0xFFFF, "no stream channel involved");
    assert!(event.timestamp > 0, "sim clock timestamp");
    assert!(event.data.is_empty());

    let mut numid = 0u64;
    let mut dtype: InfoDataType = 0;
    let mut size = std::mem::size_of::<u64>();
    assert_eq!(
        EventGetDataInfo(
            ev,
            raw.as_ptr().cast(),
            n,
            EVENT_DATA_NUMID,
            &mut dtype,
            (&mut numid as *mut u64).cast(),
            &mut size
        ),
        GC_ERR_SUCCESS
    );
    assert_eq!(numid, 7, "EVENT_DATA_NUMID");
    let (r, id_text) = get_string(|s, size| {
        EventGetDataInfo(ev, raw.as_ptr().cast(), n, EVENT_DATA_ID, &mut dtype, s as *mut c_void, size)
    });
    assert_eq!(r, GC_ERR_SUCCESS);
    assert_eq!(
        id_text, "07",
        "EVENT_DATA_ID: hex with an even number of digits (GenTL 1.6 §6.4.6.1)"
    );

    // -- EVENTDATA_CMD with a payload --------------------------------------
    write_reg(port, REG_TEST_EVENT_ID, 9);
    write_reg(port, REG_TEST_EVENT_DATA_LEN, 4);
    write_reg(port, REG_TEST_EVENT_TRIGGER, 1);
    let mut raw = [0u8; 576];
    let mut n = raw.len();
    assert_eq!(
        EventGetData(ev, raw.as_mut_ptr().cast(), &mut n, 2_000),
        GC_ERR_SUCCESS,
        "EVENTDATA_CMD within the timeout"
    );
    assert_eq!(n, 28, "24-byte record + 4 payload bytes (§17.2.1)");
    let event = GvcpEvent::decode(&raw[..n]).expect("event decodes");
    assert_eq!(event.event_id, 9);
    assert_eq!(event.data, vec![0xE0, 0xE1, 0xE2, 0xE3]);
    let mut payload = [0u8; 64];
    let mut payload_len = payload.len();
    let mut dtype: InfoDataType = 0;
    assert_eq!(
        EventGetDataInfo(
            ev,
            raw.as_ptr().cast(),
            n,
            EVENT_DATA_VALUE,
            &mut dtype,
            payload.as_mut_ptr().cast(),
            &mut payload_len
        ),
        GC_ERR_SUCCESS
    );
    assert_eq!(dtype, 13, "INFO_DATATYPE_BUFFER");
    // The value buffer is the remote event port from offset 0: the 24-byte
    // standardized header plus the payload (GigE EVENTDATA without GVCP).
    assert_eq!(payload_len, 28, "24-byte header + 4 data bytes");
    assert_eq!(&payload[..payload_len], &raw[..n]);

    // -- Flush / Kill work on a device-event handle (§6.3.7.4/6.3.7.5) ----
    assert_eq!(EventFlush(ev), GC_ERR_SUCCESS);
    assert_eq!(EventKill(ev), GC_ERR_SUCCESS);
    let mut n = raw.len();
    assert_eq!(
        EventGetData(ev, raw.as_mut_ptr().cast(), &mut n, 100),
        GC_ERR_ABORT,
        "EventKill terminates the next wait with GC_ERR_ABORT"
    );

    // -- Unregister closes the channel -------------------------------------
    assert_eq!(GCUnregisterEvent(dev, EVENT_REMOTE_DEVICE), GC_ERR_SUCCESS);
    let mut n = raw.len();
    // §6.3.7.7: unregistering terminates pending waits with GC_ERR_ABORT and
    // silently discards pending events.
    assert_eq!(
        EventGetData(ev, raw.as_mut_ptr().cast(), &mut n, 100),
        GC_ERR_INVALID_HANDLE,
        "a fresh call on an unregistered event handle is INVALID_HANDLE"
    );

    // -- Concurrent registration: exactly one caller wins ------------------
    // GenTL 6.3.7.1: one registration per module/event type. The check and
    // insert run under one lifecycle lock, so N racing registrations must
    // yield exactly one success and N-1 GC_ERR_RESOURCE_IN_USE (no duplicate
    // handles, no overwritten module entry).
    let dev_addr = dev as usize;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let tx = tx.clone();
            scope.spawn(move || {
                let dev = dev_addr as DevHandle;
                let mut h: EventHandle = ptr::null_mut();
                let r = GCRegisterEvent(dev, EVENT_REMOTE_DEVICE, &mut h);
                tx.send((r, h as usize)).unwrap();
            });
        }
        drop(tx);
    });
    let results: Vec<(GCError, usize)> = rx.iter().collect();
    assert_eq!(results.len(), 8);
    assert_eq!(
        results.iter().filter(|(r, _)| *r == GC_ERR_SUCCESS).count(),
        1,
        "exactly one concurrent registration succeeds"
    );
    assert_eq!(
        results
            .iter()
            .filter(|(r, _)| *r == GC_ERR_RESOURCE_IN_USE)
            .count(),
        7,
        "the losing registrations answer GC_ERR_RESOURCE_IN_USE"
    );
    let winner = results
        .iter()
        .find(|(r, _)| *r == GC_ERR_SUCCESS)
        .map(|(_, h)| *h)
        .unwrap();
    assert_ne!(winner, 0, "the winner got a usable event handle");
    assert_eq!(
        GCUnregisterEvent(dev, EVENT_REMOTE_DEVICE),
        GC_ERR_SUCCESS,
        "the concurrent registration can be unregistered"
    );

    assert_eq!(DevClose(dev), GC_ERR_SUCCESS);
    assert_eq!(IFClose(iface), GC_ERR_SUCCESS);
    assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
    assert_eq!(GCCloseLib(), GC_ERR_SUCCESS);
}
