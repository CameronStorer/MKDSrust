@echo off
rem Starts the game built by build.bat (default folder builds\mkds).
if not exist "%~dp0builds\mkds\play.exe" (
  echo No MKDSrust build found in builds\mkds. Run: build.bat "C:\path\to\your\game.nds"
  exit /b 1
)
"%~dp0builds\mkds\play.exe" %*
