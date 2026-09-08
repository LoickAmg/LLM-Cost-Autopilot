//! Politique de routage et moteur d'exécution.
//!
//! `Router::decide` est une fonction **pure** (aucun appel réseau, aucune
//! écriture disque) : elle ne fait que choisir une route à partir de la
//! complexité du prompt, du budget déjà consommé aujourd'hui et de la santé
//! récente de chaque route (disjoncteur). C'est délibéré : le module qui
//! embarque cette bibliothèque (typiquement "Ark") peut appeler `decide`
//! seul pour prendre la décision sans dépendre du reste (exécution HTTP,
//! journal SQLite) — voir `AutopilotEngine` pour la version "piles incluses"
//! qui exécute réellement l'appel et journalise le résultat.

use crate::circuit::RouteStats;
use crate::complexity::analyze;
use crate::pricing::PricingTable;
use crate::provider::{CompletionRequest, Provider, ProviderError};
use crate::store::{now_unix, EventStore, RoutingEvent};
use crate::tokens::estimate_tokens;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Ordre ascendant des routes, du moins cher/moins puissant au plus
/// cher/plus puissant. Les fallbacks et escalades avancent dans cet ordre.
pub const ROUTE_TIERS: [&str; 3] = ["local", "paid-cheap", "paid-premium"];

/// Hypothèse par défaut sur la taille de la réponse, utilisée uniquement pour
/// *projeter* un coût avant l'appel (routage, budget). Le coût réellement
/// journalisé après l'appel utilise toujours les tokens réels.
const ASSUMED_OUTPUT_TOKENS: usize = 500;

#[derive(Debug, Clone)]
pub struct AutopilotConfig {
    /// En-dessous de ce score de complexité, on tente le modèle local.
    pub local_complexity_threshold: f64,
    /// Au-dessus de ce score, on va direct au modèle premium.
    pub premium_complexity_threshold: f64,
    /// Budget quotidien en USD ; `None` = pas de limite.
    pub daily_budget_usd: Option<f64>,
    /// Taux de succès lissé minimal pour que le local reste éligible, même
    /// si son disjoncteur n'est pas encore ouvert (dégradation précoce).
    pub min_local_success_rate: f64,
    /// Seuil de taux de succès lissé sous lequel une route bascule "ouverte".
    pub circuit_failure_threshold: f64,
    /// Délai de repos avant qu'une route ouverte repasse en essai.
    pub circuit_cooldown: Duration,
}

impl Default for AutopilotConfig {
    fn default() -> Self {
        AutopilotConfig {
            local_complexity_threshold: 0.35,
            premium_complexity_threshold: 0.70,
            daily_budget_usd: None,
            min_local_success_rate: 0.7,
            circuit_failure_threshold: 0.5,
            circuit_cooldown: Duration::from_secs(120),
        }
    }
}

/// Décision de routage pour un prompt donné, avec sa justification.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutingDecision {
    pub route: String,
    pub complexity: f64,
    pub estimated_cost_usd: f64,
    pub forced_by_budget: bool,
    pub reason: String,
}

/// Politique de routage pure + suivi de santé des routes (disjoncteur/EMA).
pub struct Router {
    config: AutopilotConfig,
    pricing: PricingTable,
    stats: HashMap<String, RouteStats>,
}

impl Router {
    pub fn new(config: AutopilotConfig, pricing: PricingTable) -> Self {
        Router {
            config,
            pricing,
            stats: HashMap::new(),
        }
    }

    fn stats_for(&mut self, route: &str) -> &mut RouteStats {
        let (threshold, cooldown) = (
            self.config.circuit_failure_threshold,
            self.config.circuit_cooldown,
        );
        self.stats
            .entry(route.to_string())
            .or_insert_with(|| RouteStats::new(threshold, cooldown))
    }

    /// Taux de succès lissé actuel d'une route (1.0 si jamais utilisée).
    pub fn success_rate(&mut self, route: &str) -> f64 {
        self.stats_for(route).success_rate()
    }

