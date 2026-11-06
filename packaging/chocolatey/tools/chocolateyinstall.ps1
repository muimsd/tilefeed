$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName    = $env:ChocolateyPackageName
  unzipLocation  = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
  url64bit       = "https://github.com/muimsd/tilefeed/releases/download/v$($env:ChocolateyPackageVersion)/tilefeed-x86_64-pc-windows-msvc.zip"
  checksum64     = '7abedf791664e51013c53e1b0967931b8cc5ff196c703e83864d6230fb468f9a'
  checksumType64 = 'sha256'
}

Install-ChocolateyZipPackage @packageArgs
