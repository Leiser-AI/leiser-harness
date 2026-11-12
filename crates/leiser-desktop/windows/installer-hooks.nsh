; Hooks of the Windows installer (bundle.windows.nsis.installerHooks in
; tauri.release.conf.json).
;
; The harness service keeps running while the app is updated, and Windows
; cannot overwrite a running program but can rename it. So the installer
; moves the running service aside; the new one takes its place, and the
; running service moves to it once no turn runs (ADR 0007). The program
; moved aside last time is deleted when it no longer runs.

!macro NSIS_HOOK_PREINSTALL
  Delete "$INSTDIR\leiser-harness-service.old.exe"
  Rename "$INSTDIR\leiser-harness-service.exe" "$INSTDIR\leiser-harness-service.old.exe"
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  Delete "$INSTDIR\leiser-harness-service.old.exe"
!macroend
