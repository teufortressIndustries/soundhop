//! Local JSON-line IPC server and client over Unix Domain Sockets (Linux)
//! or authenticated localhost TCP (Windows / fallback).

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rand::Rng;
use serde_json::Value;
use thiserror::Error;

use crate::util::{ensure_dir, lock_or_recover, socket_path};

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("SoundHop daemon is not running")]
    DaemonNotRunning,
    #[error("{0}")]
    Error(String),
}

pub type IpcHandler = Arc<dyn Fn(Value) -> Value + Send + Sync>;

pub struct IpcServer {
    handler: IpcHandler,
    pub path: PathBuf,
    stop_flag: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl IpcServer {
    pub fn new(handler: IpcHandler, path: Option<PathBuf>) -> Self {
        Self {
            handler,
            path: path.unwrap_or_else(socket_path),
            stop_flag: Arc::new(AtomicBool::new(false)),
            thread: Mutex::new(None),
        }
    }

    pub fn start(&self) -> Result<(), IpcError> {
        if let Some(parent) = self.path.parent() {
            ensure_dir(parent)
                .map_err(|e| IpcError::Error(format!("Unsafe IPC directory: {e}")))?;
        }
        let _ = fs::remove_file(&self.path);
        let port_file = self.path.with_extension("port");
        let auth_file = self.path.with_extension("auth");
        let _ = fs::remove_file(&port_file);
        let _ = fs::remove_file(&auth_file);

        self.stop_flag.store(false, Ordering::SeqCst);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            use std::os::unix::net::UnixListener;

            if let Ok(listener) = UnixListener::bind(&self.path) {
                let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600));
                let _ = listener.set_nonblocking(true);
                let stop = Arc::clone(&self.stop_flag);
                let handler = Arc::clone(&self.handler);
                let handle = thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let _ = stream.set_nonblocking(false);
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                                let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
                                let h = Arc::clone(&handler);
                                thread::spawn(move || serve_stream(stream, None, h));
                            }
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(30));
                            }
                            Err(_) => break,
                        }
                    }
                });
                *lock_or_recover(&self.thread) = Some(handle);
                return Ok(());
            }
        }

        // Fallback to localhost TCP with token auth
        let tcp = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| IpcError::Error(format!("Failed to bind IPC socket: {e}")))?;
        let port = tcp
            .local_addr()
            .map_err(|e| IpcError::Error(e.to_string()))?
            .port();
        let token_bytes: [u8; 32] = rand::thread_rng().gen();
        let token = hex::encode(token_bytes);
        crate::util::atomic_write(&port_file, &format!("{port}\n"))
            .map_err(|e| IpcError::Error(format!("Failed to write IPC port file: {e}")))?;
        crate::util::atomic_write(&auth_file, &format!("{token}\n"))
            .map_err(|e| IpcError::Error(format!("Failed to write IPC auth file: {e}")))?;
        let _ = tcp.set_nonblocking(true);
        let stop = Arc::clone(&self.stop_flag);
        let handler = Arc::clone(&self.handler);
        let handle = thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match tcp.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
                        let h = Arc::clone(&handler);
                        let tok = Some(token.clone());
                        thread::spawn(move || serve_stream(stream, tok, h));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(30));
                    }
                    Err(_) => break,
                }
            }
        });
        *lock_or_recover(&self.thread) = Some(handle);
        Ok(())
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = lock_or_recover(&self.thread).take() {
            let _ = h.join();
        }
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(self.path.with_extension("port"));
        let _ = fs::remove_file(self.path.with_extension("auth"));
    }
}

const MAX_IPC_LINE_BYTES: u64 = 64 * 1024;

fn serve_stream<S: std::io::Read + Write>(
    mut stream: S,
    expected_token: Option<String>,
    handler: IpcHandler,
) {
    let mut line = String::new();
    {
        let limited = (&mut stream).take(MAX_IPC_LINE_BYTES);
        let mut reader = BufReader::new(limited);
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            return;
        }
    }
    if line.len() as u64 >= MAX_IPC_LINE_BYTES && !line.ends_with('\n') {
        let resp = serde_json::json!({
            "ok": false,
            "error": "IPC request exceeds maximum line length"
        });
        let mut encoded = serde_json::to_vec(&resp).unwrap_or_else(|_| b"{}".to_vec());
        encoded.push(b'\n');
        let _ = stream.write_all(&encoded);
        return;
    }
    let response = match serde_json::from_str::<Value>(line.trim()) {
        Ok(Value::Object(map)) => {
            if let Some(ref exp) = expected_token {
                let provided = map.get("auth").and_then(|v| v.as_str()).unwrap_or("");
                if !crate::security::constant_time_eq(provided.as_bytes(), exp.as_bytes()) {
                    serde_json::json!({
                        "ok": false,
                        "error": "Unauthorized: invalid or missing IPC authentication token"
                    })
                } else {
                    dispatch_request(Value::Object(map), &handler)
                }
            } else {
                dispatch_request(Value::Object(map), &handler)
            }
        }
        Ok(_) => serde_json::json!({ "ok": false, "error": "Request must be a JSON object" }),
        Err(e) => serde_json::json!({ "ok": false, "error": format!("Invalid request: {e}") }),
    };
    let mut encoded = serde_json::to_vec(&response).unwrap_or_else(|_| b"{}".to_vec());
    encoded.push(b'\n');
    let _ = stream.write_all(&encoded);
}

