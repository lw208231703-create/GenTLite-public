//! The GenCP command service: decode the host's command transfer
//! (U3VC prefix + CCD + SCD, §4.3), dispatch against the register file,
//! and encode the acknowledge. The serve loop is poll-based so the
//! stop flag stays responsive; one command at a time (GenCP over the
//! DCI is strictly request/acknowledge, §4.1).

use std::sync::Arc;

use gentl_u3v::packets;

use super::{DeviceInner, HostCommands, RegisterFile, POLL};
/// Serve GenCP commands until the stop flag or a dead stream. Runs on
/// the device's server thread.
pub(crate) fn serve_loop(
    control_out: Arc<dyn HostCommands>,
    inner: &Arc<DeviceInner>,
    regs: &Arc<RegisterFile>,
) {
    while !inner.stop.load(std::sync::atomic::Ordering::Relaxed) {
        let Some(cmd) = control_out.recv(POLL) else { continue };
        let ack = dispatch(&cmd, regs);
        if inner.control_in.send(&ack).is_err() {
            eprintln!("vgige: u3v control-IN stream closed; GenCP server stopping");
            break;
        }
    }
}

/// One command transfer in, one acknowledge out. Unknown/malformed
/// commands get a header-only NOT_IMPLEMENTED (§4.3.2.1) so the host's
/// transaction always completes.
fn dispatch(cmd: &[u8], regs: &RegisterFile) -> Vec<u8> {
    let bad = |req| packets::encode_ack(packets::GENCP_NOT_IMPLEMENTED, packets::READMEM_ACK, req, &[]);
    if cmd.len() < packets::CMD_OVERHEAD
        || u32::from_le_bytes([cmd[0], cmd[1], cmd[2], cmd[3]]) != packets::PREFIX_CONTROL
    {
        return bad(0);
    }
    let Some(ccd) = packets::CommandCcd::decode(&cmd[packets::PREFIX_LEN..]) else {
        return bad(0);
    };
    let scd = &cmd[packets::CMD_OVERHEAD..];
    match ccd.command_id {
        packets::READMEM_CMD => {
            let Some((addr, len)) = packets::decode_readmem_scd(scd) else {
                return bad(ccd.request_id);
            };
            let mut data = vec![0u8; len as usize];
            match regs.read(addr, &mut data) {
                Ok(()) => packets::encode_readmem_ack(packets::GENCP_SUCCESS, ccd.request_id, &data),
                Err(status) => packets::encode_readmem_ack(status, ccd.request_id, &[]),
            }
        }
        packets::WRITEMEM_CMD => {
            let Some((addr, off)) = packets::decode_writemem_scd(scd) else {
                return bad(ccd.request_id);
            };
            let data = &scd[off.min(scd.len())..];
            match regs.write(addr, data) {
                Ok(()) => {
                    // Written-Length field: the ABRM capability declares
                    // it (GenCP §4.4.4), so the ack carries it.
                    packets::encode_writemem_ack(
                        packets::GENCP_SUCCESS,
                        ccd.request_id,
                        Some(data.len() as u16),
                    )
                }
                Err(status) => packets::encode_writemem_ack(status, ccd.request_id, Some(0)),
            }
        }
        other => packets::encode_ack(
            packets::GENCP_NOT_IMPLEMENTED,
            other.wrapping_add(1),
            ccd.request_id,
            &[],
        ),
    }
}
