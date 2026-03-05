# Ensure Cargo is on PATH for this PowerShell session.
# Run: . .\scripts\ensure-cargo-path.ps1   (from repo root)
# Or:  & .\scripts\ensure-cargo-path.ps1

$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
if (Test-Path (Join-Path $cargoBin "cargo.exe")) {
    $env:Path = "$cargoBin;$env:Path"
    Write-Host "Cargo added to PATH for this session. You can now run: cargo build"
} else {
    Write-Host "Rust/Cargo not found at $cargoBin"
    Write-Host "Install Rust: https://rustup.rs/  (then restart the terminal)"
    exit 1
}
