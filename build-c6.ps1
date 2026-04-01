$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root

cargo build `
  --release `
  --bin uavled `
  --no-default-features `
  --features chip-c6 `
  --target riscv32imac-unknown-none-elf
