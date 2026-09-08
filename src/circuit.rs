//! Statistiques adaptatives par route et disjoncteur (circuit breaker).
//!
//! Chaque route (local, paid-cheap, paid-premium...) a un historique glissant
//! de succès/échecs. Un modèle local hébergé sur la machine peut planter,
//! être à court de mémoire, ou renvoyer des réponses vides sans lever
//! d'erreur HTTP franche — dans ce cas on ne veut pas continuer à lui envoyer
//! du trafic en boucle. Le disjoncteur suit le patron classique
//! fermé → ouvert → semi-ouvert :
//!
//! - **Fermé** : trafic normal, les échecs sont comptés dans la moyenne mobile.
//! - **Ouvert** : trop d'échecs récents → la route est court-circuitée pendant
//!   un délai de repos (`cooldown`), tout le trafic part ailleurs.
//! - **Semi-ouvert** : le délai est passé → une seule requête d'essai est
//!   autorisée ; un succès referme le disjoncteur, un échec relance le cooldown.

use std::time::{Duration, Instant};

/// Facteur de lissage de la moyenne mobile exponentielle (EMA). Plus il est
/// élevé, plus les événements récents pèsent lourd par rapport à l'historique.
const EMA_ALPHA: f64 = 0.3;

#[derive(Debug, Clone, Copy, PartialEq)]
enum BreakerState {
    Closed,
    Open { opened_at: Instant },
    HalfOpen,
}

/// Statistiques et état du disjoncteur pour une route donnée.
#[derive(Debug, Clone)]
pub struct RouteStats {
    /// Taux de succès lissé (moyenne mobile exponentielle), commence à 1.0
    /// (optimiste : une route jamais essayée n'est pas présumée en panne).
    success_rate_ema: f64,
    samples: u64,
    state: BreakerState,
    cooldown: Duration,
    /// Sous ce seuil de taux de succès lissé, le disjoncteur s'ouvre.
    failure_threshold: f64,
    /// Nombre minimum d'échantillons avant qu'un taux bas ne déclenche
    /// l'ouverture (évite d'ouvrir le circuit sur un seul échec isolé).
    min_samples_before_trip: u64,
}

impl RouteStats {
    pub fn new(failure_threshold: f64, cooldown: Duration) -> Self {
        RouteStats {
            success_rate_ema: 1.0,
            samples: 0,
            state: BreakerState::Closed,
            cooldown,
            failure_threshold,
            min_samples_before_trip: 5,
        }
    }

