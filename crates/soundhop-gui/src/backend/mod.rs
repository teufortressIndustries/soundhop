//! Non-blocking background worker and plain-language state snapshot for the Control Center GUI.

mod commands;
mod optimistic;
mod snapshot;
mod worker;

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui;
use soundhop_core::controller::Controller;

use crate::{lock_or_recover, read_or_recover, write_or_recover};

pub use commands::GuiCommand;
pub use snapshot::{security_friendly_label, AppStreamView, GuiSnapshot, GuiToast, PeerView};

use commands::is_mutating_command;
use optimistic::apply_optimistic_update;
use worker::WorkerState;

pub struct GuiBackend {
    pub snapshot: Arc<RwLock<GuiSnapshot>>,
    tx: Option<mpsc::Sender<GuiCommand>>,
    toasts: Arc<Mutex<Vec<GuiToast>>>,
    ctx: Option<egui::Context>,
    mutation_seq: Arc<AtomicU64>,
    active_commands: Arc<AtomicUsize>,
    stop_flag: Arc<AtomicBool>,
    quit_flag: Arc<AtomicBool>,
    poller_wake: Arc<(Mutex<bool>, Condvar)>,
    cmd_handle: Option<JoinHandle<()>>,
    poller_handle: Option<JoinHandle<()>>,
}

impl GuiBackend {
    pub fn start(controller: Arc<Controller>, ctx: Option<egui::Context>) -> Self {
        Self::start_with_toast(controller, ctx, None)
    }

    pub fn start_with_toast(
        controller: Arc<Controller>,
        ctx: Option<egui::Context>,
        initial_toast: Option<String>,
    ) -> Self {
        let initial = GuiSnapshot::from_controller(&controller);
        let snapshot = Arc::new(RwLock::new(initial));
        let toasts = Arc::new(Mutex::new(Vec::new()));
        if let Some(msg) = initial_toast {
            lock_or_recover(&toasts).push(GuiToast {
                message: msg,
                is_error: false,
                created_at: Instant::now(),
            });
        }
        let (tx, rx) = mpsc::channel::<GuiCommand>();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let quit_flag = Arc::new(AtomicBool::new(false));
        let onboarding_reopened = Arc::new(AtomicBool::new(false));
        let mutation_seq = Arc::new(AtomicU64::new(0));
        let active_commands = Arc::new(AtomicUsize::new(0));
        let cached_live_peers = Arc::new(Mutex::new(Vec::new()));
        let controller_sync_lock = Arc::new(Mutex::new(()));
        let poller_wake = Arc::new((Mutex::new(false), Condvar::new()));

        let cmd_worker = WorkerState {
            controller: Arc::clone(&controller),
            snapshot: Arc::clone(&snapshot),
            toasts: Arc::clone(&toasts),
            ctx: ctx.clone(),
            cached_live_peers: Arc::clone(&cached_live_peers),
            mutation_seq: Arc::clone(&mutation_seq),
            active_commands: Arc::clone(&active_commands),
            onboarding_reopened: Arc::clone(&onboarding_reopened),
            quit_flag: Arc::clone(&quit_flag),
            controller_sync_lock: Arc::clone(&controller_sync_lock),
            use_ipc: true,
        };

        let mut poller_worker = WorkerState {
            controller: Arc::clone(&controller),
            snapshot: Arc::clone(&snapshot),
            toasts: Arc::clone(&toasts),
            ctx: ctx.clone(),
            cached_live_peers,
            mutation_seq: Arc::clone(&mutation_seq),
            active_commands: Arc::clone(&active_commands),
            onboarding_reopened: Arc::clone(&onboarding_reopened),
            quit_flag: Arc::clone(&quit_flag),
            controller_sync_lock,
            use_ipc: true,
        };

        let cmd_handle = thread::spawn(move || {
            let mut worker = cmd_worker;
            while let Ok(cmd) = rx.recv() {
                let mutating = is_mutating_command(&cmd);
                worker.handle_command(cmd);
                if mutating {
                    worker.active_commands.fetch_sub(1, Ordering::SeqCst);
                }
                worker.request_repaint();
            }
        });

        let stop_poller = Arc::clone(&stop_flag);
        let quit_poller = Arc::clone(&quit_flag);
        let poller_ctx = ctx.clone();
        let wake_poller = Arc::clone(&poller_wake);
        let poller_handle = thread::spawn(move || {
            let mut telemetry_monitor =
                if controller.is_save_enabled() && !soundhop_core::ipc::is_running(None) {
                    Some(controller.start_telemetry_monitor())
                } else {
                    None
                };

            poller_worker.refresh_fast(false);
            poller_worker.refresh_peers();
            poller_worker.refresh_android();
            poller_worker.refresh_system();
            poller_worker.request_repaint();

            let mut next_fast_due = Instant::now() + Duration::from_millis(3000);
            let mut next_peers_due = Instant::now() + Duration::from_secs(6);

            while !stop_poller.load(Ordering::Relaxed)
                && !soundhop_core::util::signal_stop_requested()
                && !quit_poller.load(Ordering::Acquire)
            {
                let now = Instant::now();
                let next_due = next_fast_due.min(next_peers_due);
                if now < next_due {
                    let wait_dur = next_due.saturating_duration_since(now);
                    let (lock, cvar) = &*wake_poller;
                    let guard = lock_or_recover(lock);
                    if !*guard {
                        let _ = cvar.wait_timeout(guard, wait_dur);
                    }
                }
                if soundhop_core::util::signal_stop_requested()
                    || quit_poller.load(Ordering::Acquire)
                {
                    if let Some(ref c) = poller_ctx {
                        c.send_viewport_cmd(egui::ViewportCommand::Close);
                        c.request_repaint();
                    }
                    break;
                }
                if stop_poller.load(Ordering::Relaxed) {
                    break;
                }

                if telemetry_monitor.is_some() && soundhop_core::ipc::is_running(None) {
                    telemetry_monitor = None;
                }

                let mut changed = false;
                if Instant::now() >= next_fast_due {
                    if telemetry_monitor.is_none()
                        && controller.is_save_enabled()
                        && !soundhop_core::ipc::is_running(None)
                    {
                        telemetry_monitor = Some(controller.start_telemetry_monitor());
                    }
                    changed |= poller_worker.refresh_fast(false);
                    let active = {
                        let snap = read_or_recover(&poller_worker.snapshot);
                        snap.connected || snap.audio_active || snap.busy_message.is_some()
                    };
                    let fast_interval = if active {
                        Duration::from_millis(1500)
                    } else {
                        Duration::from_millis(3000)
                    };
                    next_fast_due = Instant::now() + fast_interval;
                }

                if Instant::now() >= next_peers_due {
                    poller_worker.refresh_peers();
                    changed |= poller_worker.refresh_fast(false);
                    next_peers_due = Instant::now() + Duration::from_secs(6);
                }

                if changed {
                    poller_worker.request_repaint();
                }
            }

            drop(telemetry_monitor);
        });

        Self {
            snapshot,
            tx: Some(tx),
            toasts,
            ctx,
            mutation_seq,
            active_commands,
            stop_flag,
            quit_flag,
            poller_wake,
            cmd_handle: Some(cmd_handle),
            poller_handle: Some(poller_handle),
        }
    }

