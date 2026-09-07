//! Estimation approximative du nombre de tokens d'un texte.
//!
//! Ce n'est **pas** un tokenizer BPE réel (type `tiktoken`) : reproduire fidèlement
//! le tokenizer exact d'un fournisseur nécessiterait sa table de vocabulaire, qui
//! change selon le modèle et n'est pas toujours publique (notamment côté modèles
//! locaux). On utilise à la place une heuristique standard et documentée —
//! ~4 caractères par token en anglais/français courant — suffisante pour une
//! décision de *routage* (avant l'appel) et une *projection* de budget. Le coût
//! réel facturé, lui, utilise le `usage` renvoyé par le fournisseur après l'appel
//! (voir `provider::CompletionResponse`), jamais cette estimation.

/// Nombre moyen de caractères par token, mesuré empiriquement sur du texte
/// anglais/français courant (GPT-2/GPT-3 tokenizer). Approximation volontaire.
const CHARS_PER_TOKEN: f64 = 4.0;

/// Estime le nombre de tokens d'un texte à partir de sa longueur en caractères.
///
/// Toujours >= 1 pour un texte non vide, 0 pour un texte vide.
pub fn estimate_tokens(text: &str) -> usize {
    let char_count = text.chars().count();
    if char_count == 0 {
        return 0;
    }
    ((char_count as f64) / CHARS_PER_TOKEN).ceil().max(1.0) as usize
}

/// Estime le nombre de tokens d'une liste de messages de chat (rôle + contenu),
/// en ajoutant une petite surcharge fixe par message pour approximer les
/// jetons de formatage (rôle, séparateurs) qu'un vrai tokenizer de chat ajoute.
pub fn estimate_chat_tokens(messages: &[(&str, &str)]) -> usize {
    const PER_MESSAGE_OVERHEAD: usize = 4;
    messages
        .iter()
        .map(|(_, content)| estimate_tokens(content) + PER_MESSAGE_OVERHEAD)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_zero_tokens() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn short_text_is_at_least_one_token() {
        assert_eq!(estimate_tokens("Hi"), 1);
    }

    #[test]
    fn longer_text_scales_roughly_with_length() {
        let short = estimate_tokens("Bonjour le monde");
        let long = estimate_tokens(&"Bonjour le monde ".repeat(20));
        assert!(long > short * 10);
    }

    #[test]
    fn estimate_is_monotonic_in_length() {
        let a = estimate_tokens("un texte court");
        let b = estimate_tokens("un texte un peu plus long que le precedent");
        assert!(b > a);
    }

    #[test]
    fn chat_tokens_include_per_message_overhead() {
        let messages = [("user", "salut"), ("assistant", "bonjour")];
        let total = estimate_chat_tokens(&messages);
        let raw: usize = messages.iter().map(|(_, c)| estimate_tokens(c)).sum();
        assert!(
            total > raw,
            "l'overhead par message doit être compté en plus"
        );
    }
}
