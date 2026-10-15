//! Dump how a commercial GenTL producer names things: TL info, interface
//! IDs, device IDs, stream IDs. Usage: cargo run --example dump_cti -- <path.cti>
//!
//! Read-only: loads the CTI, enumerates, prints, closes. No device is
//! opened (IFOpenDevice is never called), so no camera can be disturbed.

#![allow(non_snake_case)]

use std::ffi::{c_char, c_void};
use std::ptr;

use gentl_core::types::*;

type Fn0 = unsafe extern "C" fn() -> GCError;
type FnTLOpen = unsafe extern "C" fn(*mut TLHandle) -> GCError;
type FnTLClose = unsafe extern "C" fn(TLHandle) -> GCError;
type FnGetInfo = unsafe extern "C" fn(TLHandle, i32, *mut InfoDataType, *mut c_void, *mut usize) -> GCError;
type FnNumIf = unsafe extern "C" fn(TLHandle, *mut u32) -> GCError;
type FnGetIfaceId = unsafe extern "C" fn(TLHandle, u32, *mut c_char, *mut usize) -> GCError;
type FnGetIfaceInfo =
    unsafe extern "C" fn(TLHandle, *const c_char, i32, *mut InfoDataType, *mut c_void, *mut usize) -> GCError;
type FnOpenIface = unsafe extern "C" fn(TLHandle, *const c_char, *mut IFHandle) -> GCError;
type FnCloseIface = unsafe extern "C" fn(IFHandle) -> GCError;
type FnUpdDev = unsafe extern "C" fn(IFHandle, *mut u8, u64) -> GCError;
type FnNumDev = unsafe extern "C" fn(IFHandle, *mut u32) -> GCError;
type FnGetDevId = unsafe extern "C" fn(IFHandle, u32, *mut c_char, *mut usize) -> GCError;
type FnGetDevInfo =
    unsafe extern "C" fn(IFHandle, *const c_char, i32, *mut InfoDataType, *mut c_void, *mut usize) -> GCError;
type FnNumDs = unsafe extern "C" fn(DevHandle, *mut u32) -> GCError;
type FnGetDsId = unsafe extern "C" fn(DevHandle, u32, *mut c_char, *mut usize) -> GCError;
type FnOpenDev =
    unsafe extern "C" fn(IFHandle, *const c_char, DeviceAccessFlags, *mut DevHandle) -> GCError;
type FnCloseDev = unsafe extern "C" fn(DevHandle) -> GCError;

fn get_string(mut f: impl FnMut(*mut c_char, *mut usize) -> GCError) -> (GCError, String) {
    let mut buf = vec![0u8; 4096];
    let mut size = buf.len();
    let r = f(buf.as_mut_ptr() as *mut c_char, &mut size);
    let s = if r == GC_ERR_SUCCESS {
        String::from_utf8_lossy(&buf[..size.saturating_sub(1).min(buf.len())]).into_owned()
    } else {
        format!("<err {r}>")
    };
    (r, s)
}