    pub fn send(&self, cmd: GuiCommand) {
        let mutating = is_mutating_command(&cmd);
        self.mutation_seq.fetch_add(1, Ordering::SeqCst);
        {
            let mut snap = write_or_recover(&self.snapshot);
            apply_optimistic_update(&mut snap, &cmd);
        }
        if mutating {
            self.active_commands.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(ref ctx) = self.ctx {
            ctx.request_repaint();
        }
        if let Some(ref tx) = self.tx {
            if tx.send(cmd).is_err() && mutating {
                self.active_commands.fetch_sub(1, Ordering::SeqCst);
            }
        } else if mutating {
            self.active_commands.fetch_sub(1, Ordering::SeqCst);
        }
    }

    pub fn read(&self) -> GuiSnapshot {
        read_or_recover(&self.snapshot).clone()
    }

    pub fn latest_toast(&self) -> Option<GuiToast> {
        let mut guard = lock_or_recover(&self.toasts);
        guard.retain(|t| t.created_at.elapsed() < Duration::from_secs(4));
        guard.last().cloned()
    }

    pub fn dismiss_toast(&self) {
        lock_or_recover(&self.toasts).clear();
    }

    pub fn quit_requested(&self) -> bool {
        self.quit_flag.load(Ordering::Acquire)
    }

    pub fn shutdown(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.tx.take();
        {
            let (lock, cvar) = &*self.poller_wake;
            let mut stopped = lock_or_recover(lock);
            *stopped = true;
            cvar.notify_all();
        }
        if let Some(handle) = self.poller_handle.take() {
            let _ = handle.join();
        }
        if self.active_commands.load(Ordering::SeqCst) == 0 {
            if let Some(handle) = self.cmd_handle.take() {
                let _ = handle.join();
            }
        } else {
            self.cmd_handle.take();
        }
    }
}

impl Drop for GuiBackend {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use soundhop_core::backend::linux::LinuxAudioBackend;
    use soundhop_core::config::Config;
    use soundhop_core::state::State;
    use soundhop_core::util::{CommandResult, MockRunner};

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
    fn test_snapshot_plain_language_helpers() {
        let ctrl = test_controller();
        let mut snap = GuiSnapshot::from_controller(&ctrl);
        assert_eq!(snap.output_headline(), "Playing on This Device");
        assert!(snap.show_onboarding);

        snap.connected = true;
        snap.connected_name = "Living Room PC".to_string();
        assert_eq!(snap.output_headline(), "Playing on Living Room PC");

        let peer = PeerView {
            id: "1".to_string(),
            name: "Living Room".to_string(),
            host: "192.168.1.50".to_string(),
            port: 4713,
            platform: "linux".to_string(),
            security: "trusted-lan".to_string(),
            rtt_ms: Some(4.2),
            online: true,
            paired: false,
        };
        assert_eq!(peer.signal_label(), "Excellent signal");
        assert_eq!(security_friendly_label("psk"), "PIN-Paired Devices Only");
    }

