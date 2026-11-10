# shellcheck shell=bash
# The demonstration repository (doc "The demonstration repository"), built over
# the demonstration home against the daemon of scripts/dev/lib/daemon-fixture.sh,
# which must be started (df_start) before.
#
#   demo_build_repo <source> <database dir>
#       a repository named $DEMO_NAME over <source>, its database outside it
#       (--metafolder: the home is read only, and stays untouched), the files
#       watched and reconciled, then scripts/dev/demo/metadata.sh applied.
#
# The steps write under labels of their own: the log the demonstration shows
# names "Track the home folder", never the throwaway daemon's command line.

DEMO_NAME=camille
_DEMO_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

# demo_mf <args…> — the fixture's mf, the port and the repository given by the
# environment rather than on the command line, which a label would record.
demo_mf() {
    METAFOLDER_DAEMON_PORT=$DF_PORT METAFOLDER_REPO_NAME=$DEMO_NAME "$_DF_MF" "$@"
}

demo_build_repo() {
    local source=$1 db=$2 label
    demo_mf repo init --name "$DEMO_NAME" --metafolder "$db" "$source" >/dev/null || return 1
    label=$(demo_mf label begin "Track the home folder") || return 1
    METAFOLDER_LABEL=$label demo_mf metarecord -q 'mfr_path = ""' field set mf_watch:bool=true \
        >/dev/null || return 1
    METAFOLDER_LABEL=$label demo_mf reconcile >/dev/null || return 1
    MF=$_DF_MF METAFOLDER_DAEMON_PORT=$DF_PORT METAFOLDER_REPO_NAME=$DEMO_NAME \
        bash "$_DEMO_DIR/metadata.sh"
}
