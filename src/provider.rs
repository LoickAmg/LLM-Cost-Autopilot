//! Abstraction sur un fournisseur de complétion de chat, et une implémentation
//! HTTP compatible avec l'API OpenAI (`/v1/chat/completions`) — le format
//! qu'exposent aussi bien LM Studio, Ollama (depuis ses versions récentes) et
//! les API payantes usuelles. Un seul client HTTP couvre donc le local et le
//! payant : seule l'URL de base (et éventuellement une clé d'API) change.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Une requête de complétion, indépendante du fournisseur.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub model: String,
    pub prompt: String,
    pub max_tokens: Option<u32>,
}

impl CompletionRequest {
    pub fn new(model: impl Into<String>, prompt: impl Into<String>) -> Self {
        CompletionRequest {
            model: model.into(),
            prompt: prompt.into(),
            max_tokens: None,
        }
    }
}

/// Utilisation réelle de tokens telle que rapportée par le fournisseur.
/// Utilisée pour le coût *facturé* — l'estimation de `tokens::estimate_tokens`
/// ne sert qu'à la décision de routage, avant l'appel.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Usage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
}

/// Réponse d'un fournisseur.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionResponse {
    pub text: String,
    pub usage: Usage,
}

#[derive(Debug)]
pub enum ProviderError {
    /// Le fournisseur a répondu, mais avec un statut d'erreur HTTP.
    HttpStatus(u16, String),
    /// Aucune réponse (connexion refusée, timeout, DNS...).
    Transport(String),
    /// Réponse reçue mais dans un format inattendu.
    MalformedResponse(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::HttpStatus(code, body) => {
                write!(f, "erreur HTTP {code}: {body}")
            }
            ProviderError::Transport(msg) => write!(f, "erreur de transport: {msg}"),
            ProviderError::MalformedResponse(msg) => write!(f, "réponse malformée: {msg}"),
        }
    }
}

impl std::error::Error for ProviderError {}

/// Un fournisseur de complétion — implémenté par `HttpProvider` pour le cas
/// réel, et par des doubles de test dans les tests unitaires du routeur.
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, ProviderError>;
}

// --- Formats JSON compatibles OpenAI (LM Studio / Ollama / API payantes) ---

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct ChatRequestBody<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    content: String,
}

#[derive(Deserialize, Default)]
struct ChatUsageWire {
    #[serde(default)]
    prompt_tokens: usize,
    #[serde(default)]
    completion_tokens: usize,
}

#[derive(Deserialize)]
struct ChatResponseBody {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsageWire>,
}

/// Client HTTP générique compatible OpenAI, utilisable pour un modèle local
/// (LM Studio, Ollama) comme pour une API payante — seule `base_url` (et une
/// éventuelle clé d'API) diffère.
pub struct HttpProvider {
    name: String,
    base_url: String,
    api_key: Option<String>,
    timeout: Duration,
}

impl HttpProvider {
    pub fn new(name: impl Into<String>, base_url: impl Into<String>) -> Self {
        HttpProvider {
            name: name.into(),
            base_url: base_url.into(),
            api_key: None,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Provider for HttpProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );
        let body = ChatRequestBody {
            model: &req.model,
            messages: vec![ChatMessage {
                role: "user",
                content: &req.prompt,
            }],
            max_tokens: req.max_tokens,
        };

        let mut request = ureq::post(&url).timeout(self.timeout);
        if let Some(key) = &self.api_key {
            request = request.set("Authorization", &format!("Bearer {key}"));
        }

        let response = request.send_json(&body);

        let response = match response {
            Ok(resp) => resp,
            Err(ureq::Error::Status(code, resp)) => {
                let text = resp.into_string().unwrap_or_default();
                return Err(ProviderError::HttpStatus(code, text));
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(ProviderError::Transport(t.to_string()));
            }
        };

        let parsed: ChatResponseBody = response
            .into_json()
            .map_err(|e| ProviderError::MalformedResponse(e.to_string()))?;

        let text = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| ProviderError::MalformedResponse("aucun choix renvoyé".to_string()))?;

        let usage = parsed
            .usage
            .map(|u| Usage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
            })
            .unwrap_or_default();

        Ok(CompletionResponse { text, usage })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_error_display_is_informative() {
        let err = ProviderError::HttpStatus(500, "boom".to_string());
        assert!(err.to_string().contains("500"));
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn completion_request_builder_sets_defaults() {
        let req = CompletionRequest::new("gpt-x", "salut");
        assert_eq!(req.model, "gpt-x");
        assert_eq!(req.prompt, "salut");
        assert!(req.max_tokens.is_none());
    }

    // Les tests avec un vrai aller-retour HTTP (succès, erreur 500, timeout)
    // sont dans tests/http_provider.rs, contre un vrai serveur TCP local —
    // pas de mock de la fonction `complete` elle-même, un vrai socket.
}
