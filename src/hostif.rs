//! Firmware host-interface commands (spec 12.4): the X553's PHY token
//! (12.5) and firmware PHY activities (12.6). The X552/X553 NVM read
//! (11.2) is `x552::nvm_word`, which holds EEP as well.

use super::{read, sync, write, Error, Port, Registers, R};

const HICR: u32 = 0x15f00;
const FWSTS: u32 = 0x15f0c;
const FLEX_MNG: u32 = 0x15800;
const HICR_EN: u32 = 1 << 0;
const HICR_C: u32 = 1 << 1;
const HICR_SV: u32 = 1 << 2;
const FWSTS_FWRI: u32 = 1 << 9;
const TIMEOUT_US: usize = 500_000;
/// Header byte 3: the checksum is always 0xFF.
const CHECKSUM: u32 = 0xff << 24;

pub const TOKEN_COMMAND: u8 = 0x0a;
pub const TOKEN_BUSY: u8 = 0x80;
const TOKEN_OK: u8 = 0x01;
const ACTIVITY_COMMAND: u8 = 0x05;
const ACTIVITY_OK: u8 = 0x01;
const ACTIVITY_TRIES: usize = 50;

// Firmware PHY activities (spec 12.6).
pub const INIT_PHY: u16 = 1;
pub const SETUP_LINK: u16 = 2;
pub const GET_LINK_INFO: u16 = 3;
pub const FORCE_LINK_DOWN: u16 = 4;
pub const PHY_SW_RESET: u16 = 5;
pub const GET_PHY_INFO: u16 = 7;

fn header(command: u8, len: u8) -> u32 { CHECKSUM | (len as u32) << 8 | command as u32 }

/// One command (spec 12.4): `block` (header first) is written to FLEX_MNG,
/// HICR.C set and polled for up to 500 ms, SV required; the response is read
/// back into `block`. Returns the response's status byte. Holds SW_MNG.
pub fn command<Io: Registers>(io: &mut Io, port: Port, block: &mut [u32]) -> R<u8, Io::Error> {
    sync::locked(io, port, sync::SW_MNG, |io| {
        let fwsts = read(io, FWSTS)?;
        write(io, FWSTS, fwsts | FWSTS_FWRI)?;
        let hicr = read(io, HICR)?;
        if hicr & HICR_EN == 0 { return Err(Error::HostInterface { hicr }); }
        for (i, v) in block.iter().enumerate() { write(io, FLEX_MNG + 4 * i as u32, *v)?; }
        write(io, HICR, hicr | HICR_C)?;
        let mut last = hicr | HICR_C;
        for _ in 0..TIMEOUT_US / 10 {
            last = read(io, HICR)?;
            if last & HICR_C == 0 { break; }
            io.delay_us(10);
        }
        if last & HICR_C != 0 || last & HICR_SV == 0 { return Err(Error::HostInterface { hicr: last }); }
        // The response is data: all-ones is a value, so read it raw.
        let head = io.read(FLEX_MNG).map_err(Error::Io)?;
        let len = ((head >> 8) & 0xff) as usize;
        if len + 4 > block.len() * 4 { return Err(Error::HostInterface { hicr: last }); }
        block[0] = head;
        for (i, slot) in block.iter_mut().enumerate().skip(1).take(len.div_ceil(4)) {
            *slot = io.read(FLEX_MNG + 4 * i as u32).map_err(Error::Io)?;
        }
        Ok((head >> 16) as u8)
    })
}

/// Ask for (`take`) or give back the X553 PHY token (spec 12.5). Ok(false):
/// the firmware is busy, ask again; any other refusal is an error.
pub fn token<Io: Registers>(io: &mut Io, port: Port, take: bool) -> R<bool, Io::Error> {
    let mut block = [header(TOKEN_COMMAND, 2), port.lan as u32 | (!take as u32) << 8];
    match command(io, port, &mut block)? {
        TOKEN_OK => Ok(true),
        TOKEN_BUSY if take => Ok(false),
        status => Err(Error::Firmware { command: TOKEN_COMMAND, status }),
    }
}

/// A firmware PHY activity (spec 12.6): four big-endian data words in and
/// out; a failing status is retried 20 µs apart, 50 times in all.
pub fn phy_activity<Io: Registers>(io: &mut Io, port: Port, activity: u16, data: [u32; 4])
    -> R<[u32; 4], Io::Error> {
    let mut status = 0;
    for _ in 0..ACTIVITY_TRIES {
        let mut block = [0; 6];
        block[0] = header(ACTIVITY_COMMAND, 20);
        block[1] = port.lan as u32 | (activity as u32) << 16;
        for (i, d) in data.iter().enumerate() { block[2 + i] = d.swap_bytes(); }
        status = command(io, port, &mut block)?;
        if status == ACTIVITY_OK {
            return Ok([block[1].swap_bytes(), block[2].swap_bytes(), block[3].swap_bytes(), block[4].swap_bytes()]);
        }
        io.delay_us(20);
    }
    Err(Error::Firmware { command: ACTIVITY_COMMAND, status })
}
