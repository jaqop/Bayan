param(
    [Parameter(Mandatory=$true)][string]$Version,
    [string]$BinaryPath = 'target/release/bayan.exe',
    [string]$OutputDirectory = 'dist'
)
$ErrorActionPreference = 'Stop'
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if ($Version -notmatch '^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$') {
    throw 'Invalid package version'
}
if (-not [IO.Path]::IsPathRooted($BinaryPath)) { $BinaryPath = Join-Path $repo $BinaryPath }
if (-not [IO.Path]::IsPathRooted($OutputDirectory)) { $OutputDirectory = Join-Path $repo $OutputDirectory }
$name = "bayan-$Version-x86_64-pc-windows-msvc"
$dir = Join-Path $OutputDirectory $name
$zip = Join-Path $OutputDirectory "$name.zip"
if ((Test-Path -LiteralPath $dir) -or (Test-Path -LiteralPath $zip)) {
    throw 'Package output already exists; use a fresh output directory'
}
if (-not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) { throw 'Release binary missing' }
New-Item -ItemType Directory -Path $dir -Force | Out-Null
Copy-Item -LiteralPath $BinaryPath -Destination (Join-Path $dir 'bayan.exe')
foreach ($file in @('LICENSE','NOTICE','README.md','fonts/OFL.txt','fonts/OFL-KawkabMono.txt')) {
    Copy-Item -LiteralPath (Join-Path $repo $file) -Destination $dir
}
# Downloads the pinned pair and verifies both hashes and Microsoft signatures.
& (Join-Path $PSScriptRoot 'install-conpty.ps1') -OutputDirectory $dir
$files = @(Get-ChildItem -LiteralPath $dir -File | Sort-Object Name | ForEach-Object {
    @{name=$_.Name; sha256=(Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()}
})
@{version=$Version; architecture='x86_64-pc-windows-msvc'; files=$files} |
    ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $dir 'build-identity.json') -Encoding utf8
Compress-Archive -LiteralPath $dir -DestinationPath $zip
$hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
"$hash  $name.zip" | Set-Content -LiteralPath "$zip.sha256" -Encoding ascii -NoNewline
# Check the actual archive, not just the directory passed to Compress-Archive.
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [IO.Compression.ZipFile]::OpenRead($zip)
try {
    $required = @('bayan.exe','conpty.dll','OpenConsole.exe','ConPTY-LICENSE.txt',
        'LICENSE','NOTICE','README.md','OFL.txt','OFL-KawkabMono.txt','build-identity.json')
    $present = @($archive.Entries | Where-Object { $_.Name } | ForEach-Object { $_.Name })
    if ($present.Count -ne $required.Count -or @($required | Where-Object { $_ -notin $present }).Count) {
        throw 'Release archive is incomplete or contains unexpected files'
    }
} finally { $archive.Dispose() }
Write-Output "Created $zip ($hash)"