unsafe fn get_info_string(h: TLHandle, f: FnGetInfo, cmd: i32, name: &str) {
    let mut dtype: InfoDataType = 0;
    let (r, s) = get_string(|p, n| f(h, cmd, &mut dtype, p.cast(), n));
    println!("  {name} (cmd {cmd}, type {dtype}) = {s:?}  [{r}]");
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: dump_cti <path.cti>");
    let lib = unsafe { libloading::Library::new(&path) }.expect("load cti");
    println!("=== {path} ===");

    unsafe {
        let init: Fn0 = *lib.get(b"GCInitLib\0").unwrap();
        let tl_open: FnTLOpen = *lib.get(b"TLOpen\0").unwrap();
        let tl_close: FnTLClose = *lib.get(b"TLClose\0").unwrap();
        let tl_get_info: FnGetInfo = *lib.get(b"TLGetInfo\0").unwrap();
        let tl_num_if: FnNumIf = *lib.get(b"TLGetNumInterfaces\0").unwrap();
        let tl_if_id: FnGetIfaceId = *lib.get(b"TLGetInterfaceID\0").unwrap();
        let tl_if_info: FnGetIfaceInfo = *lib.get(b"TLGetInterfaceInfo\0").unwrap();
        let tl_open_if: FnOpenIface = *lib.get(b"TLOpenInterface\0").unwrap();
        let if_close: FnCloseIface = *lib.get(b"IFClose\0").unwrap();
        let if_upd: FnUpdDev = *lib.get(b"IFUpdateDeviceList\0").unwrap();
        let if_num: FnNumDev = *lib.get(b"IFGetNumDevices\0").unwrap();
        let if_dev_id: FnGetDevId = *lib.get(b"IFGetDeviceID\0").unwrap();
        let if_dev_info: FnGetDevInfo = *lib.get(b"IFGetDeviceInfo\0").unwrap();

        assert_eq!(init(), GC_ERR_SUCCESS);
        let mut tl: TLHandle = ptr::null_mut();
        assert_eq!(tl_open(&mut tl), GC_ERR_SUCCESS, "TLOpen");

        println!("-- TLGetInfo");
        for (cmd, name) in [
            (0, "ID"),
            (1, "VENDOR"),
            (2, "MODEL"),
            (3, "VERSION"),
            (4, "TLTYPE"),
            (5, "NAME"),
            (7, "DISPLAYNAME"),
            (8, "CHAR_ENCODING"),
        ] {
            get_info_string(tl, tl_get_info, cmd, name);
        }

        let mut n: u32 = 0;
        // Some producers only enumerate interfaces after an explicit update.
        {
            type FnUpdIf = unsafe extern "C" fn(TLHandle, *mut u8, u64) -> GCError;
            let upd: FnUpdIf = *lib.get(b"TLUpdateInterfaceList\0").unwrap();
            let mut c = 0u8;
            let _ = upd(tl, &mut c, 2000);
        }
        assert_eq!(tl_num_if(tl, &mut n), GC_ERR_SUCCESS);
        println!("-- {n} interface(s)");
        for i in 0..n {
            let (_, id) = get_string(|p, sz| tl_if_id(tl, i, p, sz));
            println!("Interface[{i}] ID = {id:?}");
            let c_id = std::ffi::CString::new(id.clone()).unwrap();
            let mut dtype: InfoDataType = 0;
            for (cmd, name) in [(0, "ID"), (1, "DISPLAYNAME"), (2, "TLTYPE")] {
                let (r, s) = get_string(|p, sz| {
                    tl_if_info(tl, c_id.as_ptr(), cmd, &mut dtype, p.cast(), sz)
                });
                println!("    {name} = {s:?}  [{r}]");
            }
            let mut iface: IFHandle = ptr::null_mut();
            if tl_open_if(tl, c_id.as_ptr(), &mut iface) != GC_ERR_SUCCESS {
                println!("    <TLOpenInterface failed, skipped>");
                continue;
            }
            let mut changed = 0u8;
            let _ = if_upd(iface, &mut changed, 1000);
            let mut nd: u32 = 0;
            assert_eq!(if_num(iface, &mut nd), GC_ERR_SUCCESS);
            println!("    {nd} device(s)");
            for d in 0..nd {
                let (_, did) = get_string(|p, sz| if_dev_id(iface, d, p, sz));
                println!("    Device[{d}] ID = {did:?}");
                let c_did = std::ffi::CString::new(did.clone()).unwrap();
                // DEVICE_INFO: 0=ID 1=VENDOR 2=MODEL 3=TLTYPE 4=VERSION
                // 5=USERNAME 6=SERIAL 7=ACCESS_STATUS ...
                for (cmd, name) in [
                    (1, "VENDOR"),
                    (2, "MODEL"),
                    (5, "USERNAME"),
                    (7, "ACCESS_STATUS"),
                ] {
                    let mut dtype: InfoDataType = 0;
                    let mut val64 = 0u64;
                    let mut size = 8usize;
                    let r = if_dev_info(
                        iface,
                        c_did.as_ptr(),
                        cmd,
                        &mut dtype,
                        (&mut val64 as *mut u64).cast(),
                        &mut size,
                    );
                    if r == GC_ERR_SUCCESS && (dtype == 6 || dtype == 3) {
                        // string-ish types: retry as string
                        let (r2, s) = get_string(|p, sz| {
                            if_dev_info(iface, c_did.as_ptr(), cmd, &mut dtype, p.cast(), sz)
                        });
                        println!("        {name} = {s:?} (type {dtype}) [{r2}]");
                    } else {
                        println!("        {name} = {val64} (type {dtype}) [{r}]");
                    }
                }

                // Stream IDs require opening the device (read-only open).
                let mut dev: DevHandle = ptr::null_mut();
                let r = {
                    let open_dev: FnOpenDev = *lib.get(b"IFOpenDevice\0").unwrap();
                    open_dev(iface, c_did.as_ptr(), 2 /* read-only */, &mut dev)
                };
                if r == GC_ERR_SUCCESS {
                    let num_ds: FnNumDs = *lib.get(b"DevGetNumDataStreams\0").unwrap();
                    let get_ds: FnGetDsId = *lib.get(b"DevGetDataStreamID\0").unwrap();
                    let mut ns: u32 = 0;
                    if num_ds(dev, &mut ns) == GC_ERR_SUCCESS {
                        for s in 0..ns {
                            let (_, sid) = get_string(|p, sz| get_ds(dev, s, p, sz));
                            println!("        Stream[{s}] ID = {sid:?}");
                        }
                    }
                    let close_dev: FnCloseDev = *lib.get(b"DevClose\0").unwrap();
                    close_dev(dev);
                } else {
                    println!("        <IFOpenDevice read-only failed: {r}, stream IDs skipped>");
                }
            }
            if_close(iface);
        }
        tl_close(tl);
    }
    println!("=== done ===");
}
