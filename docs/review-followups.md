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
