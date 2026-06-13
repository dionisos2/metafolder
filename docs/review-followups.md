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

## 6. `enqueue_restoration` appelée aussi en direction `Forward`

**Constat.** Dans `crates/daemon/src/log.rs`, `coordinated_step()` appelle
`enqueue_restoration(&tx, &op)` dès que `skip == true`, **quelle que soit la
direction** (`NavDir::Inverse` *ou* `NavDir::Forward`).

La sémantique « skip » de spec-event-log ("skip") est définie pour le **rollback**
(direction inverse) : ne pas défaire l'état du système de fichiers, et donc
ré-enregistrer après coup la métadonnée correspondant à l'état réel du disque.
En direction *forward* (redo vers un descendant), enfiler une opération de
restauration dérivée du snapshot `is_new = 1` n'a pas de sens établi par la spec
et pourrait produire une métadonnée incohérente.

**Où.** `crates/daemon/src/log.rs`, fonction `coordinated_step` (appel
`if skip { enqueue_restoration(&tx, &op)?; }` avant le `match dir { ... }`).

**Pistes.**
- Restreindre l'enfilage au cas inverse :
  `if skip && dir == NavDir::Inverse { enqueue_restoration(&tx, &op)?; }`.
- Ou bien définir explicitement la sémantique de `skip` en redo dans
  spec-event-log si on veut la conserver.
- Ajouter un test (TDD) : rollback coordonné qui traverse une LCA (donc une
  phase forward) avec `skip = true`, et vérifier qu'aucune opération de
  restauration parasite n'est rejouée pour les pas forward.

**Décision attendue.** Confirmer que `skip` ne concerne que la direction inverse
(quasi certain), puis ajouter la garde + le test.
