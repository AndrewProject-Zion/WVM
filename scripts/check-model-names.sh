#!/usr/bin/env bash
# Check which model names the DeepSeek API actually serves, and what Hermes is configured for.
#
# Written after a model-name alias trap: `deepseek-v4-flash` is NOT served (the real id is
# `deepseek-flash`), so every attempt to switch to it was auto-corrected for that session only and
# appeared to "revert". The config was fine; the requested name was wrong.

set -uo pipefail

echo "=== what Hermes is configured to use ==="
hermes config get model.default 2>/dev/null | sed 's/^/  model.default: /'
hermes config get model.provider 2>/dev/null | sed 's/^/  provider:      /'
echo

echo "=== what the provider actually serves ==="

# Read the key without printing it. It lives either in the environment or in the Hermes env file.
KEY="${DEEPSEEK_API_KEY:-}"

if [ -z "$KEY" ]; then
    for candidate in "$HOME/.hermes/.env" "$HOME/.hermes/env" "$HOME/zion_trading_floor/.env"; do
        if [ -f "$candidate" ]; then
            # Grab a bare or `export`-prefixed assignment, strip the key name and any quotes.
            found=$(grep -hE '^(export )?DEEPSEEK_API_KEY=' "$candidate" 2>/dev/null \
                    | head -1 | sed -E 's/^(export )?DEEPSEEK_API_KEY=//; s/^["'"'"']//; s/["'"'"']$//')
            if [ -n "$found" ]; then
                KEY="$found"
                echo "  (key read from $candidate)"
                break
            fi
        fi
    done
fi

if [ -z "$KEY" ]; then
    echo "  no API key found; cannot query the provider."
    echo "  Set DEEPSEEK_API_KEY or add it to ~/.hermes/.env"
    exit 1
fi

response=$(curl -s --max-time 20 https://api.deepseek.com/v1/models \
    -H "Authorization: Bearer $KEY" 2>/dev/null)

if [ -z "$response" ]; then
    echo "  no response from the provider (network or auth problem)"
    exit 1
fi

# Print just the ids, sorted, so the served list is unambiguous.
printf '%s' "$response" | python3 -c '
import json, sys
raw = sys.stdin.read()
try:
    data = json.loads(raw)
except Exception as e:
    print("  could not parse the response:", e)
    print("  first 200 chars:", raw[:200])
    sys.exit(1)

ids = sorted(m.get("id", "?") for m in data.get("data", []))
if not ids:
    print("  the provider returned no models")
else:
    for i in ids:
        print(f"  {i}")
'

echo
echo "=== names that are NOT served (the trap) ==="
for name in deepseek-v4-flash deepseek-v4-pro deepseek-v4 chat; do
    if printf '%s' "$response" | grep -q "\"$name\""; then
        echo "  $name — served"
    else
        echo "  $name — NOT served (requesting it triggers auto-correction, session only)"
    fi
done
