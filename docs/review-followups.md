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

## 7. `FollowsTransitive` : coût O(taille du sous-arbre) — ✅ les deux coûts levés / ⏳ la linéarité reste

**Mise à jour (septembre 2026).** Les deux coûts décrits ci-dessous ont disparu
avec l'unification du moteur (spec-indexing « No operand runs in SQL ») : il n'y
a plus de compilation SQL du tout, et la forêt est **résidente en entier**
depuis le chargement du dépôt.

**Constat (historique).** `Query::FollowsTransitive` (l'opérateur DSL `->*`,
« tous les descendants de ce nœud dans la forêt TreeRef ») était compilé de
façon *hybride* par l'ancien `query_exec.rs` :

1. la **racine** résolue via le tree cache — bon marché ;
2. les **descendants** collectés par `TreeCache::descendants`, qui **marchait la
   base** en BFS (`db::tree_children`, une requête SQL par nœud) ;
3. le `Vec<Uuid>` obtenu **inliné en littéraux** dans le SQL :
   `SELECT column1 AS uuid FROM (VALUES (x'…'),(x'…'),…)`.

D'où deux coûts linéaires en la taille du sous-arbre : **(a)** un texte SQL
géant (~19 Mo pour 500k descendants, ≈38 Mo à 1M), et **(b)** N allers-retours
`tree_children`.

**Ce qui les a remplacés.** L'expansion se fait par **itération de bitmaps** sur
l'index inverse (enfants directs) — `index::expand_subtrees` — entièrement en
mémoire : pas de texte SQL, pas d'aller-retour par nœud, et le résultat *est*
déjà un bitmap qui s'intersecte nativement avec les autres prédicats. Les pistes
listées à l'époque (`carray`, table TEMP, CTE récursive) sont donc sans objet :
elles corrigeaient une compilation SQL qui n'existe plus.

**Le vrai problème de fond (inchangé).** On **matérialise tout le sous-arbre**
alors qu'on ne veut en général que la **page** demandée (~100 résultats triés).
C'est maintenant une union de bitmaps par niveau plutôt qu'une marche en base —
la constante est petite — mais le coût dépend toujours du dossier et non de la
page. Limite inhérente à garder en tête : dès qu'on **trie par un champ**, il
faut de toute façon l'ensemble complet des candidats pour choisir le top-N — la
linéarité n'est évitable que pour les requêtes **sans tri**.

**Périmé : « le cache ne peut pas servir tel quel ».** L'argument était que la
map `children` d'un nœud était *partielle* (`resolve_path` n'insérait que les
enfants rencontrés) et qu'aucun marqueur ne disait « tous les enfants chargés ».
Les deux ont été levés : `TreeCache::populate` charge **toute** la forêt en un
seul scan au chargement du dépôt, et `is_complete()` est ce marqueur. C'est ce
qui rend l'énumération en mémoire autoritaire — et ce qui a permis à la forêt de
répondre elle-même aux feuilles `:path` / `osm` ordonné (`forest_query.rs`).

**Idée utilisateur : compteur d'enfants dénormalisé.** Stocker en DB le nombre
d'enfants par `(field_name, parent_uuid)` avait deux bénéfices : éviter la
requête `tree_children` pour les feuilles, et permettre au cache de détecter la
complétude. **Les deux sont obsolètes** (plus de `tree_children` sur le chemin
de lecture, complétude connue). Ne corrigeait de toute façon **pas** la
linéarité de fond, et coûtait une maintenance dans `log::Writer` restaurée
exactement par le rollback : à ne pas ressortir sans un nouveau motif.

**Pointeurs code :** `index/mod.rs` (`expand_subtrees`, nœud
`FollowsTransitive`), `tree_cache.rs` (`populate`, `descendants`,
`resolve_path`), `forest_query.rs`.
`docs/spec-query.org` pour la sémantique de `->*`.

## 8. Limites de requête — ✅ les deux bornes / ⏳ leur valeur, et le timeout

**Fait.** Les deux bornes vivent maintenant dans `query_validate.rs` et sont
vérifiées **avant toute évaluation**, donc identiquement pour toute requête :

- `MAX_QUERY_NODES = 2000` — nombre total de nœuds → rejet 400 (« query too
  large … decompose it »).
- `MAX_COMBINATOR_OPERANDS = 500` — opérandes d'un même `And`/`Or` → rejet 400
  (« a single 'and'/'or' may have at most 500 operands… nest or decompose it »).

