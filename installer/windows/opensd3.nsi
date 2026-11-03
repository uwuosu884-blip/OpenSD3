; OpenSD3 Windows x64 Graphical Installer (NSIS 3 / MUI2)
; Installs:
;   - VST3 Plugin (64-bit)  -> C:\Program Files\Common Files\VST3\open-sd3.vst3
;   - CLAP Plugin (64-bit)  -> C:\Program Files\Common Files\CLAP\open-sd3.clap
;   - Standalone App        -> C:\Program Files\OpenSD3\open-sd3.exe
;   - CLI Toolkit & Docs    -> C:\Program Files\OpenSD3\sd3tool.exe

Unicode True
SetCompressor /SOLID lzma
SetCompressorDictSize 32

!ifndef APP_VERSION
  !define APP_VERSION "0.3.0"
!endif

!ifndef STAGE_DIR
  !define STAGE_DIR "."
!endif

!ifndef OUT_FILE
  !define OUT_FILE "OpenSD3-${APP_VERSION}-Windows-x64-Setup.exe"
!endif

!define APP_NAME "OpenSD3 Drum Workstation"
!define APP_PUBLISHER "OpenSD3 Project"
!define APP_URL "https://github.com/uwuosu884-blip/OpenSD3"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\OpenSD3"

Name "${APP_NAME} ${APP_VERSION}"
OutFile "${OUT_FILE}"
InstallDir "$PROGRAMFILES64\OpenSD3"
InstallDirRegKey HKLM "Software\OpenSD3" "Install_Dir"
RequestExecutionLevel admin
BrandingText "${APP_NAME} v${APP_VERSION} (64-bit)"

!include "MUI2.nsh"
!include "x64.nsh"

!define MUI_ABORTWARNING
!define MUI_COMPONENTSPAGE_SMALLDESC

; Wizard pages
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${STAGE_DIR}\COPYING.txt"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES

!define MUI_FINISHPAGE_RUN "$INSTDIR\open-sd3.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Launch OpenSD3 Standalone Drum Workstation"
!insertmacro MUI_PAGE_FINISH

; Uninstaller pages
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

VIProductVersion "${APP_VERSION}.0"
VIAddVersionKey /LANG=${LANG_ENGLISH} "ProductName" "${APP_NAME}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "CompanyName" "${APP_PUBLISHER}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "FileDescription" "${APP_NAME} Windows x64 Setup"
VIAddVersionKey /LANG=${LANG_ENGLISH} "FileVersion" "${APP_VERSION}.0"
VIAddVersionKey /LANG=${LANG_ENGLISH} "ProductVersion" "${APP_VERSION}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "LegalCopyright" "GPL-3.0-or-later"

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "OpenSD3 requires 64-bit Windows 10 or Windows 11."
    Abort
  ${EndIf}
  SetRegView 64
FunctionEnd

Function un.onInit
  SetRegView 64
FunctionEnd

; ---------------------------------------------------------------------------
; Installer Sections
; ---------------------------------------------------------------------------

Section "VST3 Plugin (64-bit)" SecVST3
  SectionIn 1 2
  SetOutPath "$COMMONFILES64\VST3\open-sd3.vst3\Contents\x86_64-win"
  File "${STAGE_DIR}\vst3\open-sd3.vst3\Contents\x86_64-win\open-sd3.vst3"
SectionEnd

Section "CLAP Plugin (64-bit)" SecCLAP
  SectionIn 1 2
  SetOutPath "$COMMONFILES64\CLAP"
  File "${STAGE_DIR}\clap\open-sd3.clap"
SectionEnd

Section "Standalone Drum Workstation (open-sd3.exe)" SecStandalone
  SectionIn 1
  SetOutPath "$INSTDIR"
  File "${STAGE_DIR}\app\open-sd3.exe"

  CreateDirectory "$SMPROGRAMS\OpenSD3"
  CreateShortcut "$SMPROGRAMS\OpenSD3\OpenSD3 Drum Workstation.lnk" "$INSTDIR\open-sd3.exe" "" "$INSTDIR\open-sd3.exe" 0
SectionEnd

Section "CLI Extractor & Docs (sd3tool.exe)" SecCLI
  SectionIn 1
  SetOutPath "$INSTDIR"
  File "${STAGE_DIR}\app\sd3tool.exe"
  File "${STAGE_DIR}\README.md"
  File "${STAGE_DIR}\SUPERIOR_DRUMMER_3_FORMAT_SPEC.md"
  File "${STAGE_DIR}\COPYING.txt"

  CreateDirectory "$SMPROGRAMS\OpenSD3"
  CreateShortcut "$SMPROGRAMS\OpenSD3\OpenSD3 CLI Prompt (sd3tool).lnk" "$SYSDIR\cmd.exe" '/k "cd /d $\"$INSTDIR$\" && sd3tool.exe --help"' "$SYSDIR\cmd.exe" 0
SectionEnd

Section /o "Desktop Shortcut" SecDesktop
  IfFileExists "$INSTDIR\open-sd3.exe" 0 +2
    CreateShortcut "$DESKTOP\OpenSD3 Drum Workstation.lnk" "$INSTDIR\open-sd3.exe" "" "$INSTDIR\open-sd3.exe" 0
SectionEnd

Section -PostInstall
  SetOutPath "$INSTDIR"
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  CreateDirectory "$SMPROGRAMS\OpenSD3"
  CreateShortcut "$SMPROGRAMS\OpenSD3\Uninstall OpenSD3.lnk" "$INSTDIR\Uninstall.exe"

  WriteRegStr HKLM "Software\OpenSD3" "Install_Dir" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayVersion" "${APP_VERSION}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "Publisher" "${APP_PUBLISHER}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "URLInfoAbout" "${APP_URL}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\open-sd3.exe"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "UninstallString" "$\"$INSTDIR\Uninstall.exe$\""
  WriteRegStr HKLM "${UNINSTALL_KEY}" "QuietUninstallString" "$\"$INSTDIR\Uninstall.exe$\" /S"
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoRepair" 1
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "EstimatedSize" 26200
SectionEnd

; Component descriptions
!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecVST3} "Installs open-sd3.vst3 to C:\Program Files\Common Files\VST3 (for REAPER, Cubase, Ableton Live, FL Studio, Studio One, Bitwig)."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecCLAP} "Installs open-sd3.clap to C:\Program Files\Common Files\CLAP (for REAPER, Bitwig, FL Studio, Studio One)."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecStandalone} "Installs the standalone Windows GUI + WASAPI/MIDI drum workstation (open-sd3.exe) and Start Menu launcher."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecCLI} "Installs sd3tool.exe (lossless .obw/.ttpw WAV+SFZ extractor, scanner, ttfs unpacker & offline MIDI renderer) plus documentation."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} "Creates a Desktop shortcut for OpenSD3 Standalone."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ---------------------------------------------------------------------------
; Uninstaller Section
; ---------------------------------------------------------------------------

Section "Uninstall"
  RMDir /r "$COMMONFILES64\VST3\open-sd3.vst3"
  Delete "$COMMONFILES64\CLAP\open-sd3.clap"

  Delete "$DESKTOP\OpenSD3 Drum Workstation.lnk"
  RMDir /r "$SMPROGRAMS\OpenSD3"

  Delete "$INSTDIR\open-sd3.exe"
  Delete "$INSTDIR\sd3tool.exe"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\SUPERIOR_DRUMMER_3_FORMAT_SPEC.md"
  Delete "$INSTDIR\COPYING.txt"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey HKLM "${UNINSTALL_KEY}"
  DeleteRegKey HKLM "Software\OpenSD3"
SectionEnd
