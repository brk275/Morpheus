; Copyright 2019-2023 Tauri Programme within The Commons Conservancy
; Copyright 2023-2023 CrabNebula Ltd.
; SPDX-License-Identifier: Apache-2.0 OR MIT
; Morpheus fixed-ledger adaptation of Cargo Packager 0.11.8 installer.nsi.
; Original SHA256: 3ff5fde09ae24dfc031ab91f939b290f394e2c6d3549f2c43c37414716a47b7a
; Set the compression algorithm.
!if "{{compression}}" == ""
  SetCompressor /SOLID lzma
!else
  SetCompressor /SOLID "{{compression}}"
!endif

Unicode true

!include MUI2.nsh
!include FileFunc.nsh
!include x64.nsh
!include WordFunc.nsh
!include "FileAssociation.nsh"

!define MANUFACTURER "{{manufacturer}}"
!define PRODUCTNAME "{{product_name}}"
!define VERSION "{{version}}"
!define VERSIONWITHBUILD "{{version_with_build}}"
!define SHORTDESCRIPTION "{{short_description}}"
!define INSTALLMODE "{{install_mode}}"
!if "${INSTALLMODE}" != "currentUser"
  !error "Morpheus payload updates support currentUser installations only"
!endif
!define LICENSE "{{license}}"
!define INSTALLERICON "{{installer_icon}}"
!define SIDEBARIMAGE "{{sidebar_image}}"
!define HEADERIMAGE "{{header_image}}"
!define MAINBINARYNAME "{{main_binary_name}}"
!define MAINBINARYSRCPATH "{{main_binary_path}}"
!define IDENTIFIER "{{identifier}}"
!define COPYRIGHT "{{copyright}}"
!define OUTFILE "{{out_file}}"
!define ARCH "{{arch}}"
!define PLUGINSPATH "{{additional_plugins_path}}"
!define ALLOWDOWNGRADES "{{allow_downgrades}}"
!define DISPLAYLANGUAGESELECTOR "{{display_language_selector}}"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}"
!define MANUPRODUCTKEY "Software\${MANUFACTURER}\${PRODUCTNAME}"
!define UNINSTALLERSIGNCOMMAND "{{uninstaller_sign_cmd}}"
!define ESTIMATEDSIZE "{{estimated_size}}"

Name "${PRODUCTNAME}"
BrandingText "${COPYRIGHT}"
OutFile "${OUTFILE}"

VIProductVersion "${VERSIONWITHBUILD}"
VIAddVersionKey "ProductName" "${PRODUCTNAME}"
VIAddVersionKey "FileDescription" "${SHORTDESCRIPTION}"
VIAddVersionKey "LegalCopyright" "${COPYRIGHT}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"

; Plugins path, currently exists for linux only
!if "${PLUGINSPATH}" != ""
    !addplugindir "${PLUGINSPATH}"
!endif

!if "${UNINSTALLERSIGNCOMMAND}" != ""
  !uninstfinalize '${UNINSTALLERSIGNCOMMAND}'
!endif

; Handle install mode, `perUser`, `perMachine` or `both`
!if "${INSTALLMODE}" == "perMachine"
  RequestExecutionLevel highest
!endif

!if "${INSTALLMODE}" == "currentUser"
  RequestExecutionLevel user
!endif

!if "${INSTALLMODE}" == "both"
  !define MULTIUSER_MUI
  !define MULTIUSER_INSTALLMODE_INSTDIR "${PRODUCTNAME}"
  !define MULTIUSER_INSTALLMODE_COMMANDLINE
  !if "${ARCH}" == "x64"
    !define MULTIUSER_USE_PROGRAMFILES64
  !else if "${ARCH}" == "arm64"
    !define MULTIUSER_USE_PROGRAMFILES64
  !endif
  !define MULTIUSER_INSTALLMODE_DEFAULT_REGISTRY_KEY "${UNINSTKEY}"
  !define MULTIUSER_INSTALLMODE_DEFAULT_REGISTRY_VALUENAME "CurrentUser"
  !define MULTIUSER_INSTALLMODEPAGE_SHOWUSERNAME
  !define MULTIUSER_EXECUTIONLEVEL Highest
  !include MultiUser.nsh
!endif

; installer icon
!if "${INSTALLERICON}" != ""
  !define MUI_ICON "${INSTALLERICON}"
