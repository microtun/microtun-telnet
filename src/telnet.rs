use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpStream, ToSocketAddrs},
    time::Duration,
};

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
const INTERACTIVE_READ_TIMEOUT: Duration = Duration::from_millis(20);

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
}

impl TelnetClient {
    pub(crate) fn connect(target: &str, port: u16, timeout: Duration) -> Result<Self, String> {
        let endpoints = (target, port)
            .to_socket_addrs()
            .map_err(|error| format!("resolve {target}: {error}"))?;
        let mut last_error = None;
        let mut connected = None;

        for endpoint in endpoints {
            match TcpStream::connect_timeout(&endpoint, timeout) {
                Ok(stream) => {
                    connected = Some((stream, endpoint));
                    break;
                }
                Err(error) => last_error = Some((endpoint, error)),
            }
        }

        let (stream, endpoint) = connected.ok_or_else(|| match last_error {
            Some((endpoint, error)) => format!("connect to {endpoint}: {error}"),
            None => format!("resolve {target}: no addresses returned"),
        })?;
        stream
            .set_read_timeout(Some(INTERACTIVE_READ_TIMEOUT))
            .map_err(|error| format!("set read timeout for {endpoint}: {error}"))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|error| format!("set write timeout for {endpoint}: {error}"))?;
        let _ = stream.set_nodelay(true);
        Ok(Self {
            stream,
            rx_state: TelnetRxState::Data,
        })
    }

    pub(crate) fn shutdown(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }

    pub(crate) fn set_read_timeout(&self, timeout: Duration) -> Result<(), String> {
        self.stream
            .set_read_timeout(Some(timeout))
            .map_err(|error| format!("set Telnet read timeout: {error}"))
    }

    pub(crate) fn restore_interactive_timeout(&self) -> Result<(), String> {
        self.set_read_timeout(INTERACTIVE_READ_TIMEOUT)
    }

    pub(crate) fn read_interactive(&mut self) -> Result<NetworkRead, String> {
        let mut wire = [0u8; 4096];
        match self.stream.read(&mut wire) {
            Ok(0) => Ok(NetworkRead::Closed),
            Ok(len) => {
                let mut data = Vec::with_capacity(len);
                for &byte in &wire[..len] {
                    if let Some(byte) = self.process_wire_byte(byte)? {
                        data.push(byte);
                    }
                }
                Ok(NetworkRead::Data(data))
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(NetworkRead::Idle)
            }
            Err(error) if is_remote_disconnect(error.kind()) => Ok(NetworkRead::Closed),
            Err(error) => Err(format!("read Telnet connection: {error}")),
        }
    }

    pub(crate) fn read_data_byte(&mut self) -> Result<u8, String> {
        loop {
            let mut byte = [0u8; 1];
            self.stream
                .read_exact(&mut byte)
                .map_err(|error| format!("read Telnet connection: {error}"))?;
            if let Some(byte) = self.process_wire_byte(byte[0])? {
                return Ok(byte);
            }
        }
    }

    pub(crate) fn write_data(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut encoded = Vec::with_capacity(bytes.len() + 16);
        for &byte in bytes {
            encoded.push(byte);
            if byte == IAC {
                encoded.push(IAC);
            }
        }
        self.stream
            .write_all(&encoded)
            .map_err(|error| format!("write Telnet data: {error}"))
    }

    pub(crate) fn flush(&mut self) -> Result<(), String> {
        self.stream
            .flush()
            .map_err(|error| format!("flush Telnet connection: {error}"))
    }

    fn process_wire_byte(&mut self, byte: u8) -> Result<Option<u8>, String> {
        match self.rx_state {
            TelnetRxState::Data => {
                if byte == IAC {
                    self.rx_state = TelnetRxState::Iac;
                    Ok(None)
                } else {
                    Ok(Some(byte))
                }
            }
            TelnetRxState::Iac => match byte {
                IAC => {
                    self.rx_state = TelnetRxState::Data;
                    Ok(Some(IAC))
                }
                WILL | WONT | DO | DONT => {
                    self.rx_state = TelnetRxState::Negotiation(byte);
                    Ok(None)
                }
                SB => {
                    self.rx_state = TelnetRxState::Subnegotiation;
                    Ok(None)
                }
                _ => {
                    self.rx_state = TelnetRxState::Data;
                    Ok(None)
                }
            },
            TelnetRxState::Negotiation(command) => {
                self.reply_to_negotiation(command, byte)?;
                self.rx_state = TelnetRxState::Data;
                Ok(None)
            }
            TelnetRxState::Subnegotiation => {
                if byte == IAC {
                    self.rx_state = TelnetRxState::SubnegotiationIac;
                }
                Ok(None)
            }
            TelnetRxState::SubnegotiationIac => {
                self.rx_state = if byte == SE {
                    TelnetRxState::Data
                } else {
                    TelnetRxState::Subnegotiation
                };
                Ok(None)
            }
        }
    }

    fn reply_to_negotiation(&mut self, command: u8, option: u8) -> Result<(), String> {
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
            _ => return Ok(()),
        };
        self.stream
            .write_all(&[IAC, reply, option])
            .map_err(|error| format!("write Telnet negotiation response: {error}"))
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
