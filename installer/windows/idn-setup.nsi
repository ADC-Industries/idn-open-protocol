; IDN Media for Windows (the product's name since 2026-11-01; the `idn` command keeps
; its name): the IDN Node service with its control panel and the session agent
; (idn-session: no window; IDN Media's UI is the host application's), the `idn`
; command (the tools, idn-audio and the mixing desk; roadmap R.17), the video switcher, and
; the OBS Studio plugin; FFmpeg 8 for idn-codec and the switcher is downloaded on request
; (spec/extensions/idn-node.md §N.12).
;
; Built on Linux with NSIS 3: installer/windows/build.sh DIR OUT, where DIR holds the
; programs as the Windows zip has them.

Unicode true
Target amd64-unicode
ManifestDPIAware true
SetCompressor /SOLID lzma
RequestExecutionLevel admin

!include MUI2.nsh
!include LogicLib.nsh
!include Sections.nsh
!include nsDialogs.nsh
!include x64.nsh
!include FileFunc.nsh
!include WinVer.nsh
!include StrFunc.nsh
${StrStr}
${UnStrStr}

!define PUBLISHER "ADC Industries"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\IDN"
!define REG_KEY "Software\ADC Industries\IDN"
; The Start menu folder ("IDN" before 2026-11-01: removed on upgrade). The registry keys
; above keep the old name, so an upgrade finds the installed copy.
!define SM "$SMPROGRAMS\IDN Media"
!define OLD_SM "$SMPROGRAMS\IDN"
!define RUN_KEY "Software\Microsoft\Windows\CurrentVersion\Run"
!define TRAY_VALUE "IDN Node tray icon"
; The session agent at every log-on (in place of the tray icon, under its own name).
!define SESSION_VALUE "IDN session"
; A fixed month-end build (BtbN keeps these), checked against its SHA-256 after download.
; LGPL: no GPL parts, which IDN doesn't need (hardware encoders, software decoders).
!define FFMPEG_VERSION "8.1.3"
!define FFMPEG_FOLDER "ffmpeg-n8.1.3-9-g29e619e767-win64-lgpl-shared-8.1"
!define FFMPEG_URL "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-09-30-13-08/${FFMPEG_FOLDER}.zip"
!define FFMPEG_SHA256 "3E47BDA1607740550141E37C0E49D1E5182B34699F15ADFD137EE266D346811A"

Name "IDN Media"
Caption "IDN Media ${VERSION} (build ${BUILD}) Setup"
UninstallCaption "Uninstall IDN Media"
OutFile "${OUT}"
InstallDir "$PROGRAMFILES64\IDN Media"
InstallDirRegKey HKLM "${REG_KEY}" "InstallDir"
BrandingText "IDN Media, build ${BUILD}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "IDN Media"
VIAddVersionKey "CompanyName" "${PUBLISHER}"
VIAddVersionKey "FileDescription" "IDN Media Setup"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION} (build ${BUILD})"
VIAddVersionKey "LegalCopyright" "ADC Industries. MIT OR Apache-2.0 (the OBS plugin: GPL-2.0-or-later)"

!define MUI_ICON "${SRC}\idn.ico"
!define MUI_UNICON "${SRC}\idn.ico"
!define MUI_ABORTWARNING
!define MUI_COMPONENTSPAGE_SMALLDESC

Var ObsDir
; How the Node runs: "service" (default) or "logon" (per user, started by the session
; agent, idn-session).
Var NodeMode
Var ServiceRadio
Var LogonRadio
Var SitePass
Var SiteServer
Var PassField
Var ServerField
; The IDN Site Manager on this computer (idn-node.md §N.12): its site password, and the
; page's two fields.
Var SmPass
Var SmPassField
Var SmPass2Field

