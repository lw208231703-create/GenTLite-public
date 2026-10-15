//! GenTL 1.6 ABI types (mirror of include/GenTL.h).
//! Keep in sync with the header; only what the implementation touches.

#![allow(non_camel_case_types, dead_code)]

use std::os::raw::c_void;

pub type GCError = i32;

pub const GC_ERR_SUCCESS: GCError = 0;
pub const GC_ERR_ERROR: GCError = -1001;
pub const GC_ERR_NOT_INITIALIZED: GCError = -1002;
pub const GC_ERR_NOT_IMPLEMENTED: GCError = -1003;
pub const GC_ERR_RESOURCE_IN_USE: GCError = -1004;
pub const GC_ERR_ACCESS_DENIED: GCError = -1005;
pub const GC_ERR_INVALID_HANDLE: GCError = -1006;
pub const GC_ERR_INVALID_ID: GCError = -1007;
pub const GC_ERR_NO_DATA: GCError = -1008;
pub const GC_ERR_INVALID_PARAMETER: GCError = -1009;
pub const GC_ERR_IO: GCError = -1010;
pub const GC_ERR_TIMEOUT: GCError = -1011;
pub const GC_ERR_ABORT: GCError = -1012;
pub const GC_ERR_INVALID_BUFFER: GCError = -1013;
pub const GC_ERR_NOT_AVAILABLE: GCError = -1014;
pub const GC_ERR_INVALID_ADDRESS: GCError = -1015;
pub const GC_ERR_BUFFER_TOO_SMALL: GCError = -1016;
pub const GC_ERR_INVALID_INDEX: GCError = -1017;
pub const GC_ERR_PARSING_CHUNK_DATA: GCError = -1018;
pub const GC_ERR_INVALID_VALUE: GCError = -1019;
pub const GC_ERR_RESOURCE_EXHAUSTED: GCError = -1020;
pub const GC_ERR_OUT_OF_MEMORY: GCError = -1021;
pub const GC_ERR_BUSY: GCError = -1022;
pub const GC_ERR_AMBIGUOUS: GCError = -1023;

pub type InfoDataType = i32;
pub type TlInfoCmd = i32;
pub type PortInfoCmd = i32;
pub type InterfaceInfoCmd = i32;
pub type DeviceInfoCmd = i32;
pub type StreamInfoCmd = i32;
pub type BufferInfoCmd = i32;
pub type BufferPartInfoCmd = i32;
pub type FlowInfoCmd = i32;
pub type SegmentInfoCmd = i32;
pub type UrlInfoCmd = i32;
pub type EventInfoCmd = i32;
pub type EventDataInfoCmd = i32;
pub type EventType = i32;
pub type DeviceAccessFlags = u32;
pub type AcqStartFlags = u32;
pub type AcqStopFlags = u32;
pub type AcqQueueType = i32;

pub type TLHandle = *mut c_void;
pub type IFHandle = *mut c_void;
pub type DevHandle = *mut c_void;
pub type DSHandle = *mut c_void;
pub type PortHandle = *mut c_void;
pub type EventHandle = *mut c_void;
pub type EventSrcHandle = *mut c_void;
pub type BufferHandle = *mut c_void;

pub const EVENT_ERROR: EventType = 0;
pub const EVENT_NEW_BUFFER: EventType = 1;
pub const EVENT_FEATURE_INVALIDATE: EventType = 2;

#[repr(C)]
pub struct EventNewBufferData {
    pub buffer_handle: BufferHandle,
    pub p_user_pointer: *mut c_void,
}

#[repr(C)]
pub struct PortRegisterStackEntry {
    pub address: u64,
    pub p_buffer: *mut c_void,
    pub size: usize,
}

#[repr(C)]
pub struct SingleChunkData {
    pub chunk_id: u64,
    pub chunk_offset: isize,
    pub chunk_length: usize,
}

#[repr(C)]
pub struct DSBufferInfoStacked {
    pub i_info_cmd: BufferInfoCmd,
    pub i_type: InfoDataType,
    pub p_buffer: *mut c_void,
    pub i_size: usize,
    pub i_result: GCError,
}

#[repr(C)]
pub struct DSBufferPartInfoStacked {
    pub i_part_index: u32,
    pub i_info_cmd: BufferPartInfoCmd,
    pub i_result: GCError,
    pub i_type: InfoDataType,
    pub p_buffer: *mut c_void,
    pub i_size: usize,
}
