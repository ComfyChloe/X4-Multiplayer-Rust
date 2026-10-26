@echo off
REM Builds lua51_64.dll from the Lua 5.1.5 sources.
REM
REM X4's Windows build ships its Lua runtime as lua51_64.dll, and the C
REM reference module resolves its symbols from exactly that name. Building
REM the same DLL here lets the native module be tested on Windows the way
REM the game loads it, instead of only on Linux.
setlocal
set SOURCE=%~1
set OUTPUT=%~2
if "%SOURCE%"=="" set SOURCE=%TEMP%\lua-5.1.5
if "%OUTPUT%"=="" set OUTPUT=%TEMP%\lua-5.1.5\build
if not exist "%OUTPUT%" mkdir "%OUTPUT%"
cd /d "%SOURCE%\src"

rem The core and the standard library become one DLL. LUA_BUILD_AS_DLL makes
rem every LUA_API and LUALIB_API function a dllexport, which is the set the
rem native module resolves.
cl /nologo /LD /O2 /MT /DLUA_BUILD_AS_DLL /DLUA_CORE /DLUA_LIB /I. ^
   lapi.c lauxlib.c lbaselib.c lcode.c ldebug.c ldblib.c ldo.c ldump.c ^
   lfunc.c lgc.c linit.c liolib.c llex.c lmathlib.c lmem.c loadlib.c ^
   lobject.c lopcodes.c loslib.c lparser.c lstate.c lstring.c lstrlib.c ^
   ltable.c ltablib.c ltm.c lundump.c lvm.c lzio.c ^
   /link /OUT:"%OUTPUT%\lua51_64.dll" /IMPLIB:"%OUTPUT%\lua51_64.lib"

if errorlevel 1 (
  echo build failed
  exit /b 1
)
echo built %OUTPUT%\lua51_64.dll
endlocal
