; Hooks of the Windows installer (bundle.windows.nsis.installerHooks in
; tauri.release.conf.json).
;
; The harness service keeps running while the app is updated, and Windows
; cannot overwrite a running program but can rename it. So the installer
; moves the running service program aside under a name of its own; the new
; program takes its place, and the running service moves to it once no turn
; runs (a service update, ADR 0007). Programs moved aside earlier are
; deleted once they no longer run; one that still runs stays until the next
; install.

!macro NSIS_HOOK_PREINSTALL
  Delete "$INSTDIR\leiser-harness-service.old-*.exe"
  ; $0 may be in use by the installer around the hook.
  Push $0
  System::Call "kernel32::GetTickCount() i .r0"
  Rename "$INSTDIR\leiser-harness-service.exe" "$INSTDIR\leiser-harness-service.old-$0.exe"
  Pop $0
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  Delete "$INSTDIR\leiser-harness-service.old-*.exe"
!macroend
