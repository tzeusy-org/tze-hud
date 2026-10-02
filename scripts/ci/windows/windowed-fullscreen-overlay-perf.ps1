# Runs the real windowed compositor in fullscreen and transparent-overlay mode,
# then emits a combined JSON report with the overlay composite delta.

[CmdletBinding()]
param(
    [Parameter(Mandatory=$false)]
    [string]$ExePath = "",

    [Parameter(Mandatory=$false)]
    [string]$OutputDir = "artifacts/windowed-fullscreen-overlay-perf",

    [Parameter(Mandatory=$false)]
    [Nullable[int]]$Width = $null,

    [Parameter(Mandatory=$false)]
    [Nullable[int]]$Height = $null,

    [Parameter(Mandatory=$false)]
    [int]$Frames = 600,

    [Parameter(Mandatory=$false)]
    [int]$WarmupFrames = 120,

    [Parameter(Mandatory=$false)]
    [int]$TargetDeltaUs = 500,

    [Parameter(Mandatory=$false)]
    [switch]$FailOnBudget
)

$ErrorActionPreference = "Stop"

if (-not $ExePath) {
    $ExePath = Join-Path (Get-Location) "target\release\tze_hud.exe"
}

if (-not (Test-Path $ExePath)) {
    throw "tze_hud executable not found: $ExePath"
}

if ($Frames -le 0) {
    throw "-Frames must be greater than zero"
}

if ($WarmupFrames -lt 0) {
    throw "-WarmupFrames must be zero or greater"
}

if (($null -eq $Width) -xor ($null -eq $Height)) {
    throw "-Width and -Height must be provided together"
}

$surfaceArgs = @()
if ($null -ne $Width) {
    if ($Width -le 0 -or $Height -le 0) {
        throw "-Width and -Height must be greater than zero"
    }
    $surfaceArgs = @("--width", "$Width", "--height", "$Height")
}

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null

$configPath = Join-Path $OutputDir "windowed-benchmark.toml"
$fullscreenPath = Join-Path $OutputDir "fullscreen.json"
$overlayPath = Join-Path $OutputDir "overlay.json"
$reportPath = Join-Path $OutputDir "windowed_fullscreen_vs_overlay_report.json"
$logDir = Join-Path $OutputDir "logs"

$config = @"
[runtime]
profile = "full-display"

[[tabs]]
name = "Benchmark"
"@
Set-Content -Path $configPath -Value $config -Encoding UTF8
New-Item -ItemType Directory -Force -Path $logDir | Out-Null

function Invoke-WindowedBenchmarkMode {
    param(
        [Parameter(Mandatory=$true)]
        [ValidateSet("fullscreen", "overlay")]
        [string]$Mode,

        [Parameter(Mandatory=$true)]
        [string]$EmitPath
    )

    Write-Host "[windowed-perf] Running $Mode benchmark..."
    $stdoutPath = Join-Path $logDir "$Mode.stdout.log"
    $stderrPath = Join-Path $logDir "$Mode.stderr.log"
    $args = @(
        "--config", $configPath,
        "--window-mode", $Mode,
        "--grpc-port", "0",
        "--mcp-port", "0",
        "--benchmark-emit", $EmitPath,
        "--benchmark-frames", "$Frames",
        "--benchmark-warmup-frames", "$WarmupFrames"
    )
    $args += $surfaceArgs

    $process = Start-Process `
        -FilePath $ExePath `
        -ArgumentList $args `
        -Wait `
        -PassThru `
        -RedirectStandardOutput $stdoutPath `
        -RedirectStandardError $stderrPath

    if ($null -eq $process.ExitCode) {
        throw "$Mode benchmark exited without an exit code; stdout=$stdoutPath stderr=$stderrPath"
    }

    if ($process.ExitCode -ne 0) {
        throw "$Mode benchmark failed with exit code $($process.ExitCode); stdout=$stdoutPath stderr=$stderrPath"
    }

    if (-not (Test-Path $EmitPath)) {
        throw "$Mode benchmark did not produce expected artifact: $EmitPath; stdout=$stdoutPath stderr=$stderrPath"
    }

    return Get-Content -Path $EmitPath -Raw | ConvertFrom-Json
}