    fn is_route_healthy(&mut self, route: &str) -> bool {
        let min_success = if route == "local" {
            self.config.min_local_success_rate
        } else {
            0.0 // seul le disjoncteur protège les routes payantes, pas de seuil de qualité
        };
        let stats = self.stats_for(route);
        stats.is_available() && stats.success_rate() >= min_success
    }

    /// Cherche la première route saine en balayant `ROUTE_TIERS` à partir de
    /// `start_index`, d'abord vers le haut (plus cher/robuste), puis vers le
    /// bas si rien au-dessus n'est disponible. Le local reste un ultime
    /// recours même dégradé plutôt que de ne renvoyer aucune route.
    fn first_healthy_from(&mut self, start_index: usize) -> (usize, bool) {
        for (i, route) in ROUTE_TIERS.iter().enumerate().skip(start_index) {
            if self.is_route_healthy(route) {
                return (i, i != start_index);
            }
        }
        for i in (0..start_index).rev() {
            if self.is_route_healthy(ROUTE_TIERS[i]) {
                return (i, true);
            }
        }
        (0, start_index != 0) // dernier recours : local, même dégradé
    }

    /// Décide de la route à utiliser pour ce prompt. `spent_today_usd` vient
    /// du journal (`EventStore::cost_today`) — passé en paramètre plutôt que
    /// lu directement, pour que `decide` reste testable sans SQLite.
    pub fn decide(&mut self, prompt: &str, spent_today_usd: f64) -> RoutingDecision {
        let complexity = analyze(prompt).score;
        let tokens_in = estimate_tokens(prompt);

        let base_index = if complexity < self.config.local_complexity_threshold {
            0
        } else if complexity < self.config.premium_complexity_threshold {
            1
        } else {
            2
        };

        let (healthy_index, escalated) = self.first_healthy_from(base_index);
        let mut route_index = healthy_index;
        let mut reasons = vec![format!(
            "complexité {:.2} → palier de base « {} »",
            complexity, ROUTE_TIERS[base_index]
        )];
        if escalated {
            reasons.push(format!(
                "« {} » indisponible/dégradé → repli sur « {} »",
                ROUTE_TIERS[base_index], ROUTE_TIERS[route_index]
            ));
        }

        let mut forced_by_budget = false;
        if let Some(budget) = self.config.daily_budget_usd {
            // On ne s'arrête pas dès que le coût projeté rentre dans le
            // budget : il faut *aussi* que la route atterrie soit saine.
            // Avant ce correctif, la boucle ne regardait que le coût et
            // pouvait donc router silencieusement vers une route dont le
            // disjoncteur est ouvert simplement parce qu'elle est moins
            // chère — ce que `first_healthy_from` évite pourtant
            // soigneusement pour le choix de base. Le local (index 0)
            // reste l'ultime recours même dégradé, comme partout ailleurs
            // dans ce module : il n'existe rien de plus bas vers quoi replier.
            while route_index > 0 {
                let projected = self.pricing.cost_for(
                    ROUTE_TIERS[route_index],
                    tokens_in,
                    ASSUMED_OUTPUT_TOKENS,
                );
                let over_budget = spent_today_usd + projected > budget;
                let unhealthy = !self.is_route_healthy(ROUTE_TIERS[route_index]);
                if !over_budget && !unhealthy {
                    break;
                }
                if over_budget {
                    forced_by_budget = true;
                    reasons.push(format!(
                        "budget quotidien ({budget:.2}$) dépassé par « {} » (déjà {spent_today_usd:.2}$ dépensés) → repli sur « {} »",
                        ROUTE_TIERS[route_index], ROUTE_TIERS[route_index - 1]
                    ));
                }
                if unhealthy {
                    reasons.push(format!(
                        "« {} » indisponible/dégradé (repli budgétaire) → repli sur « {} »",
                        ROUTE_TIERS[route_index],
                        ROUTE_TIERS[route_index - 1]
                    ));
                }
                route_index -= 1;
            }
        }

        let route = ROUTE_TIERS[route_index].to_string();
        let estimated_cost_usd = self
            .pricing
            .cost_for(&route, tokens_in, ASSUMED_OUTPUT_TOKENS);

        RoutingDecision {
            route,
            complexity,
            estimated_cost_usd,
            forced_by_budget,
            reason: reasons.join(" ; "),
        }
    }