!define MUI_WELCOMEPAGE_TEXT "This installs IDN Media on this computer:$\r$\n$\r$\n\
  - the IDN Node, a service that holds this machine's IDN site identity, with its control panel (IDN Node in the Start menu) and idn-session, its side in each log-on (no window)$\r$\n\
  - the idn command: the IDN tools, sound cards (ASIO) and the mixing desk$\r$\n\
  - the IDN video switcher$\r$\n\
  - the IDN ASIO driver: IDN channels in any ASIO program$\r$\n\
  - the IDN plugin for OBS Studio, if OBS Studio is installed$\r$\n$\r$\n\
  Close OBS Studio before you continue."
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${SRC}\LICENSE.txt"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
Page custom ModePage ModePageLeave
Page custom SiteManagerPage SiteManagerPageLeave
Page custom SitePage SitePageLeave
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Open the IDN Node control panel"
!define MUI_FINISHPAGE_RUN_FUNCTION OpenPanel
!define MUI_FINISHPAGE_SHOWREADME "$INSTDIR\README-windows.txt"
!define MUI_FINISHPAGE_SHOWREADME_TEXT "Show the read-me"
!define MUI_FINISHPAGE_SHOWREADME_NOTCHECKED
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; Programs that take connections from the network: one firewall rule each (private and
; domain networks), removed again by the uninstaller.
!macro Firewall Action
  ; IDN Remote's rule from an older install (the app is gone): deleted on install and
  ; uninstall alike.
  nsExec::Exec 'netsh advfirewall firewall delete rule name="IDN: idn-remote"'
  Pop $0
  !insertmacro FirewallRule ${Action} idn
  !insertmacro FirewallRule ${Action} idn-mixer
  !insertmacro FirewallRule ${Action} idn-switcher
  !insertmacro FirewallRule ${Action} idn-node
  !insertmacro FirewallRule ${Action} idn-send
  !insertmacro FirewallRule ${Action} idn-recv
  !insertmacro FirewallRule ${Action} idn-codec
  !insertmacro FirewallRule ${Action} idn-audio
  !insertmacro FirewallRule ${Action} idn-channel
  !insertmacro FirewallRule ${Action} idn-publish
  !insertmacro FirewallRule ${Action} idn-find
  !insertmacro FirewallRule ${Action} idn-doctor
  !insertmacro FirewallRule ${Action} idn-ptz
  !insertmacro FirewallRule ${Action} idn-discovery-server
  ; Receivers too (their UDP, QUIC and multi-TCP ports, multicast; F1-4).
  !insertmacro FirewallRule ${Action} idn-display
  ; The network audio device (idn-aoip.md): controllers and other devices reach it.
  !insertmacro FirewallRule ${Action} idn-aoip
  ; An audio network's own card (no gateway) is an "unidentified network", which Windows
  ; puts on the public profile: the device is let in there too, from its own subnet only
  ; (bug run I4-10).
  nsExec::Exec 'netsh advfirewall firewall delete rule name="IDN: idn-aoip (audio network)"'
  Pop $0
  ; The site clock (in the Node) follows the audio network's PTP leader on that card too
  ; (bug run J5-2: without it the clock never locked there): its PTP ports only (bug run
  ; K8-1: the whole Node was open on public networks, a café's Wi-Fi among them).
  nsExec::Exec 'netsh advfirewall firewall delete rule name="IDN: idn-node (audio network)"'
  Pop $0
  !if ${Action} == add
    nsExec::Exec 'netsh advfirewall firewall add rule name="IDN: idn-aoip (audio network)" dir=in action=allow profile=public remoteip=localsubnet program="$INSTDIR\idn-aoip.exe" enable=yes'
    Pop $0
    nsExec::Exec 'netsh advfirewall firewall add rule name="IDN: idn-node (audio network)" dir=in action=allow profile=public remoteip=localsubnet protocol=udp localport=319,320 program="$INSTDIR\idn-node.exe" enable=yes'
    Pop $0
  !endif
!macroend

!macro FirewallRule Action Program
  nsExec::Exec 'netsh advfirewall firewall delete rule name="IDN: ${Program}"'
  Pop $0
  !if ${Action} == add
    nsExec::Exec 'netsh advfirewall firewall add rule name="IDN: ${Program}" dir=in action=allow profile=private,domain program="$INSTDIR\${Program}.exe" enable=yes'
    Pop $0
  !endif
!macroend

; The tools' old names (idn-send.exe, …): the `idn` program under that name (a hard
; link, or a copy where the disk can't hold one), which runs that command (R.17).
!macro OldName Name
  Delete "$INSTDIR\${Name}.exe"
  nsExec::Exec 'cmd.exe /C mklink /H "$INSTDIR\${Name}.exe" "$INSTDIR\idn.exe"'
  Pop $0
  ${If} $0 != 0
    CopyFiles /SILENT "$INSTDIR\idn.exe" "$INSTDIR\${Name}.exe"
  ${EndIf}
!macroend

!macro OldNames Action
  !insertmacro ${Action} idn-send
  !insertmacro ${Action} idn-recv
  !insertmacro ${Action} idn-find
  !insertmacro ${Action} idn-doctor
  !insertmacro ${Action} idn-publish
  !insertmacro ${Action} idn-channel
  !insertmacro ${Action} idn-ptz
  !insertmacro ${Action} idn-dsctl
  !insertmacro ${Action} idn-discovery-server
  !insertmacro ${Action} idn-audio
  !insertmacro ${Action} idn-mixer
!macroend

!macro DeleteExe Name
  Delete "$INSTDIR\${Name}.exe"
!macroend

; Waits until OBS Studio is closed (its plugin folder is in use while it runs).
!macro WaitForObs Prefix
  ${Do}
    nsExec::ExecToStack 'tasklist /FI "IMAGENAME eq obs64.exe" /NH'
    Pop $0
    Pop $1
    ${${Prefix}StrStr} $2 $1 "obs64.exe"
    ${If} $2 == ""
      ${Break}
    ${EndIf}
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "OBS Studio is running. Close it, then choose Retry.$\r$\n$\r$\nCancel skips the OBS plugin." /SD IDCANCEL IDRETRY +2
    ${Break}
  ${Loop}
!macroend

; IDN Voice's app from an older IDN Media install, stopped: only this folder's copy, by
; its path (taskkill can't tell copies apart: IDN Industrial's own idn-voice.exe runs on).
!macro KillOurVoice
  nsExec::Exec `powershell.exe -NoProfile -NonInteractive -Command "Get-Process -Name idn-voice -ErrorAction SilentlyContinue | Where-Object { $$_.Path -eq '$INSTDIR\idn-voice.exe' } | Stop-Process -Force"`
  Pop $0
!macroend

; IDN Voice's start at log-in (the user's own, set by the app): removed only when it
; starts this folder's copy (IDN Industrial's stays).
!macro DeleteOurVoiceRun
  ReadRegStr $0 HKCU "${RUN_KEY}" "IDN Voice"
  StrCpy $1 $0 1
  ${If} $1 == '"'
    StrCpy $0 $0 "" 1
  ${EndIf}
  StrLen $1 "$INSTDIR\idn-voice.exe"
  StrCpy $2 $0 $1
  ${If} $2 == "$INSTDIR\idn-voice.exe"
    DeleteRegValue HKCU "${RUN_KEY}" "IDN Voice"
  ${EndIf}
!macroend

!macro KillAll
  nsExec::Exec 'taskkill /F /IM idn.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-plugin-host.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-codec.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-audio.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-send.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-recv.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-discovery-server.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-channel.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-doctor.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-dsctl.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-find.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-publish.exe'
  Pop $0
!macroend