fn dispatch_request(req: Value, handler: &IpcHandler) -> Value {
    let mut res = handler(req);
    if let Some(obj) = res.as_object_mut() {
        obj.entry("ok".to_string()).or_insert(Value::Bool(true));
    }
    res
}

pub fn request(
    command: &str,
    payload: Value,
    path: Option<&Path>,
    timeout: Duration,
) -> Result<Value, IpcError> {
    let target = path.map(Path::to_path_buf).unwrap_or_else(socket_path);
    let mut req_obj = match payload {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    req_obj.insert("cmd".to_string(), Value::String(command.to_string()));

    let port_file = target.with_extension("port");
    let auth_file = target.with_extension("auth");

    if port_file.exists() {
        if let Ok(port_str) = fs::read_to_string(&port_file) {
            if let Ok(port) = port_str.trim().parse::<u16>() {
                if let Ok(auth) = fs::read_to_string(&auth_file) {
                    req_obj.insert("auth".to_string(), Value::String(auth.trim().to_string()));
                }
                let addr = SocketAddr::from(([127, 0, 0, 1], port));
                let stream = TcpStream::connect_timeout(&addr, timeout)
                    .map_err(|_| IpcError::DaemonNotRunning)?;
                let _ = stream.set_read_timeout(Some(timeout));
                let _ = stream.set_write_timeout(Some(timeout));
                return exchange_json(stream, Value::Object(req_obj));
            }
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        let stream = UnixStream::connect(&target).map_err(|_| IpcError::DaemonNotRunning)?;
        let _ = stream.set_read_timeout(Some(timeout));
        let _ = stream.set_write_timeout(Some(timeout));
        exchange_json(stream, Value::Object(req_obj))
    }

    #[cfg(not(unix))]
    {
        Err(IpcError::DaemonNotRunning)
    }
}

fn exchange_json<S: std::io::Read + Write>(mut stream: S, req: Value) -> Result<Value, IpcError> {
    let mut bytes = serde_json::to_vec(&req).map_err(|e| IpcError::Error(e.to_string()))?;
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .map_err(|_| IpcError::DaemonNotRunning)?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| IpcError::Error(format!("IPC read error: {e}")))?;
    if line.trim().is_empty() {
        return Err(IpcError::Error(
            "Empty response from the SoundHop daemon".to_string(),
        ));
    }
    let resp: Value = serde_json::from_str(line.trim())
        .map_err(|e| IpcError::Error(format!("Malformed response from daemon: {e}")))?;
    let ok = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if !ok {
        let err_msg = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown daemon error");
        return Err(IpcError::Error(err_msg.to_string()));
    }
    Ok(resp)
}

pub fn is_soundhop_process(pid: u64) -> bool {
    if pid <= 1 || pid == u64::from(std::process::id()) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(comm) = fs::read_to_string(format!("/proc/{pid}/comm")) {
            if comm.to_lowercase().contains("soundhop") {
                return true;
            }
        }
        if let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) {
            let s = String::from_utf8_lossy(&cmdline).to_lowercase();
            return s.contains("soundhop");
        }
        false
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .to_lowercase()
                    .contains("soundhop")
            })
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let Ok(pid32) = u32::try_from(pid) else {
            return false;
        };
        unsafe {
            let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid32);
            if handle.is_null() {
                return false;
            }
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len);
            CloseHandle(handle);
            if ok == 0 || len == 0 {
                return false;
            }
            let image_path = String::from_utf16_lossy(&buf[..len as usize]);
            let file_name = Path::new(&image_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&image_path);
            file_name.to_ascii_lowercase().contains("soundhop")
        }
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = pid;
        false
    }
}

