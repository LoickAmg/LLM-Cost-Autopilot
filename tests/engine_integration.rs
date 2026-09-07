//! Scénarios de bout en bout pour `AutopilotEngine` : décision + vrai appel
//! HTTP + journalisation + repli, contre des serveurs factices réels lancés
//! sur des sockets locaux.

mod common;

use common::{MockResponse, MockServer};
use cost_autopilot::store::EventStore;
use cost_autopilot::{AutopilotConfig, AutopilotEngine, HttpProvider, PricingTable};

fn engine_with(config: AutopilotConfig) -> AutopilotEngine {
    let store = EventStore::open_in_memory().unwrap();
    AutopilotEngine::new(config, PricingTable::example_default(), store)
}

#[test]
fn trivial_prompt_is_executed_against_the_local_provider_for_free() {
    let local = MockServer::start(vec![MockResponse::chat_completion("salut toi", 5, 3)]);
    let mut engine = engine_with(AutopilotConfig::default());
    engine.register_provider(
        "local",
        "llama3",
        Box::new(HttpProvider::new("local", &local.base_url)),
    );

    let result = engine.route_and_execute("Salut, comment ça va ?").unwrap();

    assert_eq!(result.route_used, "local");
    assert_eq!(result.cost_usd, 0.0);
    assert_eq!(result.text, "salut toi");
    assert!(!result.fallback_occurred);
    assert_eq!(engine.event_count().unwrap(), 1);
}

#[test]
fn local_failure_falls_back_to_paid_cheap_and_still_answers() {
    let local = MockServer::start(vec![MockResponse::error(500, "modèle local indisponible")]);
    let cheap = MockServer::start(vec![MockResponse::chat_completion(
        "réponse de secours",
        5,
        3,
    )]);

    let mut engine = engine_with(AutopilotConfig::default());
    engine.register_provider(
        "local",
        "llama3",
        Box::new(HttpProvider::new("local", &local.base_url)),
    );
    engine.register_provider(
        "paid-cheap",
        "gpt-mini",
        Box::new(HttpProvider::new("paid-cheap", &cheap.base_url)),
    );

    let result = engine.route_and_execute("Salut !").unwrap();

    assert_eq!(result.route_used, "paid-cheap");
    assert!(result.fallback_occurred);
    assert_eq!(result.text, "réponse de secours");
    assert!(result.cost_usd > 0.0, "le repli payant doit être facturé");
}

#[test]
fn repeated_local_failures_open_the_circuit_and_stop_being_tried_first() {
    // Le serveur local répond systématiquement 500 ; le serveur payant
    // répond toujours correctement. Après plusieurs échecs, le disjoncteur
    // du local doit s'ouvrir et les requêtes triviales doivent être
    // directement envoyées à la route payante (pas seulement en repli).
    let local = MockServer::start(vec![MockResponse::error(500, "en panne"); 20]);
    let cheap = MockServer::start(vec![MockResponse::chat_completion("ok", 5, 3); 20]);

    let mut engine = engine_with(AutopilotConfig::default());
    engine.register_provider(
        "local",
        "llama3",
        Box::new(HttpProvider::new("local", &local.base_url)),
    );
    engine.register_provider(
        "paid-cheap",
        "gpt-mini",
        Box::new(HttpProvider::new("paid-cheap", &cheap.base_url)),
    );

    for _ in 0..8 {
        let _ = engine.route_and_execute("Salut, ça va ?");
    }

    assert!(
        engine.success_rate("local") < 0.5,
        "le taux de succès du local doit refléter les échecs répétés"
    );

    let decision = engine.decide("Un autre message trivial").unwrap();
    assert_eq!(
        decision.route, "paid-cheap",
        "le disjoncteur ouvert doit écarter le local dès la décision, pas seulement au repli"
    );
}

#[test]
fn a_tight_daily_budget_is_respected_across_several_calls() {
    let cheap = MockServer::start(vec![MockResponse::chat_completion("réponse", 50, 50); 10]);
    let local = MockServer::start(vec![
        MockResponse::chat_completion("réponse locale", 50, 50);
        10
    ]);

    let config = AutopilotConfig {
        local_complexity_threshold: 0.0, // tout ce qui n'est pas trivial part en payant
        daily_budget_usd: Some(0.05),    // budget volontairement serré
        ..AutopilotConfig::default()
    };

    let mut engine = engine_with(config);
    engine.register_provider(
        "local",
        "llama3",
        Box::new(HttpProvider::new("local", &local.base_url)),
    );
    engine.register_provider(
        "paid-cheap",
        "gpt-mini",
        Box::new(HttpProvider::new("paid-cheap", &cheap.base_url)),
    );

    let mut local_hits = 0;
    for _ in 0..10 {
        let result = engine
            .route_and_execute("Explique pourquoi étape par étape ce bug se produit")
            .unwrap();
        if result.route_used == "local" {
            local_hits += 1;
        }
    }

    assert!(
        local_hits > 0,
        "une fois le budget quotidien épuisé, l'autopilot doit replier vers le local gratuit"
    );

    let total_spent = engine
        .store()
        .cost_today(cost_autopilot::store::now_unix())
        .unwrap();
    assert!(
        total_spent <= 0.05 + 0.02,
        "la dépense totale ({total_spent}) ne doit pas dépasser significativement le budget"
    );
}
