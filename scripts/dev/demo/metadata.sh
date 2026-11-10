#!/usr/bin/env bash
# The metadata of the demonstration repository (doc "The demonstration
# repository"): the tags, ratings and comments that "Camille Martin", the
# persona of the demonstration home, would have given her files. Each step is
# one label of the log, named as she would name it, so that the log panel reads
# as a story and any step can be undone in a demonstration.
#
#   scripts/dev/demo/metadata.sh
#
# It talks to whatever daemon and repository `mf` reaches: run by
# scripts/dev/demo.sh against a throwaway daemon, or by hand in the
# demonstration account against its own (METAFOLDER_REPO_NAME names the
# repository, METAFOLDER_DAEMON_PORT the daemon). The repository must already
# hold the home's files (watched and reconciled).
#
# Every step must find what it names, or the script stops: a demonstration home
# reorganised under it fails loudly instead of leaving a tag empty.
set -euo pipefail

MF=${MF:-mf}

die() { echo "demo metadata: $*" >&2; exit 1; }

# step <name> — what follows is written under a label of that name. Each step
# is a label of its own, not one nested in the previous step's.
step() {
    unset METAFOLDER_LABEL
    METAFOLDER_LABEL=$("$MF" label begin "$1") || die "cannot open the label $1"
    export METAFOLDER_LABEL
}

# files <folder> [<terms>] — the DSL query of the files under a folder, narrowed
# to the paths holding the terms in order (ordered substring matching).
files() {
    local query="mfr_path ->* \"$1\" AND mfr_type = \"file\""
    [ -n "${2:-}" ] && query+=" AND osm(mfr_path, \"$2\")"
    printf '%s' "$query"
}

# matching <what> <query> — the query selects something. Counted before the
# write, not by it: a write that changes nothing (a tag already given by an
# earlier line) counts no record, though the line did find its files.
matching() {
    local n
    n=$("$MF" metarecord -q "$2" get --count) || die "cannot count $1"
    [ "$n" -gt 0 ] || die "nothing found for $1"
}

# tag <add|deny> <tag> <folder> [<terms>]
tag() {
    local query
    query=$(files "$3" "${4:-}")
    matching "$2 in $3 ${4:-}" "$query"
    "$MF" tag -q "$query" "$1" "$2" >/dev/null
}

# field <spec> <folder> <terms>
field() {
    local query
    query=$(files "$2" "$3")
    matching "$1 in $2 $3" "$query"
    "$MF" metarecord -q "$query" field set "$1" >/dev/null
}

step "Classer les photos de voyage"
tag add lieu/europe/islande "/Images/Voyages/2023 Islande"
tag add lieu/europe/france/bretagne "/Images/Voyages/2024 Bretagne"
tag add lieu/asie/japon/kyoto "/Images/Voyages/2025 Japon/Kyoto"
tag add lieu/asie/japon/tokyo "/Images/Voyages/2025 Japon/Tokyo"

step "Repérer les animaux"
tag add nature/animal/oiseau /Images/Animaux/Oiseaux
tag add nature/animal/oiseau "/Images/Voyages/2023 Islande" puffin
tag add nature/animal/oiseau /Téléchargements macareux
tag add nature/animal/mammifère/chat /Images/Animaux/Chats
tag add nature/animal/mammifère/phoque "/Images/Voyages/2023 Islande" seal
tag add nature/animal/mammifère/baleine "/Images/Voyages/2023 Islande" whale
tag add nature/volcan "/Images/Voyages/2023 Islande" volcano

step "Trier les fonds d'écran"
tag add nature/paysage "/Images/Fonds d'écran" mountain
tag add nature/paysage "/Images/Fonds d'écran" landscape
tag add nature/paysage "/Images/Fonds d'écran" karst
tag add nature/paysage "/Images/Voyages/2023 Islande" Elliðaey
# An answer, not an absence: the wallpapers were looked at, and show no animal.
tag deny nature/animal "/Images/Fonds d'écran"

step "Classer les peintures"
tag add art/peinture/impressionnisme /Images/Art/Peintures Monet
tag add art/peinture/impressionnisme /Images/Art/Peintures Renoir
tag add art/peinture/impressionnisme /Images/Art/Peintures Bazille
tag add art/peinture/post-impressionnisme /Images/Art/Peintures "van Gogh"
tag add art/peinture/post-impressionnisme "/Images/Fonds d'écran" nuit-etoilee

step "Ranger la musique"
tag add musique/baroque "/Musique/Classique/Johann Sebastian Bach"
tag add musique/baroque "/Musique/_à trier" Bach
tag add musique/classique "/Musique/Classique/Wolfgang Amadeus Mozart"
tag add musique/classique "/Musique/Classique/Ludwig van Beethoven"
tag add musique/classique "/Musique/_à trier" Beeth
tag add musique/romantique "/Musique/Classique/Frédéric Chopin"
tag add musique/romantique "/Musique/_à trier" Chopin

step "Classer les vidéos"
tag add film/animation "/Vidéos/Films libres" "Big Buck Bunny"
tag add film/animation "/Vidéos/Films libres" Sintel
tag add film/animation "/Vidéos/Films libres" "Elephants Dream"
tag add film/science-fiction "/Vidéos/Films libres" "Elephants Dream"
tag add film/science-fiction "/Vidéos/Films libres" "Tears of Steel"
tag add espace /Vidéos/Espace
tag add espace /Images/Astronomie
tag add nature/ciel /Vidéos/Nature

step "Ranger les papiers"
tag add document/facture /Documents/Administratif/Factures
tag add document/administratif /Documents/Administratif/Impôts
tag add document/administratif /Documents/Administratif/Logement
tag add document/manuel /Documents/Manuels
tag add livre/roman "/Documents/Livres/Domaine public" Dumas
tag add livre/roman "/Documents/Livres/Domaine public" Verne
tag add livre/roman "/Documents/Livres/Domaine public" Hugo
tag add livre/roman "/Documents/Livres/Domaine public" Candide
tag add livre/théâtre "/Documents/Livres/Domaine public" Avare

step "Noter les favoris"
field rating:int=5 "/Images/Voyages/2023 Islande" puffin
field rating:int=5 /Images/Art/Peintures "Starry Night"
field rating:int=5 "/Vidéos/Films libres" "Big Buck Bunny"
field rating:int=5 "/Musique/Classique/Johann Sebastian Bach" Aria
field rating:int=5 "/Images/Voyages/2025 Japon/Kyoto" Togetsu-kyo
field rating:int=4 "/Images/Voyages/2023 Islande" 029a
field rating:int=4 "/Vidéos/Films libres" Sintel
field rating:int=4 "/Images/Voyages/2025 Japon/Kyoto" "Kyoto Tower seen"
field rating:int=4 /Musique/Classique/Frédéric\ Chopin "Scherzo n°1"
field rating:int=4 /Images/Art/Peintures "Almond blossom"
field rating:int=2 "/Vidéos/Films libres" "Elephants Dream"
field rating:int=2 "/Images/Voyages/2025 Japon/Tokyo" "Minato City"

step "Annoter quelques fichiers"
field "comment:string=Enfin un macareux en vol !" "/Images/Voyages/2023 Islande" puffin
field "comment:string=La plus nette des trois : garder celle-ci" "/Images/Voyages/2023 Islande" 029a
field "comment:string=Pour le prochain fond d'écran ?" "/Images/Voyages/2025 Japon/Kyoto" Togetsu-kyo
field "comment:string=À revoir avec Léa" "/Vidéos/Films libres" Sintel
field "comment:string=Payé le 15 septembre" /Documents/Administratif/Impôts avis
