# Antivirus and Houndbit

## Why antivirus sometimes complains
Houndbit isn't code-signed yet. Antivirus programs (including Windows Defender and SmartScreen) are wary of programs that:
- have no digital signature,
- are new and haven't been downloaded by many people yet,
- live in `AppData` and get started over and over by another program (that's exactly what the hook helper does each time an AI agent takes a step).

None of this means something is wrong. It just means Windows doesn't know us yet.

## What the app already does to help
- `agenttracker-hook.exe` carries proper version details (company, product name, description), so it doesn't look like an anonymous program.
- Neither the hook helper nor the app opens hidden console windows any more.

## The real fix: code signing (not done yet)
The only reliable way to stop the warnings is to sign the installer and programs with a code-signing certificate. Two options:
- **Azure Trusted Signing**: about $10 a month and the cheapest route for individuals.
- **A regular code-signing certificate** from a certificate seller.

Both need a certificate you buy, plus a change to the build workflow. That hasn't been done.

## What you can do today

### 1. Tell Windows Defender to trust the Houndbit folders
Open **PowerShell as administrator** (right-click Start, then "Terminal (Admin)") and run:

```powershell
Add-MpPreference -ExclusionPath "$env:LOCALAPPDATA\Houndbit"
Add-MpPreference -ExclusionPath "$env:USERPROFILE\.agenttracker"
```

- The first folder holds the app (the installer uses a per-user install) and the hook helper.
- The second holds Houndbit's data and settings.

To check the exclusions:

```powershell
(Get-MpPreference).ExclusionPath
```

To undo them later:

```powershell
Remove-MpPreference -ExclusionPath "$env:LOCALAPPDATA\Houndbit"
Remove-MpPreference -ExclusionPath "$env:USERPROFILE\.agenttracker"
```

If you use a different antivirus (Norton, Avast, Bitdefender and so on), add the same two folders in its "exclusions" or "exceptions" settings.

### 2. Report a false positive to Microsoft
Upload the installer at https://www.microsoft.com/en-us/wdsi/filesubmission and choose "Incorrectly detected as malware/malicious". Once Microsoft reviews it, Defender usually stops flagging that version for everyone.

### 3. SmartScreen "Windows protected your PC"
Click **More info**, then **Run anyway**. You only need to do this once per installer.
