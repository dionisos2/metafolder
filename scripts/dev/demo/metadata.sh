#!/usr/bin/env bash
# The metadata of the demonstration repository (doc "The demonstration
# repository"): the tags, ratings and comments that "Camille Martin", the
# persona of the demonstration home, would have given her files. Each step is
# one label of the log, named as she would name it, so that the log panel reads
# as a story and any step can be undone in a demonstration. In English, like the
# README the screenshots illustrate; the home's own file names stay as they are.
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

step "Sort the travel photos"
tag add place/europe/iceland "/Images/Voyages/2023 Islande"
tag add place/europe/france/brittany "/Images/Voyages/2024 Bretagne"
tag add place/asia/japan/kyoto "/Images/Voyages/2025 Japon/Kyoto"
tag add place/asia/japan/tokyo "/Images/Voyages/2025 Japon/Tokyo"

step "Spot the animals"
tag add nature/animal/bird /Images/Animaux/Oiseaux
tag add nature/animal/bird "/Images/Voyages/2023 Islande" puffin
tag add nature/animal/bird /Téléchargements macareux
tag add nature/animal/mammal/cat /Images/Animaux/Chats
tag add nature/animal/mammal/seal "/Images/Voyages/2023 Islande" seal
tag add nature/animal/mammal/whale "/Images/Voyages/2023 Islande" whale
tag add nature/volcano "/Images/Voyages/2023 Islande" volcano

step "Sort the wallpapers"
tag add nature/landscape "/Images/Fonds d'écran" mountain
tag add nature/landscape "/Images/Fonds d'écran" landscape
tag add nature/landscape "/Images/Fonds d'écran" karst
tag add nature/landscape "/Images/Voyages/2023 Islande" Elliðaey
# An answer, not an absence: the wallpapers were looked at, and show no animal.
tag deny nature/animal "/Images/Fonds d'écran"

step "Sort the paintings"
tag add art/painting/impressionism /Images/Art/Peintures Monet
tag add art/painting/impressionism /Images/Art/Peintures Renoir
tag add art/painting/impressionism /Images/Art/Peintures Bazille
tag add art/painting/post-impressionism /Images/Art/Peintures "van Gogh"
tag add art/painting/post-impressionism "/Images/Fonds d'écran" nuit-etoilee

step "Sort the music"
tag add music/baroque "/Musique/Classique/Johann Sebastian Bach"
tag add music/baroque "/Musique/_à trier" Bach
tag add music/classical "/Musique/Classique/Wolfgang Amadeus Mozart"
tag add music/classical "/Musique/Classique/Ludwig van Beethoven"
tag add music/classical "/Musique/_à trier" Beeth
tag add music/romantic "/Musique/Classique/Frédéric Chopin"
tag add music/romantic "/Musique/_à trier" Chopin

step "Sort the videos"
tag add film/animation "/Vidéos/Films libres" "Big Buck Bunny"
tag add film/animation "/Vidéos/Films libres" Sintel
tag add film/animation "/Vidéos/Films libres" "Elephants Dream"
tag add film/science-fiction "/Vidéos/Films libres" "Elephants Dream"
tag add film/science-fiction "/Vidéos/Films libres" "Tears of Steel"
tag add space /Vidéos/Espace
tag add space /Images/Astronomie
tag add nature/sky /Vidéos/Nature

step "File the paperwork"
tag add document/invoice /Documents/Administratif/Factures
tag add document/administrative /Documents/Administratif/Impôts
tag add document/administrative /Documents/Administratif/Logement
tag add document/manuel /Documents/Manuels
tag add book/novel "/Documents/Livres/Domaine public" Dumas
tag add book/novel "/Documents/Livres/Domaine public" Verne
tag add book/novel "/Documents/Livres/Domaine public" Hugo
tag add book/novel "/Documents/Livres/Domaine public" Candide
tag add book/play "/Documents/Livres/Domaine public" Avare

step "Rate the favourites"
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

step "Comment on a few files"
field "comment:string=A puffin in flight, at last!" "/Images/Voyages/2023 Islande" puffin
field "comment:string=The sharpest of the three: keep this one" "/Images/Voyages/2023 Islande" 029a
field "comment:string=Next wallpaper?" "/Images/Voyages/2025 Japon/Kyoto" Togetsu-kyo
field "comment:string=Watch again with Léa" "/Vidéos/Films libres" Sintel
field "comment:string=Paid on 15 September" /Documents/Administratif/Impôts avis
