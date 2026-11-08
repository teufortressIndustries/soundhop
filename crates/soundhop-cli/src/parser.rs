//! `clap` derive CLI parser supporting all v1 & v2 subcommands and flags.

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "soundhop",
    version,
    about = "Seamless Wi-Fi speaker sharing, multi-room broadcast, per-app routing, and Android audio bridge"
)]
pub struct Cli {
    /// Emit machine-readable JSON output
    #[arg(long, global = true)]
    pub json: bool,

    /// Enable verbose debug logging
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Disable ANSI color codes
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Internal flag to open the standalone Settings & Routing Rules window
    #[arg(long, hide = true)]
    pub settings_dialog: bool,

    /// Internal flag to open the standalone Android Audio Wizard window
    #[arg(long, hide = true)]
    pub android_wizard: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Interactive terminal dashboard (Multi-Room, App Mixer, Mic & Android, Live VU/RTT Telemetry)
    Tui,

    /// Guided first-time setup (speaker sharing, firewall rule, background daemon, and tray autostart)
    Setup(SetupArgs),

    /// Show current speaker sharing, connection, multi-room, and audio telemetry status
    Status,

    /// Discover SoundHop speakers on the local network (with RTT latency probes)
    #[command(visible_alias = "devices")]
    Discover(DiscoverArgs),

    /// Connect to one or more remote speakers (supports Multi-Room broadcast via --combine)
    Connect(ConnectArgs),

    /// Disconnect from the active remote speaker or multi-room group
    Disconnect,

    /// Control local speaker sharing over TCP
    Share(ShareArgs),

    /// Pair with a peer or configure PIN / Pre-Shared Key (PSK) HMAC & cookie security
    Pair(PairArgs),

    /// List currently playing audio application streams
    Apps,

    /// Route a specific application to the remote speaker (optionally saving a persistent rule)
    Route(RouteArgs),

    /// Restore a routed application (or all applications) back to the local speaker
    Unroute(UnrouteArgs),

    /// Control network microphone pass-through (send local mic or receive remote mic)
    Mic(MicArgs),

    /// Get or set remote speaker volume and mute state
    #[command(visible_alias = "vol")]
    Volume(VolumeArgs),

    /// Configure automatic reconnection to preferred peers
    Auto(AutoArgs),

    /// Run the background discovery, auto-connect, and IPC daemon
    Daemon,

    /// Stop the running background daemon and disconnect active sessions
    Stop,

    /// Manage the systemd user service / Windows startup entry
    Service(ServiceArgs),

    /// Inspect or open host firewall rules for SoundHop TCP/UDP ports
    Firewall(FirewallArgs),

    /// View or modify SoundHop configuration (`~/.config/soundhop/config.toml`)
    Config(ConfigArgs),

    /// Run comprehensive system, audio backend, network, and firewall diagnostics
    Doctor(DoctorArgs),

    /// Launch the native OS system tray icon & menu
    #[command(visible_alias = "gui")]
    Tray,

    /// View recent daemon log lines
    Logs(LogsArgs),

    /// Generate a diagnostic bug report bundle
    #[command(name = "bug-report", visible_alias = "report")]
    BugReport,

    /// Generate shell completion scripts (`bash`, `zsh`, `fish`, `powershell`)
    Completions(CompletionsArgs),

    /// Stream audio or microphone from an Android phone over USB or Wi-Fi ADB (`scrcpy`)
    Android(AndroidArgs),

    /// Print version and protocol information
    Version,
}

#[derive(Debug, Args)]
pub struct SetupArgs {
    /// Automatically accept defaults without interactive prompts
    #[arg(short, long)]
    pub yes: bool,

    /// Custom device name to advertise on LAN
    #[arg(long)]
    pub name: Option<String>,

    /// Skip opening firewall ports
    #[arg(long)]
    pub skip_firewall: bool,

    /// Skip installing background service
    #[arg(long)]
    pub skip_service: bool,
}

#[derive(Debug, Args)]
pub struct DiscoverArgs {
    /// Discovery scan timeout in seconds
    #[arg(short, long, default_value_t = 2.5)]
    pub timeout: f64,
}

#[derive(Debug, Args)]
pub struct ConnectArgs {
    /// One or more peer names, IDs, or IP[:port] addresses (omit to connect to preferred/sole peer)
    pub targets: Vec<String>,

    /// Broadcast simultaneously to multiple peers using a combined multi-room sink
    #[arg(long)]
    pub combine: bool,

    /// Keep local speakers playing alongside remote speaker(s) in multi-room mode
    #[arg(long)]
    pub keep_local: bool,

    /// Override tunnel buffer latency in milliseconds
    #[arg(long)]
    pub latency: Option<u32>,

    /// Set the remote speaker as default output
    #[arg(long)]
    pub default_sink: bool,

    /// Do not switch default output sink
    #[arg(long)]
    pub no_default: bool,

    /// Save the target as the preferred auto-connect peer
    #[arg(long)]
    pub remember: bool,
}

