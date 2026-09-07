//! `cost_autopilot` — routage coût/complexité entre un LLM local
//! (Ollama/LM Studio) et une API payante, avec projection de budget et
//! disjoncteur adaptatif par route.
//!
//! Pensé pour être embarqué tel quel dans un projet plus large (ex. "Ark") :
//! [`AutopilotEngine::decide`] est une décision pure sans effet de bord si
//! l'hôte gère lui-même l'appel réseau, et [`AutopilotEngine::route_and_execute`]
//! offre le parcours complet (décision + appel HTTP + journalisation SQLite +
//! mise à jour des statistiques) pour un usage autonome ou via la CLI `autopilot`.
//!
//! ```no_run
//! use cost_autopilot::{AutopilotConfig, AutopilotEngine, HttpProvider, PricingTable};
//! use cost_autopilot::store::EventStore;
//!
//! let store = EventStore::open_in_memory().unwrap();
//! let mut engine = AutopilotEngine::new(
//!     AutopilotConfig::default(),
//!     PricingTable::example_default(),
//!     store,
//! );
//! engine.register_provider("local", "llama3", Box::new(
//!     HttpProvider::new("local", "http://localhost:11434")
//! ));
//! let decision = engine.decide("Salut, ça va ?").unwrap();
//! assert_eq!(decision.route, "local");
//! ```

pub mod circuit;
pub mod complexity;
pub mod pricing;
pub mod provider;
pub mod router;
pub mod store;
pub mod tokens;

pub use complexity::{analyze, ComplexityScore, ComplexitySignal};
pub use pricing::{PricingTable, RoutePricing};
pub use provider::{
    CompletionRequest, CompletionResponse, HttpProvider, Provider, ProviderError, Usage,
};
pub use router::{
    AutopilotConfig, AutopilotEngine, AutopilotError, RoutedCompletion, Router, RoutingDecision,
    ROUTE_TIERS,
};
pub use tokens::{estimate_chat_tokens, estimate_tokens};
