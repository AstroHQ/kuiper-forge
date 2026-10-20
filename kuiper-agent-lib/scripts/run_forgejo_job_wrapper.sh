#!/bin/bash
# forgejo-runner one-job wrapper, with the same protocol as run_runner_gui_macos.sh: output goes to ~/runner.log and
# the exit code to ~/runner-exit-status, which the tart agent polls. The agent launches it in Terminal.app on macOS
# for GUI service access (code signing, keychain, etc.), and detached on linux. ~/forgejo-runner/run-job.sh is
# written by the agent first (forgejo_runner::prepare_command).

LOG_FILE="$HOME/runner.log"
EXIT_FILE="$HOME/runner-exit-status"

rm -f "$EXIT_FILE"
echo "=== forgejo-runner started at $(date '+%Y-%m-%d %H:%M:%S') ===" > "$LOG_FILE"

bash "$HOME/forgejo-runner/run-job.sh" >> "$LOG_FILE" 2>&1
EXIT_CODE=$?

echo "=== forgejo-runner exited at $(date '+%Y-%m-%d %H:%M:%S') with code $EXIT_CODE ===" >> "$LOG_FILE"

# Write exit code to signal file (atomically via temp + rename)
echo "$EXIT_CODE" > "${EXIT_FILE}.tmp"
mv "${EXIT_FILE}.tmp" "$EXIT_FILE"

exit $EXIT_CODE
