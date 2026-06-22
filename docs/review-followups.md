# Suivi de revue — points à traiter

Notes issues d'une revue globale du projet (juin 2026). Ce fichier regroupe les
points laissés de côté pour décision/relecture ultérieure.

## 5. Empoisonnement des mutex en cascade (robustesse) — ✅ RÉSOLU

**Constat (historique).** Tous les handlers du daemon (et du serveur GUI)
prenaient les verrous via `.lock().unwrap()` / `.lock().expect(...)`. Si **un
seul** handler paniquait en tenant un verrou, le `Mutex` était *empoisonné*
(`PoisonError`) et **toutes** les requêtes suivantes paniquaient à leur tour,
rendant le repo (ou le GUI) inutilisable jusqu'au redémarrage.

**Correction appliquée.**
- Trait partagé `metafolder_core::sync::MutexExt::lock_recover()` : récupère le
  guard même empoisonné (`PoisonError::into_inner`) **et** efface le drapeau
  (`Mutex::clear_poison`, Rust ≥ 1.77) pour que les accès suivants reprennent
  le chemin rapide. Couvert par un test unitaire dans `core/src/sync.rs`.
- Daemon : tous les `.lock().unwrap()` migrés vers `lock_recover()`. Cas
  spécial du **cache d'arbre** → `RepoState::lock_cache()`, qui **vide** le
  cache en cas de poison (son état mémoire peut être incohérent et désynchro
  d'un write annulé ; il se repeuple paresseusement depuis la DB).
- GUI : état central `GuiState::lock()`, `CommandRegistry`, `InputWait`,
  `DaemonProxy`, keybindings, etc. migrés vers `lock_recover()`. L'état GUI
  étant sa propre source de vérité, on récupère le guard sans le vider.
- `RecordingNotifier` (helper de test) laissé tel quel : cascade non
  pertinente.

Justification du « pas de panic » côté données : tout write SQLite passe par
une transaction atomique, donc un panic en cours de write est déjà *rollback*
par le `Drop` de `Transaction` de rusqlite (mode `unwind`, vérifié : pas de
`panic = "abort"`).

## 6. `enqueue_restoration` ignorait la direction — ✅ RÉSOLU

**Constat (historique).** `coordinated_step()` dérivait toujours la restauration
d'un `file_moved` skippé du snapshot `is_new=1`, **quelle que soit la
direction**. Correct pour un pas *inverse* (rollback), mais en pas *forward*
(redo) cela laissait `mfr_path` sur la destination du move (que le `skip`, donc
le `mv` non exécuté, n'a justement pas atteint) → métadonnée incohérente.

**Correction appliquée.** `enqueue_restoration` prend désormais `dir` et rewind
vers l'emplacement enregistré *avant le pas* — le snapshot que le pas n'a pas
appliqué : `is_new=1` en inverse, `is_new=0` en forward. Tests :
`crates/daemon/tests/coordinated_skip.rs` (forward → pré-move, inverse →
post-move).

**Sémantique « skip » clarifiée (présent vs disparu).** Suite à la relecture de
la spec, le rewind n'a de sens que pour un fichier **présent mais non
déplaçable** (il garde la métadonnée vraie). Pour un fichier **disparu**, le CLI
utilise désormais `step {}` (politique `apply`) : la métadonnée suit le rollback
et conserve le chemin post-rollback, plutôt qu'un rewind vers un emplacement
vide ou un `Nothing`. Les quatre politiques (`apply|skip|abort|ask`) restent
disponibles dans les deux situations. Implémenté dans `cli/src/log.rs`
(`decide_move` ne tente le `mv` que si le fichier est présent), spec mise à jour
(`spec-event-log.org` : section « skip » + « Policies for move_file »).

## 7. `FollowsTransitive` : coût O(taille du sous-arbre) — ⏳ DIFFÉRÉ (gros chantier)

**Constat.** `Query::FollowsTransitive` (l'opérateur DSL `->*`, « tous les
descendants de ce nœud dans la forêt TreeRef ») est compilé de façon *hybride*
(`crates/daemon/src/query_exec.rs`, nœud `FollowsTransitive`) :

1. la **racine** est résolue via le tree cache (`resolve_path`, fallback DB) —
   bon marché ;
2. les **descendants** sont collectés par `TreeCache::descendants`
   (`tree_cache.rs`), qui **marche la base** en BFS (`db::tree_children`, une
   requête SQL par nœud) — **pas l'arène mémoire** ;
3. le `Vec<Uuid>` obtenu est **inliné en littéraux** dans le SQL :
   `SELECT column1 AS uuid FROM (VALUES (x'…'),(x'…'),…)`.

Deux coûts, tous deux **linéaires en la taille du sous-arbre** (non bornée par
`max_nodes` du cache, c'est la taille réelle en DB) :

- **(a) Texte SQL géant.** Chaque littéral ≈ 38 octets ⇒ ~19 Mo de SQL pour
  500k descendants (≈38 Mo à 1M), construit en `String` puis parsé par SQLite.
  Risque de buter sur `SQLITE_MAX_SQL_LENGTH` (défaut ~1 Go) sur de très gros
  sous-arbres, coût mémoire/CPU lourd bien avant.
- **(b) N requêtes `tree_children`.** Un aller-retour SQLite par nœud du
  sous-arbre.

**Le vrai problème de fond (à régler plus tard).** On **matérialise tout le
sous-arbre** alors qu'on ne veut en général que la **page** demandée (~100
résultats triés). Le coût devrait dépendre de la taille de page, pas du dossier.
L'approche actuelle « matérialiser puis filtrer/paginer en aval » devra
probablement être revue vers une **intégration dans la requête paginée/triée**
(push-down). Limite inhérente à garder en tête : dès qu'on **trie par un
champ**, il faut de toute façon l'ensemble complet des candidats pour choisir le
top-N — la linéarité n'est totalement évitable que pour les requêtes **sans
tri** (où une CTE en flux peut s'arrêter tôt sous `LIMIT`).

**Pourquoi le cache ne peut pas servir tel quel** (utile pour le design futur).
La map `children` d'un nœud en cache est **partielle** : `resolve_path` n'insère
que les enfants rencontrés sur un chemin déjà résolu, et un *miss* sur un enfant
signifie « pas en cache », **pas** « n'existe pas ». Il n'y a **aucun marqueur
« tous les enfants chargés »**. Énumérer les descendants depuis l'arène
raterait donc silencieusement des nœuds → résultats faux. Seule la DB est
autoritaire sur la liste complète des enfants.

**Pistes (par ordre de complétude) :**

| Approche | Corrige (a) littéraux | Corrige (b) N requêtes | Coût |
|---|---|---|---|
| `carray` / `rarray(?)` (feature rusqlite `array`) | ✅ | ❌ | feature + variante `SqlValue::Array` + `array::load_module` |
| table TEMP (insert par lots) | ✅ | ❌ | gestion du cycle de vie (DROP après exécution, y compris sur erreur) |
| **CTE récursive** (`WITH RECURSIVE … JOIN field …`) | ✅ | ✅ | refonte du nœud en SQL pur ; racine = param (cas `Path`) ou sous-CTE (cas `Condition`) ; `UNION` (pas `ALL`) pour dédup + anti-cycle |

La CTE récursive est la plus complète (corrige (a) **et** (b), pas de
matérialisation Rust) et reste compatible avec les deux formes de racine
(`Path` / `Condition`). C'est elle qu'il faudra viser si on intègre la
traversée dans la requête paginée.

**Idée utilisateur : compteur d'enfants dénormalisé.** Stocker en DB le nombre
d'enfants par `(field_name, parent_uuid)`, incrémenté/décrémenté à chaque
ajout/retrait de `tree_ref`. Bénéfices : éviter la requête `tree_children` pour
les **feuilles** (compteur = 0 — souvent la majorité des nœuds), et permettre au
cache de **détecter la complétude** (taille de la map `children` == compteur DB
⇒ l'arène a tous les enfants ⇒ énumération sans DB). Limites/coûts à peser :
ne corrige **pas** la linéarité de fond (on visite quand même chaque nœud) ; et
la maintenance du compteur doit passer par le `log::Writer` (chaque write
TreeRef) **et** être restaurée exactement par le rollback (charge de cohérence
non triviale) ; la détection de complétude côté cache devrait aussi invalider le
flag « complet » à l'éviction d'un enfant.

**Pointeurs code :** `query_exec.rs` (nœud `FollowsTransitive`),
`tree_cache.rs` (`descendants`, `resolve_path`), `db.rs` (`tree_children`).
`docs/spec-query.org` pour la sémantique de `->*`.

## 8. Limites de requête — ✅ borne de nœuds / ⏳ reste

**Fait.** Borne du **nombre total de nœuds** d'une requête à `MAX_QUERY_NODES =
2000` (`query_exec.rs`), vérifiée avant compilation dans `execute`/`count` →
rejet 400 (« query too large … decompose it »). Garde-fou contre une requête
bon marché à envoyer mais coûteuse à *compiler* (un `And`/`Or` large ou
profond construirait une chaîne de CTE géante avant toute lecture). Généreuse :
les requêtes réalistes sont très en-dessous. Tests : `query_exec` (unit
`node_count`/`check_query_size`) + `tests/query.rs` (`test_oversized_query_is_rejected`).

**⏳ Reste à faire (différé) :**

- **Opérateur `In { field, values }` natif.** Aujourd'hui « ce champ vaut l'une
  de ces N valeurs » s'écrit `Or` de N `Eq` = ~2N nœuds. Un `In` natif
  compilerait en **un** `IN (…)` SQL (ou un join carray/table-temp, cf. §7) →
  O(1) nœuds, et rendrait la borne indolore pour l'appartenance.
- **Plafond `SQLITE_MAX_COMPOUND_SELECT` (défaut 500).** ✅ *Message propre fait* :
  `combine` rejette désormais un `And`/`Or` de plus de
  `MAX_COMBINATOR_OPERANDS = 500` opérandes avec une erreur claire (« a single
  'and'/'or' may have at most 500 operands… nest or decompose it »), au lieu de
  l'erreur cryptique de SQLite (test `test_wide_combinator_is_rejected_with_clear_message`
  vérifie aussi que 500 pile s'exécute). ⏳ *Reste* : pour **supporter** les
  listes larges plutôt que les rejeter, **chunker** le compound en lots
  imbriqués (≤ 500 par niveau) — ou, mieux, l'opérateur `In` natif ci-dessus.
- **Timeout d'exécution.** La borne de nœuds ne couvre que le coût de
  *compilation* ; une requête petite mais lente (`Matches` regex sur des
  millions de lignes, `->*` sur tout le repo — cf. §7) n'est pas bornée en
  *temps*. Piste : `progress_handler` SQLite (interrompt après N pas de VM) ou
  `Connection::interrupt()` depuis un watchdog après une deadline wall-clock.

## 9. Link metarecords : écritures non « link-aware » — ⏳ DIFFÉRÉ (v2)

**Contexte.** Un *link metarecord* est possédé par **plusieurs** repos (plusieurs
lignes `metarecord_db` pour le même `metarecord_uuid`). C'est un concept **v2,
non implémenté** : aujourd'hui chaque metarecord a un seul propriétaire et chaque
repo est sa propre base, donc **aucune corruption actuelle**. Les **lectures**
sont déjà link-aware (le CTE `_repo` de `query_exec` exige la propriété
**exclusive**, `COUNT(*) = 1` → les links sont invisibles aux requêtes).

**Constat (les écritures ne le sont pas).** Aucune opération d'écriture ne
vérifie l'exclusivité de propriété ni « tous les repos propriétaires chargés » :

- `log::Writer::delete_metarecord` : `DELETE FROM metarecord WHERE uuid = ?1` →
  supprime **l'entité entière** (CASCADE efface **toutes** les lignes
  `metarecord_db`, donc tous les copropriétaires).
- `log::navigate`/`prune` (vers l'état vide) : le `SELECT` est cadré
  `WHERE db_id = ?1`, mais le `DELETE` porte sur `metarecord` → efface aussi les
  copropriétaires (c'est le **M4** de l'audit).
- `set_field` / écritures de champ : opèrent sur le `uuid` sans contrôle de
  propriété.

Donc si un link existait, une suppression/rollback dans le repo A détruirait le
metarecord partagé avec B, et une modif de champ s'appliquerait à la donnée
partagée sans coordination.

**Invariant voulu (à appliquer quand les links arrivent).** Aucune modification
sur un link tant que **tous** les repos propriétaires ne sont pas chargés (pour
que le changement soit cohérent/visible des deux côtés et géré par le daemon).
Concrètement :
- **suppression** cadrée par propriétaire : retirer la ligne `metarecord_db` du
  repo courant ; ne supprimer l'entité `metarecord` que quand le **dernier**
  propriétaire la retire ;
- **modification** d'un metarecord partagé : refusée tant que les repos
  propriétaires ne sont pas tous chargés (ou coordonnée entre les repos
  chargés) ;
- cohérent avec les lectures qui excluent déjà les links.

Non implémentable maintenant : le modèle de stockage/sync des links est v2 et
non défini ; un garde-fou serait du code mort (rien ne crée de link). À traiter
lors de la conception des links (`docs/spec-sync.org`). **Pointeurs :**
`log.rs` (`delete_metarecord`, `navigate`, `prune`), `query_exec.rs` (CTE
`_repo`, le modèle d'exclusivité de référence), `db.rs` (`metarecord_db`).
