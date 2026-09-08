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

/// Vérifie que `keyword` apparaît dans `haystack` sur une frontière de mot
/// (ni précédé ni suivi d'un caractère alphanumérique), plutôt que comme
/// simple sous-chaîne. Voir le commentaire sur le signal "mot_cle_raisonnement"
/// pour l'exemple concret ("prove" dans "improve") que ça corrige.
///
/// Ceci reste une heuristique de surface : elle empêche les faux positifs
/// par sous-chaîne, mais pas le jeu délibéré — un prompt trivial qui
/// mentionne isolément un des mots-clés ("un vrai trade-off, ce dîner")
/// gagne quand même les 0.25 points, et un prompt réellement complexe qui
/// évite soigneusement tous les mots de la liste peut ne déclencher aucun
/// signal. Voir "Limites connues" dans le README.
fn contains_word(haystack: &str, keyword: &str) -> bool {
    let mut search_from = 0;
    while let Some(rel_pos) = haystack[search_from..].find(keyword) {
        let match_start = search_from + rel_pos;
        let match_end = match_start + keyword.len();

        let before_is_boundary = haystack[..match_start]
            .chars()
            .next_back()
            .map(|c| !c.is_alphanumeric())
            .unwrap_or(true);
        let after_is_boundary = haystack[match_end..]
            .chars()
            .next()
            .map(|c| !c.is_alphanumeric())
            .unwrap_or(true);

        if before_is_boundary && after_is_boundary {
            return true;
        }
        search_from = match_start + 1;
        if search_from >= haystack.len() {
            break;
        }
    }
    false
}

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
    // `contains_word` (frontière de mot), pas `str::contains` : sans ça,
    // "prove" matchait à l'intérieur de "improve" (mot anglais très courant,
    // sans rapport avec un raisonnement multi-étapes) et "optimi" à
    // l'intérieur de "optimisme" — un seul mot ordinaire suffisait alors à
    // déclencher artificiellement les 0.25 points du signal.
    let has_reasoning_keyword = REASONING_KEYWORDS
        .iter()
        .any(|kw| contains_word(&lower, kw));
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

    #[test]
    fn common_word_containing_a_keyword_substring_does_not_falsely_trigger() {
        // "improve" contient "prove" ; "optimisme" contient "optimi". Avant
        // le passage à une comparaison sur frontière de mot, ces deux mots
        // ordinaires suffisaient à déclencher artificiellement le signal
        // "mot_cle_raisonnement" (+0.25) sur un prompt par ailleurs trivial.
        let result = analyze("Can you improve this a bit? Je reste dans l'optimisme.");
        assert!(
            !result.triggered_signals().contains(&"mot_cle_raisonnement"),
            "faux positif : {:?}",
            result.signals
        );
    }

    #[test]
    fn actual_reasoning_keyword_still_triggers_at_a_word_boundary() {
        // Non-régression : la correction de frontière de mot ne doit pas
        // rendre les vrais mots-clés inopérants (ponctuation, casse,
        // début/fin de chaîne).
        let result = analyze("Please prove this theorem.");
        assert!(result.triggered_signals().contains(&"mot_cle_raisonnement"));

        let result = analyze("prove");
        assert!(result.triggered_signals().contains(&"mot_cle_raisonnement"));

        let result = analyze("Explique pourquoi le ciel est bleu.");
        assert!(result.triggered_signals().contains(&"mot_cle_raisonnement"));
    }
}
