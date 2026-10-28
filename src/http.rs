//! Minimal HTTP/1.1 client.
//!
//! `http://` URLs are spoken directly over `std::net::TcpStream`.
//! `https://` URLs are delegated to the system `curl` binary: TLS is the one
//! protocol this project refuses to hand-roll (docs/SECURITY.md). Secrets
//! never appear on curl's command line; the request is passed as a curl
//! config on stdin and the body through a private temporary file.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct HttpError {
    pub message: String,
    pub retryable: bool,
}

impl HttpError {
    fn new(message: impl Into<String>, retryable: bool) -> HttpError {
        HttpError { message: message.into(), retryable }
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Maximum idle time between reads (streaming-friendly).
    pub read_timeout: Duration,
    /// Hard cap on the whole exchange for the curl transport.
    pub total_timeout: Duration,
    pub connect_timeout: Duration,
}

impl Request {
    pub fn new(method: &str, url: &str) -> Request {
        Request {
            method: method.to_string(),
            url: url.to_string(),
            headers: Vec::new(),
            body: Vec::new(),
            read_timeout: Duration::from_secs(120),
            total_timeout: Duration::from_secs(600),
            connect_timeout: Duration::from_secs(20),
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Request {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn json(mut self, body: &crate::json::Json) -> Request {
        self.body = body.to_string().into_bytes();
        self.headers.push(("Content-Type".into(), "application/json".into()));
        self
    }

    pub fn timeouts(mut self, read: Duration, total: Duration) -> Request {
        self.read_timeout = read;
        self.total_timeout = total;
        self
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn BufRead + Send>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn text(mut self, limit: usize) -> Result<String, HttpError> {
        let mut buf = Vec::new();
        (&mut self.body)
            .take(limit as u64)
            .read_to_end(&mut buf)
            .map_err(|e| HttpError::new(format!("reading response body failed: {e}"), true))?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }
}

struct Url {
    https: bool,
    host: String,
    port: u16,
    path: String,
}

fn parse_url(url: &str) -> Result<Url, HttpError> {
    let (https, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(HttpError::new(format!("unsupported URL scheme in {url:?}"), false));
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.contains('@') {
        return Err(HttpError::new("URLs with embedded credentials are not supported", false));
    }
    let default_port = if https { 443 } else { 80 };
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let end = stripped.find(']').ok_or_else(|| HttpError::new("bad IPv6 host", false))?;
        let host = &stripped[..end];
        let port = stripped[end + 1..].strip_prefix(':').map(|p| p.parse::<u16>());
        (host.to_string(), port.transpose().map_err(|_| HttpError::new("bad port", false))?.unwrap_or(default_port))
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().map_err(|_| HttpError::new("bad port", false))?),
            None => (authority.to_string(), default_port),
        }
    };
    if host.is_empty() {
        return Err(HttpError::new("URL has no host", false));
    }
    Ok(Url { https, host, port, path: path.to_string() })
}

fn check_header(name: &str, value: &str) -> Result<(), HttpError> {
    if name.is_empty() || name.contains([':', '\r', '\n', '\0']) || value.contains(['\r', '\n', '\0']) {
        return Err(HttpError::new(format!("invalid header {name:?}"), false));
    }
    Ok(())
}

pub fn send(request: &Request) -> Result<Response, HttpError> {
    for (name, value) in &request.headers {
        check_header(name, value)?;
    }
    let url = parse_url(&request.url)?;
    if url.https { send_curl(request) } else { send_plain(request, &url) }
}

fn send_plain(request: &Request, url: &Url) -> Result<Response, HttpError> {
    let addrs: Vec<_> = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| HttpError::new(format!("cannot resolve {}: {e}", url.host), true))?
        .collect();
    let mut last_error = None;
    let mut stream = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, request.connect_timeout) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last_error = Some(e),
        }
    }
    let stream = stream.ok_or_else(|| {
        HttpError::new(
            format!("cannot connect to {}:{}: {}", url.host, url.port, last_error.map(|e| e.to_string()).unwrap_or_default()),
            true,
        )
    })?;
    stream.set_read_timeout(Some(request.read_timeout)).ok();
    stream.set_write_timeout(Some(request.connect_timeout)).ok();
    stream.set_nodelay(true).ok();
    let host_header = if (url.port == 80 && !url.https) || (url.port == 443 && url.https) {
        url.host.clone()
    } else {
        format!("{}:{}", url.host, url.port)
    };
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\nUser-Agent: melchior/{}\r\n",
        request.method,
        url.path,
        env!("CARGO_PKG_VERSION")
    );
    let mut has_length = false;
    for (name, value) in &request.headers {
        if name.eq_ignore_ascii_case("content-length") {
            has_length = true;
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !has_length && (!request.body.is_empty() || request.method != "GET") {
        head.push_str(&format!("Content-Length: {}\r\n", request.body.len()));
    }
    head.push_str("\r\n");
    let mut writer = stream.try_clone().map_err(|e| HttpError::new(e.to_string(), true))?;
    writer
        .write_all(head.as_bytes())
        .and_then(|_| writer.write_all(&request.body))
        .and_then(|_| writer.flush())
        .map_err(|e| HttpError::new(format!("sending request failed: {e}"), true))?;
    let mut reader = BufReader::new(stream);
    let (status, headers) = read_head(&mut reader)?;
    let chunked = headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked"));
    let length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<u64>().ok());
    let body: Box<dyn BufRead + Send> = if chunked {
        Box::new(BufReader::new(Chunked { inner: reader, remaining: 0, done: false }))
    } else if let Some(length) = length {
        Box::new(BufReader::new(reader.take(length)))
    } else {
        Box::new(reader)
    };
    Ok(Response { status, headers, body })
}

