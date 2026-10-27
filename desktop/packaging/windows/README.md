# Windows MSIX

Run from a Windows Rust/MSVC development environment with the Windows SDK:

```powershell
./desktop/packaging/windows/build-msix.ps1 -OutDir C:/build/pneu
```

This builds the GUI `Pneu.exe` and console `pneud.exe` from one entry point,
embeds their icon and version resources, statically links the C runtime,
indexes the logo variants, validates the manifest, and produces an
unsigned x64 package. The GUI binary is opt-in (`windows-package`); ordinary
Linux/macOS builds keep the existing binary and packaging.

For sideload testing, pass `-CertificateThumbprint` for a code-signing certificate
in CurrentUser/My whose subject exactly matches `-Publisher`. Trust the test
certificate on the disposable machine first. The script never creates or exports
keys, changes trust, or installs the package. Install from an interactive Windows
session; SSH-only deployment can fail at PLM initialization.

Before Store submission, supply these Partner Center values:

- `-IdentityName`: Package/Identity/Name.
- `-Publisher`: Package/Identity/Publisher, including the exact CN.
- `-PublisherDisplayName`: Package/Properties/PublisherDisplayName.
- `-Version`: four 16-bit components, nonzero major, revision zero; defaults to
  the workspace version plus `.0`. Increase it for every submitted package.

The defaults (`StantonMatt.Pneu.Dev`, `CN=Pneu Dev`, `Matthew Stanton`) are for
local testing. The application ID is `App`; Windows derives its AUMID and family
from the supplied identity. Keep the toast CLSID in sync with
`src/actions/windows_toasts.rs`. Inbound rules allow TCP 47821 and UDP 5353 for
both executables on all profiles, including Public (Tailscale).

The only restricted capability is `runFullTrust`: the app runs a per-user desktop
LAN file-transfer/clipboard daemon with a tray, native window, authenticated local
IPC and COM toast activation. Store review needs that justification, an accurate
privacy policy, and disclosures for network/firewall access, clipboard behavior,
optional sign-in startup, and writing received files to Downloads/Pneu. Startup
can be disabled in the app or Windows Settings. Do not add
`unvirtualizedResources`: package state uses the actual package LocalCache path,
while received files remain in Downloads after uninstall.

Restart Manager messages request the existing orderly daemon shutdown.
`RegisterApplicationRestart` uses `run` to resume in the background after an
update; StartupTask activation also stays in the background. Store-managed
updates and certification must still be checked with the registered Store
identity. Self-signed sideload tests do not establish Store approval.

The release workflow retains an **unsigned** MSIX as a workflow artifact only.
It does not attach an un-installable file to the public GitHub release, sign a
Store package, or submit to Partner Center. Logo assets are reproducible via
`generate-assets.py` with Pillow; no image tooling is needed in the build job.
