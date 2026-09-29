# Proposition : une documentation en tiddlers

Document de travail. Il remplace, à terme, les `docs/spec-*.org`, les pages du
panel `help` (`panel-types/help/pages/*.html`) et la prose de `mf --help`
au-delà des options. Le **contenu** du wiki sera en anglais, comme les specs
actuelles ; ce document-ci est en français parce qu'il sert à en discuter.

## 0. Principes retenus

1. **Une source, plusieurs sorties.** Le wiki (`docs/wiki/`) est la seule
   source. Son rendu est installé dans `~/.config/metafolder/docs/`, hors de tout
   panel, où le panel `help` le lit ; un site web ou un autre lecteur pourra le
   lire au même endroit.
2. **Tout est embarqué.** Le panel et le site contiennent aussi la
   documentation développeur. Elle est seulement masquée par défaut dans la
   recherche (une case « inclure la documentation développeur »).
3. **Une note = un point.** Un sujet se découpe en *vue d'ensemble*, *guide*,
   *référence* et *choix de conception* quand ce découpage a du sens, pas
   systématiquement.
4. **Les références mécaniques sont générées depuis le code** (options CLI,
   commandes GUI, raccourcis, scripts, routes HTTP). Un contrôle vérifie que
   chaque élément du code a sa note.
5. **Des fichiers proches du texte brut.** Les `.tid` utilisent un sous-ensemble
   restreint du wikitext. Leur rendu produit des fragments HTML simples, lus par
   un panel `help` qui reste petit et stable : il ne connaît ni TiddlyWiki ni
   les filtres.
6. **Navigable en ligne de commande** grâce à un petit outil (`scripts/doc`),
   pour toi comme pour moi.

## 1. Arborescence

```
docs/wiki/
  package.json            # tiddlywiki épinglé (devDependency), + lockfile
  tiddlywiki.info         # aucun plugin requis au départ
  tiddlers/               # notes écrites à la main, un fichier par note, à plat
  generated/              # notes produites depuis le code (commitées)
  system/                 # $:/mf/… : macros, templates de rendu, config
```

- **À plat** dans `tiddlers/`. Pas de sous-dossiers par thème : ils
  recréeraient la question « où va ce point transversal ? ». Les tags jouent ce
  rôle. Quelques centaines de fichiers à plat se parcourent sans problème avec
  grep et l'outil.
- **Nom de fichier = slug du titre** (minuscules, tout caractère non
  alphanumérique → `-`, tirets fusionnés). Par exemple `mf trash restore` →
  `mf-trash-restore.tid`. Le slug sert aussi d'identifiant de page dans le panel.
  `scripts/doc check` vérifie la correspondance et l'unicité.
- **`generated/` est commité** : je peux le lire sans build, et un changement
  de CLI se voit dans le diff de la doc. `check.sh` vérifie qu'il est à jour,
  comme pour `fmt`.
- **Le rendu n'est pas commité** (décidé). C'est un artefact de build, comme
  `frontend/dist` : `scripts/doc build` l'écrit dans `docs/wiki/dist/`
  (gitignoré), et `complete-build.sh` le lance avant d'appliquer la config.
- **Il n'appartient à aucun panel** (décidé). `metafolder-sync-config` le
  transfère, comme il transfère déjà `scripts/shipped/` → `scripts/` :

  ```
  docs/wiki/dist/index.json      -> ~/.config/metafolder/docs/index.json
  docs/wiki/dist/<slug>.html     -> ~/.config/metafolder/docs/<slug>.html
  ```

  Le serveur de la GUI le sert sous `/docs/…` (même jeton que le reste), et le
  panel `help` y lit ses pages au lieu de `panel/help/pages/`. D'autres lecteurs
  pourront s'en servir au même endroit (un site, une commande CLI plus tard).

## 2. Format d'une note

```
title: mf trash restore
tags: Trash [[CLI command]]
kind: reference
audience: user
summary: Put a trashed entry back at its original path and re-link its metarecord.
aliases: trash:restore

Restores the entry …

See [[Trash — why the metarecord goes with the file]].
```

| Champ      | Valeurs                                                     | Rôle |
|------------|-------------------------------------------------------------|------|
| `title`    | libre, unique, stable                                       | identité ; ce que le code cite |
| `tags`     | sujets + catalogues (§4)                                    | classement, listes, tables des matières |
| `kind`     | `overview` `guide` `reference` `rationale` `question`       | *un seul* par note, d'où un champ plutôt qu'un tag |
| `audience` | `user` (défaut) ou `dev`                                    | filtre par défaut de la recherche |
| `status`   | `implemented` (défaut), `deferred`, `proposed`, `record`    | remplace le tag org `:deferred:` ; `record` = design doc conservé (spec-indexing, spec-query-cost) |
| `summary`  | une phrase                                                  | listes, résultats de recherche, infobulles |
| `aliases`  | liste de titres TW                                          | résolution exacte du panel (`help trash:restore`) |
| `caption`  | court (optionnel)                                           | libellé dans les tables des matières |

