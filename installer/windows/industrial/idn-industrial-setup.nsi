; IDN Node | Industrial for Windows (spec/extensions/idn-industrial.md I.2, I.8): the
; machine's link to its company's IDN Site Manager | Industrial, as a service from boot,
; with its local API and panel at http://127.0.0.1:29208. A separate product from IDN (the
; church and media edition): its own name, icon, folder, service and state.
;
; With the option of IDN Site Manager | Industrial on the same computer (I.12): the
; company's server, its own service (idn-site-manager-industrial), its
; data in %ProgramData%\IDN Site Manager Industrial.
;
; Built on Linux with NSIS 3: installer/windows/industrial/build.sh DIR OUT.
; Unattended: idn-industrial-setup-x64.exe /S /TOKEN=IDNI1-...
;   an update (the Node's update helper): /S /UPDATE /D=<the programs' folder>
;   the Site Manager too: /SITEMANAGER /ADDRESS=server.example.com (the administrator
;   password then set on the web UI's first visit, with the API key in its folder);
;   the Site Manager alone: /SITEMANAGER /NONODE

Unicode true
Target amd64-unicode
ManifestDPIAware true
SetCompressor /SOLID lzma
RequestExecutionLevel admin

!include MUI2.nsh
!include LogicLib.nsh
!include nsDialogs.nsh
!include x64.nsh
!include FileFunc.nsh
!include Sections.nsh

!define PRODUCT "IDN Node | Industrial"
!define PUBLISHER "ADC Industries"
!define SERVICE "idn-industrial"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\IDN Node Industrial"
!define REG_KEY "Software\ADC Industries\IDN Node Industrial"
!define SM_SERVICE "idn-site-manager-industrial"

; FFmpeg 8 (LGPL, BtbN): the screen capture and H.264 encode for remote support
; (spec I.11.3). Same pinned build and checksum as the main installer.
!define FFMPEG_VERSION "8.1.3"
!define FFMPEG_FOLDER "ffmpeg-n8.1.3-9-g29e619e767-win64-lgpl-shared-8.1"
!define FFMPEG_URL "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-09-30-13-08/${FFMPEG_FOLDER}.zip"
!define FFMPEG_SHA256 "3E47BDA1607740550141E37C0E49D1E5182B34699F15ADFD137EE266D346811A"

Name "${PRODUCT}"
Caption "${PRODUCT} ${VERSION} (build ${BUILD}) Setup"
UninstallCaption "Uninstall ${PRODUCT}"
OutFile "${OUT}"
InstallDir "$PROGRAMFILES64\IDN Node Industrial"
InstallDirRegKey HKLM "${REG_KEY}" "InstallDir"
BrandingText "IDN Node | Industrial, build ${BUILD}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${PRODUCT}"
VIAddVersionKey "CompanyName" "${PUBLISHER}"
VIAddVersionKey "FileDescription" "IDN Node | Industrial Setup"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION} (build ${BUILD})"
VIAddVersionKey "LegalCopyright" "ADC Industries. MIT OR Apache-2.0"

!define MUI_ICON "${SRC}\idn-industrial.ico"
!define MUI_UNICON "${SRC}\idn-industrial.ico"
!define MUI_ABORTWARNING

Var Token
Var TokenField
; The Site Manager's page: the address machines reach it at, the administrator password
; (twice), the web UI's port.
Var SmAddress
Var SmPass
Var SmPort
Var SmAddressField
Var SmPassField
Var SmPass2Field
Var SmPortField
Var CmdSm
Var CmdNoNode

!define MUI_WELCOMEPAGE_TITLE "IDN Node | Industrial"
!define MUI_WELCOMEPAGE_TEXT "This installs IDN Node | Industrial: this machine's link to your company's IDN Site Manager | Industrial, running as a service from boot.$\r$\n$\r$\n\
  It is not the IDN for churches and media (IDN Node). If you wanted that, cancel and use the IDN setup instead.$\r$\n$\r$\n\
  Have the enrolment token from the IDN Site Manager | Industrial ready (IDNI1-...), or join later from the Node's panel.$\r$\n$\r$\n\
  On the company's server, choose IDN Site Manager | Industrial too (the next page)."
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${SRC}\LICENSE.txt"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
Page custom SiteManagerPage SiteManagerPageLeave
Page custom TokenPage TokenPageLeave
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_TEXT "What you chose is running.$\r$\n$\r$\nIDN Node | Industrial: its panel and the local API for this machine's programs are at http://127.0.0.1:29208 (the API token is in %ProgramData%\IDN Node Industrial\api.token).$\r$\n$\r$\nIDN Site Manager | Industrial: its web UI is at http://<this computer>:$SmPort/ (log in with the administrator password)."
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "IDN Node | Industrial (this machine's link to the site)" SecMain
  SetRegView 64
  ; An earlier version: stopped before its files are replaced.
  nsExec::Exec 'sc.exe stop ${SERVICE}'
  Pop $0
  ; (Its session helper, which runs as the logged-in user, held its program: stopped
  ; too; `net stop` waits for the service, sc.exe doesn't.)
  nsExec::Exec 'net stop ${SERVICE}'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-helper-industrial.exe'
  Pop $0
  Sleep 1500
  SetOutPath "$INSTDIR"
  ; The programs' folder: SYSTEM's and administrators', users may read and run (the
  ; service runs idn-industrial.exe from it as SYSTEM; a folder users could write
  ; outside Program Files let any of them replace it; bug round 2026-11-09).
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
  File "${SRC}\idn-industrial.exe"
  ; The session helper (spec I.11.2): the service launches it onto the interactive and
  ; secure desktops, where it can see and drive the screen.
  File "${SRC}\idn-helper-industrial.exe"
  File "${SRC}\idn-industrial.ico"
  File "${SRC}\LICENSE.txt"
  File "${SRC}\README-industrial.txt"
  ; The Site Manager on this computer too: its broker has 1883, so the Node's local MQTT
  ; moves to 1885 (unless the machine's own file already says where).
  Call NodeBesideSiteManager
  ; Its state (the machine's key, api.token, unsent items, secrets): SYSTEM's and
  ; administrators' only. ProgramData's default lets every user read it, so any user at
  ; the machine had the whole local API. The session helper, which runs as the logged-in
  ; user, reads only session\worker.token (it opens nothing but the worker's endpoint).
  StrCpy $3 "$APPDATA\IDN Node Industrial"
  CreateDirectory "$3"
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
  CreateDirectory "$3\session"
  nsExec::Exec 'icacls "$3\session" /grant *S-1-5-32-545:(OI)(CI)RX'
  Pop $0
  ; The service: from boot, restarted if it stops unexpectedly.
  nsExec::Exec 'sc.exe create ${SERVICE} binPath= "\"$INSTDIR\idn-industrial.exe\" service" start= auto DisplayName= "IDN Node | Industrial"'
  nsExec::Exec 'sc.exe config ${SERVICE} binPath= "\"$INSTDIR\idn-industrial.exe\" service" start= auto'
  nsExec::Exec 'sc.exe description ${SERVICE} "This machine$\'s link to its company$\'s IDN Site Manager | Industrial."'
  nsExec::Exec 'sc.exe failure ${SERVICE} reset= 86400 actions= restart/5000/restart/5000/restart/30000'
  ; Also when a start fails and the service reports itself stopped with an error (a
  ; port taken at boot): without this flag Windows restarts only a crashed one (F3-3).
  nsExec::Exec 'sc.exe failureflag ${SERVICE} 1'
  ; Let a service raise the secure attention sequence (Ctrl+Alt+Del) through SendSAS, so
  ; remote support can reach the login screen and UAC (spec I.11.2). 1 = services only.
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
  nsExec::Exec 'sc.exe start ${SERVICE}'
  ; Joining: through the running service (its local API).
  ${If} $Token != ""
    ; Once the service listens (up to 30 s; after a fixed 2 s a slow start had the enrol
    ; open the service's own files itself; F3-12).
    nsExec::Exec 'powershell.exe -NoProfile -NonInteractive -Command "for ($$i = 0; $$i -lt 60; $$i++) { try { (New-Object Net.Sockets.TcpClient(\"127.0.0.1\", 29208)).Close(); exit 0 } catch { Start-Sleep -Milliseconds 500 } }; exit 1"'
    Pop $0
    nsExec::ExecToStack '"$INSTDIR\idn-industrial.exe" enrol "$Token"'
    Pop $0
    Pop $1
    ${If} $0 != 0
      IfSilent +2
      MessageBox MB_ICONEXCLAMATION "Joining the site didn't work:$\r$\n$1$\r$\nThe Node is installed; join later from its panel (http://127.0.0.1:29208) with a new token."
    ${EndIf}
  ${EndIf}
  WriteRegStr HKLM "${REG_KEY}" "InstallDir" "$INSTDIR"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayName" "${PRODUCT}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\idn-industrial.ico"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayVersion" "${VERSION} (build ${BUILD})"
  WriteRegStr HKLM "${UNINST_KEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKLM "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair" 1
  ; The panel in the Start menu.
  CreateShortcut "$SMPROGRAMS\IDN Node Industrial panel.lnk" "http://127.0.0.1:29208/" "" "$INSTDIR\idn-industrial.ico"
SectionEnd

Section "IDN Voice (talk on the site's Voice channels)" SecVoice
  SetRegView 64
  SetShellVarContext all
  nsExec::Exec 'taskkill /F /IM idn-voice.exe'
  Pop $0
  SetOutPath "$INSTDIR"
  ; The Voice web client in a window with a talk key or pedal (idn-voice.md §V.11), and
  ; Microsoft's WebView2 loader (redistributable) it loads.
  File "${SRC}\idn-voice.exe"
  File "${SRC}\WebView2Loader.dll"
  CreateShortcut "$SMPROGRAMS\IDN Voice.lnk" "$INSTDIR\idn-voice.exe" "" \
    "$INSTDIR\idn-voice.exe" 0 SW_SHOWNORMAL "" "Talk on the site's Voice channels (push to talk, intercom, calls)"
SectionEnd

Section "FFmpeg 8 for remote screen capture" SecFfmpeg
  SetRegView 64
  ; Only the Node uses it.
  ${IfNot} ${SectionIsSelected} ${SecMain}
    Return
  ${EndIf}
!ifdef BUNDLE_FFMPEG
  ; Bundled in the installer (offline): install the DLLs shipped with the Node.
  DetailPrint "Installing FFmpeg ${FFMPEG_VERSION} (LGPL build by BtbN)..."
  SetOutPath "$INSTDIR"
  File "${SRC}\ffmpeg-dll\*.dll"
  File "/oname=FFmpeg-LICENSE.txt" "${SRC}\ffmpeg-dll\LICENSE.txt"
  WriteRegStr HKLM "${REG_KEY}" "FFmpeg" "${FFMPEG_VERSION}"
  Return
!else
  ReadRegStr $0 HKLM "${REG_KEY}" "FFmpeg"
  ${If} $0 == "${FFMPEG_VERSION}"
  ${AndIf} ${FileExists} "$INSTDIR\avcodec-62.dll"
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
  CopyFiles /SILENT "$INSTDIR\ffmpeg-dl\${FFMPEG_FOLDER}\LICENSE.txt" "$INSTDIR\FFmpeg-LICENSE.txt"
  WriteRegStr HKLM "${REG_KEY}" "FFmpeg" "${FFMPEG_VERSION}"
  RMDir /r "$INSTDIR\ffmpeg-dl"
  Return
failed:
  RMDir /r "$INSTDIR\ffmpeg-dl"
  MessageBox MB_ICONEXCLAMATION "FFmpeg couldn't be downloaded. The Node is installed and runs; only the remote-support screen picture needs it. Run Setup again later to try again." /SD IDOK
!endif
SectionEnd

Section /o "IDN Site Manager | Industrial (this computer is the company's server)" SecSM
  SetRegView 64
  ; `net stop` waits for the service to end (sc.exe doesn't: a slow stop, its database
  ; closing, still held the program after 1.5 s; deep bug run 2026-11-09).
  nsExec::Exec 'net stop ${SM_SERVICE}'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-site-manager-industrial.exe'
  Pop $0
  SetOutPath "$INSTDIR"
  File "${SRC}\idn-site-manager-industrial.exe"
  File "${SRC}\idn-industrial.ico"
  File "${SRC}\LICENSE.txt"
  File "${SRC}\README-industrial.txt"
  ; Its data (the site CA and keys, machines, the inbox): SYSTEM's and administrators'
  ; only. Kept when uninstalled; back it up with "idn-site-manager-industrial backup".
  StrCpy $3 "$APPDATA\IDN Site Manager Industrial"
  CreateDirectory "$3"
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
  ; Its settings (the service has no environment of its own); kept on an upgrade.
  ${IfNot} ${FileExists} "$3\settings.env"
    FileOpen $4 "$3\settings.env" w
    FileWrite $4 "# IDN Site Manager | Industrial's settings, read at its start (restart the service after a change).$\r$\n"
    FileWrite $4 "# What enrolment tokens tell machines (comma separated):$\r$\n"
    FileWrite $4 "IDN_PUBLIC_URLS=ws://$SmAddress:$SmPort/idn/link,idn://$SmAddress:29206$\r$\n"
    FileWrite $4 "IDN_HTTP=0.0.0.0:$SmPort$\r$\n"
    FileWrite $4 "IDN_QUIC=0.0.0.0:29206$\r$\n"
    FileWrite $4 "IDN_MQTT=0.0.0.0:1883$\r$\n"
    FileClose $4
  ${EndIf}
  ; The administrator password: set once, through the environment (never a command line).
  ${If} $SmPass != ""
    StrCpy $2 $SmPass
    System::Call 'kernel32::SetEnvironmentVariable(t "IDN_ADMIN_PASSWORD", t r2)'
    nsExec::ExecToLog '"$INSTDIR\idn-site-manager-industrial.exe" admin-password'
    Pop $0
    System::Call 'kernel32::SetEnvironmentVariable(t "IDN_ADMIN_PASSWORD", p 0)'
    ${If} $0 != 0
      MessageBox MB_ICONEXCLAMATION "The administrator password couldn't be set; set it on the web UI's first visit with the API key (in $3\api.key)." /SD IDOK
    ${EndIf}
  ${EndIf}
  nsExec::Exec 'sc.exe create ${SM_SERVICE} binPath= "\"$INSTDIR\idn-site-manager-industrial.exe\" service" start= auto DisplayName= "IDN Site Manager | Industrial"'
  nsExec::Exec 'sc.exe config ${SM_SERVICE} binPath= "\"$INSTDIR\idn-site-manager-industrial.exe\" service" start= auto'
  nsExec::Exec 'sc.exe description ${SM_SERVICE} "The company$\'s IDN Site Manager | Industrial: its machines, their data and commands."'
  nsExec::Exec 'sc.exe failure ${SM_SERVICE} reset= 86400 actions= restart/5000/restart/5000/restart/30000'
  ; Also when a start fails and the service reports itself stopped with an error (a
  ; port taken at boot): without this flag Windows restarts only a crashed one (F3-3).
  nsExec::Exec 'sc.exe failureflag ${SM_SERVICE} 1'
  ; Windows Firewall: the web UI and links, QUIC, MQTT.
  nsExec::Exec 'netsh advfirewall firewall delete rule name="IDN Site Manager | Industrial"'
  nsExec::Exec 'netsh advfirewall firewall add rule name="IDN Site Manager | Industrial" dir=in action=allow program="$INSTDIR\idn-site-manager-industrial.exe" enable=yes'
  nsExec::Exec 'sc.exe start ${SM_SERVICE}'
  WriteRegStr HKLM "${REG_KEY}" "InstallDir" "$INSTDIR"
  WriteRegDWORD HKLM "${REG_KEY}" "SiteManager" 1
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayName" "${PRODUCT}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\idn-industrial.ico"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayVersion" "${VERSION} (build ${BUILD})"
  WriteRegStr HKLM "${UNINST_KEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKLM "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair" 1
  ; (On an upgrade the port is settings.env's: the shortcut made at the first install stays.)
  ${IfNot} ${FileExists} "$SMPROGRAMS\IDN Site Manager Industrial.lnk"
    CreateShortcut "$SMPROGRAMS\IDN Site Manager Industrial.lnk" "http://127.0.0.1:$SmPort/" "" "$INSTDIR\idn-industrial.ico"
  ${EndIf}
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecMain} "This machine's link to its company's IDN Site Manager | Industrial: health, modules (SQL, Modbus, HTTP), remote support, the local API. On every machine of the site."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecVoice} "IDN Voice on this computer: the Site Manager's Voice channels in a window, a talk key or foot pedal that works from any program, calls. Needs the Microsoft Edge WebView2 Runtime (part of Windows 11)."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecFfmpeg} "The screen picture for remote support (FFmpeg 8, LGPL, BtbN's build). Without it everything else works: data, modules, chat."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecSM} "Choose this on the one computer that is the company's server: machines enrol with it and send through it, the application server takes their data from its MQTT broker, and its web UI runs the site."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Function .onInit
  ; $APPDATA is ProgramData (the services' folders) from here on.
  SetShellVarContext all
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "IDN Node | Industrial needs 64-bit Windows."
    Abort
  ${EndIf}
  SetRegView 64
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/TOKEN=" $Token
  StrCpy $SmPort "8080"
  ReadEnvStr $SmAddress "COMPUTERNAME"
  ClearErrors
  ${GetOptions} $0 "/ADDRESS=" $1
  ${IfNot} ${Errors}
    StrCpy $SmAddress $1
  ${EndIf}
  ClearErrors
  ${GetOptions} $0 "/SITEMANAGER" $1
  ${IfNot} ${Errors}
    StrCpy $CmdSm "1"
  ${EndIf}
  ClearErrors
  ${GetOptions} $0 "/NONODE" $1
  ${IfNot} ${Errors}
    StrCpy $CmdNoNode "1"
  ${EndIf}
  ; Installed before with the Site Manager: chosen again (an upgrade keeps it).
  nsExec::Exec 'sc.exe query ${SM_SERVICE}'
  Pop $1
  ${If} $1 == 0
  ${OrIf} $CmdSm == "1"
    !insertmacro SelectSection ${SecSM}
  ${EndIf}
  ${If} $CmdNoNode == "1"
    !insertmacro UnselectSection ${SecMain}
    !insertmacro UnselectSection ${SecFfmpeg}
    !insertmacro UnselectSection ${SecVoice}
  ${EndIf}
  ; An update from the Site Manager (the Node's update helper runs this with /S /UPDATE,
  ; spec I.17): exactly what is installed, nothing added.
  ClearErrors
  ${GetOptions} $0 "/UPDATE" $1
  ${IfNot} ${Errors}
    nsExec::Exec 'sc.exe query ${SERVICE}'
    Pop $1
    ${If} $1 != 0
      !insertmacro UnselectSection ${SecMain}
      !insertmacro UnselectSection ${SecFfmpeg}
    ${EndIf}
    ; IDN Voice only where it was installed before.
    ${IfNot} ${FileExists} "$INSTDIR\idn-voice.exe"
      !insertmacro UnselectSection ${SecVoice}
    ${EndIf}
  ${EndIf}
FunctionEnd

Function SiteManagerPage
  ${IfNot} ${SectionIsSelected} ${SecSM}
    Abort
  ${EndIf}
  ; An upgrade: its settings and password are kept; nothing to ask.
  ${If} ${FileExists} "$APPDATA\IDN Site Manager Industrial\settings.env"
    Abort
  ${EndIf}
  !insertmacro MUI_HEADER_TEXT "IDN Site Manager | Industrial" "The company's server: how machines reach it, and its administrator password"
  nsDialogs::Create 1018
  Pop $0
  ${NSD_CreateLabel} 0 0 100% 20u "The address machines reach this computer at (a name or IP; a tunnel's name if one is in front):"
  Pop $0
  ${NSD_CreateText} 0 22u 100% 13u "$SmAddress"
  Pop $SmAddressField
  ${NSD_CreateLabel} 0 42u 100% 10u "The web UI's administrator password (10 characters or more), twice:"
  Pop $0
  ${NSD_CreatePassword} 0 54u 48% 13u ""
  Pop $SmPassField
  ${NSD_CreatePassword} 52% 54u 48% 13u ""
  Pop $SmPass2Field
  ${NSD_CreateLabel} 0 76u 60% 10u "The web UI's port (TCP; QUIC links on UDP 29206, MQTT on TCP 1883):"
  Pop $0
  ${NSD_CreateNumber} 62% 74u 20% 13u "$SmPort"
  Pop $SmPortField
  ${NSD_CreateLabel} 0 96u 100% 30u "Windows Firewall is opened for these ports. With IDN Node | Industrial on this computer too, its local MQTT moves to 127.0.0.1:1885 (the Site Manager's broker has 1883)."
  Pop $0
  nsDialogs::Show
FunctionEnd

Function SiteManagerPageLeave
  ${NSD_GetText} $SmAddressField $SmAddress
  ${NSD_GetText} $SmPassField $SmPass
  ${NSD_GetText} $SmPass2Field $1
  ${NSD_GetText} $SmPortField $SmPort
  ${If} $SmAddress == ""
    MessageBox MB_ICONEXCLAMATION "Give the address machines reach this computer at."
    Abort
  ${EndIf}
  StrLen $2 $SmPass
  ${If} $2 < 10
    MessageBox MB_ICONEXCLAMATION "The administrator password needs 10 characters or more."
    Abort
  ${EndIf}
  ${If} $SmPass != $1
    MessageBox MB_ICONEXCLAMATION "The two passwords aren't the same."
    Abort
  ${EndIf}
  ${If} $SmPort == ""
    StrCpy $SmPort "8080"
  ${EndIf}
FunctionEnd

Function TokenPage
  ${IfNot} ${SectionIsSelected} ${SecMain}
    Abort
  ${EndIf}
  !insertmacro MUI_HEADER_TEXT "Join the site" "The enrolment token from your IDN Site Manager | Industrial"
  nsDialogs::Create 1018
  Pop $0
  ${NSD_CreateLabel} 0 0 100% 36u "Paste the token (IDNI1-...). Each token joins one machine once. Leave it empty to join later from the panel (http://127.0.0.1:29208)."
  Pop $0
  ${NSD_CreateText} 0 42u 100% 14u "$Token"
  Pop $TokenField
  nsDialogs::Show
FunctionEnd

Function TokenPageLeave
  ${NSD_GetText} $TokenField $Token
FunctionEnd


; The Node beside a Site Manager: its local MQTT on 1885 (the broker has 1883), written
; to the machine's own node.toml unless it already says something about [mqtt].
Function NodeBesideSiteManager
  ${IfNot} ${SectionIsSelected} ${SecSM}
    Return
  ${EndIf}
  StrCpy $6 "$APPDATA\IDN Node Industrial"
  CreateDirectory "$6"
  ClearErrors
  FileOpen $4 "$6\node.toml" r
  ${IfNot} ${Errors}
    ${Do}
      FileRead $4 $5
      ${If} ${Errors}
        ${ExitDo}
      ${EndIf}
      StrCpy $7 $5 6
      ${If} $7 == "[mqtt]"
        FileClose $4
        Return
      ${EndIf}
    ${Loop}
    FileClose $4
  ${EndIf}
  FileOpen $4 "$6\node.toml" a
  FileSeek $4 0 END
  FileWrite $4 "$\r$\n# The Site Manager on this computer has 1883 (the installer, ${VERSION}).$\r$\n[mqtt]$\r$\nloopback = $\"127.0.0.1:1885$\"$\r$\n"
  FileClose $4
FunctionEnd

Section "Uninstall"
  SetRegView 64
  SetShellVarContext all
  ; The secure attention sequence policy as it was before (or not set).
  ReadRegStr $1 HKLM "${REG_KEY}" "SasBefore"
  ${If} $1 == "none"
    DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Policies\System" "SoftwareSASGeneration"
  ${ElseIf} $1 != ""
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Policies\System" "SoftwareSASGeneration" $1
  ${EndIf}
  ; Stopped and waited for (`net stop`), then anything left ended: the session helper
  ; and a service slow to stop kept their files (deep bug run 2026-11-09).
  nsExec::Exec 'net stop ${SERVICE}'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-helper-industrial.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-industrial.exe'
  Pop $0
  nsExec::Exec 'sc.exe delete ${SERVICE}'
  Pop $0
  nsExec::Exec 'net stop ${SM_SERVICE}'
  Pop $0
  nsExec::Exec 'taskkill /F /IM idn-site-manager-industrial.exe'
  Pop $0
  nsExec::Exec 'sc.exe delete ${SM_SERVICE}'
  Pop $0
  nsExec::Exec 'netsh advfirewall firewall delete rule name="IDN Site Manager | Industrial"'
  Delete /REBOOTOK "$INSTDIR\idn-site-manager-industrial.exe"
  Delete "$SMPROGRAMS\IDN Site Manager Industrial.lnk"
  Delete /REBOOTOK "$INSTDIR\idn-industrial.exe"
  Delete /REBOOTOK "$INSTDIR\idn-helper-industrial.exe"
  nsExec::Exec 'taskkill /F /IM idn-voice.exe'
  Pop $0
  Delete /REBOOTOK "$INSTDIR\idn-voice.exe"
  Delete "$SMPROGRAMS\IDN Voice.lnk"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "IDN Voice"
  Delete "$INSTDIR\idn-industrial.ico"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\README-industrial.txt"
  Delete "$INSTDIR\uninstall.exe"
  ; FFmpeg's DLLs (the only DLLs installed here) and its licence.
  Delete "$INSTDIR\*.dll"
  Delete "$INSTDIR\FFmpeg-LICENSE.txt"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\IDN Node Industrial panel.lnk"
  DeleteRegKey HKLM "${UNINST_KEY}"
  DeleteRegKey HKLM "${REG_KEY}"
  ; The machine's identity and unsent items stay in %ProgramData%\IDN Node Industrial,
  ; the site in %ProgramData%\IDN Site Manager Industrial: installed again, nothing is
  ; lost.
SectionEnd
