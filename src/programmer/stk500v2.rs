use crate::errors;
use crate::programmer;
use crate::programmer::{AVRFuse, AVRFuseGet, AVRLockByteGet, Erase, MCUSignature, Programmer};
use avrisp;
use serial::core::{Error, SerialDevice, SerialPortSettings};
use std::convert::{TryFrom, TryInto};
use std::fmt;
use std::io::prelude::*;
use std::string::String;
use std::time::Duration;

#[allow(dead_code)]
mod command {

    pub enum Normal {
        SignOn = 0x01,
        SetParameter = 0x02,
        GetParameter = 0x03,
        SetDeviceParameters = 0x04,
        OSCcal = 0x05,
        LoadAddress = 0x06,
        FirmwareUpgrade = 0x07,
        SpiMulti = 0x1D,
        SetControlStack = 0x2D,
        EnterIspMode = 0x10,
        LeaveIspMode = 0x11,
    }

    impl Into<u8> for Normal {
        fn into(self) -> u8 {
            self as u8
        }
    }

    pub enum Isp {
        ChipErase = 0x12,
        ProgramFlash = 0x13,
        ReadFlash = 0x14,
        ProgramEeprom = 0x15,
        ReadEeprom = 0x16,
        ProgramFuse = 0x17,
        ReadFuse = 0x18,
        ProgramLock = 0x19,
        ReadLock = 0x1A,
        ReadSignature = 0x1B,
        ReadOsccal = 0x1C,
    }

    impl Into<u8> for Isp {
        fn into(self) -> u8 {
            self as u8
        }
    }
}

#[allow(dead_code)]
pub mod param {
    pub trait Readable {}

    pub trait Writable {}

    pub enum RO {
        BuildNumberLow = 0x80,
        BuildNumberHigh = 0x81,
        HwVer = 0x90,
        SwMajor = 0x91,
        SwMinor = 0x92,
        TopcardDetect = 0x9A, // This parameter only applies to STK500, not the AVRISP
        Status = 0x9C,
        Data = 0x9D,
    }

    impl Readable for RO {}

    impl From<RO> for u8 {
        fn from(value: RO) -> u8 {
            value as u8
        }
    }

    pub enum RW {
        Vtarget = 0x94,
        Vadjust = 0x95,
        OScPscale = 0x96,
        OscCmatch = 0x97,
        SckDuration = 0x98,
        ControllerInit = 0x9F,
        ResetPolarity = 0x9E,
    }

    impl Readable for RW {}

    impl Writable for RW {}

    impl From<RW> for u8 {
        fn from(value: RW) -> u8 {
            value as u8
        }
    }
}

#[allow(dead_code)]
pub enum Status {
    CmdOk = 0x00,
    CmdTimeout = 0x80,
    RdyBsyTout = 0x81,
    SetParamMissing = 0x82,
    CmdFailed = 0xC0,
    UnknownCmd = 0xC9,
    CheckSumError = 0xC1,
    AnswerChecksumError = 0xB0,
}

impl Into<u8> for Status {
    fn into(self) -> u8 {
        self as u8
    }
}

pub struct SwVersion {
    pub major: u8,
    pub minor: u8,
}

impl fmt::Display for SwVersion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor,)
    }
}

pub enum TopCard {
    STK501 = 0xAA,
    STK502 = 0x55,
    STK503 = 0xFA,
    STK504 = 0xEE,
    STK505 = 0xE4,
    STK520 = 0xDD,
}

/// Message structure:
/// Message start
/// Sequence number (u8 incremented for each message sent, overflows after 0xff)
/// Body length (maximum of 275 bytes, in big endian order)
/// Token
/// Body as bytes
/// Calculated checksum
#[derive(Debug)]
struct Message {
    seq: u8,
    len: u16,
    body: Vec<u8>,
}

type MessageBuffer = [u8; Message::MAX_SIZE];

