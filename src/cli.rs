/// CLI argument parser for pg-tikv server.
///
/// This module provides pure argument parsing without side effects.
/// It does not read environment variables or call process::exit.

/// Parsed CLI arguments. All fields are Option so we can distinguish
/// "not provided" from "provided" for merge with env vars.
#[derive(Debug, Clone, PartialEq)]
pub struct CliArgs {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub pd_endpoints: Option<String>,
    pub keyspace: Option<String>,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
}

/// What the CLI wants us to do.
#[derive(Debug, Clone, PartialEq)]
pub enum CliAction {
    ShowHelp,
    ShowVersion,
    Run(CliArgs),
}

/// Parse command-line arguments into a CliAction.
///
/// # Arguments
/// - `args`: Full argument list including binary name at index 0
///
/// # Returns
/// - `Ok(CliAction::ShowHelp)` if --help or -h is encountered
/// - `Ok(CliAction::ShowVersion)` if --version or -V is encountered
/// - `Ok(CliAction::Run(args))` if no help/version flags
/// - `Err(msg)` if parsing fails
///
/// # Parsing Rules
/// - Supports `--flag value` and `--flag=value` syntax
/// - `--` stops parsing (remaining args ignored)
/// - Unknown flags (starting with `-`) are errors
/// - Positional args (not starting with `-`) are errors
/// - Repeated flags: last value wins
pub fn parse_args(args: &[String]) -> Result<CliAction, String> {
    if args.is_empty() {
        return Ok(CliAction::Run(CliArgs {
            host: None,
            port: None,
            pd_endpoints: None,
            keyspace: None,
            tls_cert: None,
            tls_key: None,
        }));
    }

    let mut cli_args = CliArgs {
        host: None,
        port: None,
        pd_endpoints: None,
        keyspace: None,
        tls_cert: None,
        tls_key: None,
    };

    let mut i = 1; // Skip binary name at args[0]
    while i < args.len() {
        let arg = &args[i];

        // Stop parsing at --
        if arg == "--" {
            break;
        }

        // Help flags
        if arg == "--help" || arg == "-h" {
            return Ok(CliAction::ShowHelp);
        }

        // Version flags
        if arg == "--version" || arg == "-V" {
            return Ok(CliAction::ShowVersion);
        }

        // Value flags with = syntax
        if arg.contains('=') {
            let parts: Vec<&str> = arg.splitn(2, '=').collect();
            if parts.len() == 2 {
                let flag = parts[0];
                let value = parts[1];

                match flag {
                    "--host" => cli_args.host = Some(value.to_string()),
                    "--port" => {
                        cli_args.port = Some(parse_port(value)?);
                    }
                    "--pd-endpoints" => cli_args.pd_endpoints = Some(value.to_string()),
                    "--keyspace" => cli_args.keyspace = Some(value.to_string()),
                    "--tls-cert" => cli_args.tls_cert = Some(value.to_string()),
                    "--tls-key" => cli_args.tls_key = Some(value.to_string()),
                    _ => return Err(format!("Unknown option: '{}'", flag)),
                }
                i += 1;
                continue;
            }
        }

        // Value flags with space syntax
        match arg.as_str() {
            "--host" | "--port" | "--pd-endpoints" | "--keyspace" | "--tls-cert" | "--tls-key" => {
                if i + 1 >= args.len() {
                    return Err(format!("Option '{}' requires a value", arg));
                }
                let value = &args[i + 1];
                match arg.as_str() {
                    "--host" => cli_args.host = Some(value.clone()),
                    "--port" => {
                        cli_args.port = Some(parse_port(value)?);
                    }
                    "--pd-endpoints" => cli_args.pd_endpoints = Some(value.clone()),
                    "--keyspace" => cli_args.keyspace = Some(value.clone()),
                    "--tls-cert" => cli_args.tls_cert = Some(value.clone()),
                    "--tls-key" => cli_args.tls_key = Some(value.clone()),
                    _ => unreachable!(),
                }
                i += 2;
                continue;
            }
            _ => {}
        }

        // Unknown flag
        if arg.starts_with('-') {
            return Err(format!("Unknown option: '{}'", arg));
        }

        // Positional argument
        return Err(format!("Unexpected argument: '{}'", arg));
    }

    Ok(CliAction::Run(cli_args))
}

