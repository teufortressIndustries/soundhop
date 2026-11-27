//! Synchronous HTTP/1.1 signaling and static SPA server (`web-http-accept`).

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::constants::VERSION;
use crate::util::{local_addresses, lock_or_recover};

use super::rtc::{RtcMuxHandle, RtcOfferRequest, RtcOfferResponse};
use super::tls::{is_loopback_ip, TlsServerMaterial};
use super::ws::{WsFanoutHandle, WsStream, WsUpgradeRequest, WS_SUBPROTOCOL};
use super::{BearerAuthResult, BroadcastRuntime, WebError, WebResult};

/// Embedded single-page browser player HTML served at `GET /`.
const WEB_PLAYER_HTML: &str = include_str!("web_player.html");
/// Vendored jsQR decoder served at `GET /jsqr.js`. Kept local so QR pairing
/// works on plain-HTTP LAN addresses without a CDN or secure context.
const JSQR_JS: &str = include_str!("jsqr.min.js");

/// Maximum concurrent HTTP worker threads.
const MAX_HTTP_WORKERS: usize = 16;

/// Maximum queued connections waiting for an HTTP worker thread.
const HTTP_WORKER_QUEUE_CAP: usize = 64;

/// Per-socket read and write timeout for HTTP requests.
const HTTP_SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum allowed HTTP request header section size (8 KiB).
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// Maximum allowed HTTP request body size (64 KiB).
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Maximum `POST /webrtc/offer` requests per IP within [`OFFER_RATE_LIMIT_WINDOW`].
const OFFER_RATE_LIMIT_MAX: usize = 5;

/// Sliding rate-limit window for `POST /webrtc/offer`.
const OFFER_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(10);

/// Configuration for starting the synchronous HTTP/1.1 Web Broadcast server.
#[derive(Clone)]
pub struct HttpServerConfig {
    pub bind_ip: IpAddr,
    pub http_port: u16,
    pub runtime: Arc<BroadcastRuntime>,
    pub rtc: Arc<RtcMuxHandle>,
    pub ws: Arc<WsFanoutHandle>,
    pub tls: Option<TlsServerMaterial>,
}

/// Handle to the running `web-http-accept` thread and bounded worker pool.
pub struct HttpServerHandle {
    local_port: u16,
    stop_flag: Arc<AtomicBool>,
    queue: Arc<(Mutex<WorkerQueue>, Condvar)>,
    accept_thread: Mutex<Option<JoinHandle<()>>>,
    worker_threads: Mutex<Vec<JoinHandle<()>>>,
}

trait RtcBackend: Send + Sync + 'static {
    fn accept_offer(&self, req: RtcOfferRequest) -> WebResult<RtcOfferResponse>;
    fn remove_peer(&self, peer_id: &str) -> bool;
}

impl RtcBackend for RtcMuxHandle {
    fn accept_offer(&self, req: RtcOfferRequest) -> WebResult<RtcOfferResponse> {
        RtcMuxHandle::accept_offer(self, req)
    }

    fn remove_peer(&self, peer_id: &str) -> bool {
        RtcMuxHandle::remove_peer(self, peer_id)
    }
}

trait WsBackend: Send + Sync + 'static {
    fn accept_upgraded(&self, req: WsUpgradeRequest) -> WebResult<String>;
}

impl WsBackend for WsFanoutHandle {
    fn accept_upgraded(&self, req: WsUpgradeRequest) -> WebResult<String> {
        WsFanoutHandle::accept_upgraded(self, req)
    }
}

struct WorkerQueue {
    jobs: VecDeque<(TcpStream, SocketAddr)>,
    stopping: bool,
}

#[derive(Clone)]
struct ServerContext {
    bind_ip: IpAddr,
    local_port: u16,
    runtime: Arc<BroadcastRuntime>,
    rtc: Arc<dyn RtcBackend>,
    ws: Arc<dyn WsBackend>,
    tls: Option<TlsServerMaterial>,
    offer_rate_limiter: Arc<RateLimiter>,
}

struct RateLimiter {
    buckets: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
        }
    }

    fn check_and_record(&self, peer_ip: IpAddr, now: Instant) -> bool {
        let key = normalize_ip(peer_ip);
        let mut guard = lock_or_recover(&self.buckets);
        if guard.len() > 256 {
            guard.retain(|_, times| {
                while let Some(front) = times.front() {
                    if now.saturating_duration_since(*front) >= OFFER_RATE_LIMIT_WINDOW {
                        times.pop_front();
                    } else {
                        break;
                    }
                }
                !times.is_empty()
            });
        }

        let times = guard.entry(key).or_default();
        while let Some(front) = times.front() {
            if now.saturating_duration_since(*front) >= OFFER_RATE_LIMIT_WINDOW {
                times.pop_front();
            } else {
                break;
            }
        }
        if times.len() >= OFFER_RATE_LIMIT_MAX {
            return false;
        }
        times.push_back(now);
        true
    }
}

impl HttpServerHandle {
    /// Bind `config.bind_ip:config.http_port` and start the HTTP/1.1 accept loop.
    pub fn start(config: HttpServerConfig) -> WebResult<Self> {
        Self::start_with_backends(
            config.bind_ip,
            config.http_port,
            config.runtime,
            config.rtc,
            config.ws,
            config.tls,
        )
    }

    fn start_with_backends(
        bind_ip: IpAddr,
        http_port: u16,
        runtime: Arc<BroadcastRuntime>,
        rtc: Arc<dyn RtcBackend>,
        ws: Arc<dyn WsBackend>,
        tls: Option<TlsServerMaterial>,
    ) -> WebResult<Self> {
        let listener = TcpListener::bind(SocketAddr::new(bind_ip, http_port))?;
        listener.set_nonblocking(true)?;
        let local_port = listener.local_addr()?.port();

        let stop_flag = Arc::new(AtomicBool::new(false));
        let queue = Arc::new((
            Mutex::new(WorkerQueue {
                jobs: VecDeque::with_capacity(HTTP_WORKER_QUEUE_CAP),
                stopping: false,
            }),
            Condvar::new(),
        ));

        let ctx = ServerContext {
            bind_ip,
            local_port,
            runtime,
            rtc,
            ws,
            tls,
            offer_rate_limiter: Arc::new(RateLimiter::new()),
        };

        let mut workers: Vec<JoinHandle<()>> = Vec::with_capacity(MAX_HTTP_WORKERS);
        for idx in 0..MAX_HTTP_WORKERS {
            let queue_clone = Arc::clone(&queue);
            let ctx_clone = ctx.clone();
            let handle = match thread::Builder::new()
                .name(format!("web-http-worker-{idx}"))
                .spawn(move || worker_loop(queue_clone, ctx_clone))
            {
                Ok(h) => h,
                Err(err) => {
                    stop_flag.store(true, Ordering::Release);
                    {
                        let (lock, cvar) = &*queue;
                        lock_or_recover(lock).stopping = true;
                        cvar.notify_all();
                    }
                    for w in workers {
                        let _ = w.join();
                    }
                    return Err(err.into());
                }
            };
            workers.push(handle);
        }

        let accept_stop = Arc::clone(&stop_flag);
        let accept_queue = Arc::clone(&queue);
        let accept_thread = match thread::Builder::new()
            .name("web-http-accept".into())
            .spawn(move || accept_loop(listener, accept_stop, accept_queue))
        {
            Ok(h) => h,
            Err(err) => {
                stop_flag.store(true, Ordering::Release);
                {
                    let (lock, cvar) = &*queue;
                    lock_or_recover(lock).stopping = true;
                    cvar.notify_all();
                }
                for w in workers {
                    let _ = w.join();
                }
                return Err(err.into());
            }
        };

        Ok(Self {
            local_port,
            stop_flag,
            queue,
            accept_thread: Mutex::new(Some(accept_thread)),
            worker_threads: Mutex::new(workers),
        })
    }

