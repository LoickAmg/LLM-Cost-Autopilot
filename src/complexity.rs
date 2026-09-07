//! Estimation de la complexité d'une requête, pour décider si un modèle local
//! suffit ou s'il faut escalader vers une API payante.
//!
//! Volontairement **pas un modèle appris** : faire évaluer la complexité d'un
//! prompt par un LLM (même petit) reviendrait à payer un appel réseau pour
//! décider si on doit payer un appel réseau — ça détruit l'intérêt même de
//! l'autopilot. Le score est donc calculé par des heuristiques textuelles
//! (longueur, mots-clés de raisonnement, présence de code, nombre de
//! sous-questions...), en microsecondes, sans dépendance réseau.
//!
//! Le score est **explicable** : chaque signal déclenché est nommé et pondéré,
//! pour qu'un opérateur humain (ou un log) puisse comprendre pourquoi une
//! requête a été jugée complexe, plutôt que de recevoir un simple flottant
//! opaque.

use crate::tokens::estimate_tokens;

/// Un signal individuel de complexité, avec son poids et s'il s'est déclenché.
#[derive(Debug, Clone, PartialEq)]
pub struct ComplexitySignal {
    pub name: &'static str,
    pub weight: f64,
    pub triggered: bool,
}

/// Résultat de l'analyse de complexité d'un texte.
#[derive(Debug, Clone, PartialEq)]
pub struct ComplexityScore {
    /// Score final, toujours dans [0.0, 1.0].
    pub score: f64,
    /// Détail des signaux évalués, pour l'explicabilité.
    pub signals: Vec<ComplexitySignal>,
}

impl ComplexityScore {
    /// Liste les noms des signaux qui se sont réellement déclenchés.
    pub fn triggered_signals(&self) -> Vec<&'static str> {
        self.signals
            .iter()
            .filter(|s| s.triggered)
            .map(|s| s.name)
            .collect()
    }
}

const REASONING_KEYWORDS: &[&str] = &[
    "explique pourquoi",
    "explain why",
    "step by step",
    "étape par étape",
    "prove",
    "démontre",
    "derive",
    "dérive",
    "algorithm",
    "algorithme",
    "architecture",
    "optimi",
    "debug",
    "root cause",
    "cause racine",
    "trade-off",
    "compromis",
    "conçois un système",
    "design a system",
];

/// Analyse un prompt et retourne son score de complexité (0.0 = trivial,
/// 1.0 = très complexe) accompagné du détail des signaux.
pub fn analyze(prompt: &str) -> ComplexityScore {
    let lower = prompt.to_lowercase();
    let tokens = estimate_tokens(prompt);

    // Signal 1 : longueur. Une requête longue porte souvent plus de contexte
    // ou de sous-tâches. Croissance continue plafonnée, pas un simple seuil.
    let length_weight = ((tokens as f64) / 1500.0).min(0.30);
    let length_signal = ComplexitySignal {
        name: "longueur",
        weight: length_weight,
        triggered: tokens > 200,
    };

    // Signal 2 : bloc de code (```), signe d'une tâche d'analyse/génération
    // de code, généralement plus exigeante qu'une question factuelle.
    let has_code = lower.contains("```") || lower.matches("    ").count() > 3;
    let code_signal = ComplexitySignal {
        name: "bloc_de_code",
        weight: 0.20,
        triggered: has_code,
    };

    // Signal 3 : mots-clés associés à un raisonnement multi-étapes.
    let has_reasoning_keyword = REASONING_KEYWORDS.iter().any(|kw| lower.contains(kw));
    let reasoning_signal = ComplexitySignal {
        name: "mot_cle_raisonnement",
        weight: 0.25,
        triggered: has_reasoning_keyword,
    };

    // Signal 4 : plusieurs questions distinctes dans le même prompt (tâche
    // composite : un modèle faible tend à n'en traiter qu'une partie).
    let question_marks = prompt.matches('?').count();
    let multi_question_signal = ComplexitySignal {
        name: "questions_multiples",
        weight: 0.15,
        triggered: question_marks >= 2,
    };

    // Signal 5 : densité de contenu numérique/symbolique (calcul, formules).
    let digit_count = prompt.chars().filter(|c| c.is_ascii_digit()).count();
    let symbol_count = prompt
        .chars()
        .filter(|c| matches!(c, '+' | '-' | '*' | '/' | '=' | '^' | '∑' | '∫'))
        .count();
    let numeric_density = if tokens > 0 {
        (digit_count + symbol_count) as f64 / tokens as f64
    } else {
        0.0
    };
    let numeric_signal = ComplexitySignal {
        name: "densite_numerique",
        weight: 0.10,
        triggered: numeric_density > 0.15,
    };

    let signals = vec![
        length_signal,
        code_signal,
        reasoning_signal,
        multi_question_signal,
        numeric_signal,
    ];

    let raw_score: f64 = signals
        .iter()
        .map(|s| if s.triggered { s.weight } else { 0.0 })
        .sum();

    ComplexityScore {
        score: raw_score.clamp(0.0, 1.0),
        signals,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trivial_greeting_has_low_complexity() {
        let result = analyze("Salut, ça va ?");
        assert!(result.score < 0.2, "score inattendu : {}", result.score);
    }

    #[test]
    fn code_debugging_request_is_flagged_complex() {
        let prompt = "Debug ce code étape par étape et explique pourquoi ça plante :\n```rust\nfn main() { panic!(); }\n```";
        let result = analyze(prompt);
        // Code + mot-clé de raisonnement suffisent à sortir nettement du
        // trivial (0.0-0.2), sans pour autant atteindre le haut de l'échelle
        // réservé aux prompts qui cumulent aussi longueur/questions/calcul.
        assert!(result.score > 0.4, "score inattendu : {}", result.score);
        assert!(result.triggered_signals().contains(&"bloc_de_code"));
        assert!(result.triggered_signals().contains(&"mot_cle_raisonnement"));
    }

    #[test]
    fn multiple_questions_increase_score() {
        let single = analyze("Quelle est la capitale de la France ?");
        let multi = analyze(
            "Quelle est la capitale de la France ? Et celle de l'Allemagne ? Et celle de l'Espagne ?",
        );
        assert!(multi.score > single.score);
    }

    #[test]
    fn score_is_always_bounded() {
        let huge = "explique pourquoi step by step ```code``` ".repeat(500) + "??? 1+1=2 *";
        let result = analyze(&huge);
        assert!(result.score <= 1.0);
        assert!(result.score >= 0.0);
    }

    #[test]
    fn empty_prompt_has_zero_complexity() {
        let result = analyze("");
        assert_eq!(result.score, 0.0);
    }

    #[test]
    fn numeric_heavy_prompt_is_flagged() {
        let result = analyze("2+2=4, 3*3=9, 10/2=5, 7-1=6");
        assert!(result.triggered_signals().contains(&"densite_numerique"));
    }
}
