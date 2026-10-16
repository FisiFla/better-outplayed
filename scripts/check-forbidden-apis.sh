#!/usr/bin/env bash
# Anti-cheat guard (docs/anti-cheat.md): the project captures with Windows Graphics Capture
# and WASAPI only. Nothing here may inject into, hook, read or write another process, or
# synthesise input. Fails the build if such an API appears in Rust source.
set -euo pipefail
cd "$(dirname "$0")/.."
pattern='WriteProcessMemory|ReadProcessMemory|SetWindowsHookEx|SendInput|CreateRemoteThread|NtWriteVirtualMemory|VirtualAllocEx|keybd_event|mouse_event|SetThreadContext|QueueUserAPC|LoadLibrary[AW]?\('
if grep -rnE "$pattern" crates apps xtask --include='*.rs' | grep -vE '^\S+:[0-9]+:\s*//'; then
  echo "forbidden process-manipulation / input-synthesis API found (see docs/anti-cheat.md)" >&2
  exit 1
fi
echo "no forbidden APIs"
