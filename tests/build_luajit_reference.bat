@echo off
REM Builds LuaJIT 2.1 with MSVC and names the output lua51_64.dll.
REM
REM X4's Windows build ships its Lua runtime as lua51_64.dll, and the
REM reference native module resolves its symbols from exactly that name.
REM Building the same DLL here lets the native module be tested on Windows
REM the way the game loads it.
setlocal
set SOURCE=%~1
set OUTPUT=%~2
if "%SOURCE%"=="" set SOURCE=%TEMP%\LuaJIT\src
if "%OUTPUT%"=="" set OUTPUT=%TEMP%\LuaJIT\out
if not exist "%OUTPUT%" mkdir "%OUTPUT%"
cd /d "%SOURCE%"

call msvcbuild.bat
if errorlevel 1 (
  echo build failed
  exit /b 1
)

copy /y lua51.dll "%OUTPUT%\lua51_64.dll" >nul
copy /y lua51.lib "%OUTPUT%\lua51_64.lib" >nul
echo built %OUTPUT%\lua51_64.dll
endlocal