    /// Actual TCP port bound by `web-http-accept`.
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// Stop the HTTP accept loop and drain worker threads.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Release);

        if let Some(handle) = lock_or_recover(&self.accept_thread).take() {
            let _ = handle.join();
        }

        {
            let (lock, cvar) = &*self.queue;
            lock_or_recover(lock).stopping = true;
            cvar.notify_all();
        }

        let workers: Vec<JoinHandle<()>> =
            lock_or_recover(&self.worker_threads).drain(..).collect();
        for handle in workers {
            let _ = handle.join();
        }
    }
}

impl Drop for HttpServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn accept_loop(
    listener: TcpListener,
    stop_flag: Arc<AtomicBool>,
    queue: Arc<(Mutex<WorkerQueue>, Condvar)>,
) {
    while !stop_flag.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, peer_addr)) => {
                let pushed = {
                    let (lock, cvar) = &*queue;
                    let mut guard = lock_or_recover(lock);
                    if guard.stopping || guard.jobs.len() >= HTTP_WORKER_QUEUE_CAP {
                        false
                    } else {
                        guard.jobs.push_back((stream, peer_addr));
                        cvar.notify_one();
                        continue;
                    }
                };
                if !pushed {
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                    let mut plain = HttpStream::Plain(stream);
                    let _ = send_json_error(
                        &mut plain,
                        503,
                        "Service Unavailable",
                        "HTTP worker pool busy",
                    );
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(15));
            }
            Err(_) => {
                if stop_flag.load(Ordering::Acquire) {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn worker_loop(queue: Arc<(Mutex<WorkerQueue>, Condvar)>, ctx: ServerContext) {
    let (lock, cvar) = &*queue;
    loop {
        let job = {
            let mut guard = lock_or_recover(lock);
            while guard.jobs.is_empty() && !guard.stopping {
                guard = cvar.wait(guard).unwrap_or_else(|e| e.into_inner());
            }
            if guard.stopping && guard.jobs.is_empty() {
                return;
            }
            guard.jobs.pop_front()
        };

        if let Some((stream, peer_addr)) = job {
            handle_connection(stream, peer_addr, &ctx);
        }
    }
}

enum HttpStream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
}

impl HttpStream {
    fn shutdown_write(&mut self) {
        match self {
            Self::Plain(s) => {
                let _ = s.shutdown(Shutdown::Write);
            }
            Self::Tls(s) => {
                s.conn.send_close_notify();
                let _ = s.flush();
                let _ = s.get_ref().shutdown(Shutdown::Write);
            }
        }
    }

    fn into_ws_stream(self) -> WsStream {
        match self {
            Self::Plain(s) => WsStream::Plain(s),
            Self::Tls(s) => WsStream::Tls(s),
        }
    }
}

impl Read for HttpStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            Self::Tls(s) => s.read(buf),
        }
    }
}

impl Write for HttpStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(buf),
            Self::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            Self::Tls(s) => s.flush(),
        }
    }
}

fn handle_connection(stream: TcpStream, peer_addr: SocketAddr, ctx: &ServerContext) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(HTTP_SOCKET_TIMEOUT));
    let _ = stream.set_write_timeout(Some(HTTP_SOCKET_TIMEOUT));
    let _ = stream.set_nodelay(true);

    if !ctx.runtime.is_ip_allowed(&peer_addr.ip()) {
        let mut plain = HttpStream::Plain(stream);
        let _ = send_json_error(&mut plain, 403, "Forbidden", "peer IP not allowed");
        return;
    }

    let stream_local_ip = stream.local_addr().ok().map(|addr| normalize_ip(addr.ip()));

    let (mut http_stream, is_tls) = if let Some(ref tls) = ctx.tls {
        let mut peek = [0u8; 1];
        match stream.peek(&mut peek) {
            Ok(1) if peek[0] == 0x16 => {
                let Ok(conn) = rustls::ServerConnection::new(Arc::clone(&tls.server_config)) else {
                    return;
                };
                (
                    HttpStream::Tls(Box::new(rustls::StreamOwned::new(conn, stream))),
                    true,
                )
            }
            Ok(_) => (HttpStream::Plain(stream), false),
            Err(_) => return,
        }
    } else {
        (HttpStream::Plain(stream), false)
    };

    let req = match read_http_request(&mut http_stream) {
        Ok(Some(req)) => req,
        Ok(None) => return,
        Err(ParseError::BadRequest(msg)) => {
            let _ = send_json_error(&mut http_stream, 400, "Bad Request", &msg);
            return;
        }
        Err(ParseError::PayloadTooLarge(msg)) => {
            let _ = send_json_error(&mut http_stream, 413, "Payload Too Large", &msg);
            return;
        }
        Err(ParseError::Io) => return,
    };

    match req.header("host") {
        None => {
            if req.version == "HTTP/1.1" {
                let _ =
                    send_json_error(&mut http_stream, 400, "Bad Request", "missing Host header");
                return;
            }
        }
        Some(host) => {
            if !is_valid_host(host, ctx.local_port, stream_local_ip, ctx.bind_ip) {
                let _ = send_json_error(&mut http_stream, 403, "Forbidden", "invalid Host header");
                return;
            }
        }
    }

    if let Some(origin) = req.header("origin") {
        if !is_valid_origin(origin, is_tls, ctx.local_port, stream_local_ip, ctx.bind_ip) {
            let _ = send_json_error(&mut http_stream, 403, "Forbidden", "invalid Origin header");
            return;
        }
    }

    if ctx.tls.is_some()
        && !is_tls
        && should_redirect_plain_http_to_https(&peer_addr.ip(), &req.path)
    {
        let authority = build_redirect_authority(
            req.header("host"),
            stream_local_ip,
            ctx.bind_ip,
            ctx.local_port,
        );
        let location = format!("https://{authority}{}", req.path);
        let _ = send_redirect_https(&mut http_stream, &location);
        return;
    }

    dispatch_request(http_stream, peer_addr, req, ctx);
}

fn should_redirect_plain_http_to_https(peer_ip: &IpAddr, path: &str) -> bool {
    !is_loopback_ip(*peer_ip) && path != "/ca.crt"
}

fn build_redirect_authority(
    host_header: Option<&str>,
    stream_local_ip: Option<IpAddr>,
    bind_ip: IpAddr,
    local_port: u16,
) -> String {
    if let Some(host) = host_header.map(str::trim).filter(|h| !h.is_empty()) {
        return host.to_string();
    }
    let ip = stream_local_ip.unwrap_or_else(|| normalize_ip(bind_ip));
    match ip {
        IpAddr::V6(v6) => format!("[{v6}]:{local_port}"),
        IpAddr::V4(v4) => format!("{v4}:{local_port}"),
    }
}