/// Parse a port number from a string.
fn parse_port(s: &str) -> Result<u16, String> {
    s.parse::<u16>().map_err(|_| {
        format!(
            "Invalid value for '--port': '{}' (expected a port number 0-65535)",
            s
        )
    })
}

/// Print help message to stdout.
pub fn print_help() {
    println!("pg-tikv - PostgreSQL-compatible distributed SQL database on TiKV");
    println!();
    println!("USAGE:");
    println!("    pg-tikv [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("    -h, --help                     Print this help message and exit");
    println!("    -V, --version                  Print version information and exit");
    println!("        --host <ADDR>              Listen address [default: 127.0.0.1] [env: PG_LISTEN_ADDR]");
    println!("        --port <PORT>              Listen port [default: 5433] [env: PG_PORT]");
    println!("        --pd-endpoints <ENDPOINTS> PD endpoints, comma-separated [default: 127.0.0.1:2379] [env: PD_ENDPOINTS]");
    println!("        --keyspace <NAME>          Default TiKV keyspace for multi-tenancy [env: PG_KEYSPACE]");
    println!("        --tls-cert <PATH>          Path to TLS certificate file [env: PG_TLS_CERT]");
    println!("        --tls-key <PATH>           Path to TLS private key file [env: PG_TLS_KEY]");
    println!();
    println!("ENVIRONMENT VARIABLES:");
    println!("    Additional configuration is available via environment variables.");
    println!("    CLI flags take precedence over environment variables.");
    println!("    See documentation for the full list.");
}

/// Print version information to stdout.
pub fn print_version() {
    println!("pg-tikv {}", env!("CARGO_PKG_VERSION"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_no_args() {
        let result = parse_args(&args(&["pg-tikv"])).unwrap();
        match result {
            CliAction::Run(cli_args) => {
                assert_eq!(cli_args.host, None);
                assert_eq!(cli_args.port, None);
                assert_eq!(cli_args.pd_endpoints, None);
                assert_eq!(cli_args.keyspace, None);
                assert_eq!(cli_args.tls_cert, None);
                assert_eq!(cli_args.tls_key, None);
            }
            _ => panic!("Expected Run action"),
        }
    }

    #[test]
    fn test_help_long() {
        let result = parse_args(&args(&["pg-tikv", "--help"])).unwrap();
        assert_eq!(result, CliAction::ShowHelp);
    }

    #[test]
    fn test_help_short() {
        let result = parse_args(&args(&["pg-tikv", "-h"])).unwrap();
        assert_eq!(result, CliAction::ShowHelp);
    }

    #[test]
    fn test_version_long() {
        let result = parse_args(&args(&["pg-tikv", "--version"])).unwrap();
        assert_eq!(result, CliAction::ShowVersion);
    }

    #[test]
    fn test_version_short() {
        let result = parse_args(&args(&["pg-tikv", "-V"])).unwrap();
        assert_eq!(result, CliAction::ShowVersion);
    }

    #[test]
    fn test_port_space() {
        let result = parse_args(&args(&["pg-tikv", "--port", "1234"])).unwrap();
        match result {
            CliAction::Run(cli_args) => {
                assert_eq!(cli_args.port, Some(1234));
            }
            _ => panic!("Expected Run action"),
        }
    }

    #[test]
    fn test_port_equals() {
        let result = parse_args(&args(&["pg-tikv", "--port=5555"])).unwrap();
        match result {
            CliAction::Run(cli_args) => {
                assert_eq!(cli_args.port, Some(5555));
            }
            _ => panic!("Expected Run action"),
        }
    }

    #[test]
    fn test_host() {
        let result = parse_args(&args(&["pg-tikv", "--host", "0.0.0.0"])).unwrap();
        match result {
            CliAction::Run(cli_args) => {
                assert_eq!(cli_args.host, Some("0.0.0.0".to_string()));
            }
            _ => panic!("Expected Run action"),
        }
    }

    #[test]
    fn test_host_equals() {
        let result = parse_args(&args(&["pg-tikv", "--host=::1"])).unwrap();
        match result {
            CliAction::Run(cli_args) => {
                assert_eq!(cli_args.host, Some("::1".to_string()));
            }
            _ => panic!("Expected Run action"),
        }
    }

    #[test]
    fn test_pd_endpoints() {
        let result = parse_args(&args(&["pg-tikv", "--pd-endpoints", "a:1,b:2"])).unwrap();
        match result {
            CliAction::Run(cli_args) => {
                assert_eq!(cli_args.pd_endpoints, Some("a:1,b:2".to_string()));
            }
            _ => panic!("Expected Run action"),
        }
    }

    #[test]
    fn test_keyspace() {
        let result = parse_args(&args(&["pg-tikv", "--keyspace", "myapp"])).unwrap();
        match result {
            CliAction::Run(cli_args) => {
                assert_eq!(cli_args.keyspace, Some("myapp".to_string()));
            }
            _ => panic!("Expected Run action"),
        }
    }

    #[test]
    fn test_tls_both() {
        let result = parse_args(&args(&[
            "pg-tikv",
            "--tls-cert",
            "c.pem",
            "--tls-key",
            "k.pem",
        ]))
        .unwrap();
        match result {
            CliAction::Run(cli_args) => {
                assert_eq!(cli_args.tls_cert, Some("c.pem".to_string()));
                assert_eq!(cli_args.tls_key, Some("k.pem".to_string()));
            }
            _ => panic!("Expected Run action"),
        }
    }

    #[test]
    fn test_tls_cert_only() {
        let result = parse_args(&args(&["pg-tikv", "--tls-cert", "c.pem"])).unwrap();
        match result {
            CliAction::Run(cli_args) => {
                assert_eq!(cli_args.tls_cert, Some("c.pem".to_string()));
                assert_eq!(cli_args.tls_key, None);
            }
            _ => panic!("Expected Run action"),
        }
    }

    #[test]
    fn test_all_flags() {
        let result = parse_args(&args(&[
            "pg-tikv",
            "--host",
            "localhost",
            "--port",
            "9999",
            "--pd-endpoints",
            "pd1:2379,pd2:2379",
            "--keyspace",
            "tenant1",
            "--tls-cert",
            "cert.pem",
            "--tls-key",
            "key.pem",
        ]))
        .unwrap();
        match result {
            CliAction::Run(cli_args) => {
                assert_eq!(cli_args.host, Some("localhost".to_string()));
                assert_eq!(cli_args.port, Some(9999));
                assert_eq!(cli_args.pd_endpoints, Some("pd1:2379,pd2:2379".to_string()));
                assert_eq!(cli_args.keyspace, Some("tenant1".to_string()));
                assert_eq!(cli_args.tls_cert, Some("cert.pem".to_string()));
                assert_eq!(cli_args.tls_key, Some("key.pem".to_string()));
            }
            _ => panic!("Expected Run action"),
        }
    }

    #[test]
    fn test_unknown_flag() {
        let result = parse_args(&args(&["pg-tikv", "--banana"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown"));
    }

    #[test]
    fn test_missing_value() {
        let result = parse_args(&args(&["pg-tikv", "--port"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("requires a value"));
    }

    #[test]
    fn test_invalid_port() {
        let result = parse_args(&args(&["pg-tikv", "--port", "abc"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid"));
    }

    #[test]
    fn test_last_wins() {
        let result = parse_args(&args(&["pg-tikv", "--port", "1", "--port", "2"])).unwrap();
        match result {
            CliAction::Run(cli_args) => {
                assert_eq!(cli_args.port, Some(2));
            }
            _ => panic!("Expected Run action"),
        }
    }

    #[test]
    fn test_double_dash() {
        let result = parse_args(&args(&["pg-tikv", "--", "--port", "99"])).unwrap();
        match result {
            CliAction::Run(cli_args) => {
                assert_eq!(cli_args.port, None);
            }
            _ => panic!("Expected Run action"),
        }
    }

    #[test]
    fn test_unexpected_positional() {
        let result = parse_args(&args(&["pg-tikv", "something"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unexpected"));
    }
}