Section "IDN Node, control panel and tools" SecCore
  SectionIn RO
  ; An earlier installation (or idn-node.exe installed by hand): the session agent and the
  ; service stop so their files can be replaced. The machine's site identity stays.
  nsExec::Exec 'taskkill /F /IM idn-session.exe'
  Pop $0
  nsExec::Exec 'net stop idn-node'
  Pop $0
  ; Users' own Nodes ("when you log on"); their session agents start them again.
  nsExec::Exec 'taskkill /F /IM idn-node.exe'
  Pop $0
  ; The mixer and the switcher, if running, so their programs can be replaced.
  nsExec::Exec 'taskkill /F /IM idn-mixer.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-switcher.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-display.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-remote.exe'
  Pop $0
  ; IDN Voice's app from an older install: only this folder's copy (IDN Industrial has
  ; one of its own, which keeps running).
  !insertmacro KillOurVoice
  nsExec::Exec 'taskkill /F /IM idn-aoip.exe'
  Pop $0
  ; Every other program of ours: `idn.exe` and its other names (hard links to it: any
  ; one running holds idn.exe), the plugin host, the codec tools, the panel window (bug
  ; round 2026-11-09: a silent update could stop half-way on a held idn.exe).
  !insertmacro KillAll

  SetOutPath "$INSTDIR"
  ; The programs' folder: SYSTEM's and administrators', users may read and run. The
  ; service runs from it as SYSTEM; in a folder users could write (any folder outside
  ; Program Files, as D:\IDN Media), any user could replace idn-node.exe or a DLL it
  ; loads and run as SYSTEM.
  ; (Owned by administrators first: a folder a user made before, D:\IDN Media, stayed
  ; theirs, and its owner can always give itself access again.)
  nsExec::Exec 'icacls "$INSTDIR" /setowner *S-1-5-32-544 /T /C /Q'
  Pop $0
  ; Its list set whole: reset to what it inherits (every entry of its own gone: /grant:r
  ; alone left a user's), then only these; what is in it inherits that (bug round
  ; 2026-11-09).
  nsExec::Exec 'icacls "$INSTDIR" /reset /Q'
  Pop $0
  nsExec::Exec 'icacls "$INSTDIR" /inheritance:r /grant:r *S-1-5-18:(OI)(CI)F *S-1-5-32-544:(OI)(CI)F *S-1-5-32-545:(OI)(CI)RX /Q'
  Pop $0
  nsExec::Exec 'icacls "$INSTDIR\*" /reset /T /C /Q'
  Pop $0
  File "${SRC}\idn-node.exe"
  ; The Node's side in each desktop session (no window: IDN Media has no UI but the Node's
  ; control panel; the UI is the host application's).
  File "${SRC}\idn-session.exe"
  ; Direct3D 12's shader compiler (Microsoft's DXC, LLVM/NCSA licence), beside the programs.
  File "${SRC}\dxcompiler.dll"
  File "${SRC}\dxcompiler-LICENSE.txt"
  File "${SRC}\idn.exe"
  File "${SRC}\idn-codec.exe"
  File "${SRC}\idn-switcher.exe"
  ; Screens: an output full screen on a display (Wave 3).
  File "${SRC}\idn-display.exe"
  ; IDN Media has no UI but the Node's control panel (since 2026-11-18: the UI is the
  ; host application's): an older install's IDN Remote and IDN Voice apps go.
  Delete "$INSTDIR\idn-remote.exe"
  Delete "$INSTDIR\idn-voice.exe"
  !insertmacro DeleteOurVoiceRun
  File "${SRC}\idn-plugin-host.exe"
  ; The network audio device: this computer on a network of Dante-compatible equipment
  ; (unofficial), run by the Node when it is switched on (IDN's own engine, MIT/Apache).
  File "${SRC}\idn-aoip.exe"
  File "${SRC}\idn_asio.dll"
  ; IDN Camera's media source (Windows 11's Frame Server loads it; idn-display creates the camera).
  File "${SRC}\idn_camera.dll"
  ; A lab diagnostic (in the zip only); an older install's copy goes.
  Delete "$INSTDIR\capture-check.exe"
  !insertmacro OldNames OldName
  File "${SRC}\idn.ico"
  File "${SRC}\LICENSE.txt"
  File "${SRC}\BUILD.txt"
  File "${SRC}\README-windows.txt"
  File "${SRC}\idn-node-README.md"
  File "${SRC}\idn-audio-README.md"
  File "${SRC}\idn-obs-README.md"
  File "${SRC}\idn-mixer-README.md"
  File "${SRC}\idn-switcher-README.md"
  ; The user guide (G7), pages read in the browser.
  RMDir /r "$INSTDIR\guide"
  SetOutPath "$INSTDIR\guide"
  File /r "${SRC}\guide\*"
  SetOutPath "$INSTDIR"

  ${If} $NodeMode == "logon"
    ; No service: the session agent (idn-session) starts each user's Node at log-on
    ; (idn-node run --user). One
    ; installed before goes; its state (%ProgramData%\IDN Node) stays.
    DetailPrint "The IDN Node will run when someone logs on (no service)..."
    nsExec::ExecToLog '"$INSTDIR\idn-node.exe" uninstall'
    Pop $0
  ${Else}
    DetailPrint "Starting the IDN Node service..."
    ; Let a service raise Ctrl+Alt+Del through SendSAS, so a screen under remote control
    ; (R.25) can be given it: the Node does it for the screen's capture. 1 = services only.
    ; What it was before, kept once (an upgrade doesn't take ours as "before"): put back
    ; when uninstalled, so the machine isn't left letting any service raise it.
    ReadRegStr $1 HKLM "${REG_KEY}" "SasBefore"
    ${If} $1 == ""
      ClearErrors
      ReadRegDWORD $1 HKLM "Software\Microsoft\Windows\CurrentVersion\Policies\System" "SoftwareSASGeneration"
      ${If} ${Errors}
        StrCpy $1 "none"
      ${EndIf}
      WriteRegStr HKLM "${REG_KEY}" "SasBefore" "$1"
    ${EndIf}
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Policies\System" "SoftwareSASGeneration" 1
    nsExec::ExecToLog '"$INSTDIR\idn-node.exe" install'
    Pop $0
    ${If} $0 != 0
      MessageBox MB_ICONEXCLAMATION "The IDN Node service didn't start (see the details). The tools and the OBS plugin still work on their own." /SD IDOK
    ${EndIf}
  ${EndIf}
  WriteRegStr HKLM "${REG_KEY}" "NodeMode" "$NodeMode"

  DetailPrint "Allowing the IDN programs through Windows Defender Firewall (private networks)..."
  !insertmacro Firewall add

  ; The ASIO driver: "IDN" in every ASIO program's list of drivers.
  DetailPrint "Registering the IDN ASIO driver..."
  nsExec::ExecToLog '"$SYSDIR\regsvr32.exe" /s "$INSTDIR\idn_asio.dll"'
  nsExec::ExecToLog '"$SYSDIR\regsvr32.exe" /s "$INSTDIR\idn_camera.dll"'
  Pop $0

  ; Start menu (for all users).
  RMDir /r "${OLD_SM}"
  CreateDirectory "${SM}"
  CreateShortcut "${SM}\IDN Node.lnk" "$INSTDIR\idn-session.exe" "--open" \
    "$INSTDIR\idn.ico" 0 SW_SHOWNORMAL "" "Open the IDN Node control panel (in the browser)"
  ; The apps' shortcuts an older install made (no UI but the Node's panel now).
  Delete "${SM}\IDN Mixer.lnk"
  Delete "${SM}\IDN Switcher.lnk"
  Delete "${SM}\IDN Monitor.lnk"
  Delete "${SM}\IDN Voice.lnk"
  Delete "${SM}\IDN Remote.lnk"
  ; The Site Manager's panel an older install linked to (it has none now: its API,
  ; administered from a host application).
  Delete "${SM}\IDN Site Manager.url"
  CreateShortcut "${SM}\IDN command prompt.lnk" "$SYSDIR\cmd.exe" \
    '/k "set "PATH=$INSTDIR;%PATH%" && echo IDN: idn help lists the commands (idn find, idn doctor, idn send, idn recv, idn node status ...)"' \
    "$SYSDIR\cmd.exe" 0 SW_SHOWNORMAL "" "A command prompt for the IDN tools"
  CreateShortcut "${SM}\IDN Guide.lnk" "$INSTDIR\guide\index.html" "" \
    "$INSTDIR\idn.ico" 0 SW_SHOWNORMAL "" "The IDN user guide: setting up, running a service, troubleshooting"
  CreateShortcut "${SM}\IDN read-me.lnk" "$INSTDIR\README-windows.txt"
  CreateShortcut "${SM}\Uninstall IDN Media.lnk" "$INSTDIR\uninstall.exe"

  ; The session agent at every log-on (the old tray icon's entry gone).
  DeleteRegValue HKLM "${RUN_KEY}" "${TRAY_VALUE}"
  DeleteRegValue HKLM "${RUN_KEY}" "${SESSION_VALUE}"
  WriteRegStr HKLM "${RUN_KEY}" "${SESSION_VALUE}" '"$INSTDIR\idn-session.exe"'

  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKLM "${REG_KEY}" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "${REG_KEY}" "Build" "${BUILD}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayName" "IDN Media (Node, tools, mixer, switcher and OBS plugin)"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayVersion" "${VERSION} (build ${BUILD})"
  WriteRegStr HKLM "${UNINST_KEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\idn.ico"
  WriteRegStr HKLM "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair" 1
SectionEnd

Section "OBS Studio plugin" SecObs
  ${If} $ObsDir == ""
    Return
  ${EndIf}
  !insertmacro WaitForObs ""
  ${If} $2 != ""
    DetailPrint "OBS Studio is running: plugin skipped"
    Return
  ${EndIf}
  ; idn_obs.dll is the loader (media/idn-obs-loader): it loads the plugin built for the
  ; FFmpeg this OBS ships (OBS 32: FFmpeg 7, OBS 33: FFmpeg 8) from the plugin's data
  ; folder, where OBS doesn't load DLLs as modules. An OBS update then keeps it working.
  SetOutPath "$ObsDir\data\obs-plugins\idn_obs\bin"
  File "${SRC}\idn_obs_ffmpeg7.dll"
  File "${SRC}\idn_obs_ffmpeg8.dll"
  SetOutPath "$ObsDir\obs-plugins\64bit"
  File "${SRC}\idn_obs.dll"
  WriteRegStr HKLM "${REG_KEY}" "ObsPlugin" "$ObsDir"
  ; OBS receives and sends through the plugin: its own firewall rule (F1-4).
  nsExec::Exec 'netsh advfirewall firewall delete rule name="IDN: OBS Studio"'
  Pop $0
  nsExec::Exec 'netsh advfirewall firewall add rule name="IDN: OBS Studio" dir=in action=allow profile=private,domain program="$ObsDir\bin\64bit\obs64.exe" enable=yes'
  Pop $0
SectionEnd

Section "Audio editor plugins (CLAP and VST3)" SecAudioPlugins
  ; IDN Send and IDN Return (idn-routing.md §A.13): one library, in the folders every
  ; audio editor scans.
  SetOutPath "$COMMONFILES64\CLAP"
  File "/oname=IDN.clap" "${SRC}\idn_audio_plugins.dll"
  SetOutPath "$COMMONFILES64\VST3\IDN.vst3\Contents\x86_64-win"
  File "/oname=IDN.vst3" "${SRC}\idn_audio_plugins.dll"
  WriteRegDWORD HKLM "${REG_KEY}" "AudioPlugins" 1
SectionEnd

Section "FFmpeg 8 for idn-codec and the switcher" SecFfmpeg
!ifdef BUNDLE_FFMPEG
  ; Bundled in the installer (offline): install the DLLs shipped alongside the programs.
  DetailPrint "Installing FFmpeg ${FFMPEG_VERSION} (LGPL build by BtbN)..."
  SetOutPath "$INSTDIR"
  File "${SRC}\ffmpeg-dll\*.dll"
  File "${SRC}\ffmpeg-dll\ffmpeg.exe"
  File "${SRC}\ffmpeg-dll\ffprobe.exe"
  File "/oname=FFmpeg-LICENSE.txt" "${SRC}\ffmpeg-dll\LICENSE.txt"
  WriteRegStr HKLM "${REG_KEY}" "FFmpeg" "${FFMPEG_VERSION}"
  Return
!else
  ReadRegStr $0 HKLM "${REG_KEY}" "FFmpeg"
  ${If} $0 == "${FFMPEG_VERSION}"
  ${AndIf} ${FileExists} "$INSTDIR\avcodec-62.dll"
  ${AndIf} ${FileExists} "$INSTDIR\ffmpeg.exe"
  ${AndIf} ${FileExists} "$INSTDIR\ffprobe.exe"
    DetailPrint "FFmpeg ${FFMPEG_VERSION} is already installed"
    Return
  ${EndIf}
  DetailPrint "Downloading FFmpeg ${FFMPEG_VERSION} (LGPL build by BtbN)..."
  ; Downloaded and unpacked inside the program folder (only administrators and the
  ; system can write there), not the user's temporary folder, where another program
  ; of the user's could swap the files between the check and the copy (bug round F,
  ; F2-7).
  RMDir /r "$INSTDIR\ffmpeg-dl"
  CreateDirectory "$INSTDIR\ffmpeg-dl"
  nsExec::ExecToLog '"$SYSDIR\curl.exe" -L -f -sS --retry 3 -o "$INSTDIR\ffmpeg-dl\ffmpeg.zip" "${FFMPEG_URL}"'
  Pop $0
  ${If} $0 != 0
    Goto failed
  ${EndIf}
  nsExec::ExecToLog 'powershell.exe -NoProfile -NonInteractive -Command "if ((Get-FileHash -Algorithm SHA256 -LiteralPath $\'$INSTDIR\ffmpeg-dl\ffmpeg.zip$\').Hash -ne $\'${FFMPEG_SHA256}$\') { exit 1 }"'
  Pop $0
  ${If} $0 != 0
    DetailPrint "The download isn't the expected file"
    Goto failed
  ${EndIf}
  nsExec::ExecToLog '"$SYSDIR\tar.exe" -xf "$INSTDIR\ffmpeg-dl\ffmpeg.zip" -C "$INSTDIR\ffmpeg-dl"'
  Pop $0
  ${IfNot} ${FileExists} "$INSTDIR\ffmpeg-dl\${FFMPEG_FOLDER}\bin\avcodec-62.dll"
    Goto failed
  ${EndIf}
  CopyFiles /SILENT "$INSTDIR\ffmpeg-dl\${FFMPEG_FOLDER}\bin\*.dll" "$INSTDIR"
  ; The ffmpeg and ffprobe programs too: IDN Data's downloads join a video's picture and
  ; sound with ffmpeg (yt-dlp, idn-data.md §D.6); its library reads lengths and makes
  ; thumbnails with them (§D.8).
  CopyFiles /SILENT "$INSTDIR\ffmpeg-dl\${FFMPEG_FOLDER}\bin\ffmpeg.exe" "$INSTDIR"
  CopyFiles /SILENT "$INSTDIR\ffmpeg-dl\${FFMPEG_FOLDER}\bin\ffprobe.exe" "$INSTDIR"
  CopyFiles /SILENT "$INSTDIR\ffmpeg-dl\${FFMPEG_FOLDER}\LICENSE.txt" "$INSTDIR\FFmpeg-LICENSE.txt"
  WriteRegStr HKLM "${REG_KEY}" "FFmpeg" "${FFMPEG_VERSION}"
  RMDir /r "$INSTDIR\ffmpeg-dl"
  Return
