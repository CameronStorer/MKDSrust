@echo off
rem MKDSrust menu: play, build the game from your ROM, change the controls.
setlocal
where cargo >nul 2>nul
if errorlevel 1 (
  echo Rust is not installed. Install it from https://rustup.rs, then open a NEW terminal window and try again.
  pause
  exit /b 1
)
echo Preparing the MKDSrust menu (the first time this takes a few minutes)...
cargo build --release --locked --quiet --manifest-path "%~dp01_build_tool\Cargo.toml"
if errorlevel 1 (
  echo Could not build the builder. See the messages above.
  pause
  exit /b 1
)
"%~dp01_build_tool\target\release\mkds_builder.exe" %*
if errorlevel 1 pause
