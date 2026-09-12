use std::{collections::VecDeque, io, time::Duration};

use microtun_ymodem::{ReadError, Transport};
use tokio::{io::AsyncWriteExt, net::TcpStream, time};

const IAC: u8 = 0xff;
const WILL: u8 = 0xfb;
const WONT: u8 = 0xfc;
const DO: u8 = 0xfd;
const DONT: u8 = 0xfe;
const SB: u8 = 0xfa;
const SE: u8 = 0xf0;
const TELNET_BINARY: u8 = 0;
const TELNET_ECHO: u8 = 1;
const TELNET_SUPPRESS_GO_AHEAD: u8 = 3;

#[derive(Clone, Copy)]
enum TelnetRxState {
    Data,
    Iac,
    Negotiation(u8),
    Subnegotiation,
    SubnegotiationIac,
}

pub(crate) struct TelnetClient {
    stream: TcpStream,
    rx_state: TelnetRxState,
    decoded: VecDeque<u8>,
    negotiation_replies: Vec<u8>,
}

impl TelnetClient {
    pub(crate) async fn connect(
        target: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<Self, String> {
        let stream = match time::timeout(timeout, TcpStream::connect((target, port))).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => return Err(format!("connect to {target}:{port}: {error}")),
            Err(_) => return Err(format!("connect to {target}:{port}: timed out")),
        };
        stream
            .set_nodelay(true)
            .map_err(|error| format!("set TCP_NODELAY for {target}:{port}: {error}"))?;

        Ok(Self {
            stream,
            rx_state: TelnetRxState::Data,
            decoded: VecDeque::new(),
            negotiation_replies: Vec::new(),
        })
    }

    pub(crate) async fn shutdown(&mut self) {
        let _ = self.stream.shutdown().await;
    }

    pub(crate) async fn read_interactive(&mut self) -> Result<NetworkRead, String> {
        if !self.decoded.is_empty() {
            return Ok(NetworkRead::Data(self.take_decoded()));
        }

        self.stream
            .readable()
            .await
            .map_err(|error| format!("wait for Telnet data: {error}"))?;

        let mut wire = [0u8; 4096];
        match self.stream.try_read(&mut wire) {
            Ok(0) => Ok(NetworkRead::Closed),
            Ok(len) => {
                self.process_wire_bytes(&wire[..len]);
                Ok(NetworkRead::Data(self.take_decoded()))
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(NetworkRead::Idle),
            Err(error) if is_remote_disconnect(error.kind()) => Ok(NetworkRead::Closed),
            Err(error) => Err(format!("read Telnet connection: {error}")),
        }
    }

    pub(crate) async fn flush_negotiation(&mut self) -> Result<(), String> {
        if self.negotiation_replies.is_empty() {
            return Ok(());
        }

        let replies = std::mem::take(&mut self.negotiation_replies);
        self.stream
            .write_all(&replies)
            .await
            .map_err(|error| format!("write Telnet negotiation response: {error}"))
    }

    pub(crate) async fn write_data(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut encoded = Vec::with_capacity(bytes.len() + 16);
        for &byte in bytes {
            encoded.push(byte);
            if byte == IAC {
                encoded.push(IAC);
            }
        }
        self.stream
            .write_all(&encoded)
            .await
            .map_err(|error| format!("write Telnet data: {error}"))
    }

    async fn flush_data(&mut self) -> Result<(), String> {
        self.stream
            .flush()
            .await
            .map_err(|error| format!("flush Telnet connection: {error}"))
    }

    fn process_wire_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if let Some(byte) = self.process_wire_byte(byte) {
                self.decoded.push_back(byte);
            }
        }
    }

    fn process_wire_byte(&mut self, byte: u8) -> Option<u8> {
        match self.rx_state {
            TelnetRxState::Data => {
                if byte == IAC {
                    self.rx_state = TelnetRxState::Iac;
                    None
                } else {
                    Some(byte)
                }
            }
            TelnetRxState::Iac => match byte {
                IAC => {
                    self.rx_state = TelnetRxState::Data;
                    Some(IAC)
                }
                WILL | WONT | DO | DONT => {
                    self.rx_state = TelnetRxState::Negotiation(byte);
                    None
                }
                SB => {
                    self.rx_state = TelnetRxState::Subnegotiation;
                    None
                }
                _ => {
                    self.rx_state = TelnetRxState::Data;
                    None
                }
            },
            TelnetRxState::Negotiation(command) => {
                self.queue_negotiation_reply(command, byte);
                self.rx_state = TelnetRxState::Data;
                None
            }
            TelnetRxState::Subnegotiation => {
                if byte == IAC {
                    self.rx_state = TelnetRxState::SubnegotiationIac;
                }
                None
            }
            TelnetRxState::SubnegotiationIac => {
                self.rx_state = if byte == SE {
                    TelnetRxState::Data
                } else {
                    TelnetRxState::Subnegotiation
                };
                None
            }
        }
    }

    fn queue_negotiation_reply(&mut self, command: u8, option: u8) {
        let accepted = matches!(
            option,
            TELNET_BINARY | TELNET_ECHO | TELNET_SUPPRESS_GO_AHEAD
        );
        let reply = match command {
            WILL if accepted => DO,
            WILL => DONT,
            DO if accepted => WILL,
            DO => WONT,
            WONT => DONT,
            DONT => WONT,
            _ => return,
        };
        self.negotiation_replies
            .extend_from_slice(&[IAC, reply, option]);
    }

    fn take_decoded(&mut self) -> Vec<u8> {
        self.decoded.drain(..).collect()
    }

    async fn wait_for_wire_readable(&self, timeout_ms: u32) -> Result<(), ReadError<String>> {
        match time::timeout(
            Duration::from_millis(u64::from(timeout_ms)),
            self.stream.readable(),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(ReadError::Io(format!("wait for Telnet data: {error}"))),
            Err(_) => Err(ReadError::Timeout),
        }
    }
}

impl Transport for TelnetClient {
    type Error = String;

    async fn read_byte(&mut self, timeout_ms: u32) -> Result<u8, ReadError<Self::Error>> {
        if let Some(byte) = self.decoded.pop_front() {
            return Ok(byte);
        }

        loop {
            self.wait_for_wire_readable(timeout_ms).await?;

            let mut wire = [0u8; 4096];
            match self.stream.try_read(&mut wire) {
                Ok(0) => {
                    return Err(ReadError::Io(
                        "Telnet connection closed by remote host".to_owned(),
                    ));
                }
                Ok(len) => self.process_wire_bytes(&wire[..len]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) => {
                    return Err(ReadError::Io(format!("read Telnet connection: {error}")));
                }
            }

            self.flush_negotiation().await.map_err(ReadError::Io)?;
            if let Some(byte) = self.decoded.pop_front() {
                return Ok(byte);
            }
        }
    }

    async fn write_all(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        self.write_data(bytes).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.flush_data().await
    }
}

pub(crate) enum NetworkRead {
    Data(Vec<u8>),
    Idle,
    Closed,
}

fn is_remote_disconnect(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_disconnect_errors_are_clean_close_conditions() {
        for kind in [
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::NotConnected,
            io::ErrorKind::UnexpectedEof,
        ] {
            assert!(is_remote_disconnect(kind));
        }
        assert!(!is_remote_disconnect(io::ErrorKind::Interrupted));
        assert!(!is_remote_disconnect(io::ErrorKind::PermissionDenied));
    }
}
