```bat
@echo off
title GTS RPC - Build and Run

cd /d "%~dp0Discord RPC Rust"

echo.
echo ========================================
echo       GTS RPC - BUILD AND RUN
echo ========================================
echo.

echo [1/2] Building latest release...
cargo build --release

if errorlevel 1 (
    echo.
    echo BUILD FAILED!
    pause
    exit /b 1
)

echo.
echo [2/2] Launching GTS RPC...
echo.

start "" "%~dp0Discord RPC Rust\target\release\gts-rpc-gui.exe"

exit /b 0
```