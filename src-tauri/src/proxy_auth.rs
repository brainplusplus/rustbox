use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Mutex;

/// Local HTTP proxy forwarder that injects Proxy-Authorization for upstream
/// proxies requiring credentials (Chromium has no CLI flag for proxy auth).
///
/// Usage: app tab's proxy gets rewritten to point at 127.0.0.1:<local_port>,
/// which forwards to the real upstream with an auth header.
///
/// Both CONNECT tunnels and absolute-form HTTP requests are handled; the two
/// share the same code path because the rewrite is identical, and after a
/// successful CONNECT the connection is relayed byte for byte.
static NEXT_PORT: AtomicU16 = AtomicU16::new(18100);

static REGISTRY: Mutex<Option<HashMap<String, u16>>> = Mutex::new(None);

/// Identity of a forwarder.
///
/// The password is part of the key, hashed rather than stored in the clear: a
/// forwarder bakes the credentials it was started with into every request it
/// relays, so reusing one after the password was edited would keep sending the
/// old secret and every connection would come back 407. Hashing keeps the map
/// from holding a second plaintext copy.
fn registry_key(scheme: &str, host: &str, port: u16, user: &str, password: &str) -> String {
    format!("{scheme}://{host}:{port}@{user}#{}", hash(password))
}

/// FNV-1a over the password. Not a security primitive — the forwarder already
/// holds the plaintext — just a stable short identity for the registry map.
fn hash(value: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        h ^= *byte as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Get (or lazily start) a local forwarder for an authenticated upstream proxy.
pub fn get_local_port_for(
    scheme: &str,
    host: &str,
    port: u16,
    username: &str,
    password: &str,
) -> Result<u16, String> {
    let key = registry_key(scheme, host, port, username, password);
    let mut guard = REGISTRY.lock().map_err(|e| e.to_string())?;
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(&p) = map.get(&key) {
        return Ok(p);
    }
    // The port cannot be assumed free. A second RustBox instance, a leftover
    // process, or anything else holding 181xx would otherwise make the proxy
    // fail with "bind ... only one usage of each socket address" and the tab
    // would browse with no proxy at all. Walk the range until one binds.
    let mut last_error = String::from("no free port");
    for _ in 0..64 {
        let candidate = NEXT_PORT.fetch_add(1, Ordering::SeqCst);
        match start_forwarder(
            candidate,
            host.to_string(),
            port,
            username.to_string(),
            password.to_string(),
        ) {
            Ok(()) => {
                map.insert(key, candidate);
                return Ok(candidate);
            }
            Err(e) => last_error = e,
        }
    }
    Err(last_error)
}

fn start_forwarder(
    local_port: u16,
    host: String,
    port: u16,
    username: String,
    password: String,
) -> Result<(), String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", local_port))
        .map_err(|e| format!("bind {local_port}: {e}"))?;
    let credentials = format!("Basic {}", b64(&format!("{username}:{password}")));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(client) = stream else { continue };
            let ctx = ForwardCtx {
                host: host.clone(),
                port,
                credentials: credentials.clone(),
            };
            std::thread::spawn(move || handle_client(client, ctx));
        }
    });
    Ok(())
}

struct ForwardCtx {
    host: String,
    port: u16,
    credentials: String,
}

fn handle_client(client: std::net::TcpStream, ctx: ForwardCtx) {
    use std::io::Write;

    let mut client = client;
    let Some((head, body)) = read_head(&mut client) else {
        return;
    };

    // Connect with a timeout: a proxy that blackholes a port would otherwise
    // hold this thread for the OS default (~20 s) per request, and the browser
    // would sit on a blank tab with no error to show.
    let upstream = std::net::TcpStream::connect_timeout(
        &match format!("{}:{}", ctx.host, ctx.port).parse() {
            Ok(addr) => addr,
            Err(_) => {
                let _ = respond(&mut client, 502, "Bad upstream address");
                return;
            }
        },
        std::time::Duration::from_secs(10),
    );
    let Ok(mut upstream) = upstream else {
        // Say so instead of dropping the socket: a silent close shows up as
        // ERR_EMPTY_RESPONSE, which reads like a RustBox bug rather than a
        // proxy that refused to connect.
        let _ = respond(&mut client, 502, "Proxy connect failed");
        return;
    };

    // Rewrite the head: drop any Proxy-Authorization the client sent (it does
    // not know the credentials) and append ours to the header block.
    let out = rewrite_head(&head, &ctx.credentials);
    if upstream.write_all(out.as_bytes()).is_err() {
        return;
    }
    // Anything the client pipelined after the header block belongs to the
    // tunnel (or the request body) and must reach the upstream unmodified.
    if !body.is_empty() && upstream.write_all(&body).is_err() {
        return;
    }

    // Pump bytes both ways until either side closes.
    let mut upstream_clone = match upstream.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut client_clone = match client.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    };
    let t1 = std::thread::spawn(move || {
        let _ = std::io::copy(&mut client, &mut upstream);
    });
    let _ = std::io::copy(&mut upstream_clone, &mut client_clone);
    let _ = t1.join();
}