impl Message {
    const MESSAGE_START: u8 = 0x1B;
    const TOKEN: u8 = 0x0E;
    const HEADER_SIZE: usize = 5;
    const CHECKSUM_SIZE: usize = 1;
    const BODY_START_POSITION: usize = 5;
    const LEN_BYTE_0_POSITION: usize = 2;
    const LEN_BYTE_1_POSITION: usize = 3;
    const SEQ_PSITION: usize = 1;
    const MAX_BODY_SIZE: usize = 275;
    const MAX_SIZE: usize = Self::MAX_BODY_SIZE + Self::CHECKSUM_SIZE + Self::HEADER_SIZE;

    fn new(seq: u8, body: Vec<u8>) -> Self {
        Self {
            len: body.len() as u16,
            body,
            seq,
        }
    }

    /// Calculate checksum (XOR of all bytes)
    fn calc_checksum(bytes: &[u8]) -> u8 {
        let mut result = bytes[0];
        for byte in bytes.iter().skip(1) {
            result ^= byte;
        }
        return result;
    }
}

impl Into<Vec<u8>> for Message {
    fn into(self) -> Vec<u8> {
        let len = (self.len as u16).to_be_bytes();
        let mut bytes: Vec<u8> = vec![Self::MESSAGE_START, self.seq, len[0], len[1], Self::TOKEN];
        bytes.extend(&self.body);
        bytes.push(Self::calc_checksum(&bytes));
        bytes
    }
}

impl TryFrom<MessageBuffer> for Message {
    type Error = errors::ErrorKind;

    fn try_from(buffer: MessageBuffer) -> Result<Self, Self::Error> {
        let body_size = u16::from_be_bytes([
            buffer[Self::LEN_BYTE_0_POSITION],
            buffer[Self::LEN_BYTE_1_POSITION],
        ]) as u16;
        let end_index = Self::HEADER_SIZE + body_size as usize;
        let crc: u8 = buffer[end_index];
        if crc != Self::calc_checksum(&buffer[..end_index]) {
            return Err(errors::ErrorKind::ChecksumError);
        } else {
            Ok(Message {
                len: body_size,
                body: buffer[Self::BODY_START_POSITION..end_index].to_vec(),
                seq: buffer[Self::SEQ_PSITION],
            })
        }
    }
}

fn to_hex(slice: &[u8]) -> String {
    let mut hexes: Vec<String> = Vec::with_capacity(slice.len());
    for i in slice {
        hexes.push(format!("{:#04x}", i));
    }
    return hexes.join(", ");
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "sequence_number={} body_length={} body=[{}]",
            self.seq,
            self.len,
            to_hex(&self.body),
        )
    }
}

/// Incremented by one for each message sent.
/// Wraps to zero after 0xFF is reached.
struct SequenceGenerator {
    count: u8,
}

impl SequenceGenerator {
    fn new() -> SequenceGenerator {
        SequenceGenerator { count: 0 }
    }
}

impl Iterator for SequenceGenerator {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        let ret = Some(self.count);
        self.count = self.count.wrapping_add(1);
        ret
    }
}

pub struct STK500v2 {
    port: serial::SystemPort,
    sequencer: SequenceGenerator,
    specs: avrisp::Specs,
}

impl STK500v2 {
    pub fn open(port: &String, specs: avrisp::Specs) -> Result<STK500v2, Error> {
        let mut port = serial::open(&port)?;
        let mut settings = port.read_settings()?;

        settings.set_baud_rate(serial::Baud115200)?;
        settings.set_parity(serial::ParityNone);
        settings.set_stop_bits(serial::Stop1);
        settings.set_char_size(serial::Bits8);
        // Must be set to none.
        // Otherwise programmer may hang at random command.
        settings.set_flow_control(serial::FlowNone);

        port.write_settings(&settings)?;
        port.set_timeout(Duration::from_secs(1))?;
        Ok(STK500v2 {
            port,
            sequencer: SequenceGenerator::new(),
            specs,
        })
    }

