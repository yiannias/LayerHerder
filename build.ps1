param(
    [Parameter(Mandatory = $false)]
    [string]$Configuration = "Release",

    [Parameter(Mandatory = $false)]
    [switch]$PackageOnly,

    [Parameter(Mandatory = $false)]
    [switch]$CreateInstaller = $true,

    [Parameter(Mandatory = $false)]
    [switch]$SkipInstaller
)

if ($SkipInstaller) { $CreateInstaller = $false }

$ErrorActionPreference = "Stop"
$SolutionRoot = Split-Path -Parent $PSCommandPath
$ProjectDir = Join-Path $SolutionRoot "src\AcLayerStandardizer"
$DistDir = Join-Path $SolutionRoot "dist"
$BundleName = "LayerHerder.bundle"
$BundleDir = Join-Path $SolutionRoot $BundleName

# Single source of truth for the app version -- keep in sync with the
# installer's #define MyAppVersion (installer/LayerHerder.iss),
# which reads this same string via the MYAPPVERSION env var below.
$AppVersion = "BETA/1.4.1"
$AppVersionSafe = $AppVersion -replace "/", "-"

# One payload per AutoCAD .NET binary-compatibility era. The csproj
# multi-targets all three TFMs in a single build; the Autoloader picks the
# right folder at runtime via PackageContents.xml SeriesMin/SeriesMax.
# AutoCAD 2020 (R23) and older are NOT supported.
$Eras = @(
    @{ Folder = "R24"; Tfm = "net48";           Acad = "AutoCAD 2021-2024" },
    @{ Folder = "R25"; Tfm = "net8.0-windows";  Acad = "AutoCAD 2025-2026" },
    @{ Folder = "R26"; Tfm = "net10.0-windows"; Acad = "AutoCAD 2027" }
)

Write-Host "=== Layer Herder - Build & Package ===" -ForegroundColor Cyan
Write-Host "  Version:       $AppVersion"
Write-Host "  Configuration: $Configuration"
foreach ($Era in $Eras)
{
    Write-Host ("  {0}: {1} ({2})" -f $Era.Folder, $Era.Acad, $Era.Tfm)
}
Write-Host ""

