$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName    = $env:ChocolateyPackageName
  unzipLocation  = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
  url64bit       = "https://github.com/muimsd/postile/releases/download/v$($env:ChocolateyPackageVersion)/postile-x86_64-pc-windows-msvc.zip"
  checksum64     = '__CHECKSUM__'
  checksumType64 = 'sha256'
}

Install-ChocolateyZipPackage @packageArgs
