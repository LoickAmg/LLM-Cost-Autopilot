//! Modèle de coût par route.
//!
//! Les tarifs sont **indicatifs et configurables** : les prix des API payantes
//! changent régulièrement et varient selon le fournisseur, il serait malhonnête
//! de les coder en dur comme une vérité figée. `PricingTable` part de valeurs
//! d'exemple plausibles (ordre de grandeur d'un modèle "économique" et d'un
//! modèle "premium" mi-2026) mais chaque déploiement doit ajuster ces chiffres
//! à ses propres contrats fournisseur avant de faire confiance aux montants
//! affichés.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Tarif d'une route : coût pour 1000 tokens en entrée et en sortie, en USD.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RoutePricing {
    pub cost_per_1k_input: f64,
    pub cost_per_1k_output: f64,
}

impl RoutePricing {
    pub const FREE: RoutePricing = RoutePricing {
        cost_per_1k_input: 0.0,
        cost_per_1k_output: 0.0,
    };

    /// Coût en USD pour un nombre donné de tokens d'entrée et de sortie.
    pub fn cost_for(&self, input_tokens: usize, output_tokens: usize) -> f64 {
        (input_tokens as f64 / 1000.0) * self.cost_per_1k_input
            + (output_tokens as f64 / 1000.0) * self.cost_per_1k_output
    }
}

/// Table de tarifs par nom de route (ex: "local", "paid-cheap", "paid-premium").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingTable {
    rates: HashMap<String, RoutePricing>,
}

impl PricingTable {
    pub fn new() -> Self {
        PricingTable {
            rates: HashMap::new(),
        }
    }

    /// Table d'exemple avec trois routes standards. Tarifs indicatifs
    /// (mi-2026, ordre de grandeur) — à ajuster à tes propres contrats.
    pub fn example_default() -> Self {
        let mut table = PricingTable::new();
        table.set("local", RoutePricing::FREE);
        table.set(
            "paid-cheap",
            RoutePricing {
                cost_per_1k_input: 0.15,
                cost_per_1k_output: 0.60,
            },
        );
        table.set(
            "paid-premium",
            RoutePricing {
                cost_per_1k_input: 2.50,
                cost_per_1k_output: 10.00,
            },
        );
        table
    }

    pub fn set(&mut self, route: &str, pricing: RoutePricing) {
        self.rates.insert(route.to_string(), pricing);
    }

    pub fn get(&self, route: &str) -> Option<RoutePricing> {
        self.rates.get(route).copied()
    }

    /// Coût pour une route donnée ; une route inconnue coûte 0.0 (jamais
    /// négatif, jamais un panic) mais ce cas doit normalement être évité en
    /// amont — voir `AutopilotEngine::route_and_execute`.
    pub fn cost_for(&self, route: &str, input_tokens: usize, output_tokens: usize) -> f64 {
        self.get(route)
            .map(|p| p.cost_for(input_tokens, output_tokens))
            .unwrap_or(0.0)
    }
}

impl Default for PricingTable {
    fn default() -> Self {
        Self::example_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_route_costs_nothing() {
        assert_eq!(RoutePricing::FREE.cost_for(1_000_000, 1_000_000), 0.0);
    }

    #[test]
    fn cost_scales_linearly_with_tokens() {
        let pricing = RoutePricing {
            cost_per_1k_input: 1.0,
            cost_per_1k_output: 2.0,
        };
        assert_eq!(pricing.cost_for(1000, 0), 1.0);
        assert_eq!(pricing.cost_for(0, 1000), 2.0);
        assert_eq!(pricing.cost_for(2000, 500), 3.0);
    }

    #[test]
    fn example_default_has_three_tiers_with_increasing_cost() {
        let table = PricingTable::example_default();
        let local = table.cost_for("local", 1000, 1000);
        let cheap = table.cost_for("paid-cheap", 1000, 1000);
        let premium = table.cost_for("paid-premium", 1000, 1000);
        assert_eq!(local, 0.0);
        assert!(cheap > local);
        assert!(premium > cheap);
    }

    #[test]
    fn unknown_route_costs_zero_rather_than_panicking() {
        let table = PricingTable::new();
        assert_eq!(table.cost_for("does-not-exist", 1000, 1000), 0.0);
    }

    #[test]
    fn custom_pricing_can_be_set_and_read_back() {
        let mut table = PricingTable::new();
        table.set(
            "custom",
            RoutePricing {
                cost_per_1k_input: 5.0,
                cost_per_1k_output: 5.0,
            },
        );
        assert_eq!(table.cost_for("custom", 1000, 1000), 10.0);
    }
}