    pub fn success_rate(&self) -> f64 {
        self.success_rate_ema
    }

    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// Est-ce que cette route peut recevoir du trafic maintenant ?
    /// Fait aussi transitionner Ouvert → Semi-ouvert si le cooldown est écoulé.
    pub fn is_available(&mut self) -> bool {
        match self.state {
            BreakerState::Closed => true,
            BreakerState::HalfOpen => true,
            BreakerState::Open { opened_at } => {
                if opened_at.elapsed() >= self.cooldown {
                    self.state = BreakerState::HalfOpen;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Enregistre le résultat d'un appel et met à jour l'EMA + l'état du disjoncteur.
    pub fn record(&mut self, success: bool) {
        self.samples += 1;
        let outcome = if success { 1.0 } else { 0.0 };
        self.success_rate_ema = EMA_ALPHA * outcome + (1.0 - EMA_ALPHA) * self.success_rate_ema;

        match self.state {
            BreakerState::HalfOpen => {
                // L'essai décide : succès referme, échec relance le repos.
                if success {
                    self.state = BreakerState::Closed;
                    // Sans ce reset, l'EMA reste plombée par la série
                    // d'échecs qui a précédé l'ouverture et `samples`
                    // dépasse déjà `min_samples_before_trip` : un seul
                    // nouvel échec suffirait alors à rouvrir immédiatement
                    // le disjoncteur (la route n'aurait droit qu'à UN
                    // succès de grâce avant de retomber en essai perpétuel,
                    // sans jamais repasser vraiment "fermé"). On lui
                    // redonne le même point de départ optimiste qu'une
                    // route neuve.
                    self.success_rate_ema = 1.0;
                    self.samples = 0;
                } else {
                    self.state = BreakerState::Open {
                        opened_at: Instant::now(),
                    };
                }
            }
            BreakerState::Closed => {
                if self.samples >= self.min_samples_before_trip
                    && self.success_rate_ema < self.failure_threshold
                {
                    self.state = BreakerState::Open {
                        opened_at: Instant::now(),
                    };
                }
            }
            BreakerState::Open { .. } => {
                // Ne devrait pas arriver (is_available renvoie false), mais
                // ne pas paniquer si un appelant enregistre quand même.
            }
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self.state, BreakerState::Open { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_route_is_available_and_optimistic() {
        let mut stats = RouteStats::new(0.5, Duration::from_secs(60));
        assert!(stats.is_available());
        assert_eq!(stats.success_rate(), 1.0);
    }

    #[test]
    fn repeated_failures_trip_the_breaker_open() {
        let mut stats = RouteStats::new(0.5, Duration::from_secs(60));
        for _ in 0..10 {
            stats.record(false);
        }
        assert!(stats.is_open());
        assert!(!stats.is_available());
    }

    #[test]
    fn a_single_failure_does_not_trip_the_breaker() {
        let mut stats = RouteStats::new(0.5, Duration::from_secs(60));
        stats.record(false);
        assert!(!stats.is_open());
        assert!(stats.is_available());
    }

    #[test]
    fn breaker_moves_to_half_open_after_cooldown_and_can_close_again() {
        let mut stats = RouteStats::new(0.5, Duration::from_millis(20));
        for _ in 0..10 {
            stats.record(false);
        }
        assert!(stats.is_open());
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            stats.is_available(),
            "doit passer en semi-ouvert après le cooldown"
        );
        stats.record(true);
        assert!(
            !stats.is_open(),
            "un succès en semi-ouvert doit refermer le disjoncteur"
        );
    }

    #[test]
    fn a_failed_trial_in_half_open_reopens_the_breaker() {
        let mut stats = RouteStats::new(0.5, Duration::from_millis(20));
        for _ in 0..10 {
            stats.record(false);
        }
        std::thread::sleep(Duration::from_millis(30));
        assert!(stats.is_available());
        stats.record(false);
        assert!(
            stats.is_open(),
            "un échec de l'essai semi-ouvert doit rouvrir"
        );
    }

    #[test]
    fn success_rate_recovers_gradually_after_successes() {
        let mut stats = RouteStats::new(0.5, Duration::from_secs(60));
        stats.record(false);
        stats.record(false);
        let after_failures = stats.success_rate();
        for _ in 0..10 {
            stats.record(true);
        }
        assert!(stats.success_rate() > after_failures);
    }

    #[test]
    fn closing_after_recovery_resets_ema_and_sample_count() {
        // Avant le correctif : après une réouverture (10 échecs puis un
        // succès en semi-ouvert), l'EMA restait plombée par l'historique et
        // `samples` dépassait déjà `min_samples_before_trip` — la route
        // n'avait droit qu'à UN succès de grâce avant qu'un seul nouvel
        // échec ne la rouvre immédiatement.
        let mut stats = RouteStats::new(0.5, Duration::from_millis(10));
        for _ in 0..10 {
            stats.record(false);
        }
        assert!(stats.is_open());
        std::thread::sleep(Duration::from_millis(20));
        assert!(stats.is_available()); // passe en semi-ouvert
        stats.record(true); // l'essai réussit, referme le disjoncteur

        assert!(!stats.is_open());
        assert_eq!(
            stats.success_rate(),
            1.0,
            "l'EMA doit repartir de zéro après une fermeture, pas rester plombée"
        );
        assert_eq!(
            stats.samples(),
            0,
            "le compteur d'échantillons doit repartir de zéro après une fermeture"
        );
    }

    #[test]
    fn a_single_failure_right_after_recovery_does_not_immediately_reopen() {
        let mut stats = RouteStats::new(0.5, Duration::from_millis(10));
        for _ in 0..10 {
            stats.record(false);
        }
        std::thread::sleep(Duration::from_millis(20));
        assert!(stats.is_available());
        stats.record(true); // referme, avec le reset du correctif

        stats.record(false); // un seul échec juste après la fermeture
        assert!(
            !stats.is_open(),
            "un seul échec ne doit pas suffire à rouvrir juste après une fermeture fraîche"
        );
    }
}
