# llm-cost-autopilot

Route chaque requête vers un LLM local (Ollama/LM Studio, gratuit) ou une API
payante, selon la complexité réelle du prompt, le budget quotidien restant et
la santé récente de chaque route — pour ne payer une API que quand ça compte
vraiment.

Le problème que ça résout : un système qui appelle un LLM à chaque requête,
sans discernement, envoie aussi bien "salut" que "prouve ce théorème" au même
modèle — souvent le plus cher, "au cas où". `autopilot` décide *avant*
l'appel, en microsecondes et sans réseau, si le prompt a une chance
raisonnable d'être bien traité par un modèle local gratuit, et n'escalade
vers du payant (économique, puis premium) que si la complexité, le budget et
la fiabilité récente de chaque route le justifient.

Prévu pour être **embarqué** dans un projet plus large (typiquement "Ark") :
la décision de routage est une fonction pure sans effet de bord, séparée de
l'exécution HTTP — voir [Comment ça marche](#comment-ça-marche).

## Installation

```bash
cargo build --release
./target/release/autopilot --help
```

## Démarrage rapide

Sans rien connecter, `simulate --demo` lance trois petits serveurs HTTP
factices en local (aucune dépendance externe, aucune clé d'API) pour montrer
un cycle complet décision → appel → journalisation → bilan :

```bash
cargo run --release -- simulate --db demo.db --n 30 --seed 7 --daily-budget 2.0
```

Sortie réelle observée (le générateur de prompts synthétiques est
déterministe avec `--seed`, donc reproductible) :

```
Simulation de 30 requêtes (budget quotidien : Some(2.0))

#000 [local        ] coût=0.0000$ latence=4ms — Salut, comment ça va ?
#003 [paid-cheap   ] coût=0.0120$ latence=34ms — Quel est le compromis performance/mémoire de cet algorithme …
#015 [paid-cheap   ] coût=0.0120$ latence=31ms — Explique pourquoi, étape par étape, ce test unitaire échoue …
...

--- Bilan ---
Dépense réelle          : 0.0720$
Dépense si tout premium : 6.0000$
Économies               : 5.9280$
```

Sur ce lot, 24 des 30 requêtes (des salutations, des traductions, des tâches
triviales) sont restées gratuites en local ; seules les 6 qui contenaient un
vrai signal de complexité (raisonnement multi-étapes, code à déboguer) sont
parties en payant — jamais en premium ici, le budget de 2$ n'ayant jamais été
approché.

Pour se connecter à de vrais services plutôt qu'à la démo intégrée :

```bash
autopilot simulate --db prod.db --n 200 --daily-budget 5.0 \
  --local-url http://localhost:11434 \
  --paid-cheap-url https://api.mon-fournisseur.com \
  --paid-api-key "$MON_API_KEY"
```

`--local-url` fonctionne avec LM Studio (serveur local, `/v1/chat/completions`
activé par défaut) et avec Ollama (`ollama serve`, compatible OpenAI depuis
ses versions récentes) sans aucun changement de code.

## Comment ça marche

**1. La complexité est estimée sans appeler de LLM.** Faire évaluer un prompt
par un modèle (même petit) pour décider quel modèle utiliser annulerait
l'intérêt de l'autopilot — on paierait un appel réseau pour décider si on
paie un appel réseau. `complexity::analyze` calcule donc un score en
microsecondes à partir de signaux textuels explicables (longueur, présence de
code, mots-clés de raisonnement multi-étapes, questions multiples, densité
numérique), chacun nommé et pondéré plutôt que noyé dans un flottant opaque —
vérifié par un test qui inspecte les signaux réellement déclenchés, pas
seulement le score final.

**2. Estimation avant l'appel, facturation après.** Le nombre de tokens d'un
prompt est *estimé* (`tokens::estimate_tokens`, ~4 caractères/token — une
heuristique standard, pas un vrai tokenizer BPE) uniquement pour projeter un
coût et décider du routage. Une fois la réponse reçue, le coût réellement
journalisé utilise le `usage.prompt_tokens`/`usage.completion_tokens`
renvoyé par le fournisseur (avec repli sur l'estimation si un fournisseur ne
le fournit pas, par ex. certaines configurations d'Ollama) — jamais
l'estimation pré-appel, qui ne sert qu'à la décision.

**3. Un seul client HTTP pour le local et le payant.** LM Studio, Ollama
(récent) et les API payantes usuelles exposent toutes le même format
`/v1/chat/completions` compatible OpenAI. `HttpProvider` (`ureq`, un client
HTTP synchrone léger) ne change que l'URL de base et une éventuelle clé
d'API — testé par un vrai aller-retour socket contre un serveur HTTP factice
maison (`tests/common`), sur les trois chemins : succès, erreur HTTP, et
serveur injoignable (`tests/http_provider.rs`).

**4. Budget et disjoncteur, pas juste des seuils fixes.** `Router::decide`
compare la dépense déjà faite aujourd'hui (fenêtre glissante alignée sur le
jour civil UTC, dans le journal SQLite) au budget configuré, et rétrograde
vers une route moins chère si l'appel projeté le dépasserait — jusqu'au local
gratuit en dernier recours. Indépendamment, chaque route a un disjoncteur
classique (fermé → ouvert → semi-ouvert, `circuit.rs`) sur une moyenne mobile
exponentielle de son taux de succès : un modèle local qui plante ou renvoie
des réponses vides en boucle finit par être écarté pendant un délai de repos,
plutôt que de continuer à recevoir du trafic. Vérifié par des tests qui
provoquent réellement des échecs répétés contre un vrai serveur factice et
observent le disjoncteur s'ouvrir, puis se refermer après le cooldown.

**5. Repli automatique en cas d'échec, projet embarquable.**
`AutopilotEngine::route_and_execute` exécute réellement l'appel choisi ; en
cas d'échec, il retente une fois sur la route immédiatement supérieure avant
d'abandonner (`tests/engine_integration.rs` le vérifie contre deux vrais
serveurs, l'un en panne, l'autre fonctionnel). Pour un usage embarqué dans un
système plus large (le cas d'usage visé, "à intégrer dans Ark"),
`AutopilotEngine::decide` (ou directement `Router::decide`) expose la
décision seule, pure et sans effet de bord, à un hôte qui gère lui-même
l'appel réseau.

## Commandes

| Commande | Effet |
| --- | --- |
| `autopilot simulate --db <fichier>` | Envoie `--n` requêtes synthétiques à travers l'autopilot. `--seed` (déterminisme), `--daily-budget`, `--demo` (serveurs factices intégrés, activé par défaut), ou `--local-url`/`--paid-cheap-url`/`--paid-premium-url`/`--paid-api-key` pour de vrais services. |
| `autopilot route "<prompt>"` | Décision seule, sans appel réseau : route choisie, complexité, coût projeté, justification. |
| `autopilot report --db <fichier>` | Résume un journal existant : dépense réelle vs. dépense si tout était parti en premium, répartition par route. |

## Tests

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

46 tests : estimation de tokens et de complexité (dont les bornes et le
caractère explicable des signaux), modèle de coût, disjoncteur/EMA (ouverture
sur échecs répétés, semi-ouverture après cooldown, refermeture sur succès),
journal SQLite (filtres, fenêtre de dépense quotidienne, persistance réelle
entre deux ouvertures), politique de routage pure (seuils de complexité,
escalade sur route dégradée, rétrogradation budgétaire), et bout en bout
(`tests/engine_integration.rs`, `tests/http_provider.rs`) contre de vrais
serveurs HTTP locaux écrits à la main sur `TcpListener` — succès, erreur
HTTP, timeout, latence artificielle mesurée, repli après échec, disjoncteur
qui s'ouvre sous charge réelle, budget quotidien respecté sur plusieurs
appels consécutifs.

## Limites connues

- L'estimation de tokens (~4 caractères/token) est une heuristique standard,
  pas le tokenizer exact d'un modèle donné ; elle ne sert qu'au routage et à
  la projection de budget *avant* l'appel — le coût journalisé après coup
  utilise les tokens réels renvoyés par le fournisseur.
- Le score de complexité est un ensemble de règles explicables, pas un
  modèle appris : il capture des signaux de surface (mots-clés, longueur,
  code, questions multiples) et peut se tromper sur un prompt qui déjoue ces
  heuristiques (une question triviale très longue, ou une tâche complexe
  formulée en une phrase courte).
- Les tarifs de `PricingTable::example_default` sont indicatifs (ordre de
  grandeur mi-2026) — à ajuster à tes propres contrats fournisseur avant de
  faire confiance aux montants affichés.
- L'état du disjoncteur et les moyennes mobiles vivent en mémoire pour la
  durée du processus : chaque lancement de la CLI repart avec des routes
  "saines" par défaut. Pour un usage embarqué de longue durée (le cas visé
  dans "Ark"), garder l'instance de `Router`/`AutopilotEngine` vivante plutôt
  que d'en recréer une à chaque appel.
- Cet environnement de développement n'a pas de vrai LM Studio/Ollama/API
  payante à portée réseau pour un test de bout en bout "en vrai" ; la suite
  vérifie donc `HttpProvider` contre de vrais serveurs HTTP locaux (protocole
  identique, `/v1/chat/completions` compatible OpenAI) plutôt que contre un
  vrai fournisseur. `--local-url`/`--paid-cheap-url` pointent directement
  vers un vrai service sans changement de code.

## Licence

MIT — voir [LICENSE](LICENSE).