    fn write_message(&mut self, msg: Message) -> Result<(), errors::ErrorKind> {
        println!("write message {}", msg);
        let msg: Vec<u8> = msg.into();
        self.port.write_all(msg.as_slice())?;
        self.port.flush()?;
        return Ok(());
    }

    fn read_message(&mut self) -> Result<Message, errors::ErrorKind> {
        let mut buffer: MessageBuffer = [0; Message::MAX_SIZE];
        self.port.read_exact(&mut buffer[..Message::HEADER_SIZE])?;
        let body_size = u16::from_be_bytes([
            buffer[Message::LEN_BYTE_0_POSITION],
            buffer[Message::LEN_BYTE_1_POSITION],
        ]) as usize;
        let end = Message::HEADER_SIZE + body_size + Message::CHECKSUM_SIZE;
        self.port
            .read_exact(&mut buffer[Message::BODY_START_POSITION..end])?;
        let msg = Message::try_from(buffer)?;
        println!("got message {}", msg);
        return Ok(msg);
    }

    fn cmd(&mut self, cmd: u8, mut body: Vec<u8>) -> Result<Message, errors::ErrorKind> {
        // This will always succeed
        let seq = self.sequencer.next().unwrap();
        // Prepend body with command
        body.insert(0, cmd);
        let sent_msg = Message::new(seq, body);
        self.write_message(sent_msg)?;
        let read_msg = self.read_message()?;

        if seq != read_msg.seq {
            return Err(errors::ErrorKind::SequenceError {});
        }
        if cmd != read_msg.body[0] {
            return Err(errors::ErrorKind::AnswerIdError {});
        }
        if read_msg.body[1] != Status::CmdOk.into() {
            return Err(errors::ErrorKind::StatusError {});
        }
        Ok(read_msg)
    }

    fn set_param<T>(&mut self, param: T, value: u8) -> Result<(), errors::ErrorKind>
    where
        T: param::Writable + Into<u8>,
    {
        let bytes = vec![param.into(), value];
        let msg = self.cmd(command::Normal::SetParameter.into(), bytes)?;
        if msg.body[0] != command::Normal::SetParameter.into() {
            return Err(errors::ErrorKind::AnswerIdError {});
        }
        if msg.body[1] != Status::CmdOk.into() {
            return Err(errors::ErrorKind::StatusError {});
        }
        Ok(())
    }

    fn get_param<T>(&mut self, param: T) -> Result<u8, errors::ErrorKind>
    where
        T: param::Readable + Into<u8>,
    {
        let bytes: Vec<u8> = vec![param.into()];
        let msg = self.cmd(command::Normal::GetParameter.into(), bytes)?;
        if msg.body[0] != command::Normal::GetParameter.into() {
            return Err(errors::ErrorKind::AnswerIdError {});
        }
        if msg.body[1] != Status::CmdOk.into() {
            return Err(errors::ErrorKind::StatusError {});
        }
        // return parameter
        Ok(msg.body[2])
    }

    pub fn read_programmer_signature(&mut self) -> Result<programmer::Variant, errors::ErrorKind> {
        let msg = self.cmd(command::Normal::SignOn.into(), vec![])?;
        let variant = String::from_utf8(msg.body[3..].to_vec())?;
        Ok(programmer::Variant::try_from(variant)?)
    }
}

impl TryInto<IspMode> for STK500v2 {
    type Error = errors::ErrorKind;
    fn try_into(mut self) -> Result<IspMode, Self::Error> {
        let bytes = vec![
            self.specs.timeout,
            self.specs.stab_delay,
            self.specs.cmd_exe_delay,
            self.specs.synch_loops,
            self.specs.byte_delay,
            self.specs.pool_value,
            self.specs.pool_index,
            avrisp::PROGRAMMING_ENABLE.0,
            avrisp::PROGRAMMING_ENABLE.1,
            avrisp::PROGRAMMING_ENABLE.2,
            avrisp::PROGRAMMING_ENABLE.3,
        ];
        self.set_param(param::RW::ResetPolarity, self.specs.reset_polarity.into())?;
        self.cmd(command::Normal::EnterIspMode.into(), bytes)?;
        Ok(IspMode::new(self))
    }
}

