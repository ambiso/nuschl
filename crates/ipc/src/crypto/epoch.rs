// SPDX-License-Identifier: EUPL-1.2
//! Fixed records on a private crypto-to-RTC media-key connection.

use std::io::{self, Read, Write};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use zeroize::{Zeroize as _, Zeroizing};

const SIZE: usize = 56;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Send,
    Receive,
    ForgetReceive,
}

pub struct Update {
    pub generation: u64,
    pub peer: u64,
    pub direction: Direction,
    pub epoch: u32,
    pub key: [u8; 32],
}

impl Drop for Update {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

pub fn read(input: &mut impl Read) -> io::Result<Option<Update>> {
    let mut bytes = Zeroizing::new([0; SIZE]);
    if input.read(&mut bytes[..1])? == 0 {
        return Ok(None);
    }
    input.read_exact(&mut bytes[1..])?;
    decode(&bytes).map(Some)
}

pub async fn read_async(input: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<Update>> {
    let mut bytes = Zeroizing::new([0; SIZE]);
    if input.read(&mut bytes[..1]).await? == 0 {
        return Ok(None);
    }
    input.read_exact(&mut bytes[1..]).await?;
    decode(&bytes).map(Some)
}

pub fn write(output: &mut impl Write, update: &Update) -> io::Result<()> {
    check(
        update.generation,
        update.peer,
        update.direction,
        update.epoch,
        &update.key,
    )?;
    let mut bytes = Zeroizing::new([0; SIZE]);
    bytes[..2].copy_from_slice(&crate::PROTOCOL_VERSION.to_be_bytes());
    bytes[2] = match update.direction {
        Direction::Send => 1,
        Direction::Receive => 2,
        Direction::ForgetReceive => 3,
    };
    bytes[4..12].copy_from_slice(&update.generation.to_be_bytes());
    bytes[12..20].copy_from_slice(&update.peer.to_be_bytes());
    bytes[20..24].copy_from_slice(&update.epoch.to_be_bytes());
    bytes[24..].copy_from_slice(&update.key);
    output.write_all(&bytes[..])
}

fn decode(bytes: &[u8; SIZE]) -> io::Result<Update> {
    let version = u16::from_be_bytes([bytes[0], bytes[1]]);
    if version != crate::PROTOCOL_VERSION || bytes[3] != 0 {
        return Err(invalid());
    }
    let direction = match bytes[2] {
        1 => Direction::Send,
        2 => Direction::Receive,
        3 => Direction::ForgetReceive,
        _ => return Err(invalid()),
    };
    let generation = u64::from_be_bytes(field(&bytes[4..12])?);
    let peer = u64::from_be_bytes(field(&bytes[12..20])?);
    let epoch = u32::from_be_bytes(field(&bytes[20..24])?);
    let key = field(&bytes[24..])?;
    check(generation, peer, direction, epoch, &key)?;
    Ok(Update {
        generation,
        peer,
        direction,
        epoch,
        key,
    })
}

fn field<const N: usize>(bytes: &[u8]) -> io::Result<[u8; N]> {
    bytes.try_into().map_err(|_| invalid())
}

fn check(
    generation: u64,
    peer: u64,
    direction: Direction,
    epoch: u32,
    key: &[u8; 32],
) -> io::Result<()> {
    let valid = match direction {
        Direction::ForgetReceive => epoch == 0 && *key == [0; 32],
        Direction::Send | Direction::Receive => epoch != 0,
    };
    if generation == 0 || peer == 0 || !valid {
        return Err(invalid());
    }
    Ok(())
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid media epoch record")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_records_reject_short_unknown_and_unbound_inputs() {
        let update = Update {
            generation: 7,
            peer: 9,
            direction: Direction::Receive,
            epoch: 3,
            key: [1; 32],
        };
        let mut bytes = Vec::new();
        write(&mut bytes, &update).unwrap();
        let decoded = read(&mut &bytes[..]).unwrap().unwrap();
        assert_eq!(
            (
                decoded.generation,
                decoded.peer,
                decoded.epoch,
                decoded.direction
            ),
            (7, 9, 3, Direction::Receive)
        );
        assert_eq!(decoded.key, update.key);
        assert!(read(&mut &bytes[..SIZE - 1]).is_err());
        assert!(read(&mut &[][..]).unwrap().is_none());
        for index in [1, 2, 3, 11, 19, 23] {
            let mut invalid = bytes.clone();
            invalid[index] = if index == 3 { 1 } else { 0 };
            assert!(read(&mut &invalid[..]).is_err());
        }
    }
}
