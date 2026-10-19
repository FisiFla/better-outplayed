#!/usr/bin/env bash
# Anti-cheat guard (docs/anti-cheat.md): the project captures with Windows Graphics Capture
# and WASAPI only. Nothing here may inject into, hook, read or write another process, or
# synthesise input. Fails the build if such an API appears in Rust source.
#
# The list is of *names*, and it is long on purpose. A pattern that misses a spelling is a
# hole, and the cost of a false positive is one line in an exemption with a reason; the
# cost of a false negative is an anti-cheat ban.
#
# The window enumeration is the one group that cannot be forbidden outright: the capture
# backend has to find the window it captures, and that is the sanctioned use. So it is
# asserted to be *in that one file* rather than nowhere — a match anywhere else fails the
# build, which is the property the blanket list has.
set -euo pipefail
cd "$(dirname "$0")/.."

# Injecting, hooking, reading or writing another process, and synthesising input.
pattern='WriteProcessMemory|ReadProcessMemory|SetWindowsHookEx|SendInput|CreateRemoteThread|NtWriteVirtualMemory|NtReadVirtualMemory|ZwWriteVirtualMemory|VirtualAllocEx|keybd_event|mouse_event|SetThreadContext|GetThreadContext|QueueUserAPC|LoadLibrary([AW])?(Ex)?([AW])?\(|OpenProcess|SuspendThread|ResumeThread|RtlCreateUserThread|NtCreateThreadEx|DeviceIoControl|CreateFileMapping|MapViewOfFile|PostMessage|SendMessage|GetProcAddress|GetModuleHandle[AW]|GetAsyncKeyState|GetKeyState|FindWindow|EnumDesktopWindows|GetForegroundWindow'

# Enumerating another process's windows. The capture backend does this to find its target;
# nothing else may.
window_pattern='EnumWindows|EnumDesktopWindows|FindWindow|GetForegroundWindow'
capture_backend='crates/capture/src/wgc.rs'

fail() {
  echo "$1" >&2
  exit 1
}

# The Rust tree. The `//` filter drops the comment lines, so a file that *talks* about a
# forbidden API is not an offender; the same filter works under BSD and GNU grep.
if grep -rnE "$pattern" crates apps xtask --include='*.rs' | grep -vE '^\S+:[0-9]+:\s*//'; then
  fail "forbidden process-manipulation / input-synthesis API found (see docs/anti-cheat.md)"
fi

# The scripts, which are not Rust and are therefore not covered above. The guard itself is
# excluded: it holds the names it searches for.
if grep -rnE "$pattern" scripts --include='*.ps1' --include='*.bat' --include='*.cmd' --include='*.sh' \
  | grep -vE '^\S+:[0-9]+:\s*#' | grep -v 'check-forbidden-apis.sh'; then
  fail "forbidden process-manipulation / input-synthesis API found in a script (see docs/anti-cheat.md)"
fi

# The window enumeration belongs to the capture backend and nowhere else.
if grep -rnE "$window_pattern" crates apps xtask scripts --include='*.rs' --include='*.ps1' \
  | grep -vE '^\S+:[0-9]+:\s*//' | grep -v "^$capture_backend:"; then
  fail "a window is enumerated outside the capture backend (see docs/anti-cheat.md)"
fi

# The positive half of the process-watcher claim: it enumerates the process list, which is
# what Task Manager does, and it never opens a process — no access right is requested over
# any game, and no memory is read.
if ! grep -qE 'CreateToolhelp32Snapshot' crates/events/src/process.rs; then
  fail "the process watcher no longer enumerates the process list (see docs/anti-cheat.md)"
fi
if grep -nE 'OpenProcess' crates/events/src/process.rs | grep -vE '^[0-9]+:\s*//'; then
  fail "the process watcher opens a process (see docs/anti-cheat.md)"
fi

echo "no forbidden APIs"