fn dispatch_request(
    mut stream: HttpStream,
    peer_addr: SocketAddr,
    req: ParsedRequest,
    ctx: &ServerContext,
) {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => {
            let _ = send_response(
                &mut stream,
                200,
                "OK",
                "text/html; charset=utf-8",
                &[],
                WEB_PLAYER_HTML.as_bytes(),
            );
        }
        ("GET", "/jsqr.js") => {
            let _ = send_response(
                &mut stream,
                200,
                "OK",
                "application/javascript; charset=utf-8",
                &[],
                JSQR_JS.as_bytes(),
            );
        }
        ("GET", "/ca.crt") => {
            if let Some(pem) = ctx.tls.as_ref().and_then(|t| t.ca_cert_pem.as_ref()) {
                let _ = send_response(
                    &mut stream,
                    200,
                    "OK",
                    "application/x-x509-ca-cert",
                    &[(
                        "Content-Disposition",
                        "attachment; filename=\"soundhop-ca.crt\"",
                    )],
                    pem,
                );
            } else {
                let _ = send_json_error(
                    &mut stream,
                    404,
                    "Not Found",
                    "CA certificate not available",
                );
            }
        }
        ("GET", "/api/info") => {
            handle_api_info(&mut stream, &req, &ctx.runtime);
        }
        ("POST", "/webrtc/offer") => {
            handle_webrtc_offer(&mut stream, peer_addr, &req, ctx);
        }
        ("DELETE", path) if path.starts_with("/webrtc/peer/") => {
            handle_delete_peer(&mut stream, path, &req, ctx);
        }
        ("GET", "/ws") => {
            handle_ws_upgrade(stream, peer_addr, req, ctx);
        }
        (_, "/")
        | (_, "/jsqr.js")
        | (_, "/ca.crt")
        | (_, "/api/info")
        | (_, "/webrtc/offer")
        | (_, "/ws") => {
            let allow = match req.path.as_str() {
                "/" | "/jsqr.js" | "/ca.crt" | "/api/info" | "/ws" => "GET",
                "/webrtc/offer" => "POST",
                _ => "GET",
            };
            let _ = send_json_error_with_headers(
                &mut stream,
                405,
                "Method Not Allowed",
                "method not allowed",
                &[("Allow", allow)],
            );
        }
        (_, path) if path.starts_with("/webrtc/peer/") => {
            let _ = send_json_error_with_headers(
                &mut stream,
                405,
                "Method Not Allowed",
                "method not allowed",
                &[("Allow", "DELETE")],
            );
        }
        _ => {
            let _ = send_json_error(&mut stream, 404, "Not Found", "not found");
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiInfoAuthRequired {
    auth_required: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiInfoFull<'a> {
    version: &'a str,
    device_name: &'a str,
    sample_rate: u32,
    channels: u8,
    frame_samples: u32,
    codec: &'a str,
    ws_codec: &'a str,
    webrtc: bool,
    ws: bool,
    auth_required: bool,
}

fn handle_api_info(stream: &mut HttpStream, req: &ParsedRequest, runtime: &BroadcastRuntime) {
    match runtime.verify_http_bearer(req.header("authorization")) {
        BearerAuthResult::Missing => {
            let body = serde_json::to_vec(&ApiInfoAuthRequired {
                auth_required: true,
            })
            .unwrap_or_else(|_| b"{\"authRequired\":true}".to_vec());
            let _ = send_response(stream, 200, "OK", "application/json", &[], &body);
        }
        BearerAuthResult::Invalid => {
            let _ = send_unauthorized(stream);
        }
        BearerAuthResult::Authorized => {
            let info = ApiInfoFull {
                version: VERSION,
                device_name: runtime.device_name(),
                sample_rate: 48_000,
                channels: 2,
                frame_samples: 960,
                codec: "opus",
                ws_codec: "pcm16le",
                webrtc: true,
                ws: true,
                auth_required: runtime.token_required(),
            };
            let body = serde_json::to_vec(&info).unwrap_or_default();
            let _ = send_response(stream, 200, "OK", "application/json", &[], &body);
        }
    }
}

#[derive(Deserialize)]
struct OfferBody {
    #[serde(rename = "type")]
    kind: Option<String>,
    sdp: String,
    #[serde(rename = "clientName", alias = "client_name")]
    client_name: Option<String>,
    #[serde(default)]
    potato: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OfferAnswerBody {
    #[serde(rename = "type")]
    kind: &'static str,
    sdp: String,
    peer_id: String,
    ice_lite: bool,
}

fn handle_webrtc_offer(
    stream: &mut HttpStream,
    peer_addr: SocketAddr,
    req: &ParsedRequest,
    ctx: &ServerContext,
) {
    if ctx.runtime.verify_http_bearer(req.header("authorization")) != BearerAuthResult::Authorized {
        let _ = send_unauthorized(stream);
        return;
    }

    if !ctx
        .offer_rate_limiter
        .check_and_record(peer_addr.ip(), Instant::now())
    {
        let _ = send_json_error_with_headers(
            stream,
            429,
            "Too Many Requests",
            "rate limit exceeded",
            &[("Retry-After", "10")],
        );
        return;
    }

    let parsed: OfferBody = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(_) => {
            let _ = send_json_error(stream, 400, "Bad Request", "invalid JSON offer");
            return;
        }
    };

    if parsed.kind.as_deref().is_some_and(|k| k != "offer") {
        let _ = send_json_error(stream, 400, "Bad Request", "SDP offer type must be 'offer'");
        return;
    }
    if parsed.sdp.trim().is_empty() {
        let _ = send_json_error(stream, 400, "Bad Request", "SDP offer must not be empty");
        return;
    }

    let offer_req = RtcOfferRequest {
        offer_sdp: parsed.sdp,
        remote_addr: peer_addr,
        user_agent: req.header("user-agent").unwrap_or("").to_string(),
        client_name: parsed
            .client_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        potato: parsed.potato,
    };

    match ctx.rtc.accept_offer(offer_req) {
        Ok(resp) => {
            let payload = OfferAnswerBody {
                kind: "answer",
                sdp: resp.answer_sdp,
                peer_id: resp.peer_id,
                ice_lite: resp.ice_lite,
            };
            let body = serde_json::to_vec(&payload).unwrap_or_default();
            let _ = send_response(stream, 200, "OK", "application/json", &[], &body);
        }
        Err(WebError::BadRequest(msg)) | Err(WebError::Rtc(msg)) => {
            let _ = send_json_error(stream, 400, "Bad Request", &msg);
        }
        Err(WebError::Unauthorized(msg)) => {
            let _ = send_json_error_with_headers(
                stream,
                401,
                "Unauthorized",
                &msg,
                &[("WWW-Authenticate", "Bearer")],
            );
        }
        Err(WebError::Forbidden(msg)) => {
            let _ = send_json_error(stream, 403, "Forbidden", &msg);
        }
        Err(WebError::RateLimited(msg)) => {
            let _ = send_json_error_with_headers(
                stream,
                429,
                "Too Many Requests",
                &msg,
                &[("Retry-After", "10")],
            );
        }
        Err(WebError::MaxClientsReached { max }) => {
            let _ = send_json_error(
                stream,
                503,
                "Service Unavailable",
                &format!("maximum client limit ({max}) reached"),
            );
        }
        Err(WebError::ServiceUnavailable(msg)) => {
            let _ = send_json_error(stream, 503, "Service Unavailable", &msg);
        }
        Err(err) => {
            let _ = send_json_error(stream, 503, "Service Unavailable", &err.to_string());
        }
    }
}

fn handle_delete_peer(
    stream: &mut HttpStream,
    path: &str,
    req: &ParsedRequest,
    ctx: &ServerContext,
) {
    let peer_id = &path["/webrtc/peer/".len()..];
    if peer_id.is_empty() || peer_id.contains('/') {
        let _ = send_json_error(stream, 400, "Bad Request", "invalid peer id");
        return;
    }

    if ctx.runtime.verify_http_bearer(req.header("authorization")) != BearerAuthResult::Authorized {
        let _ = send_unauthorized(stream);
        return;
    }

    let _ = ctx.rtc.remove_peer(peer_id);
    let _ = send_no_content(stream);
}

fn handle_ws_upgrade(
    mut stream: HttpStream,
    peer_addr: SocketAddr,
    req: ParsedRequest,
    ctx: &ServerContext,
) {
    if !req.body.is_empty() {
        let _ = send_json_error(
            &mut stream,
            400,
            "Bad Request",
            "WebSocket upgrade request must not include a body",
        );
        return;
    }

    let upgrade_ok = req
        .header("upgrade")
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let connection_ok = req.headers_matching("connection").any(|v| {
        v.split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
    });
    if !upgrade_ok || !connection_ok {
        let _ = send_json_error(
            &mut stream,
            400,
            "Bad Request",
            "missing or invalid WebSocket Upgrade/Connection headers",
        );
        return;
    }

    if let Some(version) = req.header("sec-websocket-version") {
        if version != "13" {
            let _ = send_json_error(
                &mut stream,
                400,
                "Bad Request",
                "unsupported Sec-WebSocket-Version",
            );
            return;
        }
    }

    let Some(sec_websocket_key) = req
        .header("sec-websocket-key")
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
    else {
        let _ = send_json_error(&mut stream, 400, "Bad Request", "missing Sec-WebSocket-Key");
        return;
    };

    let mut client_sent_soundhop_protocol = false;
    let mut subprotocol_token: Option<String> = None;
    for proto_hdr in req.headers_matching("sec-websocket-protocol") {
        for raw_proto in proto_hdr.split(',') {
            let proto = raw_proto.trim();
            if proto == WS_SUBPROTOCOL {
                client_sent_soundhop_protocol = true;
            } else if let Some(tok) = proto.strip_prefix("auth.") {
                if !tok.is_empty() && subprotocol_token.is_none() {
                    subprotocol_token = Some(tok.to_string());
                }
            }
        }
    }

    let user_agent = req.header("user-agent").unwrap_or("").to_string();
    let upgrade_req = WsUpgradeRequest {
        stream: stream.into_ws_stream(),
        remote_addr: peer_addr,
        user_agent,
        sec_websocket_key,
        subprotocol_token,
        client_sent_soundhop_protocol,
        leftover_bytes: req.leftover,
    };

    let _ = ctx.ws.accept_upgraded(upgrade_req);
}

enum ParseError {
    BadRequest(String),
    PayloadTooLarge(String),
    Io,
}

struct ParsedRequest {
    method: String,
    path: String,
    version: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    leftover: Vec<u8>,
}

impl ParsedRequest {
    fn header(&self, lower_name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == lower_name)
            .map(|(_, v)| v.as_str())
    }

    fn headers_matching<'a>(&'a self, lower_name: &'a str) -> impl Iterator<Item = &'a str> {
        self.headers
            .iter()
            .filter(move |(k, _)| k == lower_name)
            .map(|(_, v)| v.as_str())
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn read_http_request(stream: &mut HttpStream) -> Result<Option<ParsedRequest>, ParseError> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let header_end = loop {
        if let Some(pos) = find_header_end(&buf) {
            if pos + 4 > MAX_HEADER_BYTES {
                return Err(ParseError::BadRequest(
                    "HTTP request headers exceed 8 KiB limit".into(),
                ));
            }
            break pos;
        }
        if buf.len() >= MAX_HEADER_BYTES {
            return Err(ParseError::BadRequest(
                "HTTP request headers exceed 8 KiB limit".into(),
            ));
        }
        match stream.read(&mut chunk) {
            Ok(0) => {
                if buf.is_empty() {
                    return Ok(None);
                }
                return Err(ParseError::BadRequest("incomplete HTTP request".into()));
            }
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(_) => return Err(ParseError::Io),
        }
    };

    let header_str = std::str::from_utf8(&buf[..header_end])
        .map_err(|_| ParseError::BadRequest("non-UTF-8 HTTP headers".into()))?;
    let mut lines = header_str.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| ParseError::BadRequest("missing HTTP request line".into()))?;

    let mut req_parts = request_line.split(' ');
    let method = req_parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ParseError::BadRequest("invalid request method".into()))?;
    let target = req_parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ParseError::BadRequest("invalid request target".into()))?;
    let version = req_parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ParseError::BadRequest("invalid HTTP version".into()))?;
    if req_parts.next().is_some() {
        return Err(ParseError::BadRequest("malformed HTTP request line".into()));
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(ParseError::BadRequest("unsupported HTTP version".into()));
    }
    if !target.starts_with('/') {
        return Err(ParseError::BadRequest("invalid request path".into()));
    }
    let path = target.split('?').next().unwrap_or(target).to_string();

    let mut headers = Vec::new();
    let mut content_length: Option<usize> = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((raw_name, raw_val)) = line.split_once(':') else {
            return Err(ParseError::BadRequest("malformed HTTP header".into()));
        };
        if raw_name.is_empty() || raw_name.bytes().any(|b| b <= b' ' || b == 127) {
            return Err(ParseError::BadRequest("invalid HTTP header name".into()));
        }
        let name = raw_name.to_ascii_lowercase();
        let value = raw_val.trim().to_string();

        if name == "transfer-encoding" && !value.eq_ignore_ascii_case("identity") {
            return Err(ParseError::BadRequest(
                "Transfer-Encoding is not supported".into(),
            ));
        }
        if name == "content-length" {
            let parsed_len = value
                .parse::<usize>()
                .map_err(|_| ParseError::BadRequest("invalid Content-Length".into()))?;
            if let Some(prev) = content_length {
                if prev != parsed_len {
                    return Err(ParseError::BadRequest(
                        "conflicting Content-Length headers".into(),
                    ));
                }
            } else {
                content_length = Some(parsed_len);
            }
        }
        headers.push((name, value));
    }

    let body_len = content_length.unwrap_or(0);
    if body_len > MAX_BODY_BYTES {
        return Err(ParseError::PayloadTooLarge(
            "request body exceeds 64 KiB limit".into(),
        ));
    }

    let trailing = &buf[header_end + 4..];
    let (body, leftover) = if body_len == 0 {
        (Vec::new(), trailing.to_vec())
    } else if trailing.len() >= body_len {
        (trailing[..body_len].to_vec(), Vec::new())
    } else {
        let mut body = Vec::with_capacity(body_len);
        body.extend_from_slice(trailing);
        let remaining = body_len - body.len();
        let start = body.len();
        body.resize(body_len, 0);
        stream
            .read_exact(&mut body[start..start + remaining])
            .map_err(|_| ParseError::BadRequest("truncated request body".into()))?;
        (body, Vec::new())
    };

    Ok(Some(ParsedRequest {
        method: method.to_string(),
        path,
        version: version.to_string(),
        headers,
        body,
        leftover,
    }))
}

fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        IpAddr::V4(_) => ip,
    }
}

fn parse_authority(raw: &str) -> Option<(&str, Option<u16>)> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.bytes().any(|b| b.is_ascii_whitespace())
        || trimmed.contains('/')
        || trimmed.contains('?')
        || trimmed.contains('#')
        || trimmed.contains('@')
    {
        return None;
    }
    if let Some(after_bracket) = trimmed.strip_prefix('[') {
        let (inside, rest) = after_bracket.split_once(']')?;
        let _: std::net::Ipv6Addr = inside.parse().ok()?;
        if rest.is_empty() {
            return Some((inside, None));
        }
        let port_str = rest.strip_prefix(':')?;
        let port = port_str.parse::<u16>().ok()?;
        return Some((inside, Some(port)));
    }
    match trimmed.split_once(':') {
        Some((host, port_str)) => {
            if host.is_empty() || port_str.contains(':') {
                return None;
            }
            let port = port_str.parse::<u16>().ok()?;
            Some((host, Some(port)))
        }
        None => Some((trimmed, None)),
    }
}

fn is_allowed_authority(
    authority: &str,
    local_port: u16,
    stream_local_ip: Option<IpAddr>,
    bind_ip: IpAddr,
) -> bool {
    let Some((host, port_opt)) = parse_authority(authority) else {
        return false;
    };
    if port_opt.unwrap_or(80) != local_port {
        return false;
    }
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let Ok(parsed_ip) = host.parse::<IpAddr>() else {
        return false;
    };
    let ip = normalize_ip(parsed_ip);
    if ip.is_loopback() {
        return true;
    }
    if Some(ip) == stream_local_ip {
        return true;
    }
    if !bind_ip.is_unspecified() && ip == normalize_ip(bind_ip) {
        return true;
    }
    local_addresses(true)
        .into_iter()
        .filter_map(|s| s.parse::<IpAddr>().ok().map(normalize_ip))
        .any(|local| local == ip)
}

