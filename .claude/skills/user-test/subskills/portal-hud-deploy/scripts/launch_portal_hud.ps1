# launch_portal_hud.ps1 — overlay HUD launch helper (runs ON the Windows host).
#
# Copied to the host by deploy_portal_hud.sh and invoked as the admin /
# interactive-desktop user. It registers + starts a Scheduled Task that runs
# tze_hud.exe DIRECTLY in overlay mode, then waits for both ports to bind and
# emits a single JSON result object on stdout.
#
# SECRET HANDLING (critical):
#   The runtime stores only agents' PSK hashes, in agents.toml beside the
#   config. This script reads the PSK from the host environment itself
#   (TZE_HUD_MCP_RESIDENT_PRINCIPAL, User scope first, then process scope) and
#   pairs it as agent `claude` (allow = ["*"]) by writing its SHA-256 there.
#   The PSK is NEVER accepted as a parameter, NEVER passed on the Linux/SSH
#   command line or the task action, and NEVER logged.
#
# TRANSPARENCY (critical):
#   The action executes tze_hud.exe DIRECTLY via New-ScheduledTaskAction -Execute.
#   It is NOT wrapped in cmd.exe/powershell and stdout is NOT redirected. Either
#   of those sets CREATE_NO_WINDOW, which breaks WS_EX_NOREDIRECTIONBITMAP and
#   produces a grey/opaque overlay instead of a transparent one. Do not "fix" the
#   launch by adding a wrapper or a `>` redirect.
#
# Mirrors the known-good C:\tze_hud\hud-8dht5\start-portal-hud.ps1, with the
# agent paired from the host env instead of a -Psk parameter.

param(
    [string]$TaskName   = 'TzeHudPortalDeploy',
    [int]$GrpcPort      = 50051,
    [int]$McpPort       = 9090,
    [string]$ConfigPath = 'C:\tze_hud\tze_hud.toml',
    [string]$ExePath    = 'C:\tze_hud\tze_hud.exe',
    [string]$WorkingDir = 'C:\tze_hud'
)

$ProgressPreference    = 'SilentlyContinue'
$ErrorActionPreference = 'SilentlyContinue'

$result = [ordered]@{
    task         = $TaskName
    grpc_port    = $GrpcPort
    mcp_port     = $McpPort
    config       = $ConfigPath
    exe          = $ExePath
    steps        = @()
    bound        = $false
    pid          = $null
    psk_source   = $null
}
function Add-Step($n, $s, $d) { $script:result.steps += [ordered]@{ name = $n; status = $s; detail = $d } }

# Poll a local TCP port until it reaches the desired Listen state (or timeout).
function Wait-Port($port, $open, $secs) {
    $deadline = (Get-Date).AddSeconds($secs)
    do {
        $c = Get-NetTCPConnection -State Listen -LocalPort $port -ErrorAction SilentlyContinue
        if ($open -and $c) { return $true }
        if ((-not $open) -and (-not $c)) { return $true }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)
    return $false
}

try {
    # ── PSK: source from host env, never from a parameter ────────────────────
    $psk = [Environment]::GetEnvironmentVariable('TZE_HUD_MCP_RESIDENT_PRINCIPAL', 'User')
    $pskSource = 'User'
    if ([string]::IsNullOrEmpty($psk)) {
        $psk = $env:TZE_HUD_MCP_RESIDENT_PRINCIPAL
        $pskSource = 'Process'
    }
    if ([string]::IsNullOrEmpty($psk)) {
        throw 'TZE_HUD_MCP_RESIDENT_PRINCIPAL is not set in the host environment; cannot pair the agent'
    }
    $result.psk_source = $pskSource
    Add-Step 'resolve-psk' 'ok' "source=$pskSource len=$($psk.Length)"

    # ── Pair the PSK: only its SHA-256 goes into agents.toml beside the config ─
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $hash = -join ($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($psk)) | ForEach-Object { $_.ToString('x2') })
    $agentsPath = Join-Path (Split-Path -Parent $ConfigPath) 'agents.toml'
    Set-Content -Path $agentsPath -Encoding ascii -Value "[agents.claude]`npsk_sha256 = `"$hash`"`nallow = [`"*`"]"
    Add-Step 'pair-agent' 'ok' "agents=$agentsPath"

    if (-not (Test-Path $ExePath)) { throw "exe not found at $ExePath" }

    # ── Stop any HUD currently holding the ports ─────────────────────────────
    foreach ($t in @('TzeHudOverlay', 'TzeHudPortalVal', 'TzeHud8dht5Media', $TaskName)) {
        Stop-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue
    }
    Get-Process tze_hud -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    $closed = Wait-Port $GrpcPort $false 20
    Add-Step 'stop-existing' $(if ($closed) { 'ok' } else { 'timeout' }) "port_${GrpcPort}_closed=$closed"
    if (-not $closed) { throw "port $GrpcPort did not close" }

    # ── Register the overlay task (exe-direct, NO wrapper, NO redirect) ───────
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
    $argline = "--config $ConfigPath --window-mode overlay --bind-all-interfaces --grpc-port $GrpcPort --mcp-port $McpPort"
    $action = New-ScheduledTaskAction -Execute $ExePath -Argument $argline -WorkingDirectory $WorkingDir
    # ExecutionTimeLimit MUST be unlimited (PT0S) — the overlay is a long-lived
    # presence runtime, not a batch job. A finite limit makes Windows TERMINATE
    # the process when it elapses (the prior 30-minute cap killed the HUD mid-
    # session, surfacing as the compositor's "frame loop EXITED (HB-2)" ~30 min
    # after launch). TimeSpan::Zero disables the "stop if runs longer than" gate.
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)
    Register-ScheduledTask -TaskName $TaskName -Action $action -Settings $settings -Force | Out-Null

    # ── Launch and wait for both ports to bind ───────────────────────────────
    Start-ScheduledTask -TaskName $TaskName
    $grpcBound = Wait-Port $GrpcPort $true 30
    $mcpBound  = Wait-Port $McpPort $true 30
    $result.bound = [bool]($grpcBound -and $mcpBound)

    $listeners = Get-NetTCPConnection -State Listen -LocalPort $GrpcPort, $McpPort -ErrorAction SilentlyContinue |
        Select-Object LocalPort, OwningProcess
    $result.listeners = $listeners
    $grpcOwner = $listeners | Where-Object { $_.LocalPort -eq $GrpcPort } | Select-Object -First 1
    if ($grpcOwner) { $result.pid = $grpcOwner.OwningProcess }

    Add-Step 'launch-overlay' $(if ($result.bound) { 'ok' } else { 'failed' }) "grpc_${GrpcPort}_bound=$grpcBound mcp_${McpPort}_bound=$mcpBound pid=$($result.pid)"
    if (-not $result.bound) { throw "ports did not bind (grpc=$grpcBound mcp=$mcpBound)" }
}
catch {
    $result.error = [string]$_
    Add-Step 'exception' 'error' ([string]$_)
}
finally {
    # Single JSON object on stdout — the only thing the bash entrypoint parses.
    $result | ConvertTo-Json -Depth 6 -Compress
}
