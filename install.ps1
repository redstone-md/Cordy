# Cordy installer for Windows.
#   irm https://raw.githubusercontent.com/redstone-md/Cordy/main/install.ps1 | iex
$ErrorActionPreference = 'Stop'

$repo = 'redstone-md/Cordy'
$bin  = 'cordy'

if (-not [Environment]::Is64BitOperatingSystem) { throw 'Cordy ships 64-bit Windows binaries only.' }
$target = 'x86_64-pc-windows-msvc'

Write-Host ':: resolving the latest release' -ForegroundColor Blue
$rel = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest"
$tag = $rel.tag_name
if (-not $tag) { throw 'could not resolve the latest release tag' }

$asset = "$bin-$tag-$target.zip"
$base  = "https://github.com/$repo/releases/download/$tag"
$tmp   = Join-Path $env:TEMP ("cordy-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$zip = Join-Path $tmp $asset

Write-Host ":: downloading $asset" -ForegroundColor Blue
Invoke-WebRequest "$base/$asset" -OutFile $zip

# Verify the checksum when SHA256SUMS is present.
try {
  $sums = (Invoke-WebRequest "$base/SHA256SUMS").Content
  $line = ($sums -split "`n") | Where-Object { $_ -match [regex]::Escape($asset) } | Select-Object -First 1
  if ($line) {
    $want = ($line -split '\s+')[0].ToLower()
    $got  = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
    if ($want -ne $got) { throw "checksum mismatch for $asset" }
    Write-Host ':: checksum verified' -ForegroundColor Blue
  }
} catch {
  if ($_.Exception.Message -like '*checksum*') { throw }
}

$dir = Join-Path $env:LOCALAPPDATA 'Cordy'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
Expand-Archive -Path $zip -DestinationPath $tmp -Force
$exe = Get-ChildItem -Path $tmp -Recurse -Filter 'cordy.exe' | Select-Object -First 1
if (-not $exe) { throw 'binary not found in archive' }
Copy-Item $exe.FullName (Join-Path $dir 'cordy.exe') -Force
Remove-Item $tmp -Recurse -Force

# Add to the user PATH if it isn't there yet.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $dir) {
  [Environment]::SetEnvironmentVariable('Path', "$userPath;$dir", 'User')
  Write-Host ":: added $dir to your PATH (restart the terminal to pick it up)" -ForegroundColor Blue
}

Write-Host ":: installed $bin $tag -> $dir\cordy.exe" -ForegroundColor Green
