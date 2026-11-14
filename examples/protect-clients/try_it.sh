#!/usr/bin/env bash
# End-to-end check on Linux/macOS: starts a throwaway Protect engine, then runs the Python and
# Node.js example apps against it, accepting each pairing and adding a folder through the
# protect-engine command line (standing in for the Protect window). Prints PASS or FAIL.
# Usage: examples/protect-clients/try_it.sh [path-to-protect-engine]
set -u
here="$(cd "$(dirname "$0")" && pwd)"
bin="${1:-$here/../../target/debug/protect-engine}"
work="$(mktemp -d)"
data="$work/data"; folder="$work/project"; mkdir -p "$data" "$folder"
echo "hello" > "$folder/notes.txt"
ep="$data/engine.sock"
pe() { "$bin" --data-dir "$data" --endpoint "$ep" "$@"; }

pe serve > "$work/engine.log" 2>&1 &
engine=$!
trap 'kill $engine 2>/dev/null; rm -rf "$work"' EXIT
for _ in $(seq 50); do [ -S "$ep" ] && break; sleep 0.1; done

accept_and_grant() { # $1 = app id
  for _ in $(seq 100); do
    id="$(pe pairings | python3 -c "import json,sys; r=[p['pairing_id'] for p in json.load(sys.stdin)['requests'] if p['app_id']=='$1' and p['state']=='pending']; print(r[0] if r else '')" 2>/dev/null)"
    [ -n "$id" ] && break; sleep 0.1
  done
  pe pairings accept "$id" > /dev/null && pe folders add "$1" "$folder" > /dev/null
}

fail=0
run() { # $1 = label, $2 = app id, rest = command
  label="$1"; app="$2"; shift 2
  "$@" --app-id "$app" --name "$label example" --endpoint "$ep" --key-file "$work/$app.key" --wait 20 > "$work/$app.out" 2>&1 &
  client=$!
  accept_and_grant "$app"
  wait $client; code=$?
  cat "$work/$app.out" | sed "s/^/  [$label] /"
  if [ $code -eq 0 ] && grep -q '"decision": *"allow"' "$work/$app.out" && grep -q '"executed": *false' "$work/$app.out" && grep -q 'action.not_executed' "$work/$app.out"; then
    echo "PASS $label"
  else
    echo "FAIL $label (exit $code)"; fail=1
  fi
}

run Python com.example.python python3 "$here/python/protect_client.py"
run Node com.example.node node "$here/node/protect_client.mjs"
# A second run reuses the saved key: it must sign in without asking to pair again.
python3 "$here/python/protect_client.py" --app-id com.example.python --name "Python example" --endpoint "$ep" --key-file "$work/com.example.python.key" --wait 5 > "$work/again.out" 2>&1
if [ $? -eq 0 ] && ! grep -q "Asked to pair" "$work/again.out"; then echo "PASS Python second run (no re-pairing)"; else echo "FAIL Python second run"; cat "$work/again.out"; fail=1; fi
echo "Paired apps now:"; pe pairings | python3 -c "import json,sys; [print('  ', a['app_id'], a['scopes']) for a in json.load(sys.stdin)['paired_apps']]"
exit $fail
