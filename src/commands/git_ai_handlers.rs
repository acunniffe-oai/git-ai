use crate::git::find_repository;
use crate::{commands, config};
use std::io::Read;

pub fn handle_git_ai(args: &[String]) {
    let Some(command) = args.first().map(String::as_str) else {
        print_help();
        return;
    };
    match command {
        "help" | "--help" | "-h" => print_help(),
        "version" | "--version" | "-v" => println!("{}", env!("CARGO_PKG_VERSION")),
        "config" => commands::config::handle_config(&args[1..]),
        "debug" => commands::debug::handle_debug(&args[1..]),
        "bg" | "d" | "daemon" => commands::daemon::handle_daemon(&args[1..]),
        "checkpoint" => handle_checkpoint(&args[1..]),
        "git-path" => println!("{}", config::Config::get().git_cmd()),
        "install-hooks" | "install" => {
            if let Err(error) = commands::install_hooks::run(&args[1..]) {
                eprintln!("Install failed: {error}");
                std::process::exit(1);
            }
        }
        "uninstall" | "uninstall-hooks" => {
            if let Err(error) = commands::install_hooks::run_uninstall(&args[1..]) {
                eprintln!("Uninstall failed: {error}");
                std::process::exit(1);
            }
        }
        "git-hooks" => handle_git_hooks(&args[1..]),
        "upgrade" => commands::upgrade::run_with_args(&args[1..]),
        "await" => commands::r#await::handle_await(&args[1..]),
        "login" => commands::login::handle_login(&args[1..]),
        "logout" => commands::logout::handle_logout(&args[1..]),
        "whoami" => commands::whoami::handle_whoami(&args[1..]),
        "exchange-nonce" => commands::exchange_nonce::handle_exchange_nonce(&args[1..]),
        _ => {
            eprintln!("Unknown git-ai command: {command}");
            std::process::exit(1);
        }
    }
}

fn print_help() {
    eprintln!("git-ai - link agent sessions to commits");
    eprintln!("\nUsage: git-ai <command> [args...]\n");
    eprintln!("  checkpoint <agent> --hook-input <json|stdin>  Record an agent write");
    eprintln!("  install, install-hooks      Configure agent hooks and native telemetry");
    eprintln!("  uninstall, uninstall-hooks  Remove managed hooks");
    eprintln!("  config                      Manage configuration");
    eprintln!("  daemon, bg                  Control the background service");
    eprintln!("  await [beta] [--timeout seconds]   Drain commit links and OTEL delivery");
    eprintln!("  debug                       Show support diagnostics");
    eprintln!("  upgrade                     Update git-ai");
    eprintln!("  git-path                    Show the underlying Git executable");
    eprintln!("  login, logout, whoami       Manage authentication");
    eprintln!("  version, help               Show version or help");
}

fn handle_checkpoint(args: &[String]) {
    let Some(preset) = args.first() else {
        return;
    };
    let Some(input_index) = args.iter().position(|arg| arg == "--hook-input") else {
        eprintln!("checkpoint requires --hook-input <json|stdin>");
        return;
    };
    let Some(input) = args.get(input_index + 1) else {
        return;
    };
    let input = if input == "stdin" {
        let mut bytes = Vec::new();
        if std::io::stdin().read_to_end(&mut bytes).is_err() {
            return;
        }
        match decode_hook_input_bytes(bytes) {
            Ok(input) => strip_utf8_bom(input),
            Err(error) => {
                eprintln!("Invalid hook input: {error}");
                return;
            }
        }
    } else {
        strip_utf8_bom(input.clone())
    };
    let requests =
        match commands::checkpoint_agent::orchestrator::execute_preset_checkpoint(preset, &input) {
            Ok(requests) => requests,
            Err(error) => {
                eprintln!("Checkpoint skipped: {error}");
                return;
            }
        };
    let mut sent = 0;
    for request in requests {
        match crate::daemon::send_link_checkpoint_request(&request) {
            Ok(()) => sent += 1,
            Err(error) => {
                eprintln!("Checkpoint skipped: {error}");
                break;
            }
        }
    }
    if std::env::var_os("GIT_AI_TEST_DB_PATH").is_some() {
        println!("checkpoint_requests={sent}");
    }
}

fn strip_utf8_bom(input: String) -> String {
    if let Some(stripped) = input.strip_prefix('\u{feff}') {
        stripped.to_string()
    } else {
        input
    }
}

fn decode_hook_input_bytes(bytes: Vec<u8>) -> Result<String, String> {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return decode_utf16_hook_input(&bytes[2..], Utf16Endian::Little);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return decode_utf16_hook_input(&bytes[2..], Utf16Endian::Big);
    }

    match likely_utf16_endian(&bytes) {
        Some(endian) => decode_utf16_hook_input(&bytes, endian),
        None => String::from_utf8(bytes).map_err(|e| e.to_string()),
    }
}

#[derive(Clone, Copy)]
enum Utf16Endian {
    Little,
    Big,
}

fn likely_utf16_endian(bytes: &[u8]) -> Option<Utf16Endian> {
    let sample_len = bytes.len().min(512);
    if sample_len < 8 {
        return None;
    }

    let sample = &bytes[..sample_len];
    let even_nuls = sample.iter().step_by(2).filter(|&&b| b == 0).count();
    let odd_nuls = sample
        .iter()
        .skip(1)
        .step_by(2)
        .filter(|&&b| b == 0)
        .count();
    let min_nuls = sample_len / 8;

    if odd_nuls > min_nuls && odd_nuls > even_nuls.saturating_mul(4) {
        Some(Utf16Endian::Little)
    } else if even_nuls > min_nuls && even_nuls > odd_nuls.saturating_mul(4) {
        Some(Utf16Endian::Big)
    } else {
        None
    }
}

fn decode_utf16_hook_input(bytes: &[u8], endian: Utf16Endian) -> Result<String, String> {
    let (chunks, remainder) = bytes.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err("UTF-16 hook input has an odd byte length".to_string());
    }

    let code_units = chunks.iter().map(|chunk| match endian {
        Utf16Endian::Little => u16::from_le_bytes([chunk[0], chunk[1]]),
        Utf16Endian::Big => u16::from_be_bytes([chunk[0], chunk[1]]),
    });

    String::from_utf16(&code_units.collect::<Vec<u16>>()).map_err(|e| e.to_string())
}

fn handle_git_hooks(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("remove") | Some("uninstall") => {
            let repo = match find_repository(&Vec::<String>::new()) {
                Ok(repo) => repo,
                Err(e) => {
                    eprintln!("Failed to find repository: {}", e);
                    std::process::exit(1);
                }
            };

            match commands::git_hook_handlers::remove_repo_hooks(&repo, false) {
                Ok(report) => {
                    let status = if report.changed { "removed" } else { "ok" };
                    println!(
                        "repo hooks {}: {}",
                        status,
                        report.managed_hooks_path.to_string_lossy()
                    );
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("Failed to remove repo hooks: {}", e);
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("The git core hooks feature has been sunset.");
            eprintln!("Usage: git-ai git-hooks remove");
            std::process::exit(1);
        }
    }
}
