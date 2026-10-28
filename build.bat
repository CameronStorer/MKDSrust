@echo off
rem MKDSrust.
rem   Double-click (no arguments): a menu opens and asks for your .nds file; paste its path and press Enter.
rem   From a terminal: build.bat "path\to\your game.nds" ["output folder"]
setlocal
where cargo >nul 2>nul
if errorlevel 1 (
  echo Rust is not installed. Install it from https://rustup.rs, then open a NEW terminal window and try again.
  if "%~1"=="" pause
  exit /b 1
)
set "BUILDER=%~dp01_build_tool\target\release\mkds_builder.exe"
echo Preparing the MKDSrust builder (the first time this takes a few minutes)...
cargo build --release --locked --quiet --manifest-path "%~dp01_build_tool\Cargo.toml"
if errorlevel 1 (
  echo Could not build the builder. See the messages above.
  if "%~1"=="" pause
  exit /b 1
)
if "%~1"=="" (
  rem the menu asks for the ROM, builds into builds\mkds and can start the game afterwards
  "%BUILDER%" --pick-rom
  if errorlevel 1 pause
  exit /b
)
if not exist "%~1" (
  echo ROM not found: %~1
  exit /b 1
)
set "OUT=%~2"
if "%OUT%"=="" set "OUT=%~dp0builds\mkds"
"%BUILDER%" "%~1" --build --out "%OUT%"
if errorlevel 1 (
  echo.
  echo Build failed. See the messages above, or the Troubleshooting section of README.md.
  exit /b 1
)
echo.
echo Done! Start the game with:  play.bat   (or "%OUT%\play.exe")