Les champs `created`/`modified` de TiddlyWiki sont omis : git s'en charge.
`kind: question` reprend les sections « Open questions » des specs, une
question par note.

## 3. Titres

Des titres en anglais naturel, avec des conventions **uniquement** pour les
catalogues énumérables, afin qu'un titre se devine :

| Chose                    | Titre                              |
|--------------------------|------------------------------------|
| sujet (note d'ensemble)  | `Trash`, `Event log`, `Watcher`    |
| commande CLI             | `mf trash restore` (la commande littérale) |
| commande GUI             | `trash:restore`                    |
| panel                    | `trash panel`                      |
| script livré             | `gui-tag-folder.sh`                |
| route HTTP               | `POST /repos/:repo/query`          |
| champ réservé            | `mfr_path`, `mf_watch`             |
| fichier de config        | `gui/keybindings.toml`             |

Pour les sous-notes d'un sujet, le titre dit ce qu'on y trouve : `Trash on-disk
layout`, `Why the trash owns the metarecord`. Pas de hiérarchie `Trash/Layout` :
le lien avec le sujet passe par le tag. Un titre peut changer, parce que
`scripts/doc rename` met à jour les liens, les tags et les références du code.

## 4. Tags

Deux familles. Chaque tag a sa propre note, qui sert de *hub*.

**Sujets.** Ils sont nombreux par note et peuvent être hiérarchiques (une note
de sujet taguée par un sujet parent, ce qui est natif dans TiddlyWiki et exploité
par les macros `toc`) :

- Data model → Values, TreeRef forest, Version, Reserved fields
- Query → DSL, Simplified query, Field aspects, Pagination
- File tracking → Watcher, Eligibility, Reconcile, Mount points, Orphans, Fingerprints
- Event log → Undo and redo, Rollback, Pruning
- Schema · Trash · Sync · Duplicates · Tasks · Slow log · Embedded metadata
- Storage → Backups, Indexing
- Configuration → Keybindings, Style
- Security → Auth, Sandbox
- GUI → Workspaces, Panels, Command input, Find, Input history, Scripting API
- Performance · Platform

**Catalogues.** Chacun correspond à un seul type d'objet et garantit des listes
complètes : `CLI command`, `GUI command`, `Panel`, `Shipped script`,
`HTTP endpoint`, `Reserved field`, `Config file`, `Invariant`, `Value type`.
Les « Key invariants » de CLAUDE.md deviennent le catalogue `Invariant`, une
note par invariant.

Une note de hub ressemble à ceci :

```
title: Trash
tags: [[File tracking]]
kind: overview
summary: A per-repository bin: trashing a file keeps its bytes and its metarecord.

<<summary-of>>            ← two sentences written by hand

!! Use it
<<list-kind guide>>
!! Reference
<<list-kind reference>>
!! Why it works this way
<<list-kind rationale>>
```

`<<list-kind K>>` liste les notes taguées avec le sujet courant et de ce
`kind`, et `<<topic-full>>` les transclut toutes dans l'ordre (champ `list` du
hub). On retrouve ainsi une lecture linéaire complète, à la manière d'une
spec, sans qu'elle existe comme fichier.

## 5. Granularité

- Une note répond à *une* question qu'on chercherait. Viser 20 à 150 lignes.
- Si un `!!` intérieur mériterait d'être la cible d'un lien, il devient une note.
- On ne découpe par `kind` que s'il y a matière : une commande simple tient en
  une seule note `reference`.
- Une note de hub reste courte : définition, puis listes.

## 6. Sous-ensemble de wikitext autorisé

Titres (`!`), paragraphes, listes, tableaux, blocs de code, code en ligne,
gras et italique, `[[liens]]`, transclusion `{{…}}`, et les procédures du
projet ci-dessous. Ni widgets ni HTML brut, à l'exception d'une liste blanche
vérifiée par `scripts/doc check`. Le fichier se lit donc presque comme du texte.

Procédures du projet (`system/macros.tid`) :

| Macro                          | Rendu HTML                                         |
|--------------------------------|----------------------------------------------------|
| `<<key "trash:restore" Enter>>` | `<span data-mf-key="trash:restore">Enter</span>` (rempli en direct par le panel, spec-gui « Key hints ») |
| `<<cmd "trash:restore">>`      | lien en `<code>` vers la note de la commande       |
| `<<live grammar>>`             | `<div data-mf-live="grammar"></div>` (la grammaire actuelle, injectée par le panel) |
| `<<list-kind K>>`, `<<topic-full>>`, `<<catalog "CLI command">>` | listes/transclusions **déjà développées** au rendu |