/// Returns `true` if a process with `pid` is currently running.
pub fn is_process_alive(pid: u64) -> bool {
    if pid <= 1 {
        return false;
    }
    #[cfg(unix)]
    {
        let Ok(pid_t) = libc::pid_t::try_from(pid) else {
            return false;
        };
        unsafe { libc::kill(pid_t, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let Ok(pid32) = u32::try_from(pid) else {
            return false;
        };
        unsafe {
            let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid32);
            if handle.is_null() {
                return false;
            }
            let mut exit_code: u32 = 0;
            let ok = GetExitCodeProcess(handle, &mut exit_code);
            CloseHandle(handle);
            ok != 0 && exit_code == STILL_ACTIVE as u32
        }
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = pid;
        false
    }
}

/// Sends a termination signal to a verified SoundHop process (`SIGTERM` on Unix,
/// `TerminateProcess` on Windows). Never terminates non-SoundHop PIDs or self.
pub fn terminate_process(pid: u64) -> bool {
    if !is_soundhop_process(pid) {
        return false;
    }
    #[cfg(unix)]
    {
        let Ok(pid_t) = libc::pid_t::try_from(pid) else {
            return false;
        };
        unsafe { libc::kill(pid_t, libc::SIGTERM) == 0 }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, TerminateProcess, PROCESS_TERMINATE,
        };

        let Ok(pid32) = u32::try_from(pid) else {
            return false;
        };
        unsafe {
            let handle: HANDLE = OpenProcess(PROCESS_TERMINATE, 0, pid32);
            if handle.is_null() {
                return false;
            }
            let ok = TerminateProcess(handle, 1);
            CloseHandle(handle);
            ok != 0
        }
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = pid;
        false
    }
}

/// Finds a running user-owned `soundhop` process invoked with the given subcommand argument
/// (e.g., `"tray"`), used as a fallback when upgrading from versions without lock metadata.
pub fn find_soundhop_process_with_arg(arg: &str) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let uid = unsafe { libc::getuid() };
        let self_pid = u64::from(std::process::id());
        let Ok(entries) = fs::read_dir("/proc") else {
            return None;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid_str) = name.to_str() else {
                continue;
            };
            let Ok(pid) = pid_str.parse::<u64>() else {
                continue;
            };
            if pid <= 1 || pid == self_pid {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.uid() != uid {
                continue;
            }
            let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            let args: Vec<String> = cmdline
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect();
            let is_soundhop = args
                .first()
                .is_some_and(|exe| exe.to_lowercase().contains("soundhop"))
                || fs::read_to_string(entry.path().join("comm"))
                    .is_ok_and(|c| c.to_lowercase().contains("soundhop"));
            if is_soundhop && args.iter().skip(1).any(|a| a == arg) {
                return Some(pid);
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = arg;
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcPingInfo {
    pub pid: Option<u64>,
    pub version: String,
    pub pong: bool,
}

pub fn ping(path: Option<&Path>) -> Option<IpcPingInfo> {
    let resp = request(
        "ping",
        serde_json::json!({}),
        path,
        Duration::from_millis(500),
    )
    .ok()?;
    let pong = resp.get("pong").and_then(|v| v.as_bool()).unwrap_or(false);
    if !pong {
        return None;
    }
    let version = resp
        .get("version")
        .or_else(|| resp.get("daemon").and_then(|d| d.get("version")))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let pid = resp
        .get("pid")
        .or_else(|| resp.get("daemon").and_then(|d| d.get("pid")))
        .and_then(|v| v.as_u64());
    Some(IpcPingInfo { pid, version, pong })
}

pub fn is_reachable(path: Option<&Path>) -> bool {
    ping(path).is_some()
}

pub fn is_running(path: Option<&Path>) -> bool {
    let Some(info) = ping(path) else {
        return false;
    };
    if !info.version.is_empty() && info.version != crate::constants::VERSION {
        return false;
    }
    info.pong
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ipc_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let server = IpcServer::new(
            Arc::new(|req| {
                let cmd = req.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
                if cmd == "ping" {
                    serde_json::json!({ "ok": true, "pong": true })
                } else {
                    serde_json::json!({ "ok": true, "echo": cmd })
                }
            }),
            Some(sock.clone()),
        );
        server.start().unwrap();
        assert!(is_running(Some(&sock)));

        let res = request(
            "hello",
            serde_json::json!({}),
            Some(&sock),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(res["echo"], "hello");
        server.stop();
    }

    #[test]
    fn test_ping_reachable_vs_version_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("ver.sock");
        let server = IpcServer::new(
            Arc::new(|req| {
                let cmd = req.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
                if cmd == "ping" {
                    serde_json::json!({
                        "ok": true,
                        "pong": true,
                        "pid": 424242,
                        "version": "0.0.1-old"
                    })
                } else {
                    serde_json::json!({ "ok": true })
                }
            }),
            Some(sock.clone()),
        );
        server.start().unwrap();

        let info = ping(Some(&sock)).expect("old daemon should respond to ping");
        assert_eq!(info.pid, Some(424242));
        assert_eq!(info.version, "0.0.1-old");
        assert!(info.pong);
        assert!(is_reachable(Some(&sock)));
        assert!(!is_running(Some(&sock)));

        server.stop();
    }

    #[test]
    fn test_process_inspection_self_and_invalid() {
        let self_pid = u64::from(std::process::id());
        assert!(is_process_alive(self_pid));
        // is_soundhop_process must always reject self and invalid PIDs.
        assert!(!is_soundhop_process(self_pid));
        assert!(!is_soundhop_process(0));
        assert!(!is_soundhop_process(1));
        assert!(!is_process_alive(0));
        assert!(!terminate_process(self_pid));
    }
}