failed:
  RMDir /r "$INSTDIR\ffmpeg-dl"
  MessageBox MB_ICONEXCLAMATION "FFmpeg couldn't be downloaded. Only idn-codec and the video switcher need it; everything else is installed. Run Setup again later to try again." /SD IDOK
!endif
SectionEnd

Section /o "IDN Site Manager (this computer runs the site)" SecSiteManager
  ; Kept beside the Node's state, not in it (removing the machine's identity deletes that
  ; folder): the site's keys, so SYSTEM and administrators only.
  StrCpy $3 "$APPDATA\IDN Site Manager"
  CreateDirectory "$3"
  ; Owned by SYSTEM first: a folder made earlier by a standard user stayed theirs.
  nsExec::Exec 'icacls "$3" /setowner *S-1-5-18 /T /C /Q'
  Pop $0
  ; Its list set whole: reset to what it inherits (every entry of its own gone: /grant:r
  ; alone left a user's), then only these; what is in it inherits that (bug round
  ; 2026-11-09).
  nsExec::Exec 'icacls "$3" /reset /Q'
  Pop $0
  nsExec::Exec 'icacls "$3" /inheritance:r /grant:r *S-1-5-18:(OI)(CI)F *S-1-5-32-544:(OI)(CI)F /Q'
  Pop $0
  nsExec::Exec 'icacls "$3\*" /reset /T /C /Q'
  Pop $0
  ; The Node service runs it from now on, from start-up, and makes the site's keys at its
  ; first start (an existing site keeps them). The service may need a moment to answer.
  DetailPrint "Setting up the IDN Site Manager..."
  ${For} $1 1 15
    nsExec::ExecToLog '"$INSTDIR\idn-node.exe" set sitemanager-on=true'
    Pop $0
    ${If} $0 == 0
      ${Break}
    ${EndIf}
    Sleep 1000
  ${Next}
  ${If} $0 != 0
    MessageBox MB_ICONEXCLAMATION "The IDN Node didn't take the Site Manager setting (see the details). Run, as administrator: idn node set sitemanager-on=true" /SD IDOK
    Return
  ${EndIf}
  ; Its site password, through the environment (never on a command line), written by the
  ; Node as the account the Site Manager runs as; none typed: an existing site keeps its.
  ${If} $SmPass != ""
    StrCpy $2 $SmPass
    System::Call 'kernel32::SetEnvironmentVariable(t "IDN_NODE_INPUT", t r2)'
    nsExec::ExecToLog '"$INSTDIR\idn-node.exe" set sitemanager-password=-'
    Pop $0
    System::Call 'kernel32::SetEnvironmentVariable(t "IDN_NODE_INPUT", p 0)'
    ${If} $0 != 0
      MessageBox MB_ICONEXCLAMATION "The site password couldn't be set. Run, as administrator: idn node set sitemanager-password=-" /SD IDOK
    ${EndIf}
  ${EndIf}
