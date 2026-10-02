# Run the canonical windowed overlay through the bounded WARP quiescent gate.
#
# The runtime itself records the observed GetProcessAffinityMask result and
# selected adapter in its artifact. This harness starts the measured executable
# through `cmd start /affinity`, which installs the two-CPU constraint while
# creating that application. It then makes the Python checker the final
# fail-closed authority.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $false)]
    [string]$ExePath = "target\release\tze_hud.exe",

    [Parameter(Mandatory = $false)]
    [string]$OutputDir = "test_results\windows-performance-budget\quiescent-efficiency"
)

$ErrorActionPreference = "Stop"

if ($env:OS -ne "Windows_NT") {
    throw "quiescent efficiency WARP gate requires Windows"
}
if (-not (Test-Path $ExePath)) {
    throw "canonical tze_hud executable not found: $ExePath"
}

$repoRoot = (git rev-parse --show-toplevel).Trim()
if (-not $repoRoot) {
    throw "unable to resolve repository root"
}

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$artifactPath = Join-Path $OutputDir "quiescent-efficiency.json"
$reportPath = Join-Path $OutputDir "quiescent-efficiency-gate.json"
$stdoutPath = Join-Path $OutputDir "runtime.stdout.log"
$stderrPath = Join-Path $OutputDir "runtime.stderr.log"
$configPath = Join-Path $repoRoot "app\tze_hud_app\config\benchmark.toml"

$affinityMask = "3"
$arguments = @(
    "--config", $configPath,
    "--window-mode", "overlay",
    "--width", "640",
    "--height", "360",
    "--grpc-port", "0",
    "--mcp-port", "0",
    "--quiescent-efficiency-emit", $artifactPath
)

function ConvertTo-CmdArgument {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Value
    )

    return '"' + $Value.Replace('"', '\"') + '"'
}

if (-not $env:ComSpec) {
    throw "COMSPEC is required to launch the constrained Windows application"
}

$quotedExePath = ConvertTo-CmdArgument -Value (Resolve-Path $ExePath).Path
$quotedArguments = @($arguments | ForEach-Object {
    ConvertTo-CmdArgument -Value $_
})
$launchCommand = 'start "" /b /wait /affinity {0} {1} {2}' -f `
    $affinityMask, $quotedExePath, ($quotedArguments -join " ")

# `start /affinity` applies this mask to the new tze_hud.exe process before
# its executable code can run. `cmd.exe` is only the synchronous launcher;
# the runtime artifact remains the authoritative observer of the child mask.
$launcher = Start-Process `
    -FilePath $env:ComSpec `
    -ArgumentList @("/d", "/s", "/c", $launchCommand) `
    -Wait `
    -PassThru `
    -RedirectStandardOutput $stdoutPath `
    -RedirectStandardError $stderrPath

$runtimeExit = $launcher.ExitCode
if (-not (Test-Path $artifactPath)) {
    throw "quiescent runtime did not emit an artifact; exit=$runtimeExit stdout=$stdoutPath stderr=$stderrPath"
}

& python "$repoRoot\scripts\ci\check_idle_efficiency.py" `
    $artifactPath `
    --report $reportPath `
    --require-constrained `
    --require-window-mode overlay
$checkerExit = $LASTEXITCODE

if ($runtimeExit -ne 0) {
    throw "quiescent runtime reported failure ($runtimeExit); stdout=$stdoutPath stderr=$stderrPath"
}
if ($checkerExit -ne 0) {
    exit $checkerExit
}