Tests : unités `node_count` / `check_query_size` / `query_size_check_also_bounds_combinator_width`
dans `query_validate.rs`, plus `tests/query.rs` (`test_oversized_query_is_rejected`,
`test_wide_combinator_is_rejected_with_clear_message`, qui vérifie aussi que 500
pile s'exécute).

**⏳ Reste à faire (différé) :**

- **La valeur des deux plafonds est à redécider.** 500 était
  `SQLITE_MAX_COMPOUND_SELECT` : chaque opérande devenait un terme d'un compound
  `SELECT`. Plus rien ne compile en SQL sur le chemin de service (spec-indexing
  « No operand runs in SQL ») — un `Or` large est N unions de bitmaps — donc la
  borne subsiste comme garde-fou à une valeur héritée d'une contrainte qui n'a
  plus cours. Le chunking du compound, lui, est **sans objet**.
- **Opérateur `In { field, values }` natif.** « Ce champ vaut l'une de ces N
  valeurs » s'écrit encore `Or` de N `Eq` = ~2N nœuds. Un `In` natif serait
  O(1) nœud, et rendrait la borne indolore pour l'appartenance. *Le cas des
  uuids est déjà réglé* : `uuid_in` existe, et le DSL replie tout seul un `Or`
  d'atomes UUID nus en un unique nœud.
- **Timeout d'exécution.** La borne de nœuds ne couvre que le coût de
  *préparation* ; une requête petite mais lente (`matches` sur une forte
  cardinalité, `->*` sur tout le repo — cf. §7) n'est pas bornée en *temps*. Le
  `Connection::interrupt()` évoqué à l'époque ne suffit plus : il n'arrête
  qu'une instruction SQLite, et l'évaluation est maintenant du Rust en mémoire.
  Ce qui existe : l'annulation coopérative des tâches (`spec-tasks`), que
  `run_query_filter` interroge entre les phases. Ce qui manque : une *deadline*
  qui la déclenche toute seule.

## 9. Link metarecords : le concept n'existe plus — ✅ SANS OBJET (sept. 2026)

**Constat d'origine (juin 2026).** Un *link metarecord* aurait été possédé par
**plusieurs** repos (plusieurs lignes `metarecord_db` pour le même
`metarecord_uuid`). Les lectures l'excluaient (le CTE `_repo` de `query_exec`
exigeait `COUNT(*) = 1`), mais aucune écriture n'était « link-aware » :
`delete_metarecord`, `navigate`/`prune` et `set_field` opéraient sur l'entité
entière, sans contrôle de propriété. Le point était classé « à traiter lors de
la conception des links ».

**Ce qui s'est passé.** Cette conception a eu lieu, et elle a *supprimé le
concept*. La sync cross-repo a été entièrement revue : un lien entre deux
metarecords est une ligne de la base de sync **par paire**, hors du modèle de
données (`docs/spec-sync.org` — un lien n'est pas un metarecord, n'apparaît
dans aucune requête, ni dans le journal). La table de propriété et
`MetaRecord.db_ids` sont donc devenus un singleton inutile, et ont été
**supprimés** par `6357e5b` (août 2026) : la propriété est implicite, c'est le
fichier de base qui porte le metarecord, et un second propriétaire n'est plus
représentable. Le CTE `_repo` de l'oracle (`crates/query-oracle`) a été aligné
dans le même commit sur `SELECT uuid FROM metarecord`, exactement l'univers que
construit l'index bitmap.

Il n'y a donc **rien à rendre link-aware** : ni divergence lecture/oracle, ni
invariant d'écriture à rétablir. L'avertissement de septembre 2026 qui figurait
ici (« les lectures ne sont plus link-aware ») décrivait un état déjà disparu
un mois plus tôt ; il est retiré, avec la prose correspondante dans
`index/mod.rs`, `roadmap.org`, `spec-data-model` et `spec-indexing`.

## 10. Le log du mock `mf` perd les frontières d'arguments — ⏳ DIFFÉRÉ (coût > gain)

**Constat.** `scripts/lib/mf-mock.sh` journalise chaque appel comme `sig="$*"`,
c'est-à-dire les arguments joints par des espaces, sans quoting. Idem
`scripts/lib/daemon-fixture.sh:92`. Un glob d'assertion peut donc matcher du
texte *à l'intérieur* d'un argument — typiquement une requête DSL, qui contient
des espaces : `mock_count 'tag -i rec-1 add music'` matcherait un hypothétique
`mf metarecord -q 'tag -i rec-1 add music' get`.

