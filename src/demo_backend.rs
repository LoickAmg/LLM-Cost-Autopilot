//! Serveurs HTTP factices intégrés pour `autopilot simulate --demo` : permet
//! de faire tourner une démonstration complète sans dépendre d'un vrai
//! LM Studio/Ollama local ni d'une vraie clé d'API payante. Implémentation
//! volontairement minimale (HTTP/1.1 à la main sur `TcpListener`), le strict
//! nécessaire pour répondre à une requête `/v1/chat/completions`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Les trois serveurs factices démarrés pour la démo. Tenus en vie tant que
/// cette valeur n'est pas droppée (elle coupe alors les trois threads).
pub struct DemoServers {
    pub local_url: String,
    pub paid_cheap_url: String,
    pub paid_premium_url: String,
    shutdown_flags: Vec<Arc<AtomicBool>>,
}

impl Drop for DemoServers {
    fn drop(&mut self) {
        for flag in &self.shutdown_flags {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

pub fn start_demo_backends() -> DemoServers {
    let (local_url, local_flag) = start_one("assistant local", 0);
    let (paid_cheap_url, cheap_flag) = start_one("assistant économique", 30);
    let (paid_premium_url, premium_flag) = start_one("assistant premium", 80);

    DemoServers {
        local_url,
        paid_cheap_url,
        paid_premium_url,
        shutdown_flags: vec![local_flag, cheap_flag, premium_flag],
    }
}

fn start_one(persona: &'static str, artificial_delay_ms: u64) -> (String, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind du serveur de démo");
    listener.set_nonblocking(true).expect("mode non bloquant");
    let addr = listener.local_addr().unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    thread::spawn(move || loop {
        if shutdown_clone.load(Ordering::Relaxed) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                handle(stream, persona, artificial_delay_ms);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    });

    (format!("http://{addr}"), shutdown)
}

fn handle(mut stream: TcpStream, persona: &str, artificial_delay_ms: u64) {
    stream.set_nonblocking(false).ok();
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

    // On ne se sert pas vraiment du contenu de la requête (ceci est une
    // démo hors-ligne) mais on doit quand même la consommer entièrement
    // avant de répondre, sans quoi certains clients HTTP se plaignent.
    let mut buf = [0u8; 8192];
    let mut data = Vec::new();
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                if data.windows(4).any(|w| w == b"\r\n\r\n") {
                    // Lecture suffisante pour ce cas d'usage de démo (pas de
                    // gestion générale de Content-Length ici, contrairement
                    // au serveur de test — la démo ignore le corps).
                    break;
                }
            }
            Err(_) => break,
        }
    }

    if artificial_delay_ms > 0 {
        thread::sleep(Duration::from_millis(artificial_delay_ms));
    }

    let body = serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": format!("[{persona}] réponse simulée")}}],
        "usage": {"prompt_tokens": 20, "completion_tokens": 15}
    })
    .to_string();

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}
