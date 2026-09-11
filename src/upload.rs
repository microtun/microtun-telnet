use std::{fs::File, io::Read, path::Path};

use crc::{CRC_16_XMODEM, Crc, NoTable};

use crate::telnet::TelnetClient;

const CRC16: Crc<u16, NoTable> = Crc::<u16, NoTable>::new(&CRC_16_XMODEM);
const BLOCK_SIZE: usize = 1024;
const HEADER_BLOCK_SIZE: usize = 128;

const YMODEM_SOH: u8 = 0x01;
const YMODEM_STX: u8 = 0x02;
const YMODEM_EOT: u8 = 0x04;
const YMODEM_ACK: u8 = 0x06;
const YMODEM_NAK: u8 = 0x15;
const YMODEM_CAN: u8 = 0x18;
const YMODEM_CRC_REQUEST: u8 = b'C';
const YMODEM_PAD: u8 = 0x1a;
const YMODEM_RETRIES: usize = 16;

#[derive(Clone, Copy, Debug)]
pub(crate) enum UploadEvent {
    Output(u8),
    Progress { sent: u64, total: u64 },
}

pub(crate) fn send_file_with<F>(
    client: &mut TelnetClient,
    path: &Path,
    mut notify: F,
) -> Result<u64, String>
where
    F: FnMut(UploadEvent) -> Result<(), String>,
{
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let file_size = file
        .metadata()
        .map_err(|error| format!("stat {}: {error}", path.display()))?
        .len();
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("upload.bin");
    if filename.as_bytes().contains(&0) {
        return Err("YMODEM filename contains NUL".to_owned());
    }

    let size_text = file_size.to_string();
    if filename.len() + 1 + size_text.len() + 1 > HEADER_BLOCK_SIZE {
        return Err("YMODEM filename is too long for block 0".to_owned());
    }

    notify(UploadEvent::Progress {
        sent: 0,
        total: file_size,
    })?;
    wait_for_crc_request(client, &mut notify)?;

    let mut header = [0u8; HEADER_BLOCK_SIZE];
    header[..filename.len()].copy_from_slice(filename.as_bytes());
    let size_start = filename.len() + 1;
    header[size_start..size_start + size_text.len()].copy_from_slice(size_text.as_bytes());
    send_packet(client, YMODEM_SOH, 0, &header)?;

    // A YMODEM receiver ACKs block 0, then sends another `C` before block 1.
    wait_for_crc_request(client, &mut notify)?;

    let mut block_number = 1u8;
    let mut sent = 0u64;
    loop {
        let mut data = [YMODEM_PAD; BLOCK_SIZE];
        let mut used = 0usize;
        while used < BLOCK_SIZE {
            let count = file
                .read(&mut data[used..])
                .map_err(|error| format!("read {}: {error}", path.display()))?;
            if count == 0 {
                break;
            }
            used += count;
        }
        if used == 0 {
            break;
        }

        send_packet(client, YMODEM_STX, block_number, &data)?;
        sent = sent.saturating_add(used as u64);
        notify(UploadEvent::Progress {
            sent: sent.min(file_size),
            total: file_size,
        })?;
        block_number = block_number.wrapping_add(1);
    }

    client.write_data(&[YMODEM_EOT])?;
    client.flush()?;
    match client.read_data_byte()? {
        YMODEM_NAK => {}
        YMODEM_CAN => return Err("device cancelled YMODEM transfer".to_owned()),
        other => {
            return Err(format!(
                "unexpected YMODEM first-EOT response 0x{other:02x}"
            ));
        }
    }

    client.write_data(&[YMODEM_EOT])?;
    client.flush()?;
    match client.read_data_byte()? {
        YMODEM_ACK => {}
        YMODEM_CAN => return Err("device cancelled YMODEM transfer".to_owned()),
        other => {
            return Err(format!(
                "unexpected YMODEM final-EOT response 0x{other:02x}"
            ));
        }
    }

    // Finish the one-file batch with the standard empty block 0.
    wait_for_crc_request(client, &mut notify)?;
    send_packet(client, YMODEM_SOH, 0, &[0u8; HEADER_BLOCK_SIZE])?;
    notify(UploadEvent::Progress {
        sent: file_size,
        total: file_size,
    })?;
    Ok(file_size)
}

fn crc16(bytes: &[u8]) -> u16 {
    CRC16.checksum(bytes)
}

fn wait_for_crc_request<F>(client: &mut TelnetClient, notify: &mut F) -> Result<(), String>
where
    F: FnMut(UploadEvent) -> Result<(), String>,
{
    loop {
        match client.read_data_byte()? {
            YMODEM_CRC_REQUEST => return Ok(()),
            YMODEM_CAN => return Err("device cancelled YMODEM transfer".to_owned()),
            byte => {
                // Shells print a short instruction line after the READY marker.
                // Preserve that text while ignoring it for YMODEM handshaking.
                notify(UploadEvent::Output(byte))?;
            }
        }
    }
}