SectionEnd

; The bundled drivers: VirtualDrivers' Virtual Display Driver and
; Virtual Audio Driver, MIT, signed by the SignPath Foundation. The Node installs the display
; driver with the first virtual screen; the audio one (IDN's virtual sound card, Windows 11
; only: its INF is for build 22000 and later) is asked for here, in place of VB-CABLE.
Section "-Drivers"
  SetOutPath "$INSTDIR\drivers"
  File /r "${SRC}\drivers\*"
  SetOutPath "$INSTDIR"
  ${If} ${Silent}
    Return
  ${EndIf}
  ReadRegStr $1 HKLM "SOFTWARE\Microsoft\Windows NT\CurrentVersion" "CurrentBuildNumber"
  ${If} $1 < 22000
    Return
  ${EndIf}
  ; Installed already (its root device's driver service).
  ClearErrors
  EnumRegKey $0 HKLM "SYSTEM\CurrentControlSet\Services\VirtualAudioDriver" 0
  ${IfNot} ${Errors}
    Return
  ${EndIf}
  MessageBox MB_YESNO|MB_ICONQUESTION "Install IDN's virtual sound card now?$\r$\n$\r$\nIt lets Teams, Zoom and browsers use the IDN Microphone (they pick $\"Virtual Mic Driver$\"). It is the Virtual Audio Driver by VirtualDrivers (MIT licence, signed).$\r$\n$\r$\nNot now: the IDN Node's control panel offers it again when the IDN Microphone is on." IDNO vaudio_done
  DetailPrint "Installing IDN's virtual sound card (Virtual Audio Driver)..."
  nsExec::ExecToLog '"$INSTDIR\idn-node.exe" driver install audio'
  Pop $0
  DetailPrint "Virtual Audio Driver: $0"
  vaudio_done:
SectionEnd

; VB-Audio's VB-CABLE, for the IDN Microphone on Windows:
; the package shipped AS IS beside the programs; asked here, installed by VB-Audio's own
; setup (Windows asks to trust VB-Audio's driver); declined, the IDN Node's panel offers it
; again when the IDN Microphone is on. Not asked in a silent install or an update.
; VB-CABLE is donationware by VB-Audio (www.vb-cable.com): all participations are welcome.
Section "-VB-CABLE"
  SetOutPath "$INSTDIR\vbcable"
  File /r "${SRC}\vbcable\*"
  SetOutPath "$INSTDIR"
  ${If} ${Silent}
    Return
  ${EndIf}
  ; Windows 11 has IDN's own virtual sound card (above); VB-CABLE is for Windows 10.
  ReadRegStr $1 HKLM "SOFTWARE\Microsoft\Windows NT\CurrentVersion" "CurrentBuildNumber"
  ${If} $1 >= 22000
    Return
  ${EndIf}
  ; Installed already (its driver's service).
  ClearErrors
  EnumRegKey $0 HKLM "SYSTEM\CurrentControlSet\Services\VBAudioVACMME" 0
  ${IfNot} ${Errors}
    Return
  ${EndIf}
  MessageBox MB_YESNO|MB_ICONQUESTION "Install VB-CABLE, VB-Audio's virtual audio cable, now?$\r$\n$\r$\nIt lets Teams, Zoom and browsers use the IDN Microphone. Windows will ask whether to trust VB-Audio's driver, and the computer should be restarted afterwards.$\r$\n$\r$\nVB-CABLE is donationware by VB-Audio (www.vb-cable.com): all participations are welcome.$\r$\n$\r$\nNot now: the IDN Node's control panel offers it again when the IDN Microphone is on." IDNO vbcable_done
  DetailPrint "Installing VB-Audio's VB-CABLE..."
  ExecWait '"$INSTDIR\vbcable\VBCABLE_Setup_x64.exe" -i -h' $0
  DetailPrint "VB-CABLE's setup ended ($0); restart the computer to finish it."
  SetRebootFlag true
  vbcable_done:
SectionEnd

Section "-Finish"
  ; The site passphrase from the "Join a site" page, handed over in the environment so it
  ; never appears on a command line. The service may need a moment before it answers.
  ${If} $SitePass != ""
    ; Through a register: quotes in the passphrase can't upset the call.
    StrCpy $2 $SitePass
    System::Call 'kernel32::SetEnvironmentVariable(t "IDN_NODE_INPUT", t r2)'
    ${For} $1 1 15
      nsExec::ExecToLog '"$INSTDIR\idn-node.exe" set passphrase=-'
      Pop $0
      ${If} $0 == 0
        ${Break}
      ${EndIf}
      Sleep 1000
    ${Next}
    System::Call 'kernel32::SetEnvironmentVariable(t "IDN_NODE_INPUT", p 0)'
    ${If} $0 != 0
      MessageBox MB_ICONEXCLAMATION "The site passphrase couldn't be set. Set it in the IDN Node control panel." /SD IDOK
    ${EndIf}
  ${EndIf}
  ${If} $SiteServer != ""
    nsExec::ExecToLog '"$INSTDIR\idn-node.exe" set "server=$SiteServer"'
    Pop $0
  ${EndIf}
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegDWORD HKLM "${UNINST_KEY}" "EstimatedSize" $0
  ; The session agent runs as the logged-on user, not elevated like Setup: Explorer starts it.
  ${IfNot} ${Silent}
    Exec '"$WINDIR\explorer.exe" "$INSTDIR\idn-session.exe"'
  ${EndIf}
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecCore} "The IDN Node service (this machine's site identity), its control panel, the session agent (idn-session: this computer's sound, screens and webcams, at every log-on), the idn command (tools, sound cards, the mixing desk) and the video switcher. IDN Media's UI is provided by the application that uses it."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecAudioPlugins} "IDN Send and IDN Return for REAPER, Bitwig, Studio One, Cubase and other audio editors: a track as an IDN source, and IDN sound routed into a track."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecObs} "IDN Source and IDN Output for OBS Studio (GPL). Uses the FFmpeg that comes with OBS Studio."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecSiteManager} "Choose this on the one computer that runs your site: the IDN Site Manager (machines, approvals, routing, settings, backups and updates for the whole site), run by the IDN Node service from start-up. It has no panel of its own: an application administers it through its API (https://<this computer>:29202/api/v1/)."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecFfmpeg} "FFmpeg 8 (LGPL) from github.com/BtbN/FFmpeg-Builds, for idn-codec and the video switcher: GPU encoders and decoders, video files as sources, streams and recordings."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Function .onInit
  SetRegView 64
  SetShellVarContext all
  ${IfNot} ${RunningX64}
  ${OrIfNot} ${AtLeastWin10}
    MessageBox MB_ICONSTOP "IDN needs 64-bit Windows 10 or later." /SD IDOK
    Abort
  ${EndIf}
  InitPluginsDir
  ; How the Node runs: as set by an earlier install, /LOGON on the command line (silent
  ; installs), else as a service.
  ReadRegStr $NodeMode HKLM "${REG_KEY}" "NodeMode"
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/LOGON" $1
  ${IfNot} ${Errors}
    StrCpy $NodeMode "logon"
  ${EndIf}
  ${If} $NodeMode != "logon"
    StrCpy $NodeMode "service"
  ${EndIf}
  ; This computer runs the site already: the Site Manager stays (set up again, its keys
  ; and password kept).
  ${If} ${FileExists} "$APPDATA\IDN Site Manager\ca.key"
    !insertmacro SelectSection ${SecSiteManager}
  ${EndIf}
  ; OBS Studio's installer records its folder; the default folder otherwise.
  ReadRegStr $ObsDir HKLM "SOFTWARE\OBS Studio" ""
  ${If} $ObsDir == ""
  ${OrIfNot} ${FileExists} "$ObsDir\bin\64bit\obs64.exe"
    StrCpy $ObsDir "$PROGRAMFILES64\obs-studio"
  ${EndIf}
  ${IfNot} ${FileExists} "$ObsDir\bin\64bit\obs64.exe"
    StrCpy $ObsDir ""
    SectionSetText ${SecObs} "OBS Studio plugin (OBS Studio not found)"
    SectionSetFlags ${SecObs} ${SF_RO}
  ${EndIf}
  ; /S /UPDATE: an update from the Site Manager, run by the IDN Node's update helper
  ; (idn-node.md §N.23). Everything as the earlier install chose it: how the Node runs (the
  ; NodeMode above), and the OBS plugin only where it was installed.
  ClearErrors
  ${GetOptions} $0 "/UPDATE" $1
  ${IfNot} ${Errors}
    ReadRegStr $2 HKLM "${REG_KEY}" "ObsPlugin"
    ${If} $2 == ""
      StrCpy $ObsDir ""
    ${EndIf}
  ${EndIf}