    #[test]
    fn test_stale_refresh_does_not_clobber_active_mutation() {
        let ctrl = test_controller();
        let initial = GuiSnapshot::from_controller(&ctrl);
        let snapshot = Arc::new(RwLock::new(initial));
        let mutation_seq = Arc::new(AtomicU64::new(0));
        let active_commands = Arc::new(AtomicUsize::new(1));

        let mut worker = WorkerState {
            controller: Arc::clone(&ctrl),
            snapshot: Arc::clone(&snapshot),
            toasts: Arc::new(Mutex::new(Vec::new())),
            ctx: None,
            cached_live_peers: Arc::new(Mutex::new(Vec::new())),
            mutation_seq: Arc::clone(&mutation_seq),
            active_commands: Arc::clone(&active_commands),
            onboarding_reopened: Arc::new(AtomicBool::new(false)),
            quit_flag: Arc::new(AtomicBool::new(false)),
            controller_sync_lock: Arc::new(Mutex::new(())),
            use_ipc: false,
        };

        // Simulate optimistic UI state (false) opposing controller truth (true) while a command is in flight
        snapshot.write().unwrap().sharing_enabled = false;
        mutation_seq.fetch_add(1, Ordering::SeqCst);

        // Background poller refresh (allow_during_command = false) must not revert it
        worker.refresh_fast(false);
        assert!(!snapshot.read().unwrap().sharing_enabled);

        // Once command completes (active_commands == 0), forced refresh updates to truth (true)
        active_commands.store(0, Ordering::SeqCst);
        worker.refresh_fast(true);
        assert!(snapshot.read().unwrap().sharing_enabled);
        assert!(snapshot.read().unwrap().hydrated);
    }

    #[test]
    fn test_connect_address_invalid_input_clears_busy() {
        let ctrl = test_controller();
        let initial = GuiSnapshot::from_controller(&ctrl);
        let snapshot = Arc::new(RwLock::new(initial));
        let toasts = Arc::new(Mutex::new(Vec::new()));
        let (tx, _rx) = mpsc::channel();
        let gui_backend = GuiBackend {
            snapshot: Arc::clone(&snapshot),
            tx: Some(tx),
            toasts: Arc::clone(&toasts),
            ctx: None,
            mutation_seq: Arc::new(AtomicU64::new(0)),
            active_commands: Arc::new(AtomicUsize::new(0)),
            stop_flag: Arc::new(AtomicBool::new(false)),
            quit_flag: Arc::new(AtomicBool::new(false)),
            poller_wake: Arc::new((Mutex::new(false), Condvar::new())),
            cmd_handle: None,
            poller_handle: None,
        };

        // Invalid host-less port input must not set optimistic busy_message
        gui_backend.send(GuiCommand::ConnectAddress {
            host_port: ":4713".to_string(),
        });
        assert!(gui_backend.read().busy_message.is_none());

        // Even if busy_message was already set, handle_command early return clears it
        snapshot.write().unwrap().busy_message = Some("Connecting...".to_string());
        let mut worker = WorkerState {
            controller: Arc::clone(&ctrl),
            snapshot: Arc::clone(&snapshot),
            toasts: Arc::clone(&toasts),
            ctx: None,
            cached_live_peers: Arc::new(Mutex::new(Vec::new())),
            mutation_seq: Arc::new(AtomicU64::new(0)),
            active_commands: Arc::new(AtomicUsize::new(0)),
            onboarding_reopened: Arc::new(AtomicBool::new(false)),
            quit_flag: Arc::new(AtomicBool::new(false)),
            controller_sync_lock: Arc::new(Mutex::new(())),
            use_ipc: false,
        };
        worker.handle_command(GuiCommand::ConnectAddress {
            host_port: ":4713".to_string(),
        });
        assert!(snapshot.read().unwrap().busy_message.is_none());
        assert!(toasts.lock().unwrap().last().is_some_and(|t| t.is_error));
    }

    #[test]
    fn test_lock_recovery_after_poison() {
        let m = Arc::new(Mutex::new(42));
        let m2 = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("intentional poison");
        })
        .join();
        assert_eq!(*lock_or_recover(&m), 42);

        let rw = Arc::new(RwLock::new(99));
        let rw2 = Arc::clone(&rw);
        let _ = std::thread::spawn(move || {
            let _g = rw2.write().unwrap();
            panic!("intentional rw poison");
        })
        .join();
        assert_eq!(*read_or_recover(&rw), 99);
        *write_or_recover(&rw) = 100;
        assert_eq!(*read_or_recover(&rw), 100);
    }
}