/// Write a minimal proxy error response so the browser can show something.
fn respond(client: &mut std::net::TcpStream, status: u16, message: &str) -> std::io::Result<()> {
    use std::io::Write;
    let body = format!("{status} {message}\n");
    let head = format!(
        "HTTP/1.1 {status} {message}\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    client.write_all(head.as_bytes())?;
    client.write_all(body.as_bytes())?;
    client.flush()
}

/// Read one request head, returning it with the bytes that followed it.
///
/// A single `read` is not enough: a head can arrive split across packets, and a
/// truncated head would be forwarded as if it were complete. Reading stops at
/// the blank line, so the tunnel's first bytes are handed back separately
/// instead of being mistaken for headers.
fn read_head(stream: &mut std::net::TcpStream) -> Option<(String, Vec<u8>)> {
    use std::io::Read;

    // A head this large is not a browser request; refuse rather than buffer.
    const MAX_HEAD: usize = 64 * 1024;

    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    loop {
        if find_head_end(&buf).is_some() {
            return Some(split_head(&buf));
        }
        if buf.len() >= MAX_HEAD {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
    }
}

/// Index just past the CRLFCRLF that ends a header block.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Split a buffered request into its head (blank line included) and the bytes
/// that follow it. Used by the reader and by the tests.
fn split_head(buf: &[u8]) -> (String, Vec<u8>) {
    match find_head_end(buf) {
        Some(end) => (
            String::from_utf8_lossy(&buf[..end]).to_string(),
            buf[end..].to_vec(),
        ),
        None => (String::from_utf8_lossy(buf).to_string(), Vec::new()),
    }
}

/// Rewrite a request head, adding our Proxy-Authorization and nothing else.
///
/// The blank line that ends the header block is written exactly once. An extra
/// CRLF is not cosmetic: after a CONNECT the proxy relays every following byte
/// to the origin verbatim, so a stray CRLF becomes the first two bytes of the
/// TLS ClientHello and the handshake fails with "unexpected packet format"
/// while the proxy itself reports success.
fn rewrite_head(head: &str, credentials: &str) -> String {
    let mut out = String::with_capacity(head.len() + 96);
    // `head` arrives with its terminating blank line, so `lines()` yields one
    // trailing empty string. Dropping it is what keeps the blank line below the
    // only one written: iterating it too would insert the header *and* leave
    // the original empty line in place.
    let lines: Vec<&str> = head.lines().collect();
    let body_lines = match lines.split_last() {
        Some((last, rest)) if last.is_empty() => rest,
        _ => &lines[..],
    };
    for line in body_lines {
        if line.to_ascii_lowercase().starts_with("proxy-authorization:") {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("Proxy-Authorization: ");
    out.push_str(credentials);
    out.push_str("\r\n\r\n");
    out
}

fn b64(data: &str) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = data.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Tauri command: prepare a proxy for a tab; if it has credentials, start a
/// local forwarder and return the rewritten (local) proxy URI to launch with.
#[tauri::command]
pub fn start_proxy_auth_server(
    scheme: String,
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
) -> Result<String, String> {
    let crate::config::Proxy {
        scheme: _,
        host: _,
        port: _,
        username: _,
        password: _,
    } = crate::config::Proxy::default();
    let _ = &scheme;
    let _ = &host;
    match (username, password) {
        (Some(u), Some(p)) if !u.is_empty() => {
            let local = get_local_port_for(&scheme, &host, port, &u, &p)?;
            Ok(format!("{}://127.0.0.1:{}", scheme, local))
        }
        _ => Ok(format!("{}://{}:{}", scheme, host, port)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A head split across packets must not be forwarded truncated, and the
    /// bytes that follow it must be handed back separately.
    #[test]
    fn a_head_is_split_after_its_blank_line() {
        let raw = b"GET / HTTP/1.1\r\nHost: a\r\n\r\n";
        let (head, rest) = split_head(raw);
        assert_eq!(head, "GET / HTTP/1.1\r\nHost: a\r\n\r\n");
        assert!(rest.is_empty());
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\nHost: a\r\n"), None);
        assert_eq!(find_head_end(b""), None);
    }

    /// The regression: the blank line that ends the head was written twice, so
    /// a stray CRLF preceded the tunnel's first byte. That pair is the start of
    /// the TLS ClientHello, so the handshake failed while the proxy reported
    /// success.
    #[test]
    fn rewriting_a_connect_head_emits_one_blank_line() {
        let head = "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n";
        let out = rewrite_head(head, "Basic dXNlcjpwYXNz");
        assert!(
            out.ends_with("\r\n\r\n"),
            "head must end with exactly one blank line: {out:?}"
        );
        assert!(
            !out.ends_with("\r\n\r\n\r\n"),
            "the blank line must not be doubled: {out:?}"
        );
        assert_eq!(out.matches("\r\n\r\n").count(), 1);
        assert!(out.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n"));
    }

    #[test]
    fn a_client_supplied_authorization_is_replaced_not_duplicated() {
        let head = "CONNECT example.com:443 HTTP/1.1\r\nProxy-Authorization: Basic old\r\n\r\n";
        let out = rewrite_head(head, "Basic new");
        assert_eq!(out.matches("Proxy-Authorization:").count(), 1, "{out:?}");
        assert!(out.contains("Basic new"));
        assert!(!out.contains("Basic old"));
    }

    #[test]
    fn an_absolute_form_get_keeps_its_request_line() {
        let head = "GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let out = rewrite_head(head, "Basic eA==");
        assert!(out.starts_with("GET http://example.com/ HTTP/1.1\r\n"));
        assert!(out.contains("Proxy-Authorization: Basic eA=="));
        assert!(out.ends_with("\r\n\r\n"));
    }

    /// Different passwords must not share a forwarder: the first one would keep
    /// relaying the old secret and every request would come back 407.
    #[test]
    fn the_registry_key_separates_passwords_without_storing_them() {
        let a = registry_key("http", "1.2.3.4", 8080, "user", "one");
        let b = registry_key("http", "1.2.3.4", 8080, "user", "two");
        assert_ne!(a, b);
        assert_eq!(a, registry_key("http", "1.2.3.4", 8080, "user", "one"));
        assert!(!a.contains("one"), "the key holds the plaintext: {a}");
    }

    /// End to end over real sockets: a fake upstream proxy records the head it
    /// receives and echoes whatever follows it. This is the test that would
    /// have caught both defects at once — a doubled blank line shows up as the
    /// first two payload bytes, and a truncated head shows up as a missing
    /// header.
    #[test]
    fn the_forwarder_relays_the_tunnel_untouched() {
        use std::io::{Read, Write};

        // Fake upstream: accept one connection, record the head, echo the rest.
        let upstream = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let seen_writer = seen.clone();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = upstream.accept() else {
                return;
            };
            // Read the head only, so the echo below proves the payload arrived
            // after it rather than being swallowed as headers.
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while sock.read(&mut byte).unwrap_or(0) == 1 {
                head.push(byte[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            seen_writer.lock().unwrap().extend_from_slice(&head);
            let _ = sock.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n");
            let mut payload = [0u8; 64];
            if let Ok(m) = sock.read(&mut payload) {
                let _ = sock.write_all(&payload[..m]);
            }
        });

        // The forwarder walks 181xx until it finds a free port, so a second test
        // run or a live RustBox on the same machine cannot make this flaky.
        let local_port =
            get_local_port_for("http", "127.0.0.1", upstream_port, "user", "pw").unwrap();

        let mut client = std::net::TcpStream::connect(("127.0.0.1", local_port)).unwrap();
        client
            .write_all(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n")
            .unwrap();

        let mut answer = [0u8; 128];
        let n = client.read(&mut answer).unwrap();
        assert!(
            String::from_utf8_lossy(&answer[..n]).starts_with("HTTP/1.1 200"),
            "forwarder did not relay the proxy's answer: {:?}",
            String::from_utf8_lossy(&answer[..n])
        );

        let tunnel = b"\x16\x03\x01\x00\x01\x00"; // first bytes of a ClientHello
        client.write_all(tunnel).unwrap();
        let mut back = [0u8; 16];
        let m = client.read(&mut back).unwrap();
        assert_eq!(&back[..m], tunnel, "tunnel bytes were altered");

        let got = seen.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&got).to_string();
        assert!(
            text.contains("Proxy-Authorization: Basic dXNlcjpwdw=="),
            "credentials missing from the forwarded head: {text:?}"
        );
        assert_eq!(
            text.matches("\r\n\r\n").count(),
            1,
            "head must end with exactly one blank line: {text:?}"
        );
        assert!(text.starts_with("CONNECT example.com:443 HTTP/1.1\r\n"));
    }
}