    /// Enregistre le résultat réel d'un appel pour mettre à jour l'EMA et le
    /// disjoncteur de la route utilisée.
    pub fn record_outcome(&mut self, route: &str, success: bool) {
        self.stats_for(route).record(success);
    }
}

// --- Moteur "piles incluses" : exécute réellement l'appel et journalise ---

#[derive(Debug)]
pub enum AutopilotError {
    UnknownRoute(String),
    AllRoutesFailed { last_error: ProviderError },
    Storage(rusqlite::Error),
}

impl std::fmt::Display for AutopilotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AutopilotError::UnknownRoute(r) => write!(f, "route inconnue: {r}"),
            AutopilotError::AllRoutesFailed { last_error } => {
                write!(
                    f,
                    "toutes les routes de repli ont échoué, dernière erreur: {last_error}"
                )
            }
            AutopilotError::Storage(e) => write!(f, "erreur de stockage: {e}"),
        }
    }
}

impl std::error::Error for AutopilotError {}

/// Résultat d'une exécution routée : la réponse, plus les métadonnées de
/// décision (route utilisée, coût réel, latence, repli éventuel).
#[derive(Debug, Clone)]
pub struct RoutedCompletion {
    pub text: String,
    pub route_used: String,
    pub cost_usd: f64,
    pub latency_ms: u64,
    pub fallback_occurred: bool,
    pub decision_reason: String,
}

/// Moteur complet : décide, appelle le fournisseur choisi (avec un repli en
/// cas d'échec), journalise dans `EventStore`, et alimente les statistiques
/// adaptatives du `Router`.
pub struct AutopilotEngine {
    router: Router,
    store: EventStore,
    providers: HashMap<String, Box<dyn Provider>>,
    models: HashMap<String, String>,
}

impl AutopilotEngine {
    pub fn new(config: AutopilotConfig, pricing: PricingTable, store: EventStore) -> Self {
        AutopilotEngine {
            router: Router::new(config, pricing),
            store,
            providers: HashMap::new(),
            models: HashMap::new(),
        }
    }

    /// Enregistre un fournisseur pour une route (ex: "local", "paid-cheap",
    /// "paid-premium") avec le nom de modèle à lui demander.
    pub fn register_provider(
        &mut self,
        route: impl Into<String>,
        model: impl Into<String>,
        provider: Box<dyn Provider>,
    ) {
        let route = route.into();
        self.models.insert(route.clone(), model.into());
        self.providers.insert(route, provider);
    }

    /// Décision pure, sans exécution — utile pour inspecter ce que
    /// l'autopilot ferait sans réellement appeler de fournisseur.
    pub fn decide(&mut self, prompt: &str) -> Result<RoutingDecision, AutopilotError> {
        let spent = self
            .store
            .cost_today(now_unix())
            .map_err(AutopilotError::Storage)?;
        Ok(self.router.decide(prompt, spent))
    }

    pub fn success_rate(&mut self, route: &str) -> f64 {
        self.router.success_rate(route)
    }

    pub fn event_count(&self) -> Result<u64, AutopilotError> {
        self.store.count().map_err(AutopilotError::Storage)
    }

    pub fn store(&self) -> &EventStore {
        &self.store
    }

    fn call_route(
        &self,
        route: &str,
        prompt: &str,
    ) -> Result<crate::provider::CompletionResponse, ProviderError> {
        let provider = self
            .providers
            .get(route)
            .expect("route sans fournisseur enregistré — voir register_provider");
        let model = self.models.get(route).cloned().unwrap_or_default();
        let req = CompletionRequest::new(model, prompt);
        provider.complete(&req)
    }