if (-not $PackageOnly)
{
    # AcadBridgeKit C# source: sibling checkout, or a clone via the fetch script
    Write-Host ">> Fetching AcadBridgeKit..." -ForegroundColor Yellow
    & (Join-Path $SolutionRoot "scripts\fetch-acadbridgekit.ps1")
    if ($LASTEXITCODE -ne 0) { exit 1 }

    # Restore
    Write-Host ">> Restoring packages..." -ForegroundColor Yellow
    dotnet restore $ProjectDir\AcLayerStandardizer.csproj
    if ($LASTEXITCODE -ne 0) { exit 1 }

    # Build (all target frameworks in one pass)
    Write-Host ">> Building all targets..." -ForegroundColor Yellow
    dotnet build $ProjectDir\AcLayerStandardizer.csproj `
        -p:Configuration=$Configuration `
        -p:InformationalVersion=$AppVersion `
        --no-restore
    if ($LASTEXITCODE -ne 0) { exit 1 }

    # The native mapping editor is the shipped replacement window in every
    # configuration. Package one GUI-subsystem release executable beside
    # each AutoCAD-era plugin DLL.
    Write-Host ">> Building Rust mapping editor..." -ForegroundColor Yellow
    cargo build --manifest-path (Join-Path $SolutionRoot "rust\Cargo.toml") --package acad_layer_ui --release
    if ($LASTEXITCODE -ne 0) { exit 1 }

    # Run tests (all target frameworks)
    $TestProject = Join-Path $SolutionRoot "tests\AcLayerStandardizer.Tests\AcLayerStandardizer.Tests.csproj"
    if (Test-Path $TestProject)
    {
        Write-Host ">> Running tests..." -ForegroundColor Yellow
        dotnet test $TestProject
        if ($LASTEXITCODE -ne 0) { exit 1 }
    }
}

# Build installer (on by default, every build, so the installer never drifts
# from the app -- pass -SkipInstaller or -CreateInstaller:$false to opt out)
$InstallerBuilt = $false
if ($CreateInstaller)
{
    Write-Host ">> Building installer..." -ForegroundColor Yellow

    # Find Inno Setup compiler
    $IsccPath = $null
    $PossiblePaths = @(
        "C:\Program Files (x86)\Inno Setup 6\ISCC.exe",
        "C:\Program Files\Inno Setup 6\ISCC.exe",
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    )
    foreach ($Path in $PossiblePaths)
    {
        if (Test-Path $Path)
        {
            $IsccPath = $Path
            break
        }
    }

    if (-not $IsccPath)
    {
        Write-Warning "Inno Setup not found -- skipping installer build. Install from https://jrsoftware.org/isinfo.php to have build.ps1 keep the installer current automatically."
    }
    else
    {
        # Set environment variables for Inno Setup script
        $env:MYAPPVERSION = $AppVersion

        # layer_dictionary.json's schemaVersion, baked into the installer at
        # compile time so it can prompt to overwrite an installed dictionary
        # only when the shipped one is actually newer (see ShouldInstallDictionary
        # in the .iss) without needing runtime JSON parsing in Pascal Script.
        # Read here (PowerShell) rather than in Inno's preprocessor because
        # PowerShell already has a real JSON parser and the .iss doesn't.
        $DictJsonPath = Join-Path $SolutionRoot "installer\assets\layer_dictionary.json"
        $env:MYDICTSCHEMAVERSION = (Get-Content $DictJsonPath -Raw | ConvertFrom-Json).schemaVersion

        $IssScript = Join-Path $SolutionRoot "installer\LayerHerder.iss"
        & $IsccPath $IssScript
        if ($LASTEXITCODE -ne 0) { exit 1 }

        $InstallerBuilt = $true
        $InstallerDest = Join-Path $DistDir "LayerHerder_$AppVersionSafe.exe"
        Write-Host "  Installer: $InstallerDest" -ForegroundColor Green
    }
}

# Package the loose bundle (dev-install alternative to the installer)
Write-Host ">> Packaging bundle..." -ForegroundColor Yellow

# Clean previous bundle
if (Test-Path $BundleDir)
{
    Remove-Item -Path $BundleDir -Recurse -Force
}

foreach ($Era in $Eras)
{
    $OutputDir = Join-Path $ProjectDir "bin\$Configuration\$($Era.Tfm)"
    $TargetDir = Join-Path $BundleDir "Contents\$($Era.Folder)"
    New-Item -ItemType Directory -Path $TargetDir -Force | Out-Null

    # Everything in the output dir ships: the AutoCAD reference assemblies
    # are ExcludeAssets=runtime in the csproj so they never land here, and
    # the net48 payload legitimately needs its System.Text.Json dep closure.
    Copy-Item -Path (Join-Path $OutputDir "*.dll") -Destination $TargetDir

    $RustUi = Join-Path $SolutionRoot "rust\target\release\acad_layer_ui.exe"
    if (-not (Test-Path $RustUi))
    {
        throw "Rust mapping editor was not built: $RustUi"
    }
    Copy-Item -LiteralPath $RustUi -Destination (Join-Path $TargetDir "acad_layer_ui.exe")
}

# Copy manifest and the bundle icon it references
Copy-Item -Path (Join-Path $DistDir "PackageContents.xml") -Destination $BundleDir
Copy-Item -Path (Join-Path $SolutionRoot "installer\assets\LayerHerder.ico") -Destination $BundleDir

Write-Host ""
Write-Host "=== Done ===" -ForegroundColor Green
Write-Host "  Bundle:  $BundleDir"
if ($InstallerBuilt)
{
    Write-Host "  Installer: $DistDir\LayerHerder_$AppVersionSafe.exe" -ForegroundColor Green
}
Write-Host ""
Write-Host "Installation:" -ForegroundColor Cyan
Write-Host "  Option 1: Run the installer (LayerHerder_$AppVersionSafe.exe)"
Write-Host "  Option 2: Copy '$BundleName' folder to:"
Write-Host "     %APPDATA%\Autodesk\ApplicationPlugins\"
Write-Host "  Restart AutoCAD (2021 or newer)"
Write-Host ""
