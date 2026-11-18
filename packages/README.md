# Package manager installation

Tagged Unbind versions ship a Homebrew formula, a Scoop manifest, and a WinGet manifest bundle. Each recipe uses the permanent URL and SHA-256 checksum for its architecture.

## Homebrew

Add the Unbind tap and install the formula.

```sh
brew tap nearbycoder/unbind
brew trust --formula nearbycoder/unbind/unbind
brew install nearbycoder/unbind/unbind
```

## Scoop

Add the Unbind bucket in PowerShell.

```powershell
scoop bucket add unbind https://github.com/nearbycoder/scoop-unbind
scoop install unbind/unbind
```

## WinGet

Download and extract the multi-file manifests before installing them.

```powershell
irm 'https://unbind.dev/downloads/releases/latest/unbind-winget.zip' -OutFile unbind-winget.zip
Expand-Archive unbind-winget.zip -DestinationPath unbind-winget
winget install --manifest .\unbind-winget
```

An administrator can enable WinGet's local manifest feature using `winget settings --enable LocalManifestFiles`. Publishing to the WinGet community source requires its separate package submission and review process.
