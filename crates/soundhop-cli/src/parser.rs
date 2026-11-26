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

    /// Internal flag to open the Control Center on a specific tab (`home`, `apps`, `phone`, `settings`)
    #[arg(long, hide = true)]
    pub gui_tab: Option<String>,

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

    /// Launch the SoundHop Control Center GUI (starts background tray if needed)
    Gui,

    /// Launch the native OS system tray icon & background daemon
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

    /// Broadcast PC audio to phones and browsers over Wi-Fi or USB (`adb reverse`)
    #[command(visible_alias = "broadcast")]
    Web(WebArgs),

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
    #[command(subcommand)]
    pub action: Option<PairAction>,

    /// Peer name, ID, or IP, or "status" / "clear" / "generate" (legacy PIN/PSK mode)
    pub target: Option<String>,

    /// 12-character alphanumeric pairing code or passphrase to derive shared PSK & PulseAudio cookie
    #[arg(long)]
    pub pin: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum PairAction {
    /// Initiate an interactive Pairing v2 request to a remote SoundHop device
    Request {
        /// Target host IP, hostname, or discovered peer name/ID
        host: String,
        /// Pairing TCP port (default: 4715)
        #[arg(short, long)]
        port: Option<u16>,
        /// Optional one-time headless pairing code from the remote device
        #[arg(long)]
        code: Option<String>,
    },
    /// List incoming Pairing v2 requests awaiting approval
    Pending,
    /// Approve a pending incoming Pairing v2 request by request ID or SAS verification code
    Approve {
        /// Request ID or 6-digit verification code (SAS)
        id: String,
    },
    /// Deny a pending incoming Pairing v2 request by request ID or SAS verification code
    Deny {
        /// Request ID or 6-digit verification code (SAS)
        id: String,
    },
    /// Revoke a previously trusted peer by device ID
    Revoke {
        /// Trusted peer device ID to revoke
        device_id: String,
    },
    /// List all trusted Pairing v2 devices
    List,
    /// Generate a 120-second one-time pairing code for headless / Raspberry Pi devices
    Code,
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

    /// Dotted config key (e.g. `device.name`, `routing.default_mode`)
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

#[derive(Debug, Args)]
pub struct WebArgs {
    #[command(subcommand)]
    pub action: Option<WebAction>,
}

#[derive(Debug, Subcommand)]
pub enum WebAction {
    /// Start the Web Broadcast HTTP/WebSocket/WebRTC audio server
    Start {
        /// Override HTTP/WebSocket TCP port (default: 4717)
        #[arg(short, long)]
        port: Option<u16>,

        /// Keep playing audio on local PC speakers while broadcasting
        #[arg(long, conflicts_with = "no_keep_local")]
        keep_local: bool,

        /// Mute local PC speakers while broadcasting (phone/browser only)
        #[arg(long, conflicts_with = "keep_local")]
        no_keep_local: bool,

        /// Disable per-session bearer token authentication
        #[arg(long)]
        no_token: bool,

        /// Establish a USB `adb reverse` tunnel for low-latency localhost listening
        #[arg(long)]
        adb: bool,

        /// Target Android ADB device serial when setting up `adb reverse`
        #[arg(long)]
        serial: Option<String>,

        /// Open the Web Broadcast URL in the Android phone browser (implies `--adb`)
        #[arg(long)]
        open: bool,
    },
    /// Stop Web Broadcast and remove any active USB `adb reverse` tunnels
    Stop,
    /// Show Web Broadcast status, URLs, token, and connected listeners
    Status,
    /// Render a compact UTF-8 half-block QR code for the active Web Broadcast URL
    Qr {
        /// Render the `http://localhost:<port>` URL for USB `adb reverse` instead of LAN IP
        #[arg(long)]
        adb: bool,
    },
    /// Rotate the ephemeral Web Broadcast bearer token
    #[command(name = "rotate-token")]
    RotateToken,
    /// Disconnect a connected WebRTC or WebSocket listener by client ID
    Kick {
        /// Client ID shown by `soundhop web status`
        client_id: String,
    },
}
