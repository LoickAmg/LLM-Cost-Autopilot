//! Serveur HTTP factice minimal (implémenté à la main sur `TcpListener`, sans
//! dépendance de test supplémentaire) pour exercer `HttpProvider` et
//! `AutopilotEngine` contre de vrais sockets — un vrai aller-retour réseau
//! local, pas une fonction mockée.
//!
//! Ce module est partagé par plusieurs fichiers de tests d'intégration ;
//! chacun n'utilise qu'une partie de l'API exposée ici, d'où l'annotation
//! ci-dessous plutôt que des avertissements de code mort à chaque exécution.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Clone)]
pub struct MockResponse {
    pub status: u16,
    pub body: String,
    pub delay: Duration,
}

impl MockResponse {
    pub fn json_ok(body: serde_json::Value) -> Self {
        MockResponse {
            status: 200,
            body: body.to_string(),
            delay: Duration::ZERO,
        }
    }

    pub fn chat_completion(text: &str, prompt_tokens: usize, completion_tokens: usize) -> Self {
        Self::json_ok(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": text}}],
            "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens}
        }))
    }

    pub fn error(status: u16, body: &str) -> Self {
        MockResponse {
            status,
            body: body.to_string(),
            delay: Duration::ZERO,
        }
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

/// Serveur HTTP jetable pour un test : sert les réponses scriptées dans
/// l'ordre, puis répète la dernière indéfiniment si davantage de requêtes
/// arrivent que de réponses fournies.
pub struct MockServer {
    pub base_url: String,
    shutdown: Arc<AtomicBool>,
    pub request_count: Arc<AtomicUsize>,
    pub last_request_body: Arc<Mutex<Option<String>>>,
}

impl MockServer {
    pub fn start(responses: Vec<MockResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind du serveur factice");
        listener.set_nonblocking(true).expect("mode non bloquant");
        let addr = listener.local_addr().unwrap();

        let shutdown = Arc::new(AtomicBool::new(false));
        let request_count = Arc::new(AtomicUsize::new(0));
        let last_request_body = Arc::new(Mutex::new(None));

        let shutdown_clone = shutdown.clone();
        let count_clone = request_count.clone();
        let last_body_clone = last_request_body.clone();

        thread::spawn(move || {
            let mut index = 0usize;
            loop {
                if shutdown_clone.load(Ordering::Relaxed) {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        let response = responses
                            .get(index)
                            .or_else(|| responses.last())
                            .cloned()
                            .unwrap_or_else(|| MockResponse::error(500, "aucune réponse scriptée"));
                        index += 1;
                        count_clone.fetch_add(1, Ordering::Relaxed);
                        let body = handle_connection(stream, response);
                        *last_body_clone.lock().unwrap() = body;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        MockServer {
            base_url: format!("http://{addr}"),
            shutdown,
            request_count,
            last_request_body,
        }
    }

    pub fn requests_received(&self) -> usize {
        self.request_count.load(Ordering::Relaxed)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

fn find_double_crlf(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|w| w == b"\r\n\r\n")
}

fn handle_connection(mut stream: TcpStream, response: MockResponse) -> Option<String> {
    stream.set_nonblocking(false).ok();
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

    let mut data = Vec::new();
    let mut buf = [0u8; 8192];
    let mut body = None;

    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                if let Some(header_end) = find_double_crlf(&data) {
                    let headers = String::from_utf8_lossy(&data[..header_end]);
                    let content_length: usize = headers
                        .lines()
                        .find_map(|l| {
                            let lower = l.to_ascii_lowercase();
                            lower
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse().ok())
                        })
                        .unwrap_or(0);
                    let body_start = header_end + 4;
                    while data.len() < body_start + content_length {
                        match stream.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => data.extend_from_slice(&buf[..n]),
                            Err(_) => break,
                        }
                    }
                    let end = (body_start + content_length).min(data.len());
                    body = Some(String::from_utf8_lossy(&data[body_start..end]).to_string());
                    break;
                }
            }
            Err(_) => break,
        }
    }

    if !response.delay.is_zero() {
        thread::sleep(response.delay);
    }

    let status_text = match response.status {
        200 => "OK",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let http_response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.status,
        status_text,
        response.body.len(),
        response.body
    );
    let _ = stream.write_all(http_response.as_bytes());
    let _ = stream.flush();
    body
}
