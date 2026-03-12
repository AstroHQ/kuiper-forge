#!/bin/bash
# GitHub Actions Runner GUI wrapper script for macOS
# Runs the runner in Terminal.app context for GUI service access (code signing, keychain, etc.)

LOG_FILE="$HOME/runner.log"
EXIT_FILE="$HOME/runner-exit-status"

# Clean up any previous run artifacts
rm -f "$EXIT_FILE"
echo "=== Runner started at $(date '+%Y-%m-%d %H:%M:%S') ===" > "$LOG_FILE"

# Run the runner and capture output
cd ~/actions-runner

# Check for JIT config file (written by agent for JIT/webhook mode)
JIT_CONFIG_FILE="$HOME/jit-config.txt"
if [ -f "$JIT_CONFIG_FILE" ]; then
    echo "Using JIT config (skipping config.sh)" >> "$LOG_FILE"
    JIT_CONFIG=$(cat "$JIT_CONFIG_FILE")
    rm -f "$JIT_CONFIG_FILE"
    ./run.sh --jitconfig "$JIT_CONFIG" >> "$LOG_FILE" 2>&1
else
    ./run.sh >> "$LOG_FILE" 2>&1
fi
EXIT_CODE=$?

# Record exit
echo "=== Runner exited at $(date '+%Y-%m-%d %H:%M:%S') with code $EXIT_CODE ===" >> "$LOG_FILE"

# Write exit code to signal file (atomically via temp + rename)
echo "$EXIT_CODE" > "${EXIT_FILE}.tmp"
mv "${EXIT_FILE}.tmp" "$EXIT_FILE"

exit $EXIT_CODE