fn read_line(reader: &mut dyn BufRead) -> Result<String, HttpError> {
    let mut line = String::new();
    let n = reader
        .take(64 * 1024)
        .read_line(&mut line)
        .map_err(|e| HttpError::new(format!("reading response failed: {e}"), true))?;
    if n == 0 {
        return Err(HttpError::new("connection closed before a response was received", true));
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Read a status line and headers, skipping interim (1xx) and proxy CONNECT blocks.
fn read_head(reader: &mut dyn BufRead) -> Result<(u16, Vec<(String, String)>), HttpError> {
    loop {
        let mut status_line = read_line(reader)?;
        while status_line.is_empty() {
            status_line = read_line(reader)?;
        }
        let mut parts = status_line.splitn(3, ' ');
        let version = parts.next().unwrap_or_default();
        if !version.starts_with("HTTP/") {
            return Err(HttpError::new(format!("malformed status line {status_line:?}"), true));
        }
        let status: u16 =
            parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| HttpError::new("malformed status code", true))?;
        let reason = parts.next().unwrap_or_default().to_ascii_lowercase();
        let mut headers = Vec::new();
        loop {
            let line = read_line(reader)?;
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_string(), value.trim().to_string()));
            }
        }
        if (100..200).contains(&status) || (status == 200 && reason.contains("connection established")) {
            continue;
        }
        return Ok((status, headers));
    }
}

struct Chunked<R: BufRead> {
    inner: R,
    remaining: u64,
    done: bool,
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut line = String::new();
            self.inner.read_line(&mut line)?;
            let size = line.trim().split(';').next().unwrap_or("").trim();
            if size.is_empty() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated chunked body"));
            }
            self.remaining = u64::from_str_radix(size, 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
            if self.remaining == 0 {
                self.done = true;
                loop {
                    let mut trailer = String::new();
                    if self.inner.read_line(&mut trailer)? == 0 || trailer.trim().is_empty() {
                        break;
                    }
                }
                return Ok(0);
            }
        }
        let want = buf.len().min(self.remaining as usize);
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated chunk"));
        }
        self.remaining -= n as u64;
        if self.remaining == 0 {
            let mut crlf = String::new();
            self.inner.read_line(&mut crlf)?;
        }
        Ok(n)
    }
}

// ---- curl transport ----------------------------------------------------------

fn curl_quote(value: &str) -> Result<String, HttpError> {
    if value.contains(['\n', '\r', '\0']) {
        return Err(HttpError::new("curl config values cannot contain line breaks", false));
    }
    Ok(format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")))
}

fn curl_binary() -> String {
    std::env::var("MELCHIOR_CURL").unwrap_or_else(|_| {
        for candidate in ["/usr/bin/curl", "/usr/local/bin/curl", "/bin/curl"] {
            if std::path::Path::new(candidate).exists() {
                return candidate.to_string();
            }
        }
        "curl".to_string()
    })
}

struct CurlGuard {
    child: Arc<Mutex<std::process::Child>>,
    body_file: Option<std::path::PathBuf>,
    stderr: Arc<Mutex<String>>,
}

impl Drop for CurlGuard {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        if let Some(path) = self.body_file.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

struct CurlBody {
    stdout: BufReader<std::process::ChildStdout>,
    guard: CurlGuard,
}

impl Read for CurlBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.stdout.read(buf)?;
        if n == 0 && !buf.is_empty() {
            let status = self.guard.child.lock().unwrap().wait()?;
            if !status.success() {
                let detail = self.guard.stderr.lock().unwrap().trim().to_string();
                return Err(io::Error::other(format!("curl failed ({status}): {detail}")));
            }
        }
        Ok(n)
    }
}

