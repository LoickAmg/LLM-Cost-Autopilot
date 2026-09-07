//! CLI `autopilot` : démonstration et outil d'exploitation autour de la
//! bibliothèque `cost_autopilot`.

use clap::{Parser, Subcommand};
use cost_autopilot::store::EventStore;
use cost_autopilot::{AutopilotConfig, AutopilotEngine, HttpProvider, PricingTable};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

mod demo_backend;

#[derive(Parser)]
#[command(
    name = "autopilot",
    version,
    about = "Routage cout/complexite entre LLM local et API payante"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Envoie un lot de requêtes synthétiques à travers l'autopilot et
    /// affiche les décisions de routage + un bilan de coût/économies.
    Simulate {
        #[arg(long, default_value = "autopilot.db")]
        db: String,
        #[arg(long, default_value_t = 40)]
        n: u32,
        #[arg(long)]
        seed: Option<u64>,
        #[arg(long)]
        daily_budget: Option<f64>,
        /// Lance des serveurs factices intégrés (aucune dépendance externe)
        /// au lieu de vrais LM Studio/Ollama/API payante.
        #[arg(long, default_value_t = true)]
        demo: bool,
        #[arg(long)]
        local_url: Option<String>,
        #[arg(long)]
        paid_cheap_url: Option<String>,
        #[arg(long)]
        paid_premium_url: Option<String>,
        #[arg(long)]
        paid_api_key: Option<String>,
    },
    /// Décide uniquement (aucun appel réseau) — pour inspecter le routage
    /// qu'un prompt donné recevrait.
    Route {
        prompt: String,
        #[arg(long, default_value = "autopilot.db")]
        db: String,
        #[arg(long)]
        daily_budget: Option<f64>,
    },
    /// Résume le journal existant : dépense totale, répartition par route,
    /// économies estimées par rapport à un usage 100% "paid-premium".
    Report {
        #[arg(long, default_value = "autopilot.db")]
        db: String,
    },
}

fn build_config(daily_budget: Option<f64>) -> AutopilotConfig {
    AutopilotConfig {
        daily_budget_usd: daily_budget,
        ..AutopilotConfig::default()
    }
}

const DEMO_PROMPTS: &[&str] = &[
    "Salut, comment ça va ?",
    "Quelle heure est-il à Tokyo en ce moment ?",
    "Résume-moi ce texte en une phrase.",
    "Explique pourquoi, étape par étape, ce test unitaire échoue et propose une architecture corrigée : ```code```",
    "Quel est le compromis performance/mémoire de cet algorithme ? Comment le prouver ? Et si n=1024 ?",
    "Traduis 'bonjour' en anglais.",
    "Debug ce code étape par étape : ```fn main() { panic!(); }```. Cause racine ?",
    "Écris une fonction qui additionne deux nombres.",
];

