param(
    [string]$ToolsHome = 'C:\Users\TUSB\Desktop\agent\environment',
    [string]$Ndk = '',
    [ValidateRange(21, 37)][int]$AndroidApi = 30,
    [string]$Package = 'skyward',
    [string]$Builder = 'TUSB-5834',
    [switch]$HostBuild,
    [switch]$Online,
    [switch]$Test
)
$ErrorActionPreference = 'Stop'
$Builder = $Builder.Trim()
if ($Builder.Contains("`r") -or $Builder.Contains("`n")) { throw 'Builder must fit on one line' }
$env:SKYWARD_BUILDER = $Builder
$workspace = Split-Path $PSScriptRoot -Parent
if (!$Ndk) { $Ndk = Join-Path $ToolsHome 'android-ndk-r29' }
$rust = Join-Path $ToolsHome 'rustup\toolchains\stable-x86_64-pc-windows-gnu'
$llvm = Join-Path $Ndk 'toolchains\llvm\prebuilt\windows-x86_64\bin'
$shim = Join-Path $PSScriptRoot 'host-bin'
foreach ($file in @("$rust\bin\cargo.exe", "$rust\bin\rustc.exe", "$llvm\llvm-dlltool.exe", "$llvm\aarch64-linux-android$AndroidApi-clang.cmd")) {
    if (!(Test-Path -LiteralPath $file)) { throw "Required tool missing: $file" }
}
New-Item -ItemType Directory -Force -Path $shim | Out-Null
Copy-Item -LiteralPath "$llvm\llvm-dlltool.exe" -Destination "$shim\dlltool.exe" -Force
$env:PATH = "$shim;$rust\bin;$llvm;" + $env:PATH
$env:CARGO_HOME = Join-Path $ToolsHome 'cargo'
$env:RUSTC = "$rust\bin\rustc.exe"
$env:CARGO_HTTP_MULTIPLEXING = 'false'
$env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "$rust\lib\rustlib\x86_64-pc-windows-gnu\bin\rust-lld.exe"
$env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = '-C linker-flavor=ld.lld -C link-self-contained=yes'
$env:CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER = "$llvm\aarch64-linux-android$AndroidApi-clang.cmd"
$env:CC_aarch64_linux_android = $env:CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER
$env:AR_aarch64_linux_android = "$llvm\llvm-ar.exe"
$buildTarget = if ($HostBuild) { 'x86_64-pc-windows-gnu' } else { 'aarch64-linux-android' }
$cargoArgs = @($(if ($Test) { 'test' } else { 'build' }), '--locked', '--release', '--target', $buildTarget, '-p', $Package)
if (!$Online) { $cargoArgs += '--offline' }
$taskAndroidFlags = $env:CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS
$taskStaticInit = !$HostBuild -and $Package -eq 'skyward-system-init'
if ($taskStaticInit) {
    $env:CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS = '-C target-feature=+crt-static'
}
Push-Location $workspace
try {
    & "$rust\bin\cargo.exe" @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo exited with $LASTEXITCODE" }
} finally {
    Pop-Location
    if ($taskStaticInit) { $env:CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS = $taskAndroidFlags }
}
