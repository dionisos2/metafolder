# Décisions d'architecture

## Stockage

**Décision : SQLite par base de données (dépôt)**
- **Pourquoi** : dizaines de milliers de fichiers minimum, requêtes fréquentes du type "tous les fichiers avec le tag X" → nécessite des index. Les fichiers JSON n'offrent pas d'indexation et sont mal adaptés aux écritures fréquentes et durables.
- Chaque écriture est immédiatement persistée sur disque (mode WAL de SQLite — atomique et durable par défaut).
- TagStudio a fait le même choix.

**Question reportée : cache in-memory**
Charger tout ou partie d'une base en RAM pour accélérer les lectures est faisable (ordre de grandeur : 50-150 Mo pour 100k fichiers). À évaluer selon les besoins réels de performance. Si nécessaire, implémenter via un trait Rust avec deux backends (SQLite pur / structure en mémoire).

---

## Modèle de métadonnées

### Structure fondamentale : tout est un MetadataEntry

```
MetadataEntry {
  uuid:   UUID,
  db_id:  DatabaseId,                           // attribut système — pas un field utilisateur
  fields: [(field_name: String, value: Value)]  // multi-map : clés dupliquées autorisées
}

Value = Nothing | String | Int | Float | Bool | Date | DateTime | Duration | Ref(UUID)
```

**Un seul concept universel.** Fichiers, tags, relations de préférence, groupes — tout est un `MetadataEntry`. Pas de type spécial pour les fichiers ou les tags.

- **Fichier** : entrée avec des champs `path` et `hash`. Le daemon surveille (inotify) les entrées qui ont un champ `path`.
- **Tag** : entrée avec un champ `label` et optionnellement `parent: Ref(→ autre tag)`.
- **Relation de préférence** : entrée avec des champs nommés `preferred: Ref(→ A)`, `over: Ref(→ B)`, `strength: Int(3)`. Les rôles sont nommés, plus expressif qu'une liste ordonnée.
- **Hiérarchie** : exprimée via des champs `parent`, pas un concept séparé. N'importe quelle entrée peut avoir des parents.

Exemple concret :
```
// Fichier
{ uuid: "f1", fields: [path: "/music/jazz.mp3", hash: "abc", tag: Ref("jazz"), tag: Ref("live"), rating: Int(4) ] }

// Tag jazz (référencé par le fichier)
{ uuid: "jazz", fields: [label: "jazz", parent: Ref("music")] }

// Tag music (ancêtre)
{ uuid: "music", fields: [label: "music"] }
```

### Logique à trois valeurs