FunctionEnd

Function ModePage
  !insertmacro MUI_HEADER_TEXT "How the IDN Node runs" "You can change this by running Setup again."
  nsDialogs::Create 1018
  Pop $0
  ${NSD_CreateRadioButton} 0 0 100% 12u "As a service (recommended)"
  Pop $ServiceRadio
  ${NSD_CreateLabel} 12u 14u 95% 30u "Always running, from start-up and before anyone logs on: one site identity for this computer, for every user and for programs that run as services."
  Pop $0
  ${NSD_CreateRadioButton} 0 52u 100% 12u "When you log on"
  Pop $LogonRadio
  ${NSD_CreateLabel} 12u 66u 95% 40u "No service: the IDN Node starts with the session agent (idn-session) when a user logs on and stops at log-off. Each user has a site identity of their own (join the site in the control panel)."
  Pop $0
  ${If} $NodeMode == "logon"
    ${NSD_Check} $LogonRadio
  ${Else}
    ${NSD_Check} $ServiceRadio
  ${EndIf}
  nsDialogs::Show
FunctionEnd

Function ModePageLeave
  ${NSD_GetState} $LogonRadio $0
  ${If} $0 == ${BST_CHECKED}
    ; The Site Manager runs from start-up, before anyone logs on: the Node as a service.
    ${If} ${SectionIsSelected} ${SecSiteManager}
      MessageBox MB_ICONINFORMATION "The IDN Site Manager runs from start-up, so the IDN Node has to run as a service on this computer." /SD IDOK
      Abort
    ${EndIf}
    StrCpy $NodeMode "logon"
  ${Else}
    StrCpy $NodeMode "service"
  ${EndIf}