Les liens sont rendus en `<a data-help-page="<slug>">` (variable
`tv-wikilink-template`), ce qui correspond exactement à ce que les pages
actuelles utilisent.

## 7. Références générées depuis le code

Chaque élément du code produit une note **de données** sous `$:/mf/gen/…`, dans
`generated/`. La note écrite à la main, avec le titre public, porte la prose, les
tags et les liens. Un *view template* ajoute automatiquement la partie générée
à toute note d'un catalogue. On n'écrit donc jamais les options à la main, et
la note écrite à la main n'a rien à transclure explicitement.

| Catalogue        | Source                                             | Extraction |
|------------------|----------------------------------------------------|------------|
| `CLI command`    | l'arbre clap (`CommandFactory`)                    | un test/binaire du crate cli écrit usage, arguments, doc-comments |
| `GUI command`    | registre Rust (builtins) + `commands.register('x', {label})` des panels | Rust pour les builtins ; pour les panels, une regex sur `commands.register('…'` |
| raccourcis       | `gui/default-config/keybindings.toml`              | rattachés à la note générée de chaque commande |
| `Shipped script` | en-tête `# Summary:` + bloc d'usage                | shell |
| `HTTP endpoint`  | les chemins du routeur (`routes/mod.rs`)           | extraction statique ou test |
| `Reserved field` | `reserved.rs`                                      | test |

`scripts/doc check` vérifie, dans les deux sens, qu'il existe exactement une note
écrite à la main par élément généré et aucune note de catalogue sans élément
correspondant. Il intègre ainsi l'actuelle règle « au moins une page par type de
panel ». La comparaison porte sur deux ensembles de titres : celui des notes
générées, et celui des notes taguées avec le catalogue (`[tag[GUI command]]`).
Elle ne demande aucune analyse du texte des notes.

**Une note par commande GUI** (décidé), même triviale. Certaines méritent une
vraie page, les listes deviennent de simples filtres par tag, et la complétude
se vérifie par la comparaison ci-dessus. La partie générée d'une commande
contient son libellé, ses arguments et son raccourci par défaut (rendu par
`<<key>>`, donc la touche réelle apparaît dans le panel). La note d'une commande
triviale se réduit à ses champs et à son `summary`. `doc new --from-gen` crée ces
notes minimales à la demande. `doc gen` ne les crée jamais tout seul, sinon le
contrôle de complétude ne vérifierait plus rien.

`mf --help` garde ses doc-comments clap, qui sont la source des options. Pas de
`mf doc` pour l'instant (décidé).

## 8. Le panel `help`

**Build** (`scripts/doc build`, appelé par `complete-build.sh`) :

```
tiddlywiki docs/wiki --render '[!is[system]]' '[slugify[]addsuffix[.html]]' \
    text/plain '$:/mf/templates/help-page'  # → docs/wiki/dist/
# + un template qui produit dist/index.json
```

`index.json` étend la forme actuelle `{id, title, file, aliases[]}` (changement
additif) avec `tags`, `kind`, `audience`, `status` et `summary`.

**Panel** : il garde sa logique actuelle (chargement du manifeste, grep,
résolution exacte, key hints), à laquelle s'ajoutent :
- la case « inclure la doc développeur » (filtre sur `audience`) ;
- les tags de la page, affichés en pastilles cliquables qui mènent au hub ;
- un filtre par `kind` dans les résultats (optionnel).

Le panel n'évalue jamais de filtre TiddlyWiki : toutes les listes sont
développées au build. Il continue de lire des fichiers HTML simples.

**Site** : même wiki, via `--build` d'une édition statique ou un TiddlyWiki
autonome avec sa recherche. À faire plus tard, sans rien changer à la source.

## 9. Outil `scripts/doc`

Script node (node est déjà requis pour le frontend). Il lit directement les
`.tid`, dont l'en-tête est trivial à analyser, sans lancer TiddlyWiki, et
répond donc instantanément.

```
doc ls   [--tag T]… [--kind K] [--audience A] [--status S]   # titre + summary
doc show TITLE           # résout titre ou alias → fichier, l'affiche
doc find TEXT            # grep, résultats par note
doc links TITLE          # liens sortants et entrants (backlinks)
doc refs TITLE           # où le code cite cette note
doc topic TITLE          # tout un sujet concaténé, dans l'ordre du hub
doc new TITLE --tags … --kind …   # crée le fichier au bon slug
doc rename OLD NEW       # déplace le fichier, met à jour liens, tags et refs du code
doc check                # voir ci-dessous (branché dans check.sh)
doc gen                  # régénère generated/
doc build                # rendu vers le panel help
```

