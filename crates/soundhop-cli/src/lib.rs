//! `soundhop-cli` library exports and main entrypoint dispatcher.

pub mod android_cmds;
pub mod core_cmds;
pub mod doctor;
pub mod parser;
pub mod tui;
pub mod ui;

use std::io::IsTerminal;
use std::sync::Arc;

use clap::Parser;
use soundhop_core::controller::Controller;
use soundhop_gui::{run_standalone_android_wizard, run_standalone_settings, run_tray};

use crate::android_cmds::handle_android;
use crate::core_cmds::{
    handle_apps, handle_auto, handle_completions, handle_config, handle_connect, handle_daemon,
    handle_disconnect, handle_discover, handle_firewall, handle_logs, handle_mic, handle_pair,
    handle_route, handle_service, handle_setup, handle_share, handle_status, handle_stop,
    handle_unroute, handle_version, handle_volume,
};
use crate::doctor::{run_bug_report, run_doctor};
use crate::parser::{Cli, Commands};
use crate::tui::run_tui;
use crate::ui::{error, set_no_color};

pub fn run_cli() -> i32 {
    let cli = Cli::parse();
    set_no_color(cli.no_color);

    if cli.verbose {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .try_init();
    }

    if cli.settings_dialog {
        return match run_standalone_settings() {
            Ok(code) => code,
            Err(e) => {
                error(&e);
                1
            }
        };
    }

    if cli.android_wizard {
        return match run_standalone_android_wizard() {
            Ok(code) => code,
            Err(e) => {
                error(&e);
                1
            }
        };
    }

    let controller = Arc::new(Controller::new(None, None, None, None, None, !cli.json));

    match cli.command {
        Some(Commands::Tui) => match run_tui(controller) {
            Ok(code) => code,
            Err(e) => {
                error(&e);
                1
            }
        },
        Some(Commands::Setup(ref args)) => handle_setup(&controller, args, cli.json),
        Some(Commands::Status) => handle_status(&controller, cli.json),
        Some(Commands::Discover(ref args)) => handle_discover(&controller, args, cli.json),
        Some(Commands::Connect(ref args)) => handle_connect(&controller, args, cli.json),
        Some(Commands::Disconnect) => handle_disconnect(&controller, cli.json),
        Some(Commands::Share(ref args)) => handle_share(&controller, args, cli.json),
        Some(Commands::Pair(ref args)) => handle_pair(&controller, args, cli.json),
        Some(Commands::Apps) => handle_apps(&controller, cli.json),
        Some(Commands::Route(ref args)) => handle_route(&controller, args, cli.json),
        Some(Commands::Unroute(ref args)) => handle_unroute(&controller, args, cli.json),
        Some(Commands::Mic(ref args)) => handle_mic(&controller, args, cli.json),
        Some(Commands::Volume(ref args)) => handle_volume(&controller, args, cli.json),
        Some(Commands::Auto(ref args)) => handle_auto(&controller, args, cli.json),
        Some(Commands::Daemon) => handle_daemon(cli.verbose),
        Some(Commands::Stop) => handle_stop(&controller, cli.json),
        Some(Commands::Service(ref args)) => handle_service(args, cli.json),
        Some(Commands::Firewall(ref args)) => handle_firewall(&controller, args, cli.json),
        Some(Commands::Config(ref args)) => handle_config(&controller, args, cli.json),
        Some(Commands::Doctor(ref args)) => run_doctor(&controller, args.fix, cli.json),
        Some(Commands::Tray) => match run_tray(cli.verbose) {
            Ok(code) => code,
            Err(e) => {
                error(&e);
                1
            }
        },
        Some(Commands::Logs(ref args)) => handle_logs(&controller, args, cli.json),
        Some(Commands::BugReport) => run_bug_report(&controller, cli.json),
        Some(Commands::Completions(ref args)) => handle_completions(args),
        Some(Commands::Android(ref args)) => handle_android(&controller, args, cli.json),
        Some(Commands::Version) => handle_version(cli.json),
        None => {
            if !cli.json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                match run_tui(controller) {
                    Ok(code) => code,
                    Err(e) => {
                        error(&e);
                        1
                    }
                }
            } else {
                handle_status(&controller, cli.json)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_cli_parse_subcommands() {
        let c1 = Cli::try_parse_from([
            "soundhop",
            "connect",
            "Bedroom",
            "Kitchen",
            "--combine",
            "--keep-local",
        ])
        .unwrap();
        match c1.command {
            Some(Commands::Connect(args)) => {
                assert_eq!(args.targets, vec!["Bedroom", "Kitchen"]);
                assert!(args.combine);
                assert!(args.keep_local);
            }
            _ => panic!("Expected Connect command"),
        }

        let c2 = Cli::try_parse_from(["soundhop", "route", "Spotify", "--save"]).unwrap();
        match c2.command {
            Some(Commands::Route(args)) => {
                assert_eq!(args.app, "Spotify");
                assert!(args.save);
            }
            _ => panic!("Expected Route command"),
        }

        let c3 =
            Cli::try_parse_from(["soundhop", "pair", "LivingRoom", "--pin", "123456"]).unwrap();
        match c3.command {
            Some(Commands::Pair(args)) => {
                assert_eq!(args.target.as_deref(), Some("LivingRoom"));
                assert_eq!(args.pin.as_deref(), Some("123456"));
            }
            _ => panic!("Expected Pair command"),
        }
    }
}
