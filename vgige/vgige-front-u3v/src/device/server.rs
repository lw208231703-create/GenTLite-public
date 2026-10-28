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
        // A None here is normally a poll timeout (idle camera); the
        // bridge (gadget.rs) is immortal, so a disconnected channel —
        // the only case that would spin this loop — coincides with
        // teardown, where the stop flag ends us.
        let Some(cmd) = control_out.recv(POLL) else {
            continue;
        };
        let Some(ack) = dispatch(&cmd, regs) else {
            continue;
        };
        if inner.control_in.send(&ack).is_err() {
            eprintln!("vgige: u3v control-IN stream closed; GenCP server stopping");
            break;
        }
    }
}

/// One command transfer in, one acknowledge out. Unknown command IDs
/// get a header-only NOT_IMPLEMENTED (§4.3.2.1) so the host's
/// transaction always completes. A transfer that does not conform to
/// the command format MUST be ignored completely (§4.1.2) — signalled
/// here as None, which the server loop drops on the floor (an empty
/// ack Vec is avoided because zero bytes is not a representable GenCP
/// ack anyway).
fn decode_command(cmd: &[u8]) -> Option<(packets::CommandCcd, &[u8])> {
    if cmd.len() < packets::CMD_OVERHEAD
        || u32::from_le_bytes([cmd[0], cmd[1], cmd[2], cmd[3]]) != packets::PREFIX_CONTROL
    {
        return None;
    }
    let ccd = packets::CommandCcd::decode(&cmd[packets::PREFIX_LEN..])?;
    if cmd.len() != packets::CMD_OVERHEAD + ccd.length as usize {
        return None;
    }
    match ccd.command_id {
        packets::READMEM_CMD if ccd.length as usize != packets::READMEM_SCD_LEN => return None,
        packets::WRITEMEM_CMD if (ccd.length as usize) < packets::WRITEMEM_SCD_HEADER_LEN => {
            return None
        }
        _ => {}
    }
    Some((ccd, &cmd[packets::CMD_OVERHEAD..]))
}

fn dispatch(cmd: &[u8], regs: &RegisterFile) -> Option<Vec<u8>> {
    let (ccd, scd) = decode_command(cmd)?;
    match ccd.command_id {
        packets::READMEM_CMD => {
            let (addr, len) = packets::decode_readmem_scd(scd)?;
            let mut data = vec![0u8; len as usize];
            match regs.read(addr, &mut data) {
                Ok(()) => Some(packets::encode_readmem_ack(
                    packets::GENCP_SUCCESS,
                    ccd.request_id,
                    &data,
                )),
                Err(status) => Some(packets::encode_readmem_ack(status, ccd.request_id, &[])),
            }
        }
        packets::WRITEMEM_CMD => {
            let (addr, off) = packets::decode_writemem_scd(scd)?;
            let data = &scd[off..];
            match regs.write(addr, data) {
                Ok(()) => {
                    // Written-Length field: the ABRM capability declares
                    // it (GenCP §4.4.4), so the ack carries it.
                    Some(packets::encode_writemem_ack(
                        packets::GENCP_SUCCESS,
                        ccd.request_id,
                        Some(data.len() as u16),
                    ))
                }
                Err(status) => Some(packets::encode_writemem_ack(
                    status,
                    ccd.request_id,
                    Some(0),
                )),
            }
        }
        other => Some(packets::encode_ack(
            packets::GENCP_NOT_IMPLEMENTED,
            other.wrapping_add(1),
            ccd.request_id,
            &[],
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_commands_have_exact_envelopes() {
        let read = packets::encode_readmem(7, 0x1000, 4);
        assert!(decode_command(&read).is_some());
        let write = packets::encode_writemem(8, 0x1000, &[1, 2, 3, 4]);
        assert!(decode_command(&write).is_some());
    }

    #[test]
    fn malformed_lengths_are_ignored() {
        let mut read = packets::encode_readmem(7, 0x1000, 4);
        read[8..10].copy_from_slice(&0u16.to_le_bytes());
        assert!(decode_command(&read).is_none());

        let mut write = packets::encode_writemem(8, 0x1000, &[1, 2, 3, 4]);
        write.truncate(write.len() - 1);
        assert!(decode_command(&write).is_none());

        let mut short_write = packets::encode_writemem(9, 0x1000, &[]);
        short_write[8..10].copy_from_slice(&4u16.to_le_bytes());
        short_write.truncate(packets::CMD_OVERHEAD + 4);
        assert!(decode_command(&short_write).is_none());
    }
}
