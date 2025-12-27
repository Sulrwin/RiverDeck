param(
	[Parameter(Mandatory = $true)][string]$Version,
	[Parameter(Mandatory = $true)][string]$Target,
	[Parameter(Mandatory = $true)][string]$OutDir
)

$ErrorActionPreference = "Stop"

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$riverdeckExe = Join-Path -Path (Get-Location) -ChildPath "target\$Target\release\riverdeck.exe"
$riverdeckPiExe = Join-Path -Path (Get-Location) -ChildPath "target\$Target\release\riverdeck-pi.exe"

if (!(Test-Path $riverdeckExe)) { throw "Missing $riverdeckExe" }
if (!(Test-Path $riverdeckPiExe)) { throw "Missing $riverdeckPiExe" }

$wixBin = "${env:ProgramFiles(x86)}\WiX Toolset v3.11\bin"
if (!(Test-Path $wixBin)) {
	throw "WiX Toolset not found at $wixBin (install wixtoolset via Chocolatey first)"
}

$candle = Join-Path $wixBin "candle.exe"
$light = Join-Path $wixBin "light.exe"

$wxs = Join-Path -Path (Get-Location) -ChildPath "packaging\windows\riverdeck.wxs"
$wixobj = Join-Path -Path $OutDir -ChildPath "riverdeck.wixobj"
$msi = Join-Path -Path $OutDir -ChildPath ("RiverDeck_{0}_{1}.msi" -f $Version, $Target)

& $candle `
	-nologo `
	-dVersion="$Version" `
	-dRiverDeckExe="$riverdeckExe" `
	-dRiverDeckPiExe="$riverdeckPiExe" `
	-out "$wixobj" `
	"$wxs"

& $light `
	-nologo `
	-out "$msi" `
	"$wixobj"

Write-Output "Built MSI: $msi"