fn is_valid_host(
    host: &str,
    local_port: u16,
    stream_local_ip: Option<IpAddr>,
    bind_ip: IpAddr,
) -> bool {
    is_allowed_authority(host, local_port, stream_local_ip, bind_ip)
}

fn is_valid_origin(
    origin: &str,
    is_tls: bool,
    local_port: u16,
    stream_local_ip: Option<IpAddr>,
    bind_ip: IpAddr,
) -> bool {
    let trimmed = origin.trim();
    let expected_prefix = if is_tls { "https://" } else { "http://" };
    let Some(prefix) = trimmed.get(..expected_prefix.len()) else {
        return false;
    };
    if !prefix.eq_ignore_ascii_case(expected_prefix) {
        return false;
    }
    is_allowed_authority(
        &trimmed[expected_prefix.len()..],
        local_port,
        stream_local_ip,
        bind_ip,
    )
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

fn send_unauthorized(stream: &mut HttpStream) -> std::io::Result<()> {
    send_json_error_with_headers(
        stream,
        401,
        "Unauthorized",
        "unauthorized",
        &[("WWW-Authenticate", "Bearer")],
    )
}

fn send_json_error(
    stream: &mut HttpStream,
    status: u16,
    reason: &str,
    message: &str,
) -> std::io::Result<()> {
    send_json_error_with_headers(stream, status, reason, message, &[])
}

fn send_json_error_with_headers(
    stream: &mut HttpStream,
    status: u16,
    reason: &str,
    message: &str,
    extra_headers: &[(&str, &str)],
) -> std::io::Result<()> {
    let body = serde_json::to_vec(&ErrorBody { error: message })
        .unwrap_or_else(|_| b"{\"error\":\"error\"}".to_vec());
    send_response(
        stream,
        status,
        reason,
        "application/json",
        extra_headers,
        &body,
    )
}

fn send_no_content(stream: &mut HttpStream) -> std::io::Result<()> {
    let headers = "HTTP/1.1 204 No Content\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\r\n";
    stream.write_all(headers.as_bytes())?;
    let _ = stream.flush();
    stream.shutdown_write();
    Ok(())
}

fn send_redirect_https(stream: &mut HttpStream, location: &str) -> std::io::Result<()> {
    let headers = format!(
        "HTTP/1.1 308 Permanent Redirect\r\n\
         Location: {location}\r\n\
         Cache-Control: no-store\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(headers.as_bytes())?;
    let _ = stream.flush();
    stream.shutdown_write();
    Ok(())
}

fn send_response(
    stream: &mut HttpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n",
        body.len()
    );
    for (k, v) in extra_headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes())?;
    if !body.is_empty() {
        stream.write_all(body)?;
    }
    let _ = stream.flush();
    stream.shutdown_write();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::wire::AudioParams;
    use crate::transport::CaptureEndpoint;
    use crate::web::{CaptureFactory, WebBroadcastConfig, WebBroadcastManager, WebCaptureTarget};
    use std::net::Ipv4Addr;

    fn dummy_capture_factory() -> CaptureFactory {
        Arc::new(|| {
            #[cfg(feature = "gstreamer")]
            let endpoint = CaptureEndpoint::TestSineWave {
                freq_hz: 440.0,
                netsim: None,
            };
            #[cfg(not(feature = "gstreamer"))]
            let endpoint = CaptureEndpoint::TestSineWave { freq_hz: 440.0 };

            Ok(WebCaptureTarget {
                endpoint,
                audio_params: AudioParams::default(),
                label: "test-sine-440".into(),
            })
        })
    }

    struct TestHttpResponse {
        status: u16,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    impl TestHttpResponse {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .get(&name.to_ascii_lowercase())
                .map(String::as_str)
        }

        fn body_str(&self) -> &str {
            std::str::from_utf8(&self.body).expect("utf-8 response body")
        }

        fn json(&self) -> serde_json::Value {
            serde_json::from_slice(&self.body).expect("json response body")
        }
    }

    fn send_raw_http(port: u16, raw_request: &[u8]) -> TestHttpResponse {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect http test");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set_read_timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("set_write_timeout");
        stream.write_all(raw_request).expect("write request");

        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read response");
        parse_raw_http_response(&buf)
    }

    fn parse_raw_http_response(raw: &[u8]) -> TestHttpResponse {
        let head_end = find_header_end(raw).expect("response has CRLFCRLF");
        let head = std::str::from_utf8(&raw[..head_end]).expect("utf-8 response head");
        let mut lines = head.split("\r\n");
        let status_line = lines.next().expect("status line");
        let status = status_line
            .split(' ')
            .nth(1)
            .expect("status code")
            .parse::<u16>()
            .expect("u16 status");
        let mut headers = HashMap::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        TestHttpResponse {
            status,
            headers,
            body: raw[head_end + 4..].to_vec(),
        }
    }

    fn start_test_manager(require_token: bool) -> (WebBroadcastManager, u16, Option<String>) {
        let mgr = WebBroadcastManager::new();
        let cfg = WebBroadcastConfig {
            http_port: 0,
            webrtc_udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            require_token,
            https_enabled: false,
            ..Default::default()
        };
        let status = mgr
            .start(cfg, dummy_capture_factory())
            .expect("start web broadcast manager");
        (mgr, status.http_port, status.token)
    }

    struct MockRtc {
        removed_peers: Mutex<Vec<String>>,
        next_result: Mutex<Option<WebResult<RtcOfferResponse>>>,
        potato_requests: Mutex<Vec<bool>>,
    }

    impl MockRtc {
        fn new() -> Self {
            Self {
                removed_peers: Mutex::new(Vec::new()),
                next_result: Mutex::new(None),
                potato_requests: Mutex::new(Vec::new()),
            }
        }

        fn set_next_result(&self, res: WebResult<RtcOfferResponse>) {
            *lock_or_recover(&self.next_result) = Some(res);
        }

        fn take_potato_requests(&self) -> Vec<bool> {
            std::mem::take(&mut *lock_or_recover(&self.potato_requests))
        }
    }

    impl RtcBackend for MockRtc {
        fn accept_offer(&self, req: RtcOfferRequest) -> WebResult<RtcOfferResponse> {
            lock_or_recover(&self.potato_requests).push(req.potato);
            if let Some(res) = lock_or_recover(&self.next_result).take() {
                return res;
            }
            Ok(RtcOfferResponse {
                peer_id: "peer-1".into(),
                answer_sdp: format!("v=0\r\na=x-offer-len:{}\r\n", req.offer_sdp.len()),
                ice_lite: true,
            })
        }

        fn remove_peer(&self, peer_id: &str) -> bool {
            lock_or_recover(&self.removed_peers).push(peer_id.to_string());
            true
        }
    }

    struct CapturedWsUpgrade {
        remote_addr: SocketAddr,
        user_agent: String,
        sec_websocket_key: String,
        subprotocol_token: Option<String>,
        client_sent_soundhop_protocol: bool,
        leftover_bytes: Vec<u8>,
    }

    struct MockWs {
        captured: Mutex<Vec<CapturedWsUpgrade>>,
        reply_101: bool,
    }

    impl MockWs {
        fn new(reply_101: bool) -> Self {
            Self {
                captured: Mutex::new(Vec::new()),
                reply_101,
            }
        }
    }

    impl WsBackend for MockWs {
        fn accept_upgraded(&self, mut req: WsUpgradeRequest) -> WebResult<String> {
            lock_or_recover(&self.captured).push(CapturedWsUpgrade {
                remote_addr: req.remote_addr,
                user_agent: req.user_agent,
                sec_websocket_key: req.sec_websocket_key,
                subprotocol_token: req.subprotocol_token,
                client_sent_soundhop_protocol: req.client_sent_soundhop_protocol,
                leftover_bytes: req.leftover_bytes,
            });
            if self.reply_101 {
                let resp = "HTTP/1.1 101 Switching Protocols\r\n\
                            Upgrade: websocket\r\n\
                            Connection: Upgrade\r\n\
                            Sec-WebSocket-Protocol: soundhop.v1\r\n\r\n";
                let _ = req.stream.write_all(resp.as_bytes());
                let _ = req.stream.flush();
                let _ = req.stream.tcp().shutdown(Shutdown::Write);
            }
            Ok("ws-1".into())
        }
    }

    fn start_mock_http_server(
        require_token: bool,
        allowed_networks: Vec<String>,
        rtc: Arc<MockRtc>,
        ws: Arc<MockWs>,
    ) -> (WebBroadcastManager, Arc<BroadcastRuntime>, HttpServerHandle) {
        let mgr = WebBroadcastManager::new();
        let cfg = WebBroadcastConfig {
            http_port: 0,
            webrtc_udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            require_token,
            allowed_networks,
            device_name: "SoundHop-TestBox".into(),
            https_enabled: false,
            ..Default::default()
        };
        mgr.start(cfg, dummy_capture_factory()).expect("start mgr");
        let rt = mgr.runtime().expect("runtime");
        let handle = HttpServerHandle::start_with_backends(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            Arc::clone(&rt),
            rtc,
            ws,
            None,
        )
        .expect("start mock http");
        (mgr, rt, handle)
    }

    #[test]
    fn get_root_serves_html_with_security_headers() {
        let (mgr, port, _) = start_test_manager(true);
        let req = format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
        let resp = send_raw_http(port, req.as_bytes());
        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.header("content-type"),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(resp.header("cache-control"), Some("no-store"));
        assert_eq!(resp.header("x-content-type-options"), Some("nosniff"));
        assert!(resp.body_str().contains("SoundHop"));
        assert_eq!(resp.body_str(), WEB_PLAYER_HTML);
        mgr.stop();
    }

    #[test]
    fn root_html_offers_native_camera_capture_fallback() {
        // getUserMedia is blocked on plain-HTTP LAN origins, so the scanner must
        // also offer the native camera app via a capture file input.
        assert!(WEB_PLAYER_HTML.contains("id=\"scannerCapture\""));
        assert!(WEB_PLAYER_HTML.contains("capture=\"environment\""));
        assert!(WEB_PLAYER_HTML.contains("handleScannerFile(els.scannerCapture)"));
    }

    #[test]
    fn root_html_has_force_opus_transport_toggle() {
        assert!(WEB_PLAYER_HTML.contains("id=\"modeWebrtcBtn\""));
        assert!(WEB_PLAYER_HTML.contains("data-mode=\"webrtc\""));
        assert!(WEB_PLAYER_HTML.contains("Force Opus"));
    }

    #[test]
    fn get_jsqr_asset_is_served_without_auth() {
        let (mgr, port, _) = start_test_manager(true);
        let req = format!("GET /jsqr.js HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
        let resp = send_raw_http(port, req.as_bytes());
        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.header("content-type"),
            Some("application/javascript; charset=utf-8")
        );
        assert_eq!(resp.header("cache-control"), Some("no-store"));
        assert!(resp.body_str().contains("jsQR"));
        mgr.stop();
    }

    #[test]
    fn get_api_info_unauthenticated_vs_valid_and_invalid_bearer() {
        let (mgr, port, token_opt) = start_test_manager(true);
        let token = token_opt.expect("token enabled");

        // Missing Authorization -> 200 {"authRequired":true}
        let req_missing = format!("GET /api/info HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n");
        let resp_missing = send_raw_http(port, req_missing.as_bytes());
        assert_eq!(resp_missing.status, 200);
        let json_missing = resp_missing.json();
        assert_eq!(json_missing, serde_json::json!({ "authRequired": true }));

        // Invalid Bearer token -> 401
        let req_bad = format!(
            "GET /api/info HTTP/1.1\r\nHost: localhost:{port}\r\nAuthorization: Bearer wrong-token\r\n\r\n"
        );
        let resp_bad = send_raw_http(port, req_bad.as_bytes());
        assert_eq!(resp_bad.status, 401);

        // Non-Bearer Authorization -> 401
        let req_basic = format!(
            "GET /api/info HTTP/1.1\r\nHost: localhost:{port}\r\nAuthorization: Basic dXNlcjpwYXNz\r\n\r\n"
        );
        let resp_basic = send_raw_http(port, req_basic.as_bytes());
        assert_eq!(resp_basic.status, 401);

        // Valid Bearer token -> 200 full metadata
        let req_ok = format!(
            "GET /api/info HTTP/1.1\r\nHost: localhost:{port}\r\nAuthorization: Bearer {token}\r\n\r\n"
        );
        let resp_ok = send_raw_http(port, req_ok.as_bytes());
        assert_eq!(resp_ok.status, 200);
        let json_ok = resp_ok.json();
        assert_eq!(json_ok["version"], VERSION);
        assert_eq!(json_ok["sampleRate"], 48000);
        assert_eq!(json_ok["channels"], 2);
        assert_eq!(json_ok["frameSamples"], 960);
        assert_eq!(json_ok["codec"], "opus");
        assert_eq!(json_ok["wsCodec"], "pcm16le");
        assert_eq!(json_ok["webrtc"], true);
        assert_eq!(json_ok["ws"], true);
        assert_eq!(json_ok["authRequired"], true);
        assert!(json_ok["deviceName"].is_string());

        mgr.stop();

        // Token disabled -> unauthenticated returns full metadata with authRequired: false
        let (mgr_open, port_open, token_none) = start_test_manager(false);
        assert!(token_none.is_none());
        let req_open = format!("GET /api/info HTTP/1.1\r\nHost: 127.0.0.1:{port_open}\r\n\r\n");
        let resp_open = send_raw_http(port_open, req_open.as_bytes());
        assert_eq!(resp_open.status, 200);
        let json_open = resp_open.json();
        assert_eq!(json_open["authRequired"], false);
        assert_eq!(json_open["version"], VERSION);
        mgr_open.stop();
    }

    #[test]
    fn host_origin_and_subnet_validation_return_403() {
        let (mgr, port, _) = start_test_manager(false);

        // Host mismatches -> 403
        for bad_host in [
            format!("evil.example:{port}"),
            format!("127.0.0.1:{}", port.saturating_add(1)),
            "localhost".to_string(),
        ] {
            let req = format!("GET / HTTP/1.1\r\nHost: {bad_host}\r\n\r\n");
            let resp = send_raw_http(port, req.as_bytes());
            assert_eq!(resp.status, 403, "expected 403 for Host: {bad_host}");
        }

        // Origin mismatches -> 403
        for bad_origin in [
            format!("http://evil.example:{port}"),
            format!("http://127.0.0.1:{}", port.saturating_add(1)),
            format!("https://localhost:{port}"),
            "null".to_string(),
            format!("http://localhost:{port}/extra"),
        ] {
            let req = format!(
                "GET /api/info HTTP/1.1\r\nHost: localhost:{port}\r\nOrigin: {bad_origin}\r\n\r\n"
            );
            let resp = send_raw_http(port, req.as_bytes());
            assert_eq!(resp.status, 403, "expected 403 for Origin: {bad_origin}");
        }

        // Allowed Origins -> 200
        for good_origin in [
            format!("http://localhost:{port}"),
            format!("http://127.0.0.1:{port}"),
        ] {
            let req = format!(
                "GET /api/info HTTP/1.1\r\nHost: localhost:{port}\r\nOrigin: {good_origin}\r\n\r\n"
            );
            let resp = send_raw_http(port, req.as_bytes());
            assert_eq!(resp.status, 200, "expected 200 for Origin: {good_origin}");
        }

        mgr.stop();

        // Subnet 403 rejection when peer_addr is outside allowed_networks/loopback
        let rtc = Arc::new(MockRtc::new());
        let ws = Arc::new(MockWs::new(false));
        let (mgr2, rt2, http2) =
            start_mock_http_server(false, vec!["10.42.0.0/16".into()], rtc.clone(), ws.clone());

        let probe_listener = TcpListener::bind("127.0.0.1:0").expect("bind probe");
        let probe_port = probe_listener.local_addr().unwrap().port();
        let mut client = TcpStream::connect(("127.0.0.1", probe_port)).expect("connect probe");
        let (server_stream, _) = probe_listener.accept().expect("accept probe");

        let ctx = ServerContext {
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            local_port: http2.local_port(),
            runtime: rt2,
            rtc,
            ws,
            tls: None,
            offer_rate_limiter: Arc::new(RateLimiter::new()),
        };

        let disallowed_peer: SocketAddr = "8.8.8.8:54321".parse().unwrap();
        let handle_thread = thread::spawn(move || {
            handle_connection(server_stream, disallowed_peer, &ctx);
        });
        client
            .write_all(
                format!(
                    "GET /api/info HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                    http2.local_port()
                )
                .as_bytes(),
            )
            .expect("write");
        let mut raw_resp = Vec::new();
        client.read_to_end(&mut raw_resp).expect("read");
        handle_thread.join().expect("join");

        let parsed = parse_raw_http_response(&raw_resp);
        assert_eq!(parsed.status, 403);

        http2.stop();
        mgr2.stop();
    }

    #[test]
    fn header_and_body_size_limits_return_400_and_413() {
        let (mgr, port, token_opt) = start_test_manager(true);
        let token = token_opt.expect("token");

        // > 8 KiB headers -> 400
        let oversized_hdr = "a".repeat(MAX_HEADER_BYTES + 256);
        let req_big_hdr =
            format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Big: {oversized_hdr}\r\n\r\n");
        let resp_big_hdr = send_raw_http(port, req_big_hdr.as_bytes());
        assert_eq!(resp_big_hdr.status, 400);

        // > 64 KiB body -> 413
        let oversized_len = MAX_BODY_BYTES + 1;
        let req_big_body = format!(
            "POST /webrtc/offer HTTP/1.1\r\n\
             Host: 127.0.0.1:{port}\r\n\
             Authorization: Bearer {token}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {oversized_len}\r\n\r\n"
        );
        let resp_big_body = send_raw_http(port, req_big_body.as_bytes());
        assert_eq!(resp_big_body.status, 413);

        mgr.stop();
    }

    #[test]
    fn post_webrtc_offer_auth_rate_limit_and_error_codes() {
        let rtc = Arc::new(MockRtc::new());
        let ws = Arc::new(MockWs::new(false));
        let (mgr, rt, http) =
            start_mock_http_server(true, vec!["127.0.0.1".into()], rtc.clone(), ws);
        let port = http.local_port();
        let token = rt.current_token().expect("token");
        let valid_body =
            r#"{"type":"offer","sdp":"v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\n","clientName":"Pixel"}"#;

        // 401 without Bearer token
        let req_unauth = format!(
            "POST /webrtc/offer HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\n\r\n{valid_body}",
            valid_body.len()
        );
        assert_eq!(send_raw_http(port, req_unauth.as_bytes()).status, 401);

        // 400 on malformed JSON (consumes 1 rate-limit token)
        let bad_json = b"{not-json";
        let req_bad_json = format!(
            "POST /webrtc/offer HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n",
            bad_json.len()
        );
        let mut req_bad_bytes = req_bad_json.into_bytes();
        req_bad_bytes.extend_from_slice(bad_json);
        assert_eq!(send_raw_http(port, &req_bad_bytes).status, 400);

        // 200 OK (2nd request)
        let req_ok = format!(
            "POST /webrtc/offer HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{valid_body}",
            valid_body.len()
        );
        let resp_ok = send_raw_http(port, req_ok.as_bytes());
        assert_eq!(resp_ok.status, 200);
        let answer = resp_ok.json();
        assert_eq!(answer["type"], "answer");
        assert_eq!(answer["peerId"], "peer-1");
        assert_eq!(answer["iceLite"], true);
        assert!(answer["sdp"].as_str().unwrap().contains("v=0"));

        // 400 from rtc.accept_offer BadRequest/Rtc (3rd request)
        rtc.set_next_result(Err(WebError::BadRequest("invalid SDP".into())));
        assert_eq!(send_raw_http(port, req_ok.as_bytes()).status, 400);

        // 503 from MaxClientsReached (4th request)
        rtc.set_next_result(Err(WebError::MaxClientsReached { max: 8 }));
        assert_eq!(send_raw_http(port, req_ok.as_bytes()).status, 503);

        // 503 from ServiceUnavailable (5th request)
        rtc.set_next_result(Err(WebError::ServiceUnavailable("no gstreamer".into())));
        assert_eq!(send_raw_http(port, req_ok.as_bytes()).status, 503);

        // 6th request within 10s -> 429 Too Many Requests
        let resp_429 = send_raw_http(port, req_ok.as_bytes());
        assert_eq!(resp_429.status, 429);
        assert_eq!(resp_429.header("retry-after"), Some("10"));

        http.stop();
        mgr.stop();
    }

    #[test]
    fn post_webrtc_offer_parses_potato_true_false_and_omitted() {
        let rtc = Arc::new(MockRtc::new());
        let ws = Arc::new(MockWs::new(false));
        let (mgr, _rt, http) =
            start_mock_http_server(false, vec!["127.0.0.1".into()], rtc.clone(), ws);
        let port = http.local_port();

        let bodies = [
            r#"{"type":"offer","sdp":"v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\n"}"#,
            r#"{"type":"offer","sdp":"v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\n","potato":true}"#,
            r#"{"type":"offer","sdp":"v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\n","potato":false}"#,
        ];

        for body in bodies {
            let req = format!(
                "POST /webrtc/offer HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            assert_eq!(send_raw_http(port, req.as_bytes()).status, 200);
        }

        assert_eq!(rtc.take_potato_requests(), vec![false, true, false]);

        http.stop();
        mgr.stop();
    }

    #[test]
    fn delete_webrtc_peer_requires_auth_and_returns_204() {
        let rtc = Arc::new(MockRtc::new());
        let ws = Arc::new(MockWs::new(false));
        let (mgr, rt, http) =
            start_mock_http_server(true, vec!["127.0.0.1".into()], rtc.clone(), ws);
        let port = http.local_port();
        let token = rt.current_token().expect("token");

        let req_unauth =
            format!("DELETE /webrtc/peer/peer-42 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
        assert_eq!(send_raw_http(port, req_unauth.as_bytes()).status, 401);
        assert!(lock_or_recover(&rtc.removed_peers).is_empty());

        let req_ok = format!(
            "DELETE /webrtc/peer/peer-42 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\r\n"
        );
        let resp = send_raw_http(port, req_ok.as_bytes());
        assert_eq!(resp.status, 204);
        assert!(resp.body.is_empty());
        assert_eq!(
            *lock_or_recover(&rtc.removed_peers),
            vec!["peer-42".to_string()]
        );

        http.stop();
        mgr.stop();
    }

    #[test]
    fn get_ws_validates_headers_and_hands_off_upgrade_request() {
        let rtc = Arc::new(MockRtc::new());
        let ws = Arc::new(MockWs::new(true));
        let (mgr, _rt, http) =
            start_mock_http_server(true, vec!["127.0.0.1".into()], rtc, ws.clone());
        let port = http.local_port();

        // Missing Sec-WebSocket-Key -> 400
        let req_no_key = format!(
            "GET /ws HTTP/1.1\r\n\
             Host: localhost:{port}\r\n\
             Origin: http://localhost:{port}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\r\n"
        );
        assert_eq!(send_raw_http(port, req_no_key.as_bytes()).status, 400);

        // Bad Origin -> 403
        let req_bad_origin = format!(
            "GET /ws HTTP/1.1\r\n\
             Host: localhost:{port}\r\n\
             Origin: http://evil.test:{port}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
        );
        assert_eq!(send_raw_http(port, req_bad_origin.as_bytes()).status, 403);

        // Valid upgrade with subprotocol auth.<token> + pipelined leftover bytes
        let mut req_ok = format!(
            "GET /ws HTTP/1.1\r\n\
             Host: localhost:{port}\r\n\
             Origin: http://localhost:{port}\r\n\
             User-Agent: SoundHopTest/1.0\r\n\
             Upgrade: websocket\r\n\
             Connection: keep-alive, Upgrade\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Protocol: soundhop.v1, auth.secret123\r\n\r\n"
        )
        .into_bytes();
        req_ok.extend_from_slice(b"\x81\x02hi");

        let resp = send_raw_http(port, &req_ok);
        assert_eq!(resp.status, 101);
        assert_eq!(resp.header("sec-websocket-protocol"), Some("soundhop.v1"));

        let captured = lock_or_recover(&ws.captured);
        assert_eq!(captured.len(), 1);
        let item = &captured[0];
        assert!(item.remote_addr.ip().is_loopback());
        assert_eq!(item.user_agent, "SoundHopTest/1.0");
        assert_eq!(item.sec_websocket_key, "dGhlIHNhbXBsZSBub25jZQ==");
        assert_eq!(item.subprotocol_token.as_deref(), Some("secret123"));
        assert!(item.client_sent_soundhop_protocol);
        assert_eq!(item.leftover_bytes, b"\x81\x02hi");
        drop(captured);

        http.stop();
        mgr.stop();
    }

    #[test]
    fn bounded_worker_pool_handles_concurrent_requests() {
        let (mgr, port, _) = start_test_manager(false);
        let mut threads = Vec::new();
        for _ in 0..24 {
            threads.push(thread::spawn(move || {
                let req = format!("GET /api/info HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
                let resp = send_raw_http(port, req.as_bytes());
                assert_eq!(resp.status, 200);
            }));
        }
        for t in threads {
            t.join().expect("worker request thread");
        }
        mgr.stop();
    }

    #[test]
    fn https_serves_html_ca_cert_wss_and_enforces_https_origin() {
        use rustls_pki_types::pem::PemObject;
        use rustls_pki_types::{CertificateDer, ServerName};
        use tempfile::TempDir;

        let dir = TempDir::new().expect("tempdir");
        let mgr = WebBroadcastManager::new();
        let cfg = WebBroadcastConfig {
            http_port: 0,
            webrtc_udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            require_token: false,
            device_name: "SoundHop-TlsTest".into(),
            https_enabled: true,
            tls_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let status = mgr
            .start(cfg, dummy_capture_factory())
            .expect("start https mgr");
        let port = status.http_port;
        assert!(status.localhost_url.starts_with("http://localhost:"));

        // Plain HTTP on loopback (adb reverse path) continues to work and serves /ca.crt.
        let ca_resp = send_raw_http(
            port,
            format!("GET /ca.crt HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n").as_bytes(),
        );
        assert_eq!(ca_resp.status, 200);
        assert_eq!(
            ca_resp.header("content-type"),
            Some("application/x-x509-ca-cert")
        );

        let mut root_store = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(&ca_resp.body) {
            root_store
                .add(cert.expect("parse CA cert"))
                .expect("add CA cert");
        }
        let client_cfg = Arc::new(
            rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_root_certificates(root_store)
            .with_no_client_auth(),
        );

        let send_https = |raw_req: &[u8]| -> TestHttpResponse {
            let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect tls");
            tcp.set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout");
            let server_name = ServerName::try_from("localhost").expect("server name");
            let conn = rustls::ClientConnection::new(Arc::clone(&client_cfg), server_name)
                .expect("client conn");
            let mut tls = rustls::StreamOwned::new(conn, tcp);
            tls.write_all(raw_req).expect("write tls req");
            tls.flush().expect("flush tls req");
            let mut buf = Vec::new();
            let _ = tls.read_to_end(&mut buf);
            parse_raw_http_response(&buf)
        };

        // HTTPS GET / returns player HTML.
        let root_resp =
            send_https(format!("GET / HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n").as_bytes());
        assert_eq!(root_resp.status, 200);
        assert_eq!(root_resp.body_str(), WEB_PLAYER_HTML);

        // HTTPS Origin accepted, plain HTTP Origin rejected on TLS connection.
        let good_origin = send_https(
            format!(
                "GET /api/info HTTP/1.1\r\nHost: localhost:{port}\r\nOrigin: https://localhost:{port}\r\n\r\n"
            )
            .as_bytes(),
        );
        assert_eq!(good_origin.status, 200);

        let bad_origin = send_https(
            format!(
                "GET /api/info HTTP/1.1\r\nHost: localhost:{port}\r\nOrigin: http://localhost:{port}\r\n\r\n"
            )
            .as_bytes(),
        );
        assert_eq!(bad_origin.status, 403);

        // Real wss:// WebSocket upgrade over TLS.
        let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect wss");
        tcp.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("wss read timeout");
        let server_name = ServerName::try_from("localhost").expect("wss server name");
        let conn =
            rustls::ClientConnection::new(Arc::clone(&client_cfg), server_name).expect("wss conn");
        let tls = rustls::StreamOwned::new(conn, tcp);
        let req = tungstenite::http::Request::builder()
            .uri(format!("wss://localhost:{port}/ws"))
            .header("Host", format!("localhost:{port}"))
            .header("Origin", format!("https://localhost:{port}"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tungstenite::handshake::client::generate_key(),
            )
            .header("Sec-WebSocket-Protocol", WS_SUBPROTOCOL)
            .body(())
            .expect("build wss request");
        let (_ws_client, ws_resp) = tungstenite::client(req, tls).expect("wss handshake");
        assert_eq!(ws_resp.status(), 101);

        mgr.stop();
    }

    #[test]
    fn non_loopback_http_redirect_helper_skips_loopback_and_ca_cert() {
        let lan_ip: IpAddr = "192.168.1.50".parse().expect("lan ip");
        let loopback_ip: IpAddr = "127.0.0.1".parse().expect("loopback ip");

        assert!(should_redirect_plain_http_to_https(&lan_ip, "/"));
        assert!(should_redirect_plain_http_to_https(&lan_ip, "/api/info"));
        assert!(!should_redirect_plain_http_to_https(&lan_ip, "/ca.crt"));
        assert!(!should_redirect_plain_http_to_https(&loopback_ip, "/"));

        assert_eq!(
            build_redirect_authority(Some("192.168.1.10:4717"), None, lan_ip, 4717),
            "192.168.1.10:4717"
        );
        assert_eq!(
            build_redirect_authority(None, Some(lan_ip), lan_ip, 4717),
            "192.168.1.50:4717"
        );
    }
}