    /// Décide d'une route, exécute réellement l'appel, journalise le
    /// résultat et met à jour les statistiques adaptatives. En cas d'échec
    /// sur la route choisie, tente une seule fois la route immédiatement
    /// supérieure avant d'abandonner.
    pub fn route_and_execute(&mut self, prompt: &str) -> Result<RoutedCompletion, AutopilotError> {
        let decision = self.decide(prompt)?;

        if !self.providers.contains_key(&decision.route) {
            return Err(AutopilotError::UnknownRoute(decision.route.clone()));
        }

        let start = Instant::now();
        let first_attempt = self.call_route(&decision.route, prompt);
        let mut fallback_occurred = false;
        let mut used_route = decision.route.clone();

        let outcome = match first_attempt {
            Ok(resp) => {
                self.router.record_outcome(&used_route, true);
                Ok(resp)
            }
            Err(first_error) => {
                self.router.record_outcome(&used_route, false);
                let current_index = ROUTE_TIERS
                    .iter()
                    .position(|r| *r == used_route)
                    .unwrap_or(0);
                let fallback_route = ROUTE_TIERS
                    .get(current_index + 1)
                    .filter(|r| self.providers.contains_key(**r));

                match fallback_route {
                    Some(route) if *route != used_route => {
                        fallback_occurred = true;
                        used_route = route.to_string();
                        match self.call_route(&used_route, prompt) {
                            Ok(resp) => {
                                self.router.record_outcome(&used_route, true);
                                Ok(resp)
                            }
                            Err(second_error) => {
                                self.router.record_outcome(&used_route, false);
                                Err(second_error)
                            }
                        }
                    }
                    _ => Err(first_error),
                }
            }
        };

        let latency_ms = start.elapsed().as_millis() as u64;

        match outcome {
            Ok(resp) => {
                let actual_input = if resp.usage.prompt_tokens > 0 {
                    resp.usage.prompt_tokens
                } else {
                    estimate_tokens(prompt)
                };
                let actual_output = if resp.usage.completion_tokens > 0 {
                    resp.usage.completion_tokens
                } else {
                    estimate_tokens(&resp.text)
                };
                let cost_usd =
                    self.router
                        .pricing
                        .cost_for(&used_route, actual_input, actual_output);

                self.store
                    .record(&RoutingEvent {
                        timestamp: now_unix(),
                        route: used_route.clone(),
                        complexity: decision.complexity,
                        input_tokens: actual_input,
                        output_tokens: actual_output,
                        cost_usd,
                        latency_ms,
                        success: true,
                        fallback: fallback_occurred,
                        decision_reason: decision.reason.clone(),
                    })
                    .map_err(AutopilotError::Storage)?;

                Ok(RoutedCompletion {
                    text: resp.text,
                    route_used: used_route,
                    cost_usd,
                    latency_ms,
                    fallback_occurred,
                    decision_reason: decision.reason,
                })
            }
            Err(last_error) => {
                self.store
                    .record(&RoutingEvent {
                        timestamp: now_unix(),
                        route: used_route.clone(),
                        complexity: decision.complexity,
                        input_tokens: estimate_tokens(prompt),
                        output_tokens: 0,
                        cost_usd: 0.0,
                        latency_ms,
                        success: false,
                        fallback: fallback_occurred,
                        decision_reason: decision.reason.clone(),
                    })
                    .map_err(AutopilotError::Storage)?;
                Err(AutopilotError::AllRoutesFailed { last_error })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::PricingTable;

    fn router_with(config: AutopilotConfig) -> Router {
        Router::new(config, PricingTable::example_default())
    }

    #[test]
    fn trivial_prompt_routes_to_local() {
        let mut router = router_with(AutopilotConfig::default());
        let decision = router.decide("Salut !", 0.0);
        assert_eq!(decision.route, "local");
        assert_eq!(decision.estimated_cost_usd, 0.0);
    }

    #[test]
    fn very_complex_prompt_routes_to_premium() {
        let mut router = router_with(AutopilotConfig::default());
        // Cumule volontairement tous les signaux (longueur, code, mots-clés
        // de raisonnement, questions multiples, densité numérique) : c'est
        // ce cumul, pas un seul signal isolé, qui doit faire basculer en
        // « paid-premium ».
        let prompt = format!(
            "{} Explique pourquoi, étape par étape, cet algorithme de tri en O(n^2 + 3*n - 1) \
             est incorrect, et propose une architecture corrigée : ```code buggé``` \
             (complexité mesurée sur 128, 256, 512 et 1024 éléments = 4 essais). \
             Quel est le compromis performance/mémoire ? Comment le prouver formellement ? \
             Et comment le déboguer si la preuve échoue à l'étape 3 ?",
            "contexte détaillé du système de production concerné ".repeat(40)
        );
        let decision = router.decide(&prompt, 0.0);
        assert_eq!(decision.route, "paid-premium");
    }

    #[test]
    fn open_local_circuit_escalates_to_paid_cheap() {
        let mut router = router_with(AutopilotConfig::default());
        for _ in 0..10 {
            router.record_outcome("local", false);
        }
        let decision = router.decide("Salut !", 0.0);
        assert_eq!(decision.route, "paid-cheap");
        assert!(decision.reason.contains("indisponible"));
    }

    #[test]
    fn tight_budget_forces_downgrade_from_premium_to_cheap() {
        let config = AutopilotConfig {
            daily_budget_usd: Some(0.01),
            ..AutopilotConfig::default()
        };
        let mut router = router_with(config);
        let prompt =
            "Explique pourquoi, étape par étape, cet algorithme est incorrect ```code```. \
                       Compromis ? Architecture ? Preuve ?";
        let decision = router.decide(prompt, 0.0095);
        assert_ne!(decision.route, "paid-premium");
        assert!(decision.forced_by_budget);
    }

    #[test]
    fn exhausted_budget_forces_everything_down_to_local() {
        let config = AutopilotConfig {
            daily_budget_usd: Some(1.0),
            ..AutopilotConfig::default()
        };
        let mut router = router_with(config);
        let decision = router.decide("prompt trivial", 1.0);
        assert_eq!(decision.route, "local");
    }

    #[test]
    fn no_budget_configured_never_forces_a_downgrade() {
        let mut router = router_with(AutopilotConfig::default());
        let prompt =
            "Explique pourquoi, étape par étape, cet algorithme est incorrect ```code```. \
                       Compromis ? Architecture ? Preuve ?";
        let decision = router.decide(prompt, 1_000_000.0);
        assert!(!decision.forced_by_budget);
    }

    #[test]
    fn recording_success_keeps_a_route_healthy() {
        let mut router = router_with(AutopilotConfig::default());
        for _ in 0..20 {
            router.record_outcome("local", true);
        }
        assert!(router.success_rate("local") > 0.9);
        let decision = router.decide("Salut", 0.0);
        assert_eq!(decision.route, "local");
    }

    #[test]
    fn budget_downgrade_skips_a_route_whose_breaker_is_open() {
        // Avant le correctif, le repli budgétaire ne regardait que le coût
        // projeté : si "paid-cheap" tombait pile dans le budget mais que son
        // disjoncteur était ouvert, la décision y routait quand même,
        // silencieusement, au lieu de continuer à descendre vers "local".
        // Budget choisi pour que "paid-cheap" soit largement abordable
        // (~0.32$) mais "paid-premium" ne le soit pas (~5$+) : un repli
        // budgétaire seul s'arrêterait donc normalement sur "paid-cheap".
        let config = AutopilotConfig {
            daily_budget_usd: Some(1.0),
            ..AutopilotConfig::default()
        };
        let mut router = router_with(config);
        for _ in 0..10 {
            router.record_outcome("paid-cheap", false);
        }
        assert!(!router.stats_for("paid-cheap").is_available());

        let prompt =
            "Explique pourquoi, étape par étape, cet algorithme est incorrect ```code```. \
                       Compromis ? Architecture ? Preuve ?";
        let decision = router.decide(prompt, 0.0);
        assert_eq!(decision.route, "local");
    }

    #[test]
    fn budget_downgrade_still_lands_on_local_even_if_its_breaker_is_open() {
        // Local reste le dernier recours : même dégradé, il n'existe rien de
        // plus bas vers quoi replier (comportement documenté et inchangé).
        let config = AutopilotConfig {
            daily_budget_usd: Some(0.0),
            ..AutopilotConfig::default()
        };
        let mut router = router_with(config);
        for _ in 0..10 {
            router.record_outcome("local", false);
        }
        assert!(!router.stats_for("local").is_available());

        let decision = router.decide("prompt trivial", 0.0);
        assert_eq!(decision.route, "local");
    }
}
