#!/bin/bash
curl -s http://localhost:4040/api/requests/http | \
  jq -r '.requests[].id' | \
  while read id; do
    curl -s "http://localhost:4040/api/requests/http/$id" > "requests/request_$id.json"
  done
