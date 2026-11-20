# Install a checksum-verified release for the current Windows user.
& {
    $ErrorActionPreference = 'Stop'
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    $architecture = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    if ($architecture -ne 'AMD64') { throw 'anticrumb currently provides Windows x64 packages only.' }
    $requested = if ($env:ANTICRUMB_VERSION) { $env:ANTICRUMB_VERSION.Trim() } else { '' }
    $isNightly = $requested -ceq 'nightly'
    if ($isNightly) {
        $releaseUri = 'https://api.github.com/repos/ivere27/anticrumb/releases/tags/nightly'
        $notFoundMessage = 'No nightly anticrumb release is available yet. See https://github.com/ivere27/anticrumb/releases.'
    } elseif ($requested) {
        $stripped = if ($requested.StartsWith('v')) { $requested.Substring(1) } else { $requested }
        if ($stripped -cnotmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') {
            throw 'ANTICRUMB_VERSION must be nightly or a stable version such as 0.1.0.'
        }
        $requested = $stripped
        $releaseUri = "https://api.github.com/repos/ivere27/anticrumb/releases/tags/v$requested"
        $notFoundMessage = 'No stable anticrumb release is available yet. See https://github.com/ivere27/anticrumb/releases.'
    } else {
        $releaseUri = 'https://api.github.com/repos/ivere27/anticrumb/releases/latest'
        $notFoundMessage = 'No stable anticrumb release is available yet. See https://github.com/ivere27/anticrumb/releases.'
    }
    try {
        $release = Invoke-RestMethod -Uri $releaseUri -Headers @{
            Accept = 'application/vnd.github+json'; 'X-GitHub-Api-Version' = '2026-03-10'
        }
    } catch {
        $status = if ($_.Exception.Response) { [int]$_.Exception.Response.StatusCode } else { 0 }
        if ($status -eq 403 -or $status -eq 429) {
            throw 'GitHub API access was denied or rate-limited. Wait and retry, or download from https://github.com/ivere27/anticrumb/releases.'
        }
        if ($status -eq 404) { throw $notFoundMessage }
        throw 'Cannot contact GitHub releases; try again later.'
    }
    if ($isNightly) {
        if ($release.tag_name -isnot [string] -or $release.tag_name -cne 'nightly' -or
            $release.draft -isnot [bool] -or $release.draft -or $release.prerelease -isnot [bool] -or -not $release.prerelease -or
            $release.assets -isnot [array]) {
            throw 'Invalid nightly release metadata; nothing was installed.'
        }
        $version = 'nightly'
    } else {
        if ($release.tag_name -isnot [string] -or $release.tag_name -cnotmatch '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' -or
            ($requested -and $release.tag_name -cne "v$requested") -or
            $release.draft -isnot [bool] -or $release.draft -or $release.prerelease -isnot [bool] -or $release.prerelease -or
            $release.assets -isnot [array]) {
            throw 'Invalid stable release metadata; nothing was installed.'
        }
        $version = $release.tag_name.Substring(1)
    }
    $target = 'x86_64-pc-windows-msvc'
    $packages = @($release.assets | Where-Object { $_.name -ceq "anticrumb-$target.zip" })
    if ($packages.Count -eq 0) {
        $target = 'x86_64-pc-windows-gnu'
        $packages = @($release.assets | Where-Object { $_.name -ceq "anticrumb-$target.zip" })
    }
    if ($packages.Count -ne 1) { throw 'Release must contain exactly one package for this platform.' }
    $asset = $packages[0]
    $expectedName = "anticrumb-$target.zip"
    $expectedUrl = "https://github.com/ivere27/anticrumb/releases/download/$($release.tag_name)/$expectedName"
    if ($asset.name -cne $expectedName -or $asset.browser_download_url -cne $expectedUrl -or $asset.state -cne 'uploaded' -or
        $asset.digest -isnot [string] -or $asset.digest -cnotmatch '^sha256:[0-9a-fA-F]{64}$' -or
        ($asset.size -isnot [int] -and $asset.size -isnot [long]) -or $asset.size -le 0 -or $asset.size -gt 536870912) {
        throw 'Invalid package URL, size or SHA-256 asset digest; nothing was installed.'
    }
    $expectedDigest = $asset.digest.Substring(7)
    $scratch = Join-Path ([IO.Path]::GetTempPath()) ('anticrumb-install-' + [Guid]::NewGuid().ToString('N'))
    [IO.Directory]::CreateDirectory($scratch) | Out-Null
    $stagedFiles = @()
    $zip = $null
    try {
        $archive = Join-Path $scratch 'package.zip'
        Write-Host "Downloading anticrumb $version for $target..."
        Invoke-WebRequest -UseBasicParsing -Uri $asset.browser_download_url -OutFile $archive
        if ((Get-Item -LiteralPath $archive).Length -ne $asset.size) {
            throw 'Package size or SHA-256 mismatch; nothing was installed.'
        }
        $sha256 = [Security.Cryptography.SHA256]::Create()
        try {
            $stream = [IO.File]::OpenRead($archive)
            try {
                $hashBytes = $sha256.ComputeHash($stream)
                $actualDigest = [BitConverter]::ToString($hashBytes).Replace('-', '')
            } finally { $stream.Dispose() }
        } finally { $sha256.Dispose() }
        if ($actualDigest -ine $expectedDigest) {
            throw 'Package size or SHA-256 mismatch; nothing was installed.'
        }
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [IO.Compression.ZipFile]::OpenRead($archive)
        $installDir = if ($env:ANTICRUMB_INSTALL_DIR) { $env:ANTICRUMB_INSTALL_DIR } else { Join-Path $env:USERPROFILE '.local\bin' }
        [IO.Directory]::CreateDirectory($installDir) | Out-Null
        # Read only named regular entries. Archive directory paths are never extracted.
        foreach ($name in @('anticrumb.exe')) {
            $entries = @($zip.Entries | Where-Object { $_.FullName -ceq "anticrumb-$target/$name" })
            if ($entries.Count -ne 1) { throw "Package must contain exactly one $name." }
            $entry = $entries[0]
            $kind = ($entry.ExternalAttributes -shr 16) -band 0xF000
            if (($kind -ne 0 -and $kind -ne 0x8000) -or $entry.Length -le 0 -or $entry.Length -gt 268435456) {
                throw "Invalid regular-file entry for $name."
            }
            $destination = Join-Path $installDir $name
            if (Test-Path -LiteralPath $destination -PathType Container) { throw "Destination is a directory: $destination" }
            $staged = Join-Path $installDir ('.anticrumb-' + [Guid]::NewGuid().ToString('N'))
            $stagedFiles += @{ Source = $staged; Destination = $destination }
            $source = $entry.Open()
            try {
                $output = [IO.File]::Open($staged, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
                try { $source.CopyTo($output) } finally { $output.Dispose() }
            } finally { $source.Dispose() }
        }
        $zip.Dispose()
        $zip = $null
        foreach ($file in $stagedFiles) {
            if (Test-Path -LiteralPath $file.Destination) {
                [IO.File]::Replace($file.Source, $file.Destination, [NullString]::Value)
            } else { [IO.File]::Move($file.Source, $file.Destination) }
        }
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        if (@($userPath -split ';' | Where-Object { $_.TrimEnd('\') -ieq $installDir.TrimEnd('\') }).Count -eq 0) {
            [Environment]::SetEnvironmentVariable('Path', "$installDir;$userPath", 'User')
        }
        if (@($env:Path -split ';' | Where-Object { $_.TrimEnd('\') -ieq $installDir.TrimEnd('\') }).Count -eq 0) {
            $env:Path = "$installDir;$env:Path"
        }
        Write-Host "Installed anticrumb $version to $installDir"
        Write-Host 'Run: anticrumb'
        Write-Host 'First use: /browser login. Models load automatically after sign-in.'
        if (-not (Get-Command rg -ErrorAction SilentlyContinue)) { Write-Host 'Local coding tools also need ripgrep (rg).' }
    } finally {
        if ($zip) { $zip.Dispose() }
        foreach ($file in $stagedFiles) {
            if (Test-Path -LiteralPath $file.Source) { Remove-Item -LiteralPath $file.Source -Force }
        }
        Remove-Item -LiteralPath $scratch -Recurse -Force
    }
}
