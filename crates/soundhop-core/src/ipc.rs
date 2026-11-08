//! Local JSON-line IPC server and client over Unix Domain Sockets (Linux)
//! or authenticated localhost TCP (Windows / fallback).

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rand::Rng;
use serde_json::Value;
use thiserror::Error;

use crate::util::{ensure_dir, socket_path};

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
            let _ = ensure_dir(parent);
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
                *self.thread.lock().unwrap() = Some(handle);
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
        let _ = fs::write(&port_file, format!("{port}\n"));
        let _ = fs::write(&auth_file, format!("{token}\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&auth_file, fs::Permissions::from_mode(0o600));
        }
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
        *self.thread.lock().unwrap() = Some(handle);
        Ok(())
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.thread.lock().unwrap().take() {
            let _ = h.join();
        }
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(self.path.with_extension("port"));
        let _ = fs::remove_file(self.path.with_extension("auth"));
    }
}

fn serve_stream<S: std::io::Read + Write>(
    mut stream: S,
    expected_token: Option<String>,
    handler: IpcHandler,
) {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&mut stream);
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            return;
        }
    }
    let response = match serde_json::from_str::<Value>(line.trim()) {
        Ok(Value::Object(map)) => {
            if let Some(ref exp) = expected_token {
                let provided = map.get("auth").and_then(|v| v.as_str()).unwrap_or("");
                if provided != exp {
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

pub fn is_running(path: Option<&Path>) -> bool {
    let Ok(resp) = request(
        "ping",
        serde_json::json!({}),
        path,
        Duration::from_millis(500),
    ) else {
        return false;
    };
    let ver_opt = resp
        .get("version")
        .or_else(|| resp.get("daemon").and_then(|d| d.get("version")))
        .and_then(|v| v.as_str());
    if let Some(ver) = ver_opt {
        if ver != crate::constants::VERSION {
            #[cfg(unix)]
            {
                let pid_opt = resp
                    .get("pid")
                    .or_else(|| resp.get("daemon").and_then(|d| d.get("pid")))
                    .and_then(|v| v.as_u64());
                if let Some(pid) = pid_opt {
                    let my_pid = u64::from(std::process::id());
                    if pid > 1 && pid != my_pid {
                        unsafe {
                            libc::kill(pid as libc::pid_t, libc::SIGTERM);
                        }
                        thread::sleep(Duration::from_millis(250));
                    }
                }
            }
            let target = path.map(Path::to_path_buf).unwrap_or_else(socket_path);
            let _ = fs::remove_file(target);
            return false;
        }
    }
    resp.get("pong").and_then(|v| v.as_bool()).unwrap_or(false)
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
}