function Get-EffectiveSurfaceDimensions {
    param(
        [Parameter(Mandatory=$true)]
        [object]$Artifact,

        [Parameter(Mandatory=$true)]
        [ValidateSet("fullscreen", "overlay")]
        [string]$Mode
    )

    $integerTypeCodes = @(
        [System.TypeCode]::SByte,
        [System.TypeCode]::Byte,
        [System.TypeCode]::Int16,
        [System.TypeCode]::UInt16,
        [System.TypeCode]::Int32,
        [System.TypeCode]::UInt32,
        [System.TypeCode]::Int64,
        [System.TypeCode]::UInt64
    )

    $windowProperty = $Artifact.PSObject.Properties["window"]
    if ($null -eq $windowProperty -or $null -eq $windowProperty.Value) {
        throw "$Mode benchmark artifact missing window object"
    }

    $widthProperty = $windowProperty.Value.PSObject.Properties["width"]
    if ($null -eq $widthProperty -or $null -eq $widthProperty.Value) {
        throw "$Mode benchmark artifact missing window.width"
    }
    [uint32]$parsedWidth = 0
    $widthTypeCode = [System.Type]::GetTypeCode($widthProperty.Value.GetType())
    if ($widthTypeCode -notin $integerTypeCodes -or
        -not [uint32]::TryParse([string]$widthProperty.Value, [ref]$parsedWidth) -or
        $parsedWidth -eq 0) {
        throw "$Mode benchmark artifact has malformed window.width: $($widthProperty.Value)"
    }

    $heightProperty = $windowProperty.Value.PSObject.Properties["height"]
    if ($null -eq $heightProperty -or $null -eq $heightProperty.Value) {
        throw "$Mode benchmark artifact missing window.height"
    }
    [uint32]$parsedHeight = 0
    $heightTypeCode = [System.Type]::GetTypeCode($heightProperty.Value.GetType())
    if ($heightTypeCode -notin $integerTypeCodes -or
        -not [uint32]::TryParse([string]$heightProperty.Value, [ref]$parsedHeight) -or
        $parsedHeight -eq 0) {
        throw "$Mode benchmark artifact has malformed window.height: $($heightProperty.Value)"
    }

    return [pscustomobject]@{
        width = $parsedWidth
        height = $parsedHeight
    }
}

function Assert-ComparableEffectiveSurfaces {
    param(
        [Parameter(Mandatory=$true)]
        [object]$FullscreenArtifact,

        [Parameter(Mandatory=$true)]
        [object]$OverlayArtifact
    )

    $fullscreenSurface = Get-EffectiveSurfaceDimensions `
        -Artifact $FullscreenArtifact `
        -Mode "fullscreen"
    $overlaySurface = Get-EffectiveSurfaceDimensions `
        -Artifact $OverlayArtifact `
        -Mode "overlay"

    if ($fullscreenSurface.width -ne $overlaySurface.width -or
        $fullscreenSurface.height -ne $overlaySurface.height) {
        throw "effective surface mismatch: fullscreen=$($fullscreenSurface.width)x$($fullscreenSurface.height) overlay=$($overlaySurface.width)x$($overlaySurface.height)"
    }

    return $fullscreenSurface
}

$fullscreen = Invoke-WindowedBenchmarkMode -Mode "fullscreen" -EmitPath $fullscreenPath
$overlay = Invoke-WindowedBenchmarkMode -Mode "overlay" -EmitPath $overlayPath
$effectiveSurface = Assert-ComparableEffectiveSurfaces `
    -FullscreenArtifact $fullscreen `
    -OverlayArtifact $overlay

$fullscreenP50 = [int64]$fullscreen.frame_time.p50_us
$fullscreenP99 = [int64]$fullscreen.frame_time.p99_us
$fullscreenP999 = [int64]$fullscreen.frame_time.p99_9_us
$overlayP50 = [int64]$overlay.frame_time.p50_us
$overlayP99 = [int64]$overlay.frame_time.p99_us
$overlayP999 = [int64]$overlay.frame_time.p99_9_us
$deltaP50 = $overlayP50 - $fullscreenP50
$deltaP99 = $overlayP99 - $fullscreenP99
$deltaP999 = $overlayP999 - $fullscreenP999
$passesTarget = $deltaP99 -le $TargetDeltaUs

$report = [ordered]@{
    schema = "tze_hud.windowed_fullscreen_vs_overlay_report.v1"
    generated_at_utc = (Get-Date).ToUniversalTime().ToString("o")
    target = [ordered]@{
        overlay_composite_delta_p99_us = $TargetDeltaUs
        pass = $passesTarget
    }
    command = [ordered]@{
        exe_path = $ExePath
        auto_size = $null -eq $Width
        width = $Width
        height = $Height
        frames = $Frames
        warmup_frames = $WarmupFrames
    }
    effective_surface = [ordered]@{
        width = $effectiveSurface.width
        height = $effectiveSurface.height
    }
    fullscreen = $fullscreen
    overlay = $overlay
    composite_delta = [ordered]@{
        p50_us = $deltaP50
        p99_us = $deltaP99
        p99_9_us = $deltaP999
    }
}

$report | ConvertTo-Json -Depth 32 | Set-Content -Path $reportPath -Encoding UTF8

Write-Host "[windowed-perf] Report: $reportPath"
Write-Host "[windowed-perf] fullscreen p50/p99/p99.9: $fullscreenP50 / $fullscreenP99 / $fullscreenP999 us"
Write-Host "[windowed-perf] overlay    p50/p99/p99.9: $overlayP50 / $overlayP99 / $overlayP999 us"
Write-Host "[windowed-perf] delta      p50/p99/p99.9: $deltaP50 / $deltaP99 / $deltaP999 us"
Write-Host "[windowed-perf] target p99 delta <= $TargetDeltaUs us: $passesTarget"

if ($FailOnBudget -and -not $passesTarget) {
    exit 2
}