#[derive(Debug, Args)]
pub struct ShareArgs {
    #[command(subcommand)]
    pub action: Option<ShareAction>,

    /// Override TCP port when enabling sharing
    #[arg(short, long)]
    pub port: Option<u16>,
}

#[derive(Debug, Subcommand)]
pub enum ShareAction {
    /// Enable speaker sharing
    On {
        #[arg(short, long)]
        port: Option<u16>,
    },
    /// Disable speaker sharing
    Off,
    /// Show speaker sharing status
    Status,
}

#[derive(Debug, Args)]
pub struct PairArgs {
    /// Peer name, ID, or IP, or "status" / "clear" / "generate"
    pub target: Option<String>,

    /// 6-digit PIN or passphrase to derive shared PSK & PulseAudio cookie
    #[arg(long)]
    pub pin: Option<String>,
}

#[derive(Debug, Args)]
pub struct RouteArgs {
    /// Application name (e.g. Firefox, Spotify, VLC)
    pub app: String,

    /// Persist this routing rule in config.toml (`remote` or `local`)
    #[arg(long)]
    pub save: bool,

    /// Route the app to local output instead of remote when saving a rule
    #[arg(long)]
    pub local: bool,
}

#[derive(Debug, Args)]
pub struct UnrouteArgs {
    /// Application name to restore to local speakers (or omit with --all)
    pub app: Option<String>,

    /// Restore all routed applications to local speakers
    #[arg(long)]
    pub all: bool,

    /// Also remove any saved persistent rule for this application from config.toml
    #[arg(long)]
    pub remove_rule: bool,
}

#[derive(Debug, Args)]
pub struct MicArgs {
    /// Mode: `send`, `receive`, `off`, or `status`
    #[arg(default_value = "status")]
    pub mode: String,

    /// Optional target peer when sending or receiving microphone audio
    pub target: Option<String>,

    /// Optional local microphone source name
    #[arg(long)]
    pub source: Option<String>,

    /// Optional latency in milliseconds
    #[arg(long)]
    pub latency: Option<u32>,
}

#[derive(Debug, Args)]
pub struct VolumeArgs {
    /// Volume level (`0-150`, `+10`, `-10`, `mute`, `unmute`, `toggle`)
    pub value: Option<String>,
}

#[derive(Debug, Args)]
pub struct AutoArgs {
    /// Action: `on`, `off`, `status`, or `prefer`
    #[arg(default_value = "status")]
    pub action: String,

    /// Peer name or ID when setting preferred peer
    pub peer: Option<String>,
}

#[derive(Debug, Args)]
pub struct ServiceArgs {
    /// Action: `install`, `uninstall`, `start`, `stop`, `restart`, or `status`
    #[arg(default_value = "status")]
    pub action: String,

    /// Skip installing desktop tray autostart entry
    #[arg(long)]
    pub no_tray: bool,
}

#[derive(Debug, Args)]
pub struct FirewallArgs {
    /// Action: `status` or `open`
    #[arg(default_value = "status")]
    pub action: String,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// Action: `show`, `path`, `get`, `set`, or `reset`
    #[arg(default_value = "show")]
    pub action: String,

    /// Dotted config key (e.g. `general.device_name`, `routing.default_mode`)
    pub key: Option<String>,

    /// Value to assign when action is `set`
    pub value: Option<String>,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Automatically attempt to remediate common issues (orphaned modules, firewall, service)
    #[arg(long)]
    pub fix: bool,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    /// Number of tail lines to display
    #[arg(short = 'n', long, default_value_t = 50)]
    pub lines: usize,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ShellChoice {
    Bash,
    Zsh,
    Fish,
    Powershell,
}

#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// Target shell (`bash`, `zsh`, `fish`, `powershell`)
    #[arg(value_enum)]
    pub shell: ShellChoice,
}

#[derive(Debug, Args)]
pub struct AndroidArgs {
    /// Action: `start`, `stop`, `status`, `devices`, `wifi`, `connect`, `disconnect`, `install`, `wizard`
    #[arg(default_value = "status")]
    pub action: String,

    /// Target ADB device serial or IP[:port]
    pub target: Option<String>,

    /// Audio source (`output`, `mic`, `playback`)
    #[arg(long, default_value = "output")]
    pub source: String,

    /// Audio codec (`opus`, `aac`, `flac`, `raw`)
    #[arg(long, default_value = "opus")]
    pub codec: String,

    /// Audio bitrate (e.g. `128K`)
    #[arg(long, default_value = "128K")]
    pub bitrate: String,

    /// Audio buffer in milliseconds
    #[arg(long, default_value_t = 50)]
    pub buffer: u32,

    /// Route Android audio into a virtual PC microphone (`SoundHop_Android_Mic`)
    #[arg(long)]
    pub as_mic: bool,

    /// Run capture in the foreground until Ctrl-C
    #[arg(short, long)]
    pub foreground: bool,
}
