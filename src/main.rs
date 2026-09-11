use std::{
    io::{self, IsTerminal},
    process::ExitCode,
    time::Duration,
};

use clap::Parser;
mod keymap;
mod telnet;
mod tui;
mod upload;

use telnet::TelnetClient;

const DEFAULT_TELNET_PORT: u16 = 23;

#[derive(Parser)]
#[command(
    name = "microtun-telnet",
    version,
    about = "Interactive Telnet client with YMODEM file upload capabilities",
    arg_required_else_help = true
)]
struct Cli {
    /// Device IP address or hostname.
    #[arg(value_name = "TARGET")]
    target: String,

    /// TCP port. Defaults to 23.
    #[arg(short, long)]
    port: Option<u16>,

    /// Telnet connect and YMODEM transfer timeout in seconds.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..))]
    timeout: u64,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("microtun-telnet requires an interactive terminal".to_owned());
    }

    let port = cli.port.unwrap_or(DEFAULT_TELNET_PORT);
    eprintln!("connecting to {} on port {port}", cli.target);

    let client = TelnetClient::connect(&cli.target, port, Duration::from_secs(cli.timeout))?;
    tui::run_session(client, &cli.target, port, Duration::from_secs(cli.timeout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_direct_connection() {
        let cli = Cli::try_parse_from([
            "microtun-telnet",
            "100.64.0.3",
            "--port",
            "2323",
            "--timeout",
            "20",
        ])
        .unwrap();
        assert_eq!(cli.target, "100.64.0.3");
        assert_eq!(cli.port, Some(2323));
        assert_eq!(cli.timeout, 20);
    }

    #[test]
    fn cli_parses_hostname_connection_target() {
        let cli = Cli::try_parse_from(["microtun-telnet", "device.local"]).unwrap();
        assert_eq!(cli.target, "device.local");
    }
}
