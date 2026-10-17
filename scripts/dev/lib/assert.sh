# shellcheck shell=bash
# Minimal assertion helpers shared by the shipped-script test suites. Sourcing
# this defines `pass`/`fail` counters and three assertions that take the test as
# a COMMAND (so `[` runs as a real command — no fragile `[ … ]; check $?`, which
# ShellCheck flags as SC2319). End a suite with `assert_summary`, whose exit
# status is non-zero iff anything failed.
#
#   assert            <name> <command…>       pass iff the command succeeds
#   assert_eq         <name> <expected> <got> pass iff the two strings are equal
#   assert_contains   <name> <haystack> <needle>  pass iff needle is a substring
#   assert_summary                            print "passed=… failed=…"; exit code

pass=0
fail=0

assert() { # <name> <command...>
    local name=$1; shift
    if "$@"; then
        pass=$((pass + 1)); printf 'ok   %s\n' "$name"
    else
        fail=$((fail + 1)); printf 'FAIL %s\n' "$name"
    fi
}

assert_not() { # <name> <command...>  — pass iff the command FAILS
    local name=$1; shift
    if "$@"; then
        fail=$((fail + 1)); printf 'FAIL %s\n' "$name"
    else
        pass=$((pass + 1)); printf 'ok   %s\n' "$name"
    fi
}

assert_eq() { # <name> <expected> <got>
    if [ "$2" = "$3" ]; then
        pass=$((pass + 1)); printf 'ok   %s\n' "$1"
    else
        fail=$((fail + 1))
        printf 'FAIL %s\n     want=%q\n     got =%q\n' "$1" "$2" "$3"
    fi
}

assert_contains() { # <name> <haystack> <needle>
    case "$2" in
        *"$3"*) pass=$((pass + 1)); printf 'ok   %s\n' "$1" ;;
        *) fail=$((fail + 1)); printf 'FAIL %s\n     needle=%q\n     in    =%q\n' "$1" "$3" "$2" ;;
    esac
}

assert_summary() {
    echo "----"
    echo "passed=$pass failed=$fail"
    [ "$fail" -eq 0 ]
}
