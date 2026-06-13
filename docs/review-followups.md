# Suivi de revue — points à traiter

Notes issues d'une revue globale du projet (juin 2026). Ce fichier regroupe les
points laissés volontairement de côté pour décision/relecture ultérieure.

## 5. Empoisonnement des mutex en cascade (robustesse)

**Constat.** Tous les handlers du daemon (et du serveur GUI) prennent les
verrous via `.lock().unwrap()` : `repo.conn`, `repo.cache`, `repo.schema`,
`repo.rollback_lock`, etc. Si **un seul** handler panique alors qu'il détient
l'un de ces verrous, le `Mutex` est *empoisonné* (`PoisonError`) et **toutes**
les requêtes suivantes sur ce repository paniquent à leur tour sur le
`.unwrap()`, rendant le repo définitivement inutilisable jusqu'au redémarrage
du daemon.

**Où.** Principalement `crates/daemon/src/routes.rs` (très nombreux
`.lock().unwrap()`), `crates/daemon/src/state.rs`, et le serveur GUI
(`crates/gui/src/server/*`, `crates/gui/src/state/*`).

**Pistes.**
- Récupérer les données même en cas de poison :
  `let guard = mutex.lock().unwrap_or_else(|e| e.into_inner());`
  (acceptable car les données protégées restent cohérentes : tout write passe
  par une transaction SQLite atomique, donc un panic au milieu d'un write a
  déjà *rollback* la transaction — l'état en mémoire du cache d'arbre pourrait
  en revanche être partiellement à jour ; à vider par sécurité).
- Centraliser via une petite extension/`helper` (`fn lock_recover<T>(m) -> MutexGuard`)
  pour ne pas dupliquer le pattern partout.
- Alternative plus lourde : isoler chaque write dans un `catch_unwind` ou
  s'appuyer sur `parking_lot::Mutex` (pas d'empoisonnement par construction).

**Décision attendue.** Choisir entre `unwrap_or_else(into_inner)` généralisé
(simple, peu invasif) ou bascule `parking_lot`. Penser à vider le cache d'arbre
(`cache.clear()`) sur le chemin de récupération, car son état mémoire peut être
incohérent après un panic en cours de mutation.

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