fn random_demo_prompt(rng: &mut StdRng) -> String {
    let base = DEMO_PROMPTS[rng.gen_range(0..DEMO_PROMPTS.len())];
    base.to_string()
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Simulate {
            db,
            n,
            seed,
            daily_budget,
            demo,
            local_url,
            paid_cheap_url,
            paid_premium_url,
            paid_api_key,
        } => cmd_simulate(
            &db,
            n,
            seed,
            daily_budget,
            demo,
            local_url,
            paid_cheap_url,
            paid_premium_url,
            paid_api_key,
        ),
        Command::Route {
            prompt,
            db,
            daily_budget,
        } => cmd_route(&prompt, &db, daily_budget),
        Command::Report { db } => cmd_report(&db),
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_simulate(
    db: &str,
    n: u32,
    seed: Option<u64>,
    daily_budget: Option<f64>,
    demo: bool,
    local_url: Option<String>,
    paid_cheap_url: Option<String>,
    paid_premium_url: Option<String>,
    paid_api_key: Option<String>,
) {
    let store = EventStore::open(db).expect("ouverture de la base de journalisation");
    let mut engine = AutopilotEngine::new(
        build_config(daily_budget),
        PricingTable::example_default(),
        store,
    );

    // Gardés en vie jusqu'à la fin de la fonction : les serveurs factices
    // du mode démo s'arrêtent quand ces valeurs sortent de portée.
    let _demo_servers = if demo && local_url.is_none() && paid_cheap_url.is_none() {
        let servers = demo_backend::start_demo_backends();
        engine.register_provider(
            "local",
            "demo-local",
            Box::new(HttpProvider::new("local", &servers.local_url)),
        );
        engine.register_provider(
            "paid-cheap",
            "demo-cheap",
            Box::new(HttpProvider::new("paid-cheap", &servers.paid_cheap_url)),
        );
        engine.register_provider(
            "paid-premium",
            "demo-premium",
            Box::new(HttpProvider::new("paid-premium", &servers.paid_premium_url)),
        );
        Some(servers)
    } else {
        if let Some(url) = &local_url {
            engine.register_provider(
                "local",
                "local-model",
                Box::new(HttpProvider::new("local", url)),
            );
        }
        if let Some(url) = &paid_cheap_url {
            let mut provider = HttpProvider::new("paid-cheap", url);
            if let Some(key) = &paid_api_key {
                provider = provider.with_api_key(key.clone());
            }
            engine.register_provider("paid-cheap", "paid-cheap-model", Box::new(provider));
        }
        if let Some(url) = &paid_premium_url {
            let mut provider = HttpProvider::new("paid-premium", url);
            if let Some(key) = &paid_api_key {
                provider = provider.with_api_key(key.clone());
            }
            engine.register_provider("paid-premium", "paid-premium-model", Box::new(provider));
        }
        None
    };

    let mut rng = match seed {
        Some(s) => StdRng::seed_from_u64(s),
        None => StdRng::from_entropy(),
    };

    println!("Simulation de {n} requêtes (budget quotidien : {daily_budget:?})\n");
    for i in 0..n {
        let prompt = random_demo_prompt(&mut rng);
        match engine.route_and_execute(&prompt) {
            Ok(result) => {
                let flag = if result.fallback_occurred {
                    " [repli]"
                } else {
                    ""
                };
                println!(
                    "#{i:03} [{route:<13}]{flag} coût={cost:.4}$ latence={lat}ms — {prompt}",
                    route = result.route_used,
                    flag = flag,
                    cost = result.cost_usd,
                    lat = result.latency_ms,
                    prompt = truncate(&prompt, 60),
                );
            }
            Err(e) => println!("#{i:03} ÉCHEC : {e}"),
        }
    }

    println!();
    print_report(&engine);
}

fn cmd_route(prompt: &str, db: &str, daily_budget: Option<f64>) {
    let store = EventStore::open(db).expect("ouverture de la base de journalisation");
    let mut engine = AutopilotEngine::new(
        build_config(daily_budget),
        PricingTable::example_default(),
        store,
    );
    let decision = engine.decide(prompt).expect("échec de la décision");
    println!("Route choisie : {}", decision.route);
    println!("Complexité estimée : {:.3}", decision.complexity);
    println!("Coût projeté : {:.4}$", decision.estimated_cost_usd);
    println!("Forcé par le budget : {}", decision.forced_by_budget);
    println!("Justification : {}", decision.reason);
}

fn cmd_report(db: &str) {
    let store = EventStore::open(db).expect("ouverture de la base de journalisation");
    let events = store.all().expect("lecture du journal");
    if events.is_empty() {
        println!("Aucun événement journalisé dans {db}.");
        return;
    }

    let pricing = PricingTable::example_default();
    let total_cost: f64 = events.iter().map(|e| e.cost_usd).sum();
    let baseline_cost: f64 = events
        .iter()
        .map(|e| pricing.cost_for("paid-premium", e.input_tokens, e.output_tokens.max(1)))
        .sum();
    let savings = baseline_cost - total_cost;
    let savings_pct = if baseline_cost > 0.0 {
        100.0 * savings / baseline_cost
    } else {
        0.0
    };

    println!("# Rapport autopilot — {} événements\n", events.len());
    println!("Dépense réelle           : {total_cost:.4}$");
    println!("Dépense si tout premium  : {baseline_cost:.4}$");
    println!("Économies                : {savings:.4}$ ({savings_pct:.1}%)\n");

    for route in cost_autopilot::ROUTE_TIERS {
        let route_events: Vec<_> = events.iter().filter(|e| e.route == route).collect();
        if route_events.is_empty() {
            continue;
        }
        let successes = route_events.iter().filter(|e| e.success).count();
        let cost: f64 = route_events.iter().map(|e| e.cost_usd).sum();
        let avg_latency: f64 = route_events
            .iter()
            .map(|e| e.latency_ms as f64)
            .sum::<f64>()
            / route_events.len() as f64;
        println!(
            "- {route:<13} : {n:>4} appels, {successes}/{n} succès, {cost:.4}$, latence moy. {avg_latency:.0}ms",
            n = route_events.len(),
        );
    }
}

fn print_report(engine: &AutopilotEngine) {
    let events = engine.store().all().expect("lecture du journal");
    let pricing = PricingTable::example_default();
    let total_cost: f64 = events.iter().map(|e| e.cost_usd).sum();
    let baseline_cost: f64 = events
        .iter()
        .map(|e| pricing.cost_for("paid-premium", e.input_tokens, e.output_tokens.max(1)))
        .sum();
    let savings = baseline_cost - total_cost;
    println!("--- Bilan ---");
    println!("Dépense réelle          : {total_cost:.4}$");
    println!("Dépense si tout premium : {baseline_cost:.4}$");
    println!("Économies               : {savings:.4}$");
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{truncated}…")
    }
}
