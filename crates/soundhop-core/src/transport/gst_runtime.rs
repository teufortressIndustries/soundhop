//! GStreamer process initialization, bundled plugin environment discovery, and runtime diagnostics.

use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use gstreamer as gst;
use serde::{Deserialize, Serialize};

use crate::constants::{APP_SLUG, VERSION};
use crate::transport::{TransportError, TransportResult};
use crate::util::{ensure_dir, home_dir};

static GST_INIT: OnceLock<Result<(), String>> = OnceLock::new();
static BUNDLED_PLUGIN_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Required GStreamer element factory names for encrypted SRTP/Opus & PCM audio transport.
const REQUIRED_CORE_ELEMENTS: &[&str] = &[
    "opusenc",
    "opusdec",
    "rtpopuspay",
    "rtpopusdepay",
    "rtpL16pay",
    "rtpL16depay",
    "rtpjitterbuffer",
    "srtpenc",
    "srtpdec",
    "audioconvert",
    "audioresample",
    "volume",
    "udpsrc",
    "udpsink",
];

/// Diagnostic status of the GStreamer runtime and required audio/SRTP plugins.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GstreamerRuntimeStatus {
    pub available: bool,
    pub version: String,
    pub missing_elements: Vec<String>,
    pub audio_src_available: bool,
    pub audio_sink_available: bool,
    pub bundled_plugin_dir: Option<String>,
}

/// Platform cache directory used for versioned GStreamer plugin registries.
pub(crate) fn cache_dir() -> PathBuf {
    if let Some(base) = env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(base).join(APP_SLUG);
    }
    #[cfg(windows)]
    {
        if let Some(local) = env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            return PathBuf::from(local).join(APP_SLUG).join("cache");
        }
    }
    home_dir().join(".cache").join(APP_SLUG)
}

fn find_bundled_plugin_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            candidates.push(exe_dir.join("lib").join("gstreamer-1.0"));
        }
    }
    if let Some(appdir) = env::var_os("APPDIR").filter(|v| !v.is_empty()) {
        candidates.push(PathBuf::from(appdir).join("lib").join("gstreamer-1.0"));
    }
    candidates.into_iter().find(|p| p.is_dir())
}

fn configure_bundled_env_for_dir(plugin_dir: &Path) {
    if env::var_os("GST_PLUGIN_PATH_1_0").is_some_and(|v| !v.is_empty()) {
        return;
    }
    // Use `plugin_dir` instead of `""`: on Windows, SetEnvironmentVariableW deletes the
    // variable when passed an empty string, causing GStreamer to fall back to scanning
    // system plugin directories.
    env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", plugin_dir);
    env::set_var("GST_PLUGIN_PATH_1_0", plugin_dir);
    env::set_var("GST_REGISTRY_FORK", "no");

    if env::var_os("GST_REGISTRY_1_0").is_none_or(|v| v.is_empty()) {
        let dir = cache_dir();
        let _ = ensure_dir(&dir);
        let reg_path = dir.join(format!("gst-registry-{VERSION}.bin"));
        env::set_var("GST_REGISTRY_1_0", reg_path);
    }
}

/// Configure GStreamer plugin search paths and registry cache if bundled plugins are present.
///
/// Must be called before `gst::init()`. Checks `<exe_dir>/lib/gstreamer-1.0` and
/// `$APPDIR/lib/gstreamer-1.0`; if present and `GST_PLUGIN_PATH_1_0` is not already set,
/// isolates GStreamer to the bundled plugins and places a versioned registry in `cache_dir()`.
pub fn configure_gst_env_if_bundled() -> Option<PathBuf> {
    BUNDLED_PLUGIN_DIR
        .get_or_init(|| {
            let found = find_bundled_plugin_dir();
            if let Some(ref dir) = found {
                configure_bundled_env_for_dir(dir);
            }
            found
        })
        .clone()
}

/// Initialize GStreamer once per process (configuring bundled plugin paths first).
pub fn ensure_gst_init() -> TransportResult<()> {
    let res = GST_INIT.get_or_init(|| {
        let _ = configure_gst_env_if_bundled();
        gst::init().map_err(|e| format!("GStreamer init failed: {e}"))
    });
    match res {
        Ok(()) => Ok(()),
        Err(msg) => Err(TransportError::Pipeline(msg.clone())),
    }
}

fn os_audio_element_names() -> (&'static str, &'static str) {
    #[cfg(target_os = "linux")]
    {
        ("pulsesrc", "pulsesink")
    }
    #[cfg(target_os = "windows")]
    {
        ("wasapi2src", "wasapi2sink")
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        ("autoaudiosrc", "autoaudiosink")
    }
}

/// Inspect the GStreamer runtime and verify that all required SRTP/Opus/audio elements exist.
pub fn check_gstreamer_runtime() -> Result<GstreamerRuntimeStatus, TransportError> {
    ensure_gst_init()?;

    let (maj, min, micro, nano) = gst::version();
    let version = if nano > 0 {
        format!("{maj}.{min}.{micro}.{nano}")
    } else {
        format!("{maj}.{min}.{micro}")
    };

    let mut missing_elements = Vec::new();
    for &name in REQUIRED_CORE_ELEMENTS {
        if gst::ElementFactory::find(name).is_none() {
            missing_elements.push(name.to_string());
        }
    }

    let (src_name, sink_name) = os_audio_element_names();
    let audio_src_available = gst::ElementFactory::find(src_name).is_some();
    let audio_sink_available = gst::ElementFactory::find(sink_name).is_some();
    if !audio_src_available {
        missing_elements.push(src_name.to_string());
    }
    if !audio_sink_available {
        missing_elements.push(sink_name.to_string());
    }

    let bundled_plugin_dir = BUNDLED_PLUGIN_DIR
        .get()
        .and_then(|opt| opt.as_ref())
        .map(|p| p.to_string_lossy().into_owned());

    Ok(GstreamerRuntimeStatus {
        available: missing_elements.is_empty(),
        version,
        missing_elements,
        audio_src_available,
        audio_sink_available,
        bundled_plugin_dir,
    })
}

/// Build a GStreamer element by factory name with a clear `TransportError::Pipeline` if missing.
pub(crate) fn make_element(factory: &str, name: Option<&str>) -> TransportResult<gst::Element> {
    let mut builder = gst::ElementFactory::make(factory);
    if let Some(n) = name {
        builder = builder.name(n);
    }
    builder.build().map_err(|e| {
        TransportError::Pipeline(format!(
            "failed to create GStreamer element '{factory}': {e}"
        ))
    })
}
