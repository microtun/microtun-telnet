use std::{io, path::Path, time::Duration};

use core::ops::AsyncFnMut;

use microtun_ymodem::{Config, Metadata, SendError, SendEvent, Source, send_with};
use tokio::{fs::File, io::AsyncReadExt};

use crate::telnet::TelnetClient;

pub(crate) use microtun_ymodem::SendEvent as UploadEvent;

struct TokioFileSource {
    file: File,
}

impl Source for TokioFileSource {
    type Error = io::Error;

    async fn read(&mut self, output: &mut [u8]) -> Result<usize, Self::Error> {
        self.file.read(output).await
    }
}

pub(crate) async fn send_file_with<F>(
    client: &mut TelnetClient,
    path: &Path,
    timeout: Duration,
    notify: F,
) -> Result<u64, String>
where
    F: AsyncFnMut(SendEvent) -> Result<(), String>,
{
    let file = File::open(path)
        .await
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    let file_size_u64 = file
        .metadata()
        .await
        .map_err(|error| format!("stat {}: {error}", path.display()))?
        .len();
    let file_size = usize::try_from(file_size_u64)
        .map_err(|_| format!("{} is too large for this platform", path.display()))?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("upload.bin");

    let mut source = TokioFileSource { file };
    let transfer = send_with(
        client,
        &mut source,
        Metadata {
            filename: filename.as_bytes(),
            file_size,
        },
        config_from_timeout(timeout),
        notify,
    )
    .await
    .map_err(format_send_error)?;

    Ok(transfer.file_size as u64)
}

fn config_from_timeout(timeout: Duration) -> Config {
    let timeout_ms = u32::try_from(timeout.as_millis())
        .unwrap_or(u32::MAX)
        .max(1);
    Config {
        start_timeout_ms: timeout_ms,
        transfer_timeout_ms: timeout_ms,
        start_retries: 1,
        ..Config::default()
    }
}

fn format_send_error(error: SendError<String, io::Error, String>) -> String {
    match error {
        SendError::Io(error) => error,
        SendError::Timeout => "YMODEM transfer timed out".to_owned(),
        SendError::Cancelled => "device cancelled YMODEM transfer".to_owned(),
        SendError::Protocol => "unexpected YMODEM protocol response".to_owned(),
        SendError::InvalidFilename => "YMODEM filename is empty or contains NUL".to_owned(),
        SendError::HeaderTooLong => "YMODEM filename is too long for block 0".to_owned(),
        SendError::UnexpectedEof => "upload file ended before its advertised size".to_owned(),
        SendError::InvalidSourceRead => "upload source returned an invalid read length".to_owned(),
        SendError::Source(error) => format!("read upload file: {error}"),
        SendError::Observer(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use microtun_ymodem::{BLOCK_SIZE, HEADER_BLOCK_SIZE, crc16};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
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

    const YMODEM_SOH: u8 = 0x01;
    const YMODEM_STX: u8 = 0x02;
    const YMODEM_EOT: u8 = 0x04;
    const YMODEM_ACK: u8 = 0x06;
    const YMODEM_NAK: u8 = 0x15;
    const YMODEM_CRC_REQUEST: u8 = b'C';

    async fn read_telnet_data_byte(stream: &mut TcpStream) -> u8 {
        loop {
            let mut byte = [0u8; 1];
            stream.read_exact(&mut byte).await.unwrap();
            if byte[0] != IAC {
                return byte[0];
            }

            stream.read_exact(&mut byte).await.unwrap();
            match byte[0] {
                IAC => return IAC,
                WILL | WONT | DO | DONT => {
                    stream.read_exact(&mut byte).await.unwrap();
                }
                SB => loop {
                    stream.read_exact(&mut byte).await.unwrap();
                    if byte[0] == IAC {
                        stream.read_exact(&mut byte).await.unwrap();
                        if byte[0] == SE {
                            break;
                        }
                    }
                },
                _ => {}
            }
        }
    }

    async fn read_ymodem_packet(
        stream: &mut TcpStream,
        control: u8,
        block: u8,
        size: usize,
    ) -> Vec<u8> {
        assert_eq!(read_telnet_data_byte(stream).await, control);
        assert_eq!(read_telnet_data_byte(stream).await, block);
        assert_eq!(read_telnet_data_byte(stream).await, !block);
        let mut data = vec![0u8; size];
        for byte in &mut data {
            *byte = read_telnet_data_byte(stream).await;
        }
        let crc = [
            read_telnet_data_byte(stream).await,
            read_telnet_data_byte(stream).await,
        ];
        assert_eq!(u16::from_be_bytes(crc), crc16(&data));
        data
    }

    async fn receive_test_ymodem(stream: &mut TcpStream) -> (String, Vec<u8>) {
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
            .await
            .unwrap();
        stream.flush().await.unwrap();

        let header = read_ymodem_packet(stream, YMODEM_SOH, 0, HEADER_BLOCK_SIZE).await;
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
        stream
            .write_all(&[YMODEM_ACK, YMODEM_CRC_REQUEST])
            .await
            .unwrap();
        stream.flush().await.unwrap();

        let mut output = Vec::new();
        let mut expected_block = 1u8;
        while output.len() < expected_len {
            let data = read_ymodem_packet(stream, YMODEM_STX, expected_block, BLOCK_SIZE).await;
            let count = (expected_len - output.len()).min(BLOCK_SIZE);
            output.extend_from_slice(&data[..count]);
            stream.write_all(&[YMODEM_ACK]).await.unwrap();
            stream.flush().await.unwrap();
            expected_block = expected_block.wrapping_add(1);
        }

        assert_eq!(read_telnet_data_byte(stream).await, YMODEM_EOT);
        stream.write_all(&[YMODEM_NAK]).await.unwrap();
        stream.flush().await.unwrap();
        assert_eq!(read_telnet_data_byte(stream).await, YMODEM_EOT);
        stream
            .write_all(&[YMODEM_ACK, YMODEM_CRC_REQUEST])
            .await
            .unwrap();
        stream.flush().await.unwrap();

        let final_header = read_ymodem_packet(stream, YMODEM_SOH, 0, HEADER_BLOCK_SIZE).await;
        assert!(final_header.iter().all(|&byte| byte == 0));
        stream.write_all(&[YMODEM_ACK]).await.unwrap();
        stream.flush().await.unwrap();
        (filename, output)
    }

    fn temp_upload_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "microtun-telnet-test-{}-config.ini",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn handles_telnet_binary_negotiation_and_iac_payload() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (filename, payload) = receive_test_ymodem(&mut stream).await;
            assert!(filename.ends_with("config.ini"));
            assert_eq!(payload, b"before\xffafter");
        });

        let path = temp_upload_path();
        tokio::fs::write(&path, b"before\xffafter").await.unwrap();
        let mut client = TelnetClient::connect("127.0.0.1", address.port(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(
            send_file_with(&mut client, &path, Duration::from_secs(1), async |_| Ok(()))
                .await
                .unwrap(),
            12
        );
        tokio::fs::remove_file(path).await.unwrap();
        server.await.unwrap();
    }

    #[test]
    fn cli_timeout_maps_to_one_ymodem_wait() {
        let config = config_from_timeout(Duration::from_secs(7));
        assert_eq!(config.start_timeout_ms, 7_000);
        assert_eq!(config.transfer_timeout_ms, 7_000);
        assert_eq!(config.start_retries, 1);
    }
}