!endif

; installer sidebar image
!if "${SIDEBARIMAGE}" != ""
  !define MUI_WELCOMEFINISHPAGE_BITMAP "${SIDEBARIMAGE}"
!endif

; installer header image
!if "${HEADERIMAGE}" != ""
  !define MUI_HEADERIMAGE
  !define MUI_HEADERIMAGE_BITMAP  "${HEADERIMAGE}"
!endif

; Language choice remains in memory. MUI's registry persistence callbacks
; must not read unrelated values or write after the installed lock is released.

; Installer pages, must be ordered as they appear
; 1. Welcome Page
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_WELCOME

; 2. License Page (if defined)
!if "${LICENSE}" != ""
  !define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
  !insertmacro MUI_PAGE_LICENSE "${LICENSE}"
!endif

; 3. Install mode (if it is set to `both`)
!if "${INSTALLMODE}" == "both"
  !define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
  !insertmacro MULTIUSER_PAGE_INSTALLMODE
!endif


; Existing installations are never maintained by this executable. The owner
; must use the installed application's supported updater or uninstall it
; explicitly before selecting a new destination. No registry value is executed.

; 5. Choose install directoy page
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_DIRECTORY

; 6. Start menu shortcut page
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
Var AppStartMenuFolder
!insertmacro MUI_PAGE_STARTMENU Application $AppStartMenuFolder

; 7. Installation page
!insertmacro MUI_PAGE_INSTFILES

; 8. Finish page
;
; Don't auto jump to finish page after installation page,
; because the installation page has useful info that can be used debug any issues with the installer.
!define MUI_FINISHPAGE_NOAUTOCLOSE
; Use show readme button in the finish page as a button create a desktop shortcut
!define MUI_FINISHPAGE_SHOWREADME
!define MUI_FINISHPAGE_SHOWREADME_TEXT "$(createDesktop)"
!define MUI_FINISHPAGE_SHOWREADME_FUNCTION CreateDesktopShortcut
; Show run app after installation.
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_FUNCTION MorpheusRunInstalled
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_FINISH

; Uninstaller Pages
; 1. Confirm uninstall page

!insertmacro MUI_UNPAGE_CONFIRM

; 2. Uninstalling Page
!insertmacro MUI_UNPAGE_INSTFILES

