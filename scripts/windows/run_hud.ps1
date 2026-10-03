# scripts/windows/run_hud.ps1
#
# Lock-aware HUD launcher for interactive /user-test sessions.
#
# PURPOSE
#   Launches the HUD for interactive /user-test sessions by triggering the
#   scheduled task, waits for tze_hud.exe to appear, and monitors it until it
#   exits. (The GPU lock this wrapper once took is gone: the runtime no longer
#   has one.)
#
# LAUNCH CONSTRAINT — READ THIS BEFORE MODIFYING
#   tze_hud.exe MUST be started via the Windows Task Scheduler task
#   "TzeHudOverlay" (run as the interactive desktop user "admin-user").  A direct
#   Start-Process or SSH-spawned launch produces a grey/opaque window because the
#   process cannot access the desktop GPU and WS_EX_NOREDIRECTIONBITMAP is not
#   honoured outside an interactive session.  This wrapper triggers the task; it
#   does NOT spawn tze_hud.exe directly.
#
#   See: docs/ci/windows-d18-runner-setup.md §7
#        docs/design/tzehouse-windows-gpu-scheduling.md §3
#        .claude/skills/user-test/SKILL.md ("Behavior Rules")
#
# USAGE
#   run_hud.ps1 [-TaskName <name>] [-PollIntervalSec <n>] [-TimeoutSec <n>]
#               [-WhatIf]
#
# PARAMETERS
#   -TaskName        Scheduled task name (default: TzeHudOverlay)
#   -PollIntervalSec How often to poll for tze_hud.exe presence (default: 2)
#   -TimeoutSec      Seconds to wait for tze_hud.exe to appear after task start
#                    before giving up (default: 30)
#   -WhatIf          Dry-run: validate task existence without launching
#
# EXIT CODES
#   0   HUD session ran and exited cleanly (tze_hud.exe process gone)
#   3   Scheduled task not found or failed to start
#   4   tze_hud.exe did not appear within -TimeoutSec seconds
#
# COMPATIBILITY
#   Windows PowerShell 5.1 and PowerShell 7+.
#   No modules beyond the core Windows-bundled set are required.

[CmdletBinding(SupportsShouldProcess)]
param(
    [Parameter(Mandatory=$false)]
    [string]$TaskName = "TzeHudOverlay",

    [Parameter(Mandatory=$false)]
    [int]$PollIntervalSec = 2,

    [Parameter(Mandatory=$false)]
    [int]$TimeoutSec = 30,

    [Parameter(Mandatory=$false)]
    [switch]$WhatIf
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# ── Helpers ──────────────────────────────────────────────────────────────────

function Write-HudLog([string]$msg) {
    Write-Host "[run_hud] $msg"
}

function Exit-WithCode([int]$code, [string]$reason) {
    Write-HudLog "EXIT $code — $reason"
    exit $code
}

# ── Pre-flight: verify the scheduled task exists ──────────────────────────────

Write-HudLog "Checking scheduled task '$TaskName' ..."
$task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if (-not $task) {
    Write-HudLog "ERROR: Scheduled task '$TaskName' not found."
    Write-HudLog "       Register it first (see docs/ci/windows-d18-runner-setup.md §7.7):"
    Write-HudLog "       Register-ScheduledTask -TaskName '$TaskName' ..."
    Exit-WithCode 3 "scheduled task not found"
}
Write-HudLog "Task '$TaskName' found (State: $($task.State))."

# ── WhatIf / dry-run mode ─────────────────────────────────────────────────────

if ($WhatIf) {
    Write-HudLog "WhatIf: would run '$TaskName'."
    Write-HudLog "WhatIf: dry-run complete — no task started."
    exit 0
}

# ── Launch + monitor ──────────────────────────────────────────────────────────

# Start the scheduled task
Write-HudLog "Starting scheduled task '$TaskName' ..."
try {
    Start-ScheduledTask -TaskName $TaskName
} catch {
    Write-HudLog "ERROR: Failed to start scheduled task '$TaskName': $_"
    Exit-WithCode 3 "task start failed"
}
Write-HudLog "Task started. Waiting for tze_hud.exe to appear ..."

# Step 2b: Wait for tze_hud.exe to appear in the process list
$deadline = (Get-Date).AddSeconds($TimeoutSec)
$hudProcess = $null
while ((Get-Date) -lt $deadline) {
    $hudProcess = Get-Process -Name "tze_hud" -ErrorAction SilentlyContinue
    if ($hudProcess) {
        Write-HudLog "tze_hud.exe is running (PID $($hudProcess.Id))."
        break
    }
    Start-Sleep -Seconds $PollIntervalSec
}

if (-not $hudProcess) {
    Write-HudLog "ERROR: tze_hud.exe did not appear within $TimeoutSec seconds."
    Write-HudLog "       Check the scheduled task log and ensure the task action path is correct."
    Exit-WithCode 4 "tze_hud.exe did not start in time"
}

# Step 2c: Monitor until the process exits
Write-HudLog "Monitoring tze_hud.exe (PID $($hudProcess.Id)). Press Ctrl-C to stop."

# Re-fetch by Id to get a stable handle; process name can be ambiguous
$trackedPid = $hudProcess.Id
while ($true) {
    $running = Get-Process -Id $trackedPid -ErrorAction SilentlyContinue
    if (-not $running) {
        Write-HudLog "tze_hud.exe (PID $trackedPid) has exited."
        break
    }
    Start-Sleep -Seconds $PollIntervalSec
}

exit 0
