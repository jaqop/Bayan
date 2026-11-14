param(
    [Parameter(Mandatory=$true)][string]$OutputDirectory,
    [ValidateSet('x86_64','aarch64','x86')][string]$Architecture = 'x86_64'
)
$ErrorActionPreference = 'Stop'
$manifest = Get-Content (Join-Path $PSScriptRoot '../resources/conpty.json') -Raw | ConvertFrom-Json
$pins = $manifest.architectures.$Architecture
$stageRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$stage = Join-Path $stageRoot ('bayan-conpty-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $stage | Out-Null
try {
    $version = $manifest.version
    $package = Join-Path $stage 'conpty.zip'
    Invoke-WebRequest "https://api.nuget.org/v3-flatcontainer/microsoft.windows.console.conpty/$version/microsoft.windows.console.conpty.$version.nupkg" -OutFile $package
    Expand-Archive -LiteralPath $package -DestinationPath (Join-Path $stage 'package')
    $files = @{
        'conpty.dll' = Join-Path $stage "package/runtimes/win-$($pins.runtime)/native/conpty.dll"
        'OpenConsole.exe' = Join-Path $stage "package/build/native/runtimes/$($pins.runtime)/OpenConsole.exe"
    }
    foreach ($name in $files.Keys) {
        if ((Get-FileHash -LiteralPath $files[$name] -Algorithm SHA256).Hash -ine $pins.$name) {
            throw "Pinned SHA-256 mismatch: $name"
        }
        $signature = Get-AuthenticodeSignature -LiteralPath $files[$name]
        if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch '(^|, )CN=Microsoft Corporation(,|$)') {
            throw "Microsoft signature missing: $name"
        }
    }
    New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
    foreach ($name in $files.Keys) {
        Copy-Item -LiteralPath $files[$name] -Destination (Join-Path $OutputDirectory $name)
    }
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot '../resources/ConPTY-LICENSE.txt') -Destination (Join-Path $OutputDirectory 'ConPTY-LICENSE.txt')
    Write-Output "Installed pinned Microsoft ConPTY $version ($Architecture)."
} finally {
    # Only the unique directory created above, under the resolved temp root.
    $resolved = [IO.Path]::GetFullPath($stage)
    if ($resolved.StartsWith((Join-Path $stageRoot 'bayan-conpty-'), [StringComparison]::OrdinalIgnoreCase)) {
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}
