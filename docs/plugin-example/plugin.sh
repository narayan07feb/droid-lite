#!/bin/sh
# A Droid Lite plugin in plain sh (docs/PLUGINS.md). Droid Lite's own tests run it too.
case "$1" in
  list)
    echo '[{"label":"Login","children":[
      {"id":"ok","label":"passes","file":"flows/login.yaml","line":2},
      {"id":"bad","label":"fails","file":"flows/login.yaml","line":2}]},
      {"id":"solo","label":"solo case","file":"flows/solo.yaml","line":1}]' ;;
  run)
    echo "starting $2"
    if [ "$2" = bad ]; then
      echo '{"label":"step 1","status":"failed","message":"button missing","file":"flows/login.yaml","line":2}'
      exit 1
    fi
    echo '{"label":"step 1","status":"passed"}' ;;
  watch)
    echo '{"title":"Phone idle","rows":[["Phone","fake-1"]]}'
    sleep 30 ;;
  failures)
    echo '[{"label":"F-1","status":"failed","message":"stuck on Home","file":"flows/login.yaml","line":2}]' ;;
  check)
    echo '{"label":"tool","status":"passed","message":"ready"}' ;;
esac
