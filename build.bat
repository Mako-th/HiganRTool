@echo off
rem Build HiganRTool (Rust, i686-pc-windows-msvc)
rem   -> build\bin\HiganRTool.dll, build\bin\HiganRTool.exe, build\bin\peer_sim.exe
rem
rem NOTE: comments in this file are ASCII on purpose.
rem       cmd.exe reads .bat as the ANSI codepage here; UTF-8 Japanese
rem       comments corrupt line parsing. Do not add non-ASCII text to this file.
rem
rem NOTE: this machine has NoDefaultCurrentDirectoryInExePath=1, which can make
rem       tools invoked without a full path fail to find files in the current
rem       directory. It is cleared below.
rem
rem Steps: cargo build --release, cargo test, peer_sim's and the DLL's --ignored
rem real-time tests, copy + rename the outputs to build\bin, then check with objdump
rem that the DLL and the EXE are pei-i386 (th09.exe is 32-bit; a 64-bit DLL
rem cannot be loaded into it).
rem The target triple and static CRT come from .cargo\config.toml.
rem rustc finds the MSVC linker (VS Build Tools) by itself; no vcvars32 needed.
rem
rem Usage:  cmd /c build.bat [/skip-unit] [/skip-realtime]
rem         (from Git Bash: MSYS_NO_PATHCONV=1 cmd /c build.bat)
rem
rem Run by hand with no arguments, every step runs (1/4 to 4/4 including 2b and 2c).
rem tools/run_all_tests.py --full passes both switches, because it runs those steps
rem itself (2026-10-08, docs/investigation/Test_Time_20261008_WIP.md):
rem   /skip-unit      skip 2/4 (cargo test). run_all_tests.py runs the very same
rem                   command as its cargo_test step, so it was run twice (S1).
rem   /skip-realtime  skip 2b and 2c. run_all_tests.py runs them as its own steps:
rem                   2b as peer_sim_loopback, 2c split in two groups (the timing
rem                   tests one at a time, the rest in parallel) so that it takes
rem                   a few minutes instead of seven (S3).

setlocal
set "NoDefaultCurrentDirectoryInExePath="

set "ROOT=%~dp0"

set "SKIP_UNIT="
set "SKIP_REALTIME="
:parse_args
if "%~1"=="" goto args_done
if /i "%~1"=="/skip-unit" (
    set "SKIP_UNIT=1"
) else if /i "%~1"=="/skip-realtime" (
    set "SKIP_REALTIME=1"
) else (
    echo [build.bat] unknown argument: "%~1"
    echo [build.bat] usage: build.bat [/skip-unit] [/skip-realtime]
    exit /b 2
)
shift
goto parse_args
:args_done

set "TARGET=i686-pc-windows-msvc"
set "RELDIR=%ROOT%target\%TARGET%\release"
set "OUT=%ROOT%build\bin"
set "OBJDUMP=C:\toolkit\w64devkit-i686\bin\objdump.exe"

set "CARGO=cargo"
where cargo >nul 2>nul
if errorlevel 1 set "CARGO=%USERPROFILE%\.cargo\bin\cargo.exe"
if not "%CARGO%"=="cargo" if not exist "%CARGO%" (
    echo [build.bat] cargo not found on PATH nor at "%CARGO%"
    echo [build.bat] install: winget install Rustlang.Rustup, then rustup target add %TARGET%
    exit /b 1
)

echo === 1/4 cargo build --release ===
"%CARGO%" build --release --manifest-path "%ROOT%Cargo.toml"
if errorlevel 1 exit /b 1

if defined SKIP_UNIT goto after_unit
echo === 2/4 cargo test ===
"%CARGO%" test --manifest-path "%ROOT%Cargo.toml"
if errorlevel 1 exit /b 1
:after_unit

if defined SKIP_REALTIME goto after_realtime
rem peer_sim's loopback tests (tests/loopback.rs) use real-time waits and are
rem marked #[ignore] so the cheap run (tools/run_all_tests.py) skips them.
rem The answer only changes when the source/build changes, so run them here,
rem in the full run only, with --ignored (see the project test rules, Q1-Q3).
echo === 2b/4 cargo test -p peer_sim --test loopback -- --ignored ===
"%CARGO%" test --manifest-path "%ROOT%Cargo.toml" -p peer_sim --test loopback -- --ignored
if errorlevel 1 exit /b 1

rem The DLL's real-time tests (M2-1: the fake game + real UDP layer against
rem peer_sim over loopback, and the high-resolution timer precision) are also
rem #[ignore]; run them here in the full run only. One at a time: the M7 pacer
rem test measures OS wake-ups and failed once (7 late frames of 180) while the
rem loopback tests spun the CPU next to it (2026-09-28).
rem The selection is the whole workspace plus --lib (not -p higanrtool_dll), the
rem same way as step 2/4, so the DLL's test binary is not rebuilt (S4). That also
rem picks up two tools that are run by hand only; they are skipped by name.
rem MANUAL_TOOL_SKIPS in tools/run_all_tests.py has the same names (a test checks it).
echo === 2c/4 cargo test --lib -- --ignored --test-threads=1 ===
"%CARGO%" test --manifest-path "%ROOT%Cargo.toml" --lib -- --ignored --test-threads=1 --skip measure_ --skip built_build_bin_is_fresh
if errorlevel 1 exit /b 1
:after_realtime

echo === 3/4 copy outputs to build\bin ===
if not exist "%OUT%" mkdir "%OUT%"
call :copyout "%RELDIR%\higanrtool.dll" "%OUT%\HiganRTool.dll" || exit /b 1
call :copyout "%RELDIR%\higanrtool_loader.exe" "%OUT%\HiganRTool.exe" || exit /b 1
call :copyout "%RELDIR%\peer_sim.exe" "%OUT%\peer_sim.exe" || exit /b 1

echo === 4/4 verify: pei-i386 ===
if not exist "%OBJDUMP%" (
    echo [build.bat] objdump.exe not found: "%OBJDUMP%"
    exit /b 1
)
call :verify "%OUT%\HiganRTool.dll" || exit /b 1
call :verify "%OUT%\HiganRTool.exe" || exit /b 1

echo === output: %OUT%\HiganRTool.dll, %OUT%\HiganRTool.exe, %OUT%\peer_sim.exe ===
exit /b 0

:copyout
if not exist "%~1" (
    echo [build.bat] missing output: "%~1"
    exit /b 1
)
copy /y "%~1" "%~2" >nul
if errorlevel 1 (
    echo [build.bat] copy failed: "%~1" to "%~2"
    exit /b 1
)
exit /b 0

:verify
"%OBJDUMP%" -f "%~1" > "%TEMP%\higanrtool_objdump_check.txt"
findstr /c:"pei-i386" "%TEMP%\higanrtool_objdump_check.txt" >nul
if errorlevel 1 (
    echo [build.bat] not pei-i386: "%~1"
    type "%TEMP%\higanrtool_objdump_check.txt"
    del "%TEMP%\higanrtool_objdump_check.txt" >nul 2>nul
    exit /b 1
)
findstr /c:"file format" "%TEMP%\higanrtool_objdump_check.txt"
del "%TEMP%\higanrtool_objdump_check.txt" >nul 2>nul
exit /b 0