;Languages
{{#each languages}}
!insertmacro MUI_LANGUAGE "{{this}}"
{{/each}}
!insertmacro MUI_RESERVEFILE_LANGDLL
{{#each language_files}}
  !include "{{this}}"
{{/each}}

!macro SetContext
  !if "${INSTALLMODE}" == "currentUser"
    SetShellVarContext current
  !else if "${INSTALLMODE}" == "perMachine"
    SetShellVarContext all
  !endif

  ${If} ${RunningX64}
    !if "${ARCH}" == "x64"
      SetRegView 64
    !else if "${ARCH}" == "arm64"
      SetRegView 64
    !else
      SetRegView 32
    !endif
  ${EndIf}
!macroend

!macro MorpheusRejectRegisteredInstallation ROOT VIEW
  !define MORPHEUS_REG_ID ${__LINE__}
  System::Call 'advapi32::RegOpenKeyExW(p ${ROOT}, w "${UNINSTKEY}", i 0, i ${VIEW}, *p.r0)i.r1'
  StrCmp $1 0 morpheus_registered_${MORPHEUS_REG_ID}
  StrCmp $1 2 morpheus_registration_absent_${MORPHEUS_REG_ID}
  StrCmp $1 3 morpheus_registration_absent_${MORPHEUS_REG_ID}
  Abort "The existing installation could not be checked. Handle it manually before installing Morpheus."
  morpheus_registered_${MORPHEUS_REG_ID}:
    System::Call 'advapi32::RegCloseKey(p r0)'
    Abort "Morpheus is already registered. Use its supported updater or uninstall it explicitly before installing again."
  morpheus_registration_absent_${MORPHEUS_REG_ID}:
  !undef MORPHEUS_REG_ID
!macroend

Function MorpheusRequireFreshDestination
  ; Do not enumerate unrelated products, inspect UninstallString, select a
  ; destination from registry values, run an uninstaller, or remove old files.
  !insertmacro MorpheusRejectRegisteredInstallation 0x80000001 0x101
  !insertmacro MorpheusRejectRegisteredInstallation 0x80000001 0x201
  !insertmacro MorpheusRejectRegisteredInstallation 0x80000002 0x101
  !insertmacro MorpheusRejectRegisteredInstallation 0x80000002 0x201
  ClearErrors
  GetFullPathName $R8 "$INSTDIR"
  IfErrors morpheus_bad_destination
  StrLen $R9 $R8
  IntCmp $R9 3 morpheus_bad_destination morpheus_bad_destination
  StrCpy $0 $R8 1 1
  StrCmp $0 ":" 0 morpheus_bad_destination
  StrCpy $0 $R8 1 2
  StrCmp $0 "\" 0 morpheus_bad_destination
  StrCpy $INSTDIR $R8
  StrCpy $0 $INSTDIR 3
  System::Call 'kernel32::GetDriveTypeW(w r0)i.r1'
  IntCmp $1 3 +2
    Goto morpheus_bad_destination
  ; Never overwrite even an empty existing destination. In particular, an
  ; unregistered legacy app or unrelated directory is not an update target.
  System::Call 'kernel32::GetFileAttributesW(w "$INSTDIR")i.r0 ?e'
  Pop $1
  StrCmp $0 -1 0 morpheus_existing_destination
  StrCmp $1 2 morpheus_check_parents
  StrCmp $1 3 morpheus_check_parents
  Goto morpheus_bad_destination
  morpheus_check_parents:
    ${GetParent} "$R8" $R8
    StrCmp $R8 "" morpheus_bad_destination
    StrLen $R9 $R8
    IntCmp $R9 2 morpheus_drive_parent morpheus_bad_destination morpheus_parent_attributes
    morpheus_drive_parent:
      StrCpy $R8 "$R8\"
    morpheus_parent_attributes:
    System::Call 'kernel32::GetFileAttributesW(w "$R8")i.r0'
    StrCmp $0 -1 morpheus_bad_destination
    IntOp $1 $0 & 0x400
    IntCmp $1 0 +2
      Goto morpheus_bad_destination
    IntOp $1 $0 & 0x10
    IntCmp $1 0 morpheus_bad_destination
    StrLen $R9 $R8
    IntCmp $R9 3 morpheus_destination_ok morpheus_bad_destination morpheus_check_parents
  morpheus_existing_destination:
    Abort "Choose a new, nonexistent directory. An existing installation must be updated or uninstalled explicitly first."
  morpheus_bad_destination:
    Abort "Choose a new local directory with existing parents and no junctions or symbolic links."
  morpheus_destination_ok:
FunctionEnd

Var PassiveMode
Function .onInit
  ${GetOptions} $CMDLINE "/P" $PassiveMode
  IfErrors +2 0
    StrCpy $PassiveMode 1

  !if "${DISPLAYLANGUAGESELECTOR}" == "true"
    !insertmacro MUI_LANGDLL_DISPLAY
  !endif

  !insertmacro SetContext

  ${If} $INSTDIR == ""
    ; Set default install location
    !if "${INSTALLMODE}" == "perMachine"
      ${If} ${RunningX64}
        !if "${ARCH}" == "x64"
          StrCpy $INSTDIR "$PROGRAMFILES64\${PRODUCTNAME}"
        !else if "${ARCH}" == "arm64"
          StrCpy $INSTDIR "$PROGRAMFILES64\${PRODUCTNAME}"
        !else
          StrCpy $INSTDIR "$PROGRAMFILES\${PRODUCTNAME}"
        !endif
      ${Else}
        StrCpy $INSTDIR "$PROGRAMFILES\${PRODUCTNAME}"
      ${EndIf}
    !else if "${INSTALLMODE}" == "currentUser"
      StrCpy $INSTDIR "$LOCALAPPDATA\${PRODUCTNAME}"
    !endif

  ${EndIf}


  !if "${INSTALLMODE}" == "both"
    !insertmacro MULTIUSER_INIT
  !endif
  Call MorpheusRequireFreshDestination
FunctionEnd


Var MorpheusLifetimeLock
!macro MorpheusAcquireLifetimeLock
  !define MORPHEUS_LOCK_ID ${__LINE__}
  ClearErrors
  ; The app and helper hold this same share-denying file for their lifetimes.
  ; Never kill a process by name, elevate, or overwrite an unfinished update.
  CreateDirectory "$INSTDIR.morpheus-update"
  IfErrors morpheus_lock_failed_${MORPHEUS_LOCK_ID}
  System::Call 'kernel32::GetFileAttributesW(w "$INSTDIR.morpheus-update")i.r0'
  IntOp $0 $0 & 0x400
  IntCmp $0 0 +2
    Goto morpheus_lock_failed_${MORPHEUS_LOCK_ID}
  System::Call 'kernel32::CreateFileW(w "$INSTDIR.morpheus-update\lock", i 0xC0000000, i 0, p 0, i 4, i 0x00200000, p 0)p.r0'
  StrCmp $0 -1 morpheus_lock_failed_${MORPHEUS_LOCK_ID}
  StrCpy $MorpheusLifetimeLock $0
  System::Call 'kernel32::GetFileAttributesW(w "$INSTDIR.morpheus-update\lock")i.r0'
  IntOp $0 $0 & 0x400
  IntCmp $0 0 +2
    Goto morpheus_lock_failed_${MORPHEUS_LOCK_ID}
  IfFileExists "$INSTDIR.morpheus-update\journal.json" morpheus_lock_failed_${MORPHEUS_LOCK_ID}
  Goto morpheus_lock_ok_${MORPHEUS_LOCK_ID}
  morpheus_lock_failed_${MORPHEUS_LOCK_ID}:
    Abort "Close Morpheus and finish any pending update before changing this installation."
  morpheus_lock_ok_${MORPHEUS_LOCK_ID}:
  !undef MORPHEUS_LOCK_ID
!macroend

!macro MorpheusReleaseLifetimeLock
  StrCmp $MorpheusLifetimeLock 0 +3
    System::Call 'kernel32::CloseHandle(p $MorpheusLifetimeLock)'
    StrCpy $MorpheusLifetimeLock 0
!macroend

; NSIS DeleteRegKey /ifempty ignores values. Query BOTH counts natively;
; failed/unknown queries retain the key, including an empty-named default.
!macro MorpheusDeleteEmptyUserKey KEY
  !define MORPHEUS_EMPTY_ID ${__LINE__}
  !if "${ARCH}" == "x64"
    !define MORPHEUS_EMPTY_VIEW 0x100
    !define MORPHEUS_EMPTY_READ 0x109
  !else if "${ARCH}" == "arm64"
    !define MORPHEUS_EMPTY_VIEW 0x100
    !define MORPHEUS_EMPTY_READ 0x109
  !else
    !define MORPHEUS_EMPTY_VIEW 0x200
    !define MORPHEUS_EMPTY_READ 0x209
  !endif
  System::Call 'advapi32::RegOpenKeyExW(p 0x80000001, w "${KEY}", i 0, i ${MORPHEUS_EMPTY_READ}, *p.r0)i.r1'
  StrCmp $1 0 0 morpheus_empty_done_${MORPHEUS_EMPTY_ID}
  StrCpy $2 -1
  StrCpy $3 -1
  System::Call 'advapi32::RegQueryInfoKeyW(p r0, p 0, p 0, p 0, *i.r2, p 0, p 0, *i.r3, p 0, p 0, p 0, p 0)i.r1'
  StrCmp $1 0 0 morpheus_empty_close_${MORPHEUS_EMPTY_ID}
  StrCmp $2 0 0 morpheus_empty_close_${MORPHEUS_EMPTY_ID}
  StrCmp $3 0 0 morpheus_empty_close_${MORPHEUS_EMPTY_ID}
  ; RegDeleteKeyEx also refuses subkeys created since the checked snapshot.
  ; Current-user writes remain under the installed-path lifetime exclusion.
  System::Call 'advapi32::RegDeleteKeyExW(p 0x80000001, w "${KEY}", i ${MORPHEUS_EMPTY_VIEW}, i 0)i.r1'
  morpheus_empty_close_${MORPHEUS_EMPTY_ID}:
  System::Call 'advapi32::RegCloseKey(p r0)i.r1'
  morpheus_empty_done_${MORPHEUS_EMPTY_ID}:
  !undef MORPHEUS_EMPTY_READ
  !undef MORPHEUS_EMPTY_VIEW
  !undef MORPHEUS_EMPTY_ID
!macroend

Section Install
  Call MorpheusRequireFreshDestination
  !insertmacro MorpheusAcquireLifetimeLock
  Call MorpheusRequireFreshDestination
  SetOutPath $INSTDIR

  ; Copy main executable
  File "${MAINBINARYSRCPATH}"

  ; Create resources directory structure
  {{#each resources_dirs}}
    CreateDirectory "$INSTDIR\\{{this}}"
  {{/each}}

  ; Copy resources
  {{#each resources}}
    File /a "/oname={{this}}" "{{@key}}"
  {{/each}}

  ; Copy external binaries
  {{#each binaries}}
    File /a "/oname={{this}}" "{{@key}}"
  {{/each}}

  ; Create file associations
  {{#each file_associations as |association| ~}}
    {{#each association.extensions as |ext| ~}}
       !insertmacro APP_ASSOCIATE "{{ext}}" "{{or association.name ext}}" "{{association-description association.description ext}}" "$INSTDIR\${MAINBINARYNAME}.exe,0" "Open with ${PRODUCTNAME}" "$INSTDIR\${MAINBINARYNAME}.exe $\"%1$\""
    {{/each}}
  {{/each}}

  ; Register deep links
  {{#each deep_link_protocols as |protocol| ~}}
    WriteRegStr SHCTX "Software\Classes\\{{protocol}}" "URL Protocol" ""
    WriteRegStr SHCTX "Software\Classes\\{{protocol}}" "" "URL:${BUNDLEID} protocol"
    WriteRegStr SHCTX "Software\Classes\\{{protocol}}\DefaultIcon" "" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\",0"
    WriteRegStr SHCTX "Software\Classes\\{{protocol}}\shell\open\command" "" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\" $\"%1$\""
  {{/each}}

  ; Create uninstaller
  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; Save $INSTDIR in registry for future installations
  WriteRegStr SHCTX "${MANUPRODUCTKEY}" "" $INSTDIR

  !if "${INSTALLMODE}" == "both"
    ; Save install mode to be selected by default for the next installation such as updating
    ; or when uninstalling
    WriteRegStr SHCTX "${UNINSTKEY}" $MultiUser.InstallMode 1
  !endif

  ; Registry information for add/remove programs
  WriteRegStr SHCTX "${UNINSTKEY}" "DisplayName" "${PRODUCTNAME}"
  WriteRegStr SHCTX "${UNINSTKEY}" "DisplayIcon" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\""
  WriteRegStr SHCTX "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr SHCTX "${UNINSTKEY}" "Publisher" "${MANUFACTURER}"
  WriteRegStr SHCTX "${UNINSTKEY}" "InstallLocation" "$\"$INSTDIR$\""
  WriteRegStr SHCTX "${UNINSTKEY}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegDWORD SHCTX "${UNINSTKEY}" "NoModify" "1"
  WriteRegDWORD SHCTX "${UNINSTKEY}" "NoRepair" "1"
  WriteRegDWORD SHCTX "${UNINSTKEY}" "EstimatedSize" "${ESTIMATEDSIZE}"

  ; Create start menu shortcut (GUI)
  !insertmacro MUI_STARTMENU_WRITE_BEGIN Application
    Call CreateStartMenuShortcut
  !insertmacro MUI_STARTMENU_WRITE_END

  ; Create shortcuts for silent and passive installers, which
  ; can be disabled by passing `/NS` flag
  ; GUI installer has buttons for users to control creating them
  IfSilent check_ns_flag 0
  ${IfThen} $PassiveMode == 1 ${|} Goto check_ns_flag ${|}
  Goto shortcuts_done
  check_ns_flag:
    ${GetOptions} $CMDLINE "/NS" $R0
    IfErrors 0 shortcuts_done
      Call CreateDesktopShortcut
      Call CreateStartMenuShortcut
  shortcuts_done:

  ; Auto close this page for passive mode
  ${IfThen} $PassiveMode == 1 ${|} SetAutoClose true ${|}
SectionEnd

Function MorpheusRunInstalled
  !insertmacro MorpheusReleaseLifetimeLock
  Exec '"$INSTDIR\${MAINBINARYNAME}.exe"'
FunctionEnd

Function .onInstSuccess
  ; Check for `/R` flag only in silent and passive installers because
  ; GUI installer has a toggle for the user to (re)start the app
  IfSilent check_r_flag 0
  ${IfThen} $PassiveMode == 1 ${|} Goto check_r_flag ${|}
  Goto run_done
  check_r_flag:
    ${GetOptions} $CMDLINE "/R" $R0
    IfErrors run_done 0
      Call MorpheusRunInstalled
  run_done:
FunctionEnd

Function un.onInit
  !insertmacro SetContext

  !if "${INSTALLMODE}" == "both"
    !insertmacro MULTIUSER_UNINIT
  !endif

  !insertmacro MUI_UNGETLANGUAGE
FunctionEnd

Section Uninstall
  !insertmacro MorpheusAcquireLifetimeLock

  ; Delete the app directory and its content from disk
  ; Copy main executable
  Delete "$INSTDIR\${MAINBINARYNAME}.exe"

  ; Delete resources
  {{#each resources}}
    Delete "$INSTDIR\\{{this}}"
  {{/each}}

  ; Delete external binaries
  {{#each binaries}}
    Delete "$INSTDIR\\{{this}}"
  {{/each}}

  ; Delete app associations
  {{#each file_associations as |association| ~}}
    {{#each association.ext as |ext| ~}}
      !insertmacro APP_UNASSOCIATE "{{ext}}" "{{or association.name ext}}"
    {{/each}}
  {{/each}}

  ; Delete deep links
  {{#each deep_link_protocols as |protocol| ~}}
    ReadRegStr $R7 SHCTX "Software\Classes\\{{protocol}}\shell\open\command" ""
    !if $R7 == "$\"$INSTDIR\${MAINBINARYNAME}.exe$\" $\"%1$\""
      DeleteRegKey SHCTX "Software\Classes\\{{protocol}}"
    !endif
  {{/each}}

  ; Delete uninstaller
  Delete "$INSTDIR\uninstall.exe"

  {{#each resources_dirs}}
  RMDir /REBOOTOK "$INSTDIR\\{{this}}"
  {{/each}}
  RMDir "$INSTDIR"

  ; Remove start menu shortcut
  !insertmacro MUI_STARTMENU_GETFOLDER Application $AppStartMenuFolder
  Delete "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk"
  RMDir "$SMPROGRAMS\$AppStartMenuFolder"

  ; Remove desktop shortcuts
  Delete "$DESKTOP\${PRODUCTNAME}.lnk"

  ; Remove only our exact saved install path. Preserve any foreign replacement
  ; or unrelated values/subkeys, then remove empty owned product/vendor keys.
  ClearErrors
  ReadRegStr $R8 SHCTX "${MANUPRODUCTKEY}" ""
  IfErrors morpheus_keep_manufacturer_value
  StrCmpS $R8 $INSTDIR 0 morpheus_keep_manufacturer_value
  DeleteRegValue SHCTX "${MANUPRODUCTKEY}" ""
  !insertmacro MorpheusDeleteEmptyUserKey "${MANUPRODUCTKEY}"
  !insertmacro MorpheusDeleteEmptyUserKey "Software\${MANUFACTURER}"
  morpheus_keep_manufacturer_value:

  ; Remove registry information for add/remove programs
  !if "${INSTALLMODE}" == "both"
    DeleteRegKey SHCTX "${UNINSTKEY}"
  !else if "${INSTALLMODE}" == "perMachine"
    DeleteRegKey HKLM "${UNINSTKEY}"
  !else
    DeleteRegKey HKCU "${UNINSTKEY}"
  !endif




  ${GetOptions} $CMDLINE "/P" $R0
  IfErrors +2 0
    SetAutoClose true
  !insertmacro MorpheusReleaseLifetimeLock
  Delete "$INSTDIR.morpheus-update\lock"
  RMDir "$INSTDIR.morpheus-update"
SectionEnd



Function SkipIfPassive
  ${IfThen} $PassiveMode == 1  ${|} Abort ${|}
FunctionEnd

Function CreateDesktopShortcut
  CreateShortcut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  ApplicationID::Set "$DESKTOP\${PRODUCTNAME}.lnk" "${IDENTIFIER}"
FunctionEnd

Function CreateStartMenuShortcut
  CreateDirectory "$SMPROGRAMS\$AppStartMenuFolder"
  CreateShortcut "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  ApplicationID::Set "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk" "${IDENTIFIER}"
FunctionEnd