**Pourquoi c'est différé.** Aucun faux positif observé, et les formes en jeu sont
contrivées. Le coût, lui, est réel : le format du log est l'interface de
`mock_count` / `mock_calls_matching`, donc en changer la forme demande de
réécrire la centaine d'assertions des six suites (`test-gui-tag-*.sh`,
`test-example-gui-sort-folder.sh`, `test-scripts-integration.sh`). Fragilité
latente, pas un bug.

**Piste si ça mord un jour.** Garder `$*` comme log lisible (les assertions
existantes ne bougent pas) et écrire *en plus* un log `argv` séparé par un
caractère impossible (`\x1f`), avec un `mock_count_argv` pour les assertions qui
ont besoin de précision. Migration incrémentale, pas de big bang.

## 11. `mf path` par question dans les scripts de tagging — ⏳ DIFFÉRÉ (demande une surface CLI)

**Constat.** `gui-tag-folder.sh` appelle `mf path "$uuid"` pour l'aperçu à chaque
question alors qu'il vient de lire le chemin *relatif* du même metarecord
(`--resolve-tree mfr_path`) ; `gui-tag-pair.sh` fait de même. Il ne manque que la
**racine absolue du dépôt** pour reconstruire le chemin absolu sans aller-retour.
(La refonte de §12 n'y change rien : elle a supprimé la carte `PATH_OF` lue
d'avance, pas le besoin du chemin absolu pour l'aperçu.)

**Pourquoi c'est différé.** Le coût est d'un aller-retour HTTP par *question*,
donc par touche pressée par un humain — invisible à côté du temps de réponse.
(Le vrai coût, lui, était le nombre de *processus* par entrée : traité, cf.
`gui-tag-next.sh`, 31,8 s → 0,19 s.)

**Ce qui manque pour le faire proprement.** Aucune commande CLI n'imprime la
racine d'un dépôt : `mf repo list` rend le JSON brut de `GET /repos`, dont
extraire `root` en bash demanderait `jq` (nouvelle dépendance) ou du sed
fragile. Déduire la racine en soustrayant le chemin relatif d'un `mf path`
absolu est pire. La correction propre est d'ajouter `mf repo root` (ou un
`--root` à `mf repo list`), puis de le lire une fois par run — petite addition
de surface CLI à peser, pas un refactor de script.

## 12. `gui-tag-folder.sh` tenait tout le scope en mémoire — ✅ FAIT (sept. 2026)

**Constat (historique).** `gui-tag-folder.sh` (`collect`) lisait **tout** son
scope avant la première question — sept lectures, puis des tableaux associatifs
bash (`RANK`, `KIND`, `PATH_OF`, `DECIDED`, `SUBTREE`…). Le scope ÉTAIT la
mémoire du run, et `MF_GUI_MAX_ENTRIES` (20000) n'était qu'une borne : un gros
scope échouait bruyamment au lieu d'échouer lentement.

**Ce que disait cette section, et qui était faux.** « Paginer coûterait soit
l'ordre du walk, soit le total exact : les deux exigent l'ensemble complet. »
Non : **la requête est l'état du walk**. Une réponse est déjà une écriture, donc
« ce qu'il reste à demander » est une *requête*, pas une comptabilité. Chaque
étape pose UNE question avec `--limit 1` sur

    <scope> AND <pas encore décidé> AND <pas sauté>

et chaque réponse sort son sujet de cet ensemble — un dossier répondu en bloc y
emmène tout son sous-arbre, puisque le tag est écrit dessus. L'ordre vient du
`--sort` de la requête (donc `mf order` est respecté), et le total vient d'un
`--count`, exact et O(1) sur l'index. On garde les deux.

**Livré.** Le walk est en profondeur d'abord et ne tient qu'une chaîne, le
dossier courant (*le chemin EST la pile*) ; `DECIDED`/`PRUNED`/`SUBTREE`/`RANK`/
`KIND`/`PATH_OF`/`ENTRIES`/`WALK`/`STEPS`, le `sort` externe et la borne de
scope ont disparu. Un saut (`s`) est désormais *enregistré* (champ
`gui_tag_skipped`, un ref vers l'entrée du tag : un marqueur par tag), donc il
survit au run, s'annule par `back` comme toute autre réponse, et est proposé au
nettoyage en fin de run puis au démarrage suivant. Prérequis levés en chemin :
`:parent` servi par l'index, puis `mf … --count`.

Conception : [gui-tag-folder-rework.md](gui-tag-folder-rework.md). Les deux
autres scripts (`gui-tag-classify.sh`, `gui-tag-pair.sh`) lisent toujours leur
scope d'avance — mais ils classent UN metarecord (classify) ou une paire
(pair), pas un sous-arbre : la borne y suffit.