fn send_curl(request: &Request) -> Result<Response, HttpError> {
    let mut config = String::new();
    config.push_str(&format!("url = {}\n", curl_quote(&request.url)?));
    config.push_str(&format!("request = {}\n", curl_quote(&request.method)?));
    for (name, value) in &request.headers {
        config.push_str(&format!("header = {}\n", curl_quote(&format!("{name}: {value}"))?));
    }
    config.push_str(&format!("header = {}\n", curl_quote(&format!("User-Agent: melchior/{}", env!("CARGO_PKG_VERSION")))?));
    config.push_str(&format!("connect-timeout = {}\n", request.connect_timeout.as_secs().max(1)));
    config.push_str(&format!("max-time = {}\n", request.total_timeout.as_secs().max(1)));
    config.push_str(&format!("speed-time = {}\nspeed-limit = 1\n", request.read_timeout.as_secs().max(1)));
    let mut body_file = None;
    if !request.body.is_empty() || request.method != "GET" {
        let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
        let path = std::path::PathBuf::from(dir)
            .join(format!("melchior-{}-{}.body", std::process::id(), crate::util::random_hex(8)));
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| HttpError::new(format!("cannot stage request body: {e}"), false))?;
        file.write_all(&request.body).map_err(|e| HttpError::new(format!("cannot stage request body: {e}"), false))?;
        config.push_str(&format!("data-binary = {}\n", curl_quote(&format!("@{}", path.display()))?));
        body_file = Some(path);
    }
    let spawned = Command::new(curl_binary())
        .args(["--silent", "--show-error", "--no-buffer", "--include", "--config", "-"])
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| {
            matches!(k.as_str(), "PATH" | "HOME" | "SSL_CERT_FILE" | "SSL_CERT_DIR" | "CURL_CA_BUNDLE")
                || k.ends_with("_proxy")
                || k.ends_with("_PROXY")
        }))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            if let Some(path) = &body_file {
                let _ = std::fs::remove_file(path);
            }
            return Err(HttpError::new(format!("https requires curl, which could not be started: {e}"), false));
        }
    };
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let stderr = Arc::new(Mutex::new(String::new()));
    let sink = stderr.clone();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = (&mut stderr_pipe).take(16 * 1024).read_to_string(&mut buf);
        *sink.lock().unwrap() = buf;
    });
    let guard = CurlGuard { child: crate::proc::track(child), body_file, stderr: stderr.clone() };
    stdin.write_all(config.as_bytes()).map_err(|e| HttpError::new(format!("cannot configure curl: {e}"), false))?;
    drop(stdin);
    let mut body = CurlBody { stdout: BufReader::new(stdout), guard };
    match read_head(&mut body.stdout) {
        Ok((status, headers)) => Ok(Response { status, headers, body: Box::new(BufReader::new(body)) }),
        Err(_) => {
            let status = body.guard.child.lock().unwrap().wait().ok();
            std::thread::sleep(Duration::from_millis(20));
            let detail = stderr.lock().unwrap().trim().to_string();
            let code = status.and_then(|s| s.code()).unwrap_or(-1);
            // 6 resolve, 7 connect, 28 timeout, 35 TLS handshake, 52 empty reply, 55/56 send/recv.
            let retryable = matches!(code, 6 | 7 | 28 | 35 | 52 | 55 | 56);
            Err(HttpError::new(format!("https request failed (curl exit {code}): {detail}"), retryable))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn parses_urls() {
        let url = parse_url("http://127.0.0.1:8000/v1/agents/a%20b/mcp").unwrap();
        assert_eq!((url.host.as_str(), url.port, url.path.as_str()), ("127.0.0.1", 8000, "/v1/agents/a%20b/mcp"));
        let url = parse_url("https://openrouter.ai/api/v1").unwrap();
        assert!(url.https && url.port == 443);
        assert!(parse_url("ftp://x").is_err());
        assert!(parse_url("http://user:pw@x/").is_err());
    }

    #[test]
    fn reads_chunked_responses() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf);
            socket
                .write_all(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n")
                .unwrap();
        });
        let response = send(&Request::new("POST", &format!("http://127.0.0.1:{port}/x"))).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.text(1024).unwrap(), "hello world");
    }

    #[test]
    fn rejects_header_injection() {
        let request = Request::new("GET", "http://127.0.0.1:1/").header("X", "a\r\nInjected: 1");
        assert!(send(&request).is_err());
    }
}