`doc check` vérifie :
- la correspondance slug/fichier et l'unicité ;
- les champs requis et leurs valeurs autorisées ;
- l'absence de liens et de transclusions cassés, et d'alias en double ;
- que chaque note est atteignable depuis un hub (pas d'orphelin) ;
- le respect du sous-ensemble de wikitext ;
- la complétude des catalogues (§7) et la fraîcheur de `generated/` ;
- que chaque `doc "…"` cité dans le code existe.

## 10. Références depuis le code

Les 821 citations `spec-x "Section"` deviennent `doc "Titre"`. Chaque
migration de sujet produit sa table de correspondance, et un script réécrit
les citations. Les deux formes coexistent pendant la migration ; `doc check` ne
valide que la nouvelle.

## 11. Migration

0. **Infrastructure** : squelette du wiki, macros, templates, `scripts/doc`,
   build vers le panel, panel adapté (case audience, pastilles de tags),
   générateurs du §7, route `/docs/…` et transfert par `sync-config`. Les 27
   pages HTML actuelles restent servies tant que leur sujet n'est pas migré. Le
   panel fusionne les deux manifestes pendant la transition.
1. **Pilote : la corbeille.** Sources : `spec-trash.org`, `help/pages/trash.html`
   et `mf trash --help`. On vérifie le contenu contre le code au passage. C'est
   là qu'on juge la granularité et les conventions, avant de les figer.
2. **Un sujet par commit.** Chaque commit supprime le fichier spec et la page
   help d'origine une fois tout repris, et réécrit les citations du code. Les
   décisions de conception du sujet qui ne vivent que dans mes fichiers de
   mémoire deviennent des notes `rationale` `audience: dev` (décidé). Le
   fichier de mémoire est alors réduit à ce qui ne relève pas de la doc (pièges
   d'environnement, méthode de débogage), ou supprimé s'il ne reste rien.
   On commence par les petits sujets autonomes (duplicates, slow log, auth,
   config, tasks, trash, sync), et on finit par les gros sujets transversaux
   (data model, query, file tracking, event log, puis spec-gui, qui se découpe
   naturellement par panel).
3. **Fin** : mise à jour de CLAUDE.md (« Specs and roadmap », conventions
   `:deferred:`, règle « when deviating, update the spec » → « update the
   doc »). `roadmap.org` reste à part : c'est un plan, pas de la doc.

## 12. Conflits avec l'existant (à acter)

- **CLAUDE.md « Specs and roadmap »** : liste les specs comme référence
  normative de l'implémentation. Pendant la transition, la référence est la
  spec *ou* la note migrée. À terme, seulement le wiki.
- **Convention `:deferred:` + nommé** (mémoire « tags de version retirés ») :
  elle devient `status: deferred`, et la règle « toujours nommer la chose
  reportée » est conservée.
- **spec-gui « Help »** : la forme de `index.json` s'étend (changement additif,
  sans bump d'`API_VERSION` : ce n'est pas le protocole daemon), et les pages
  deviennent un artefact de build au lieu de sources.
- **spec-config « On-disk layout »** : le dépôt de config est « un
  sous-dossier par crate », plus une catégorie transverse, `scripts/`. `docs/`
  en devient une deuxième, et c'est la première dont la source est un produit
  de build. Contrairement à `scripts/shipped/` (optionnel, « a checkout may
  lack it »), `docs/wiki/dist/` absent doit faire **échouer** `sync-config`.
  Sinon, un `sync-config` lancé seul sur un checkout neuf retirerait `docs/` de
  la branche `default` et laisserait un panel `help` vide.
- **La GUI ne lit que `panel-types/`, les styles et les keybindings** dans la
  config. Elle gagne une route `/docs/…` sur le répertoire de config : fichier
  manquant = 404, sans solution de repli, comme partout (spec-config « No
  runtime fallback »).

## 13. Décisions prises

1. Le rendu HTML n'est pas commité ; `sync-config` échoue s'il manque.
2. Pas de `mf doc` pour l'instant. Le rendu vit hors du panel, dans
   `~/.config/metafolder/docs/`, pour pouvoir être lu depuis plusieurs endroits.
3. Les décisions de conception conservées dans la mémoire migrent en notes
   `rationale` `audience: dev`, sujet par sujet.
4. Une note par commande GUI.

## 14. Questions ouvertes

Aucune pour l'instant ; le pilote (§11, étape 1) en fera sans doute apparaître.