Intégrée naturellement dans le modèle :
- **Present** : le champ existe avec une valeur non-Nothing
- **Absent** : le champ existe avec la valeur `Nothing` (confirmation explicite de l'absence)
- **Unknown** : le champ n'existe pas (état par défaut)

`Nothing` remplace les valeurs sentinelles (ex : `-1` pour une durée non applicable).

### Multi-valeur

Les champs sont un **multi-map** : plusieurs champs avec le même nom sont autorisés.
- **Pourquoi** : solution naturelle du modèle EAV, la plus rapide en requêtes.
- "Tous les tags du fichier X" → `WHERE entry_uuid=X AND field_name='tag'` (lookup indexé)
- "Tous les fichiers avec le tag jazz" → `WHERE field_name='tag' AND value_ref='jazz-uuid'` (lookup indexé)

### Stockage SQLite (EAV)

```sql
CREATE TABLE entry (uuid TEXT PRIMARY KEY);
CREATE TABLE field (
  entry_uuid  TEXT REFERENCES entry(uuid),
  field_name  TEXT,
  value_type  TEXT,   -- "nothing"|"string"|"int"|"float"|"bool"|"date"|"duration"|"ref"
  value_str   TEXT,   value_int  INTEGER,  value_real REAL,  value_ref  TEXT
);
CREATE INDEX idx_entry   ON field(entry_uuid, field_name);
CREATE INDEX idx_reverse ON field(field_name, value_ref);
```

Les requêtes hiérarchiques ("tous les descendants de 'music'") utilisent des CTEs récursifs SQLite (`WITH RECURSIVE`).

**Note** : le modèle EAV sur SQL est plus complexe à implémenter qu'un schéma spécialisé, mais c'est le bon choix pour la flexibilité requise. À l'échelle d'un usage personnel (centaines de milliers de fichiers), les performances restent gérables avec les bons index.

### Schéma (optionnel, par dépôt)

**Format** : fichier JSON dans `.metafolder/`, lu et interprété par le daemon au démarrage. Pas une MetadataEntry — sémantique propre et fixe.

**But** : réduire la généricité du système pour garantir que les commandes peuvent se fier aux types qu'elles reçoivent (ex : une commande qui cherche les musiques préférées peut supposer que `rating` est un Int).

**Structure** :
```json
{
  "fields": {
    "rating":   { "type": "int",    "cardinality": "single",   "min": 0, "max": 10 },
    "tag":      { "type": "ref",    "cardinality": "multiple"  },
    "creation": { "type": "date",   "cardinality": "single"    },
    "path":     { "type": "string", "cardinality": "single"    },
    "label":    { "type": "string", "cardinality": "single"    }
  }
}
```

**`Nothing` reste toujours valide** quelle que soit la contrainte — c'est le marqueur "explicitement absent" indépendant du type.

**Enforcement strict, tout ou rien** :
- **À l'écriture** : le daemon rejette toute écriture qui viole le schéma avec une erreur claire. La donnée n'est jamais écrite.
- **À la lecture/requête** : les entrées qui violent le schéma sont exclues des résultats. Le daemon joint un rapport avec trois niveaux de verbosité : présence de violations, liste des entrées problématiques, détail de la règle non respectée.
- **Données préexistantes invalides** (schéma ajouté après coup) : le daemon continue à fonctionner, les données invalides sont lisibles mais signalées par `metafolder validate`.

**Contraintes conditionnelles** (ex : "champ requis seulement sur les entrées avec un champ `path`") : deferré. Les contraintes globales par nom de champ couvrent l'essentiel pour l'instant.

---

## Identité des fichiers

**Décision : UUID + hash + chemin comme trois concepts distincts**
- **UUID** : identité stable d'un fichier. Survit aux renommages, déplacements, et modifications de contenu.
- **Hash** : empreinte du contenu. Sert à la détection de doublons et à la réconciliation (retrouver un fichier déplacé).
- **Chemin** : métadonnée système indiquant où trouver le fichier sur disque. Peut être obsolète si le fichier a été déplacé sans que le daemon le sache.

**Décision : suivi en temps réel via inotify (pas de root requis)**
- Le daemon surveille les répertoires des dépôts chargés via inotify.
- Un déplacement/renommage détecté en temps réel met à jour le chemin dans la base immédiatement.

**Décision : la réconciliation est une commande explicite, pas automatique au démarrage**
- **Pourquoi** : scanner et hasher tous les fichiers au démarrage serait coûteux pour un cas rare.
- En fonctionnement courant, si un fichier est introuvable à son chemin, le daemon retourne une erreur "fichier [UUID] non trouvé" avec les métadonnées associées. L'utilisateur peut alors : corriger le chemin manuellement, lancer une réconciliation, ou ne rien faire.
- Cas non récupérable automatiquement : fichier déplacé ET modifié simultanément (ni chemin ni hash ne correspondent) → résolution manuelle.

**Décision : la réconciliation est une opération du daemon, déclenchée via son API**
- Le daemon connaît les répertoires en scope (dépôts chargés) — il est mieux placé que la commande pour faire le scan.
- La commande Rust se contente de déclencher l'opération et d'afficher le résultat.

---

## Modèle de dépôt

**Décision : un dépôt = un dossier racine + un dossier `.metafolder/`**
- `.metafolder/` contient la base SQLite et un fichier de configuration.
- La configuration indique le dossier racine du dépôt (par défaut `../`, soit le dossier parent de `.metafolder/`).
- Ce dossier racine peut être modifié pour permettre une **base de données externe** : `.metafolder/` vit ailleurs (ex : sur un second disque ou sur la machine), mais pointe vers le dossier racine à surveiller. Utile si le disque de données est en lecture seule ou si on ne veut pas y écrire.
- Tous les chemins des fichiers dans la base sont **relatifs à la racine du dépôt**, ce qui rend le dépôt portable (déplacer le disque ne casse rien).

**Décision : un dépôt couvre récursivement tout son dossier racine, et rien en dehors**
- Raisons : chemins relatifs (portabilité), sécurité, simplicité.

**Décision : les dépôts sont complètement indépendants les uns des autres**
- Un fichier couvert par deux dépôts aura deux UUIDs distincts, sans coordination entre les bases.
- **Pourquoi** : les dépôts peuvent être chargés à des moments différents (disques branchés/débranchés). Imposer une coordination entre BDDs serait incompatible avec ce cas d'usage.
- Si deux dépôts sont chargés simultanément et couvrent les mêmes fichiers, c'est la responsabilité de l'utilisateur. Un outil de transfert entre dépôts sera fourni (commande de haut niveau), mais les dépôts eux-mêmes ne se connaissent pas.

---

## Système de requêtes

### Prédicats de base

```
// Logique à trois valeurs
duration IS UNKNOWN       // le champ n'existe pas
duration IS ABSENT        // le champ existe avec valeur Nothing
rating   IS PRESENT       // le champ existe avec une valeur non-Nothing

// Comparaisons (types ordonnés : Int, Float, Date, DateTime, Duration)
rating > 3
creation >= 2023-01-01

// Égalité
label = "jazz"
rating IN [4, 5]
```

### Traversée de références (opérateur de chemin)

La primitive générique est une **expression de chemin** sur le graphe de références :

```
→    saut unique      : suivre ce champ une fois
→*   fermeture transitive : suivre ce champ zéro ou plusieurs fois
```

Exemples :
```
tag → (label = "jazz")
// tag pointe vers une entrée où label = "jazz"

tag → parent →* (label = "music")
// tag pointe vers une entrée X, depuis laquelle en suivant
// parent zéro ou plusieurs fois, on atteint une entrée où label = "music"
// couvre : fichier tagué directement "music" OU tagué avec n'importe quel descendant

belongs_to →* (name = "projet-X")
// groupe imbriqué quelconque

related → author → (name = "Coltrane")
// traversée multi-champs arbitraire
```

`→+` (un hop minimum) et les contraintes sur le nombre de hops sont différés — `→*` couvre l'essentiel pour l'instant.

### Combinateurs

`AND`, `OR`, `NOT`, parenthèses pour le groupage.

Exemple complet :
```
path IS PRESENT
AND tag → parent →* (label = "music")
AND rating > 3
AND duration IS UNKNOWN
```

### Format

- **JSON** pour l'API HTTP (représentation interne des requêtes)
- **DSL textuel** comme sucre syntaxique pour les commandes CLI et les scripts bash, compilé vers la représentation JSON

### Sémantique multi-map

Quand un champ a plusieurs valeurs (multi-map), `field = X` signifie "au moins une occurrence du champ a la valeur X".

### Résultats et schéma

Les entrées qui violent le schéma sont exclues des résultats avec un rapport de violations (cf. section Schéma).

---

## Synchronisation entre bases de données

**Décision : le daemon ne touche jamais aux fichiers**
- Le daemon gère uniquement les métadonnées.
- Les opérations sur les fichiers (copie, déplacement, sync) sont toujours faites par des commandes ou des scripts.

**Décision : la synchronisation est implémentée dans des scripts utilisateur, pas dans une commande Rust fixe**
- La sync est trop spécialisée (dépend du setup, des stratégies de conflit, des outils externes) pour être une commande générique.
- Les commandes Rust fournissent les **primitives** nécessaires pour que les scripts soient pratiques à écrire.

**Modèle de synchronisation : "dernier état connu" (comme git remote tracking)**
- Chaque BDD stocke, pour chaque fichier lié à une autre BDD : son propre état actuel + un snapshot "dernier état synchro" de l'autre BDD.
- Lors d'une sync entre BDD X et BDD Y, on dispose de 4 états pour chaque fichier lié : `FXX` (état actuel dans X), `FXY` (dernier état connu de Y, stocké dans X), `FYX` (dernier état connu de X, stocké dans Y), `FYY` (état actuel dans Y).
- Détection automatique : modifié seulement dans X (`FXX≠FXY`, `FYY=FYX`), seulement dans Y, dans les deux (conflit), ou aucun changement.

**Infrastructure nécessaire dans la BDD :**
- Table de liens : `(uuid_local, uuid_distant, bdd_distante_id)`
- Snapshot "dernier état synchro" par fichier lié et par BDD distante

**Primitives Rust nécessaires pour les scripts de sync :**
- `links list --db-a --db-b` : liste des liens entre deux BDDs
- `links detect --db-a --db-b` : candidats à la liaison par hash identique
- `links create --db-a --uuid-a --db-b --uuid-b` : créer un lien
- `sync-state --db-a --uuid-a --db-b --uuid-b` : retourner les 4 états (JSON)
- `sync-commit ...` : mettre à jour les snapshots après sync réussie

**Mécanisme de transfert :**
- Abstrait — le script choisit l'outil (rsync, git, SSH, disque externe, etc.).
- Le daemon et les commandes Rust fonctionnent de la même manière quelle que soit la méthode de transfert.

---

## Logs et retour en arrière

**Décision : event sourcing — le log est la source de vérité**
- Chaque opération sur les métadonnées est enregistrée immédiatement dans le log avant/après modification.
- L'état actuel de la BDD est la projection du log.
- Structure dans SQLite :
  - `Operation { id, timestamp, transaction_id, type, entity, old_state, new_state, status }`
  - `Transaction { id, timestamp, label }` — label = tag optionnel posé par l'utilisateur

**Décision : rollback arbitraire dans le passé, avec états taggés**
- On peut revenir à n'importe quel point passé (par id, date, ou label).
- L'utilisateur peut poser un tag sur l'état courant avant de faire des modifications risquées, puis revenir à ce tag si besoin.
- Rollback = annuler toutes les transactions postérieures au point cible, dans l'ordre inverse.

**Décision : le rollback des déplacements de fichiers est géré par une commande, pas le daemon**
- Les déplacements de fichiers faits avec `mv` sont détectés par le daemon (inotify ou réconciliation) et loggés comme changements de chemin.
- Une commande `metafolder rollback` effectue elle-même les `mv` inverses — le daemon ne touche jamais aux fichiers.

**Décision : protocole rollback en mode coordonné (inotify suspendu)**
- Problème : si inotify reste actif pendant le rollback, chaque `mv` inverse crée une nouvelle entrée de log au lieu d'annuler l'ancienne.
- Solution : protocole en étapes entre la commande et le daemon :
  1. Commande → daemon : "démarre rollback jusqu'au point X, suspends inotify"
  2. Daemon passe en mode rollback, retourne la liste des opérations inverses
  3. Pour chaque opération : commande effectue le `mv`, daemon marque l'entrée comme "rolled back"
  4. Commande → daemon : "rollback terminé, reprends inotify"
- Si crash en cours de rollback : le daemon connaît sa position exacte (statut des entrées) et peut reprendre au redémarrage.
- Les entrées de log sont **marquées "rolled back"**, pas supprimées — préserve l'audit trail et permet un "undo de l'undo".

**Décision : log linéaire par défaut, avec préservation optionnelle de l'historique futur**
- Après un rollback, l'historique futur est supprimé par défaut (comportement simple).
- Option explicite : conserver l'historique futur sous un label nommé avant de l'écraser.

**Décision : rollback partiel en cas d'opérations irréversibles**
- Chaque type d'opération est classé réversible ou irréversible au moment de l'enregistrement dans le log.
- Irréversibles : suppression de fichier (contenu perdu), modification du contenu d'un fichier (hash changé — le daemon ne stocke pas le contenu).
- Comportement configurable via option : bloquer le rollback entier, ou continuer malgré les opérations irréversibles.
- Dans les deux cas : rapport clair listant ce qui n'a pas pu être annulé et pourquoi.

**Décision : pruning du log**
- L'utilisateur peut supprimer tout l'historique antérieur à un point donné (id, date, label).
- Les états taggés peuvent être conservés explicitement.
- Pas de politique automatique imposée — l'utilisateur gère sa propre rétention (ex : garder 10 Mo, supprimer le reste périodiquement).

---

## Workspace Rust

**Structure :**
```
crates/
  core/    → metafolder-core   (types partagés, pas de dépendances lourdes)
  daemon/  → metafolder-daemon (serveur HTTP, SQLite, inotify)
  cli/     → metafolder        (binaire CLI)
  gui/     → metafolder-gui    (différé)
```

**Stack technique :**
| Besoin | Crate |
|--------|-------|
| Async runtime | `tokio` |
| Serveur HTTP (daemon) | `axum` |
| Client HTTP (cli/gui) | `reqwest` |
| SQLite | `rusqlite` (feature `bundled` — SQLite compilé dans le binaire) |
| CLI args | `clap` (feature `derive`) |
| Sérialisation | `serde` + `serde_json` |
| Surveillance fichiers | `notify` |
| GUI | `egui` / `eframe` |
| UUIDs | `uuid` (feature `v4`) |

**Ordre d'implémentation recommandé :** `core` → `daemon` → `cli` → `gui`

---

## API du daemon

**Décision : HTTP/REST + JSON**
- **Pourquoi** : le choix du protocole a peu d'impact sur les performances réelles (les requêtes SQLite dominent), donc on optimise pour la commodité.
- Utilisable directement depuis bash avec `curl`, sans outil supplémentaire.
- Facile à déboguer.
- **Vrai goulot d'étranglement pour les scripts** : le démarrage du binaire Rust (~5–50 ms/invocation), pas le protocole. Mitigation : concevoir des APIs batch (ex : récupérer les métadonnées de N fichiers en une seule requête).

---

## Architecture générale

**Décision : plusieurs outils partageant un daemon commun**
- **Daemon** : gestion générique des métadonnées. API bien définie.
- **Commandes Rust** : outils génériques qui parlent au daemon.
- **GUI** : cas d'usage interactifs/visuels (ex : comparaison de musiques côte à côte).
- **Scripts bash** : workflows spécialisés par l'utilisateur, construits sur les commandes Rust.
- **Pourquoi** : sépare le générique (daemon, commandes) du spécialisé (scripts). Ressemble à l'architecture git + shell scripts.
