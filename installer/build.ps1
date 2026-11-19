<#
.SYNOPSIS
    Builds the Windows installer, dist\Candy-Corn-Setup-<version>.exe.

.DESCRIPTION
    1. Builds a release Candy-Corn.exe with the C runtime linked statically, so the installed program does not
       need the Visual C++ Redistributable. The build uses its own target directory (<cargo target dir>\installer),
       so the regular debug and release builds keep their cache.
    2. Checks that the exe does not import VCRUNTIME140.dll.
    3. Compiles installer\candy-corn.iss with Inno Setup 6 (ISCC.exe).

.PARAMETER Iscc
    Path to ISCC.exe. Default: $env:ISCC, then ISCC.exe on PATH, then the usual Inno Setup 6 folders.

.PARAMETER SkipBuild
    Packages the exe of the last run without building it again.

.EXAMPLE
    .\installer\build.ps1
#>
[CmdletBinding()]
param(
    [string]$Iscc,
    [switch]$SkipBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Runs a native command. cargo writes its progress to stderr, so only the exit code decides success.
function Invoke-Native {
    param(
        [Parameter(Mandatory)][string]$FilePath,
        [string[]]$ArgumentList = @()
    )
    $ErrorActionPreference = 'Continue'
    & $FilePath @ArgumentList
    if ($LASTEXITCODE -ne 0) {
        throw "$FilePath $($ArgumentList -join ' ') failed with exit code $LASTEXITCODE"
    }
}

function Find-Iscc {
    if ($env:ISCC) { return $env:ISCC }
    $cmd = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $dirs = @("$env:LOCALAPPDATA\Programs", ${env:ProgramFiles(x86)}, $env:ProgramFiles) | Where-Object { $_ }
    foreach ($dir in $dirs) {
        $candidate = Join-Path $dir 'Inno Setup 6\ISCC.exe'
        if (Test-Path -LiteralPath $candidate) { return $candidate }
    }
    throw 'Inno Setup 6 (ISCC.exe) was not found. Install it (winget install JRSoftware.InnoSetup) or pass -Iscc.'
}

$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $metaJson = (Invoke-Native cargo @('metadata', '--no-deps', '--format-version', '1', '--locked')) -join "`n"
    $meta = $metaJson | ConvertFrom-Json
    $package = @($meta.packages | Where-Object { $_.name -eq 'candy-corn' })[0]
    $version = $package.version
    $targetDir = [IO.Path]::GetFullPath((Join-Path $meta.target_directory 'installer'))
    $exe = Join-Path $targetDir 'release\Candy-Corn.exe'

    if (-not $SkipBuild) {
        $savedFlags = $env:RUSTFLAGS
        $env:RUSTFLAGS = (@($savedFlags, '-C target-feature=+crt-static') | Where-Object { $_ }) -join ' '
        try {
            Invoke-Native cargo @('build', '--release', '--locked', '--target-dir', $targetDir)
        }
        finally {
            $env:RUSTFLAGS = $savedFlags
        }
    }
    if (-not (Test-Path -LiteralPath $exe)) {
        throw "Missing $exe. Run the script without -SkipBuild."
    }

    # A dynamically linked exe names VCRUNTIME140.dll in its import table; a static CRT build does not.
    $exeText = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($exe))
    if ($exeText.IndexOf('VCRUNTIME140', [StringComparison]::OrdinalIgnoreCase) -ge 0) {
        throw "$exe imports VCRUNTIME140.dll, so the installed program would need the Visual C++ Redistributable."
    }

    if (-not $Iscc) { $Iscc = Find-Iscc }
    Invoke-Native $Iscc @("/DAppVersion=$version", "/DAppExe=$exe", (Join-Path $PSScriptRoot 'candy-corn.iss'))

    # OutputDir and OutputBaseFilename in candy-corn.iss
    $setup = Join-Path $root "dist\Candy-Corn-Setup-$version.exe"
    if (-not (Test-Path -LiteralPath $setup)) { throw "ISCC finished but $setup is missing." }
    $hash = (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash
    Write-Host "Installer: $setup"
    Write-Host "SHA-256:   $hash"
}
finally {
    Pop-Location
}
