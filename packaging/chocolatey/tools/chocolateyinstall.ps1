$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName    = $env:ChocolateyPackageName
  unzipLocation  = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
  url64bit       = "https://github.com/muimsd/tilefeed/releases/download/v$($env:ChocolateyPackageVersion)/tilefeed-x86_64-pc-windows-msvc.zip"
  checksum64     = 'bf7a744d4e188fbd4e719961deb6ed4128eaa605ef71091daf202d456beca24b'
  checksumType64 = 'sha256'
}

Install-ChocolateyZipPackage @packageArgs
