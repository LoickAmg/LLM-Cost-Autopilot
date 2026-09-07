//! Vérifie `HttpProvider` contre un vrai serveur HTTP local (voir
//! `tests/common/mod.rs`) : succès, erreur HTTP, et transport indisponible —
//! aucune de ces trois voies n'est mockée au niveau de la fonction Rust.

mod common;

use common::{MockResponse, MockServer};
use cost_autopilot::provider::{CompletionRequest, Provider, ProviderError};
use cost_autopilot::HttpProvider;
use std::time::Duration;

#[test]
fn successful_completion_returns_text_and_real_usage() {
    let server = MockServer::start(vec![MockResponse::chat_completion("bonjour !", 12, 3)]);
    let provider = HttpProvider::new("test", &server.base_url);

    let response = provider
        .complete(&CompletionRequest::new("test-model", "salut"))
        .expect("l'appel doit réussir");

    assert_eq!(response.text, "bonjour !");
    assert_eq!(response.usage.prompt_tokens, 12);
    assert_eq!(response.usage.completion_tokens, 3);
    assert_eq!(server.requests_received(), 1);
}

#[test]
fn request_body_actually_carries_the_model_and_prompt() {
    let server = MockServer::start(vec![MockResponse::chat_completion("ok", 1, 1)]);
    let provider = HttpProvider::new("test", &server.base_url);

    provider
        .complete(&CompletionRequest::new(
            "mon-modele",
            "un prompt bien précis",
        ))
        .unwrap();

    let body = server.last_request_body.lock().unwrap().clone().unwrap();
    assert!(body.contains("mon-modele"));
    assert!(body.contains("un prompt bien précis"));
}

#[test]
fn http_error_status_is_surfaced_as_a_provider_error() {
    let server = MockServer::start(vec![MockResponse::error(500, "modèle non chargé")]);
    let provider = HttpProvider::new("test", &server.base_url);

    let err = provider
        .complete(&CompletionRequest::new("m", "salut"))
        .unwrap_err();

    match err {
        ProviderError::HttpStatus(code, body) => {
            assert_eq!(code, 500);
            assert!(body.contains("modèle non chargé"));
        }
        other => panic!("erreur inattendue : {other:?}"),
    }
}

#[test]
fn unreachable_server_is_a_transport_error() {
    // Port dans la plage éphémère très peu probable d'être occupé, et de
    // toute façon rien n'y écoute — connexion refusée réelle, pas simulée.
    let provider =
        HttpProvider::new("test", "http://127.0.0.1:1").with_timeout(Duration::from_millis(500));

    let err = provider
        .complete(&CompletionRequest::new("m", "salut"))
        .unwrap_err();

    assert!(matches!(err, ProviderError::Transport(_)));
}

#[test]
fn latency_reflects_a_real_artificial_delay() {
    let server = MockServer::start(vec![
        MockResponse::chat_completion("lent", 1, 1).with_delay(Duration::from_millis(150))
    ]);
    let provider = HttpProvider::new("test", &server.base_url);

    let start = std::time::Instant::now();
    provider
        .complete(&CompletionRequest::new("m", "salut"))
        .unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed >= Duration::from_millis(140),
        "latence mesurée trop courte : {elapsed:?}"
    );
}
