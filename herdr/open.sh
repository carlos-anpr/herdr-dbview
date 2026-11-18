#!/bin/sh
# Abre dbview como popup sobre el panel activo.
H="${HERDR_BIN_PATH:-herdr}"
exec "$H" plugin pane open --plugin local.dbview --entrypoint viewer --placement popup \
  --width 96% --height 94% --env "HERDR_PLUGIN_CONTEXT_JSON=${HERDR_PLUGIN_CONTEXT_JSON:-}"
