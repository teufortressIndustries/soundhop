//! Core CLI subcommand handlers (`setup`, `status`, `discover`, `connect`, `disconnect`,
//! `share`, `pair`, `apps`, `route`, `unroute`, `mic`, `volume`, `auto`, `daemon`, `stop`,
//! `service`, `firewall`, `config`, `logs`, `completions`, `web`, `version`).

mod config;
mod connect;
mod routing;
mod share_pair;
mod system;
mod web;

use std::time::Duration;

use soundhop_core::ipc::{is_running as ipc_is_running, request as ipc_request, IpcError};

pub use config::{handle_config, handle_logs};
pub use connect::{handle_auto, handle_connect, handle_disconnect, handle_discover, handle_status};
pub use routing::{
    handle_apps, handle_mic, handle_route, handle_unroute, handle_volume, parse_volume_input,
};
pub use share_pair::{handle_pair, handle_share};
pub use system::{
    handle_completions, handle_daemon, handle_firewall, handle_service, handle_setup, handle_stop,
    handle_version,
};
pub(crate) use web::format_bytes;
pub use web::handle_web;

pub(crate) fn try_ipc(
    command: &str,
    payload: serde_json::Value,
    timeout: Duration,
) -> Option<Result<serde_json::Value, String>> {
    if !ipc_is_running(None) {
        return None;
    }
    match ipc_request(command, payload, None, timeout) {
        Ok(resp) => Some(Ok(resp)),
        Err(IpcError::DaemonNotRunning) => None,
        Err(IpcError::Error(e)) => Some(Err(e)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use soundhop_core::backend::linux::LinuxAudioBackend;
    use soundhop_core::config::Config;
    use soundhop_core::controller::Controller;
    use soundhop_core::state::State;
    use soundhop_core::util::{CommandResult, MockRunner};

    use super::*;
    use crate::parser::{AutoArgs, ConfigArgs, FirewallArgs, MicArgs, ServiceArgs};

    pub(super) fn test_controller() -> Arc<Controller> {
        let runner = Arc::new(MockRunner::new(|args, _| {
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }));
        let backend = Arc::new(LinuxAudioBackend::with_runner(runner));
        let ctrl = Arc::new(Controller::new(
            Some(Config::default()),
            Some(State::default()),
            Some(backend),
            None,
            None,
            false,
        ));
        ctrl.set_save_enabled(false);
        ctrl
    }

    #[test]
    fn test_unknown_actions_return_nonzero() {
        let ctrl = test_controller();
        assert_eq!(
            handle_mic(
                &ctrl,
                &MicArgs {
                    mode: "bogus".to_string(),
                    target: None,
                    source: None,
                    latency: None,
                },
                true,
            ),
            1
        );
        assert_eq!(
            handle_auto(
                &ctrl,
                &AutoArgs {
                    action: "bogus".to_string(),
                    peer: None,
                },
                true,
            ),
            1
        );
        assert_eq!(
            handle_service(
                &ServiceArgs {
                    action: "bogus".to_string(),
                    no_tray: false,
                },
                true,
            ),
            1
        );
        assert_eq!(
            handle_firewall(
                &ctrl,
                &FirewallArgs {
                    action: "bogus".to_string(),
                },
                true,
            ),
            1
        );
        assert_eq!(
            handle_config(
                &ctrl,
                &ConfigArgs {
                    action: "bogus".to_string(),
                    key: None,
                    value: None,
                },
                true,
            ),
            1
        );
    }
}
