<#
.SYNOPSIS
  Makes the AcadBridgeKit C# source available to the Layer Herder .NET build.

.DESCRIPTION
  If the sibling checkout ..\AcadBridgeKit exists (SpatialStudio umbrella layout), it is used
  and nothing is downloaded. Otherwise https://github.com/yiannias/AcadBridgeKit is cloned into
  external\AcadBridgeKit (or fetched, if already cloned) and checked out at the rev in
  acadbridgekit.rev. The repo is private, so this needs git credentials for GitHub.

.EXAMPLE
  ./scripts/fetch-acadbridgekit.ps1
#>
$ErrorActionPreference = 'Stop'
$LayerHerderDir = Split-Path -Parent $PSScriptRoot
$SiblingDir = Join-Path (Split-Path -Parent $LayerHerderDir) 'AcadBridgeKit'

if (Test-Path (Join-Path $SiblingDir 'csharp\AcadBridgeKit.projitems')) {
    Write-Host "AcadBridgeKit: using sibling checkout $SiblingDir"
    exit 0
}

$Rev = (Get-Content (Join-Path $LayerHerderDir 'acadbridgekit.rev') -Raw).Trim()
$TargetDir = Join-Path $LayerHerderDir 'external\AcadBridgeKit'

if (Test-Path (Join-Path $TargetDir '.git')) {
    Write-Host "AcadBridgeKit: fetching into $TargetDir"
    git -C $TargetDir fetch origin
    if ($LASTEXITCODE -ne 0) { Write-Error "git fetch failed"; exit 1 }
} else {
    Write-Host "AcadBridgeKit: cloning into $TargetDir"
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $TargetDir) | Out-Null
    git clone https://github.com/yiannias/AcadBridgeKit $TargetDir
    if ($LASTEXITCODE -ne 0) { Write-Error "git clone failed"; exit 1 }
}

git -C $TargetDir checkout --detach $Rev
if ($LASTEXITCODE -ne 0) { Write-Error "git checkout $Rev failed"; exit 1 }
Write-Host "AcadBridgeKit: ready at $Rev"
exit 0