FunctionEnd

Function SiteManagerPage
  ${IfNot} ${SectionIsSelected} ${SecSiteManager}
    Abort
  ${EndIf}
  !insertmacro MUI_HEADER_TEXT "The IDN Site Manager" "Programs log in to it with the site password."
  nsDialogs::Create 1018
  Pop $0
  ${If} ${FileExists} "$APPDATA\IDN Site Manager\ca.key"
    ${NSD_CreateLabel} 0 0 100% 36u "This computer runs a site already: it keeps its keys and its machines. Type a new site password to change it, or leave both empty to keep it."
  ${Else}
    ${NSD_CreateLabel} 0 0 100% 36u "Choose the site password (at least 6 characters). Any program using the Site Manager's API (https://<this computer>:29202/api/v1/) logs in to the Site Manager with it, from any computer. Each machine that joins the site is approved there; this one too."
  ${EndIf}
  Pop $0
  ${NSD_CreateLabel} 0 44u 30% 12u "Site password:"
  Pop $0
  ${NSD_CreatePassword} 32% 42u 68% 13u ""
  Pop $SmPassField
  ${NSD_CreateLabel} 0 64u 30% 12u "Again:"
  Pop $0
  ${NSD_CreatePassword} 32% 62u 68% 13u ""
  Pop $SmPass2Field
  nsDialogs::Show
FunctionEnd

Function SiteManagerPageLeave
  ${NSD_GetText} $SmPassField $SmPass
  ${NSD_GetText} $SmPass2Field $0
  ${If} $SmPass != $0
    MessageBox MB_ICONEXCLAMATION "The two passwords differ." /SD IDOK
    Abort
  ${EndIf}
  StrLen $1 $SmPass
  ${If} $SmPass == ""
    ${IfNot} ${FileExists} "$APPDATA\IDN Site Manager\ca.key"
      MessageBox MB_ICONEXCLAMATION "Choose a site password: the Site Manager's API opens only with it." /SD IDOK
      Abort
    ${EndIf}
  ${ElseIf} $1 < 6
    MessageBox MB_ICONEXCLAMATION "The site password needs at least 6 characters." /SD IDOK
    Abort
  ${EndIf}
FunctionEnd