fn send_packet(
    client: &mut TelnetClient,
    control: u8,
    block_number: u8,
    data: &[u8],
) -> Result<(), String> {
    let crc = crc16(data);
    let mut frame = Vec::with_capacity(data.len() + 5);
    frame.push(control);
    frame.push(block_number);
    frame.push(!block_number);
    frame.extend_from_slice(data);
    frame.extend_from_slice(&crc.to_be_bytes());

    for _ in 0..YMODEM_RETRIES {
        client.write_data(&frame)?;
        client.flush()?;
        match client.read_data_byte()? {
            YMODEM_ACK => return Ok(()),
            YMODEM_NAK => continue,
            YMODEM_CAN => return Err("device cancelled YMODEM transfer".to_owned()),
            other => {
                return Err(format!(
                    "unexpected YMODEM response 0x{other:02x} for block {block_number}"
                ));
            }
        }
    }
    Err(format!(
        "device did not acknowledge YMODEM block {block_number}"
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        thread,
        time::Duration,
    };

    use super::*;

    const IAC: u8 = 0xff;
    const WILL: u8 = 0xfb;
    const WONT: u8 = 0xfc;
    const DO: u8 = 0xfd;
    const DONT: u8 = 0xfe;
    const SB: u8 = 0xfa;
    const SE: u8 = 0xf0;
    const TELNET_BINARY: u8 = 0;

    fn read_telnet_data_byte(stream: &mut TcpStream) -> u8 {
        loop {
            let mut byte = [0u8; 1];
            stream.read_exact(&mut byte).unwrap();
            if byte[0] != IAC {
                return byte[0];
            }

            stream.read_exact(&mut byte).unwrap();
            match byte[0] {
                IAC => return IAC,
                WILL | WONT | DO | DONT => {
                    stream.read_exact(&mut byte).unwrap();
                }
                SB => loop {
                    stream.read_exact(&mut byte).unwrap();
                    if byte[0] == IAC {
                        stream.read_exact(&mut byte).unwrap();
                        if byte[0] == SE {
                            break;
                        }
                    }
                },
                _ => {}
            }
        }
    }

    fn read_ymodem_packet(stream: &mut TcpStream, control: u8, block: u8, size: usize) -> Vec<u8> {
        assert_eq!(read_telnet_data_byte(stream), control);
        assert_eq!(read_telnet_data_byte(stream), block);
        assert_eq!(read_telnet_data_byte(stream), !block);
        let mut data = vec![0u8; size];
        for byte in &mut data {
            *byte = read_telnet_data_byte(stream);
        }
        let crc = [read_telnet_data_byte(stream), read_telnet_data_byte(stream)];
        assert_eq!(u16::from_be_bytes(crc), crc16(&data));
        data
    }

    fn receive_test_ymodem(stream: &mut TcpStream) -> (String, Vec<u8>) {
        stream
            .write_all(&[
                IAC,
                WILL,
                TELNET_BINARY,
                IAC,
                DO,
                TELNET_BINARY,
                YMODEM_CRC_REQUEST,
            ])
            .unwrap();
        stream.flush().unwrap();

        let header = read_ymodem_packet(stream, YMODEM_SOH, 0, HEADER_BLOCK_SIZE);
        let name_end = header.iter().position(|&byte| byte == 0).unwrap();
        let filename = String::from_utf8(header[..name_end].to_vec()).unwrap();
        let rest = &header[name_end + 1..];
        let size_end = rest
            .iter()
            .position(|&byte| byte == 0 || byte == b' ')
            .unwrap_or(rest.len());
        let expected_len = std::str::from_utf8(&rest[..size_end])
            .unwrap()
            .parse::<usize>()
            .unwrap();
        stream.write_all(&[YMODEM_ACK, YMODEM_CRC_REQUEST]).unwrap();
        stream.flush().unwrap();

        let mut output = Vec::new();
        let mut expected_block = 1u8;
        while output.len() < expected_len {
            let data = read_ymodem_packet(stream, YMODEM_STX, expected_block, BLOCK_SIZE);
            let count = (expected_len - output.len()).min(BLOCK_SIZE);
            output.extend_from_slice(&data[..count]);
            stream.write_all(&[YMODEM_ACK]).unwrap();
            stream.flush().unwrap();
            expected_block = expected_block.wrapping_add(1);
        }

        assert_eq!(read_telnet_data_byte(stream), YMODEM_EOT);
        stream.write_all(&[YMODEM_NAK]).unwrap();
        stream.flush().unwrap();
        assert_eq!(read_telnet_data_byte(stream), YMODEM_EOT);
        stream.write_all(&[YMODEM_ACK, YMODEM_CRC_REQUEST]).unwrap();
        stream.flush().unwrap();

        let final_header = read_ymodem_packet(stream, YMODEM_SOH, 0, HEADER_BLOCK_SIZE);
        assert!(final_header.iter().all(|&byte| byte == 0));
        stream.write_all(&[YMODEM_ACK]).unwrap();
        stream.flush().unwrap();
        (filename, output)
    }

    #[test]
    fn crc16_matches_xmodem_check_value() {
        assert_eq!(crc16(b"123456789"), 0x31c3);
    }

    #[test]
    fn handles_telnet_binary_negotiation_and_iac_payload() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (filename, payload) = receive_test_ymodem(&mut stream);
            assert!(filename.ends_with("config.ini"));
            assert_eq!(payload, b"before\xffafter");
        });

        let path = std::env::temp_dir().join(format!(
            "microtun-telnet-test-{}-config.ini",
            std::process::id()
        ));
        fs::write(&path, b"before\xffafter").unwrap();
        let mut client =
            TelnetClient::connect("localhost", address.port(), Duration::from_secs(1)).unwrap();
        client.set_read_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(send_file_with(&mut client, &path, |_| Ok(())).unwrap(), 12);
        fs::remove_file(path).unwrap();
        server.join().unwrap();
    }
}
