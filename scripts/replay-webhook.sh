#!/usr/bin/env bash
#
# Replay a captured webhook request to the local coordinator for testing.
#
# Usage: ./scripts/replay-webhook.sh <request_id>
#
# Example:
#   ./scripts/replay-webhook.sh airt_38cd782uvDgf63j6ES3r02DStRE
#
# The script reads the request file from requests/request_<id>.json,
# extracts the necessary headers and body, and POSTs to localhost:9443/webhook.

set -euo pipefail

COORDINATOR_URL="${COORDINATOR_URL:-https://localhost:9443}"

if [[ $# -ne 1 ]]; then
    echo "Usage: $0 <request_id>" >&2
    echo "Example: $0 airt_38cd782uvDgf63j6ES3r02DStRE" >&2
    exit 1
fi

REQUEST_ID="$1"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
REQUEST_FILE="$PROJECT_DIR/requests/request_${REQUEST_ID}.json"

if [[ ! -f "$REQUEST_FILE" ]]; then
    echo "Error: Request file not found: $REQUEST_FILE" >&2
    exit 1
fi

echo "Replaying webhook request: $REQUEST_ID"
echo "Reading from: $REQUEST_FILE"

# Extract the raw base64 request and decode it
RAW_B64=$(jq -r '.request.raw' "$REQUEST_FILE")

# The raw field contains the full HTTP request (headers + body)
# Split on the double CRLF to get the body
BODY=$(echo "$RAW_B64" | base64 -d | sed '1,/^\r$/d')

# Extract the webhook-relevant headers
X_GITHUB_EVENT=$(jq -r '.request.headers["X-Github-Event"][0]' "$REQUEST_FILE")
X_GITHUB_DELIVERY=$(jq -r '.request.headers["X-Github-Delivery"][0]' "$REQUEST_FILE")
X_HUB_SIGNATURE=$(jq -r '.request.headers["X-Hub-Signature"][0]' "$REQUEST_FILE")
X_HUB_SIGNATURE_256=$(jq -r '.request.headers["X-Hub-Signature-256"][0]' "$REQUEST_FILE")
CONTENT_TYPE=$(jq -r '.request.headers["Content-Type"][0]' "$REQUEST_FILE")

echo "Event: $X_GITHUB_EVENT"
echo "Delivery: $X_GITHUB_DELIVERY"
echo "Posting to: $COORDINATOR_URL/webhook"
echo ""

# Build curl command with headers (-k for self-signed certs)
curl -v -k "$COORDINATOR_URL/webhook" \
    -H "Content-Type: $CONTENT_TYPE" \
    -H "X-Github-Event: $X_GITHUB_EVENT" \
    -H "X-Github-Delivery: $X_GITHUB_DELIVERY" \
    -H "X-Hub-Signature: $X_HUB_SIGNATURE" \
    -H "X-Hub-Signature-256: $X_HUB_SIGNATURE_256" \
    --data-raw "$BODY"

echo ""
echo "Done."
