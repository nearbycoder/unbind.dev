param([string] $BaseUrl = 'https://unbind.dev')

& {
    param([string] $BaseUrl)
    $ErrorActionPreference = 'Stop'

    $BaseUrl = $BaseUrl.TrimEnd('/')
    $Uri = [Uri] $BaseUrl
    if ($Uri.Scheme -ne 'https' -and -not ($Uri.Scheme -eq 'http' -and $Uri.IsLoopback)) {
        throw 'Use an HTTPS download URL.'
    }

    $Architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    $Asset = switch ($Architecture) {
        'X64' { 'unbind-windows-amd64.exe' }
        'Arm64' { 'unbind-windows-arm64.exe' }
        default { throw "Unsupported Windows architecture $Architecture." }
    }

    $Manifest = (Invoke-WebRequest -UseBasicParsing "$BaseUrl/downloads/SHA256SUMS").Content
    if ($Manifest -is [byte[]]) { $Manifest = [Text.Encoding]::UTF8.GetString($Manifest) }
    $ChecksumPattern = '^([a-fA-F0-9]{64})\s+\*?' + [Regex]::Escape($Asset) + '\s*$'
    $Expected = foreach ($Line in $Manifest -split "`n") {
        if ($Line -match $ChecksumPattern) { $Matches[1]; break }
    }
    if (-not $Expected) { throw "No published Windows build for $Architecture at $BaseUrl." }

    $BinDir = $env:UNBIND_BIN_DIR
    if (-not $BinDir) { $BinDir = Join-Path $env:LOCALAPPDATA 'Programs\unbind' }
    $null = New-Item -ItemType Directory -Path $BinDir -Force
    $Staged = Join-Path $BinDir ('.unbind-' + [Guid]::NewGuid().ToString('N') + '.tmp')
    try {
        Invoke-WebRequest -UseBasicParsing "$BaseUrl/downloads/$Asset" -OutFile $Staged
        $Actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $Staged).Hash
        if ($Actual -ne $Expected) { throw "Checksum mismatch for $Asset." }
        Move-Item -LiteralPath $Staged -Destination (Join-Path $BinDir 'unbind.exe') -Force
    } finally {
        if (Test-Path -LiteralPath $Staged) { Remove-Item -LiteralPath $Staged -Force }
    }

    $UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (($UserPath -split ';') -notcontains $BinDir) {
        $UpdatedPath = (@($UserPath, $BinDir) | Where-Object { $_ }) -join ';'
        [Environment]::SetEnvironmentVariable('Path', $UpdatedPath, 'User')
    }
    if (($env:Path -split ';') -notcontains $BinDir) { $env:Path = "$env:Path;$BinDir" }

    Write-Host "Installed unbind to $BinDir\unbind.exe"
    Write-Host 'Run unbind --help to get started.'
} $BaseUrl
