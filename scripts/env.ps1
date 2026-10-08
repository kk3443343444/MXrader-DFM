# env.ps1 — 在 Windows 上准备 Rust/工具链环境（dot-source 一下即可）
#
#     . .\scripts\env.ps1
#     cargo test
#
# 为什么需要它：
#   * rustup 装完之后新加的 PATH 只在**新开的**进程里生效。DSH 里的 pwsh 是新进程
#     但继承的是宿主的旧环境，所以这里显式把路径接上。
#   * Windows 没有内置链接器：这里用 winget 装的 WinLibs MinGW（GNU 工具链），
#     因此必须告诉 cargo 用哪个 gcc 当 linker。
#
# 依赖（一次性安装，已在本机装好）：
#   winget install --id Rustlang.Rustup -e
#   winget install --id BrechtSanders.WinLibs.POSIX.UCRT -e
#   rustup toolchain install stable-x86_64-pc-windows-gnu

$ErrorActionPreference = 'Continue'

$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'

# WinLibs 由 winget 装到 Packages 目录下，包名里带源标识，所以用通配符找。
$mingwBin = $null
$pkgRoot = Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages'
if (Test-Path $pkgRoot) {
    $gcc = Get-ChildItem $pkgRoot -Recurse -Filter 'gcc.exe' -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match 'mingw64\\bin\\gcc\.exe$' } |
        Select-Object -First 1
    if ($gcc) { $mingwBin = $gcc.DirectoryName }
}

if (-not (Test-Path $cargoBin)) {
    Write-Warning "找不到 $cargoBin —— 先跑：winget install --id Rustlang.Rustup -e"
} else {
    $env:Path = "$cargoBin;" + ($env:Path -split ';' | Where-Object { $_ -and $_ -ne $cargoBin } ) -join ';'
}

if ($mingwBin) {
    $env:Path = "$mingwBin;" + (($env:Path -split ';' | Where-Object { $_ -and $_ -ne $mingwBin }) -join ';')
    # 让 cargo 的 GNU 目标用 MinGW 的 gcc 当 linker。
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = Join-Path $mingwBin 'gcc.exe'
    $env:CARGO_BUILD_TARGET = ''   # 不强制目标，用 toolchain 默认
} else {
    Write-Warning "找不到 MinGW 的 gcc.exe —— GNU 目标会链接失败。跑：winget install --id BrechtSanders.WinLibs.POSIX.UCRT -e"
}

$env:CARGO_TERM_COLOR = 'never'

Write-Host "cargo  : $(if (Get-Command cargo -ErrorAction SilentlyContinue) { (& cargo --version) } else { '缺失' })"
Write-Host "gcc    : $(if ($mingwBin) { & (Join-Path $mingwBin 'gcc.exe') --version | Select-Object -First 1 } else { '缺失' })"
Write-Host ""
Write-Host "用法（建议始终显式带 GNU toolchain）："
Write-Host "  cd core"
Write-Host "  cargo +stable-x86_64-pc-windows-gnu test --no-fail-fast"
Write-Host "  cargo +stable-x86_64-pc-windows-gnu run --bin battle_replay -- --capture <ndjson> --sweep"