Function SitePage
  ; A machine that has run the Node before keeps its site; nothing to ask. Run when
  ; someone logs on, each user's Node joins in its own control panel. The computer that
  ; runs the Site Manager joins its own site (approved through the Site Manager's API).
  ${If} ${FileExists} "$APPDATA\IDN Node\node.xml"
  ${OrIf} $NodeMode == "logon"
  ${OrIf} ${SectionIsSelected} ${SecSiteManager}
    Abort
  ${EndIf}
  !insertmacro MUI_HEADER_TEXT "Join a site" "Optional: you can do this later in the IDN Node control panel."
  nsDialogs::Create 1018
  Pop $0
  ${NSD_CreateLabel} 0 0 100% 36u "If this machine belongs to an IDN site, type the site's passphrase. Every IDN program on this machine then uses it to encrypt; the site's operator approves the machine on the IDN Site Manager."
  Pop $0
  ${NSD_CreateLabel} 0 44u 30% 12u "Site passphrase:"
  Pop $0
  ${NSD_CreatePassword} 32% 42u 68% 13u ""
  Pop $PassField
  ${NSD_CreateLabel} 0 66u 30% 12u "Site Manager address:"
  Pop $0
  ${NSD_CreateText} 32% 64u 68% 13u ""
  Pop $ServerField
  ${NSD_CreateLabel} 32% 80u 68% 24u "Leave it empty: the IDN Node finds the site's Site Manager on the network by itself."
  Pop $0
  ${NSD_CreateLabel} 0 112u 100% 24u "Leave both empty to skip."
  Pop $0
  nsDialogs::Show
FunctionEnd

Function SitePageLeave
  ${NSD_GetText} $PassField $SitePass
  ${NSD_GetText} $ServerField $SiteServer
FunctionEnd

Function OpenPanel
  ; Through Explorer, so the browser doesn't run elevated like Setup.
  Exec '"$WINDIR\explorer.exe" "${SM}\IDN Node.lnk"'
FunctionEnd

Function un.onInit
  SetRegView 64
  SetShellVarContext all
FunctionEnd

Section "Uninstall"
  ; The secure attention sequence policy as it was before (or not set).
  ReadRegStr $1 HKLM "${REG_KEY}" "SasBefore"
  ${If} $1 == "none"
    DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Policies\System" "SoftwareSASGeneration"
  ${ElseIf} $1 != ""
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Policies\System" "SoftwareSASGeneration" $1
  ${EndIf}
  nsExec::Exec 'taskkill /F /IM idn-session.exe'
  Pop $0
  ; The service stopped first, as a stop: killed, its failure actions (restart after
  ; 5 s) started it again during the uninstall (deep bug run 2026-11-09).
  nsExec::Exec 'net stop idn-node'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-node.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-mixer.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-switcher.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-remote.exe'
  Pop $0
  !insertmacro KillOurVoice
  nsExec::Exec 'taskkill /F /IM idn-aoip.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-display.exe'
  Pop $0
  ; Every other program of ours (idn.exe and its names, the codec tools, the panel…):
  ; one left running kept its file.
  !insertmacro KillAll
  DetailPrint "Removing the IDN Node service..."
  nsExec::ExecToLog '"$INSTDIR\idn-node.exe" uninstall'
  Pop $0
  !insertmacro Firewall delete
  nsExec::Exec 'netsh advfirewall firewall delete rule name="IDN: OBS Studio"'
  Pop $0
  nsExec::ExecToLog '"$SYSDIR\regsvr32.exe" /s /u "$INSTDIR\idn_asio.dll"'
  Pop $0
  nsExec::ExecToLog '"$SYSDIR\regsvr32.exe" /s /u "$INSTDIR\idn_camera.dll"'
  Pop $0

  ; The audio editors' plugins (a running editor holds them: removed at the restart then).
  Delete /REBOOTOK "$COMMONFILES64\CLAP\IDN.clap"
  Delete /REBOOTOK "$COMMONFILES64\VST3\IDN.vst3\Contents\x86_64-win\IDN.vst3"
  RMDir "$COMMONFILES64\VST3\IDN.vst3\Contents\x86_64-win"
  RMDir "$COMMONFILES64\VST3\IDN.vst3\Contents"
  RMDir "$COMMONFILES64\VST3\IDN.vst3"

  ReadRegStr $3 HKLM "${REG_KEY}" "ObsPlugin"
  ${If} $3 != ""
    !insertmacro WaitForObs "Un"
    Delete /REBOOTOK "$3\obs-plugins\64bit\idn_obs.dll"
    Delete /REBOOTOK "$3\data\obs-plugins\idn_obs\bin\idn_obs_ffmpeg7.dll"
    Delete /REBOOTOK "$3\data\obs-plugins\idn_obs\bin\idn_obs_ffmpeg8.dll"
    RMDir "$3\data\obs-plugins\idn_obs\bin"
    RMDir "$3\data\obs-plugins\idn_obs"
  ${EndIf}

  ; The bundled drivers' devices and packages (virtual screens, IDN's virtual sound card).
  nsExec::ExecToLog '"$INSTDIR\idn-node.exe" driver remove all'
  Pop $0
  RMDir /r "$INSTDIR\drivers"
  Delete /REBOOTOK "$INSTDIR\idn-node.exe"
  Delete /REBOOTOK "$INSTDIR\idn-session.exe"
  ; VB-CABLE's package (its driver stays: VB-Audio's own setup removes it).
  RMDir /r "$INSTDIR\vbcable"
  Delete "$INSTDIR\dxcompiler.dll"
  Delete "$INSTDIR\dxcompiler-LICENSE.txt"
  !insertmacro OldNames DeleteExe
  Delete /REBOOTOK "$INSTDIR\idn.exe"
  Delete /REBOOTOK "$INSTDIR\idn-codec.exe"
  Delete /REBOOTOK "$INSTDIR\idn-switcher.exe"
  Delete /REBOOTOK "$INSTDIR\idn-remote.exe"
  Delete /REBOOTOK "$INSTDIR\idn-voice.exe"
  Delete /REBOOTOK "$INSTDIR\idn-display.exe"
  Delete /REBOOTOK "$INSTDIR\idn-plugin-host.exe"
  Delete /REBOOTOK "$INSTDIR\idn-aoip.exe"
  Delete /REBOOTOK "$INSTDIR\idn_asio.dll"
  Delete /REBOOTOK "$INSTDIR\idn_camera.dll"
  Delete "$INSTDIR\capture-check.exe"
  Delete "$INSTDIR\av*.dll"
  Delete "$INSTDIR\sw*.dll"
  Delete "$INSTDIR\ffmpeg.exe"
  Delete "$INSTDIR\ffprobe.exe"
  Delete "$INSTDIR\FFmpeg-LICENSE.txt"
  Delete "$INSTDIR\idn.ico"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\BUILD.txt"
  Delete "$INSTDIR\README-windows.txt"
  Delete "$INSTDIR\idn-node-README.md"
  Delete "$INSTDIR\idn-audio-README.md"
  Delete "$INSTDIR\idn-obs-README.md"
  Delete "$INSTDIR\idn-mixer-README.md"
  Delete "$INSTDIR\idn-switcher-README.md"
  RMDir /r "$INSTDIR\guide"
  Delete "$INSTDIR\uninstall.exe"
  RMDir /REBOOTOK "$INSTDIR"

  Delete "${SM}\IDN Node.lnk"
  Delete "${SM}\IDN Mixer.lnk"
  Delete "${SM}\IDN Switcher.lnk"
  Delete "${SM}\IDN Monitor.lnk"
  Delete "${SM}\IDN Remote.lnk"
  Delete "${SM}\IDN Voice.lnk"
  !insertmacro DeleteOurVoiceRun
  Delete "${SM}\IDN command prompt.lnk"
  Delete "${SM}\IDN Guide.lnk"
  Delete "${SM}\IDN read-me.lnk"
  Delete "${SM}\Uninstall IDN Media.lnk"
  Delete "${SM}\IDN Site Manager.url"
  RMDir "${SM}"
  RMDir /r "${OLD_SM}"

  DeleteRegValue HKLM "${RUN_KEY}" "${TRAY_VALUE}"
  DeleteRegValue HKLM "${RUN_KEY}" "${SESSION_VALUE}"
  DeleteRegKey HKLM "${UNINST_KEY}"
  DeleteRegKey HKLM "${REG_KEY}"
  DeleteRegKey /ifempty HKLM "Software\ADC Industries"

  ; The site's IDN Site Manager (its keys and machines) stays unless the user says
  ; otherwise: without them every machine of the site would have to join a new site.
  ${If} ${FileExists} "$APPDATA\IDN Site Manager\ca.key"
    MessageBox MB_YESNO|MB_ICONEXCLAMATION|MB_DEFBUTTON2 "This computer runs the site's IDN Site Manager. Also remove it (the site's keys, its machines, settings and backups kept here)?$\r$\n$\r$\nChoose No to keep it: installed again, this computer runs the same site, and its machines stay members. Remove it only if the site is gone or runs elsewhere (restore a backup there first)." /SD IDNO IDNO keepsite
      RMDir /r "$APPDATA\IDN Site Manager"
    keepsite:
  ${EndIf}
  ; The machine's site identity stays unless the user says otherwise: reinstalling then
  ; keeps the machine in its site without a new approval. A silent uninstall keeps it.
  MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 "Also remove this machine's IDN site identity (its key and site membership)?$\r$\n$\r$\nChoose No to keep it: if IDN is installed again, this machine is still a member of its site and needs no new approval." /SD IDNO IDNO keep
    RMDir /r "$APPDATA\IDN Node"
    MessageBox MB_ICONINFORMATION "This machine's site identity is removed. If it was in a site, revoke it on the IDN Site Manager too, so the site stops listing it." /SD IDOK
  keep:
SectionEnd