pub struct IspMode {
    prog: STK500v2,
}

impl IspMode {
    fn new(prog: STK500v2) -> IspMode {
        IspMode { prog }
    }

    // Does not work on atmega2560.
    // Requires some kind of different handling when loading memory address
    pub fn read_flash(&mut self, start: usize, buffer: &mut [u8]) -> Result<(), errors::ErrorKind> {
        // For word-addressed memories (program flash), the Address parameter is the word address.
        let bytes_to_read = self.prog.specs.flash.page_size;
        // Block size is given in Kwords. Word is 16 bit.
        let step_by = self.prog.specs.flash.block_size;
        for (addr, buffer) in (start..buffer.len())
            .step_by(step_by)
            .zip(buffer.chunks_exact_mut(bytes_to_read))
        {
            let dst_addr = (addr as u32).to_be_bytes().to_vec();
            self.prog
                .cmd(command::Normal::LoadAddress.into(), dst_addr)?;
            // Read Program Memory command byte #1
            let read_command = avrisp::READ_FLASH_LOW.0;
            let to_read_as_bytes = (bytes_to_read as u16).to_be_bytes();
            let mut msg = self.prog.cmd(
                command::Isp::ReadFlash.into(),
                vec![to_read_as_bytes[0], to_read_as_bytes[1], read_command],
            )?;
            let data_offset = 2;
            buffer.swap_with_slice(&mut msg.body[data_offset..(bytes_to_read + data_offset)]);
        }
        Ok(())
    }

    // Does not work on atmega2560.
    // Requires some kind of different handling when loading memory address
    pub fn read_eeprom(&mut self, start: usize, bytes: &mut [u8]) -> Result<(), errors::ErrorKind> {
        for (addr, buffer) in (start..(bytes.len() + start))
            .step_by(self.prog.specs.eeprom.page_size)
            .zip(bytes.chunks_exact_mut(self.prog.specs.eeprom.page_size))
        {
            self.prog.cmd(
                command::Normal::LoadAddress.into(),
                (addr as u32).to_be_bytes().to_vec(),
            )?;
            let length_bytes = (self.prog.specs.eeprom.page_size as u16).to_be_bytes();
            let mut msg = self.prog.cmd(
                command::Isp::ReadEeprom.into(),
                vec![length_bytes[0], length_bytes[1], avrisp::READ_EEPROM.0],
            )?;
            let data_offset = 2;
            buffer.swap_with_slice(
                &mut msg.body[data_offset..(self.prog.specs.eeprom.page_size + data_offset)],
            );
        }
        Ok(())
    }

    fn read_fuse(&mut self, cmd: avrisp::IspCommand) -> Result<u8, errors::ErrorKind> {
        let msg = self.prog.cmd(
            command::Isp::ReadFuse.into(),
            vec![self.prog.specs.fuse_poll_index, cmd.0, cmd.1, cmd.2, cmd.3],
        )?;
        Ok(msg.body[2])
    }
}

impl Erase for IspMode {
    fn erase(&mut self) -> Result<(), errors::ErrorKind> {
        self.prog.cmd(
            command::Isp::ChipErase.into(),
            vec![
                self.prog.specs.erase_delay,
                self.prog.specs.erase_poll_method,
                avrisp::CHIP_ERASE.0,
                avrisp::CHIP_ERASE.1,
                avrisp::CHIP_ERASE.2,
                avrisp::CHIP_ERASE.3,
            ],
        )?;
        Ok(())
    }
}

impl Programmer for IspMode {
    fn close(mut self) -> Result<(), errors::ErrorKind> {
        let bytes = vec![self.prog.specs.pre_delay, self.prog.specs.post_delay];
        self.prog.cmd(command::Normal::LeaveIspMode.into(), bytes)?;
        Ok(())
    }
}

impl AVRLockByteGet for IspMode {
    fn get_lock_byte(&mut self) -> Result<u8, errors::ErrorKind> {
        let msg = self.prog.cmd(
            command::Isp::ReadLock.into(),
            vec![
                self.prog.specs.lock_poll_index,
                avrisp::READ_LOCK.0,
                avrisp::READ_LOCK.1,
                avrisp::READ_LOCK.2,
                avrisp::READ_LOCK.3,
            ],
        )?;
        Ok(msg.body[2])
    }
}

impl AVRFuseGet for IspMode {
    fn get_fuses(&mut self) -> Result<AVRFuse, errors::ErrorKind> {
        Ok(AVRFuse {
            low: self.read_fuse(avrisp::READ_LOW_FUSE)?,
            high: self.read_fuse(avrisp::READ_HIGH_FUSE)?,
            extended: self.read_fuse(avrisp::READ_EXTENDED_FUSE)?,
        })
    }
}

impl MCUSignature for IspMode {
    fn get_mcu_signature(&mut self) -> Result<avrisp::Signature, errors::ErrorKind> {
        let mut signature: [u8; 3] = [0; 3];
        for addr in 0..signature.len() {
            let msg = self.prog.cmd(
                command::Isp::ReadSignature.into(),
                vec![
                    self.prog.specs.signature_poll_index,
                    avrisp::READ_SIGNATURE.0,
                    avrisp::READ_SIGNATURE.1,
                    addr as u8,
                    avrisp::READ_SIGNATURE.3,
                ],
            )?;
            signature[addr] = msg.body[2];
        }
        Ok(avrisp::Signature::from(signature))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claim::*;

    mod sequence_generator {

        use super::*;

        #[test]
        fn overflows_after_255() {
            let mut gen = SequenceGenerator::new().skip(256);
            assert_eq!(gen.next(), Some(0));
        }

        #[test]
        fn starts_at_0() {
            let mut gen = SequenceGenerator::new();
            assert_eq!(gen.next(), Some(0));
        }

        #[test]
        fn increments_by_1() {
            let mut gen = SequenceGenerator::new().skip(1);
            assert_eq!(gen.next(), Some(1));
        }
    }

    mod message {
        use super::*;

        #[test]
        fn calculates_checksum() {
            assert_eq!(Message::calc_checksum(&[2, 55, 22, 78]), 109);
        }

        #[test]
        fn try_from_array_is_ok() {
            let mut buffer: MessageBuffer = [0; Message::MAX_SIZE];
            buffer[0] = Message::MESSAGE_START;
            buffer[1] = 1;
            buffer[2] = 0;
            buffer[3] = 4;
            buffer[4] = Message::TOKEN;
            buffer[5] = 89;
            buffer[6] = 100;
            buffer[7] = 78;
            buffer[8] = 109;
            buffer[9] = 14;
            assert_ok!(Message::try_from(buffer));
        }
        #[test]
        fn try_from_array_bad_checksum() {
            let mut buffer: MessageBuffer = [0; Message::MAX_SIZE];
            buffer[0] = Message::MESSAGE_START;
            buffer[1] = 1;
            buffer[2] = 0;
            buffer[3] = 4;
            buffer[4] = Message::TOKEN;
            buffer[5] = 89;
            buffer[6] = 100;
            buffer[7] = 78;
            buffer[8] = 109;
            buffer[9] = 0;
            let err = Message::try_from(buffer).unwrap_err();
            match err {
                errors::ErrorKind::ChecksumError => (),
                _ => panic!("wrong error returned"),
            };
        }
    }
}
