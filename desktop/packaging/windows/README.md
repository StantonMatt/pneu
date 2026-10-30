# Windows MSIX

Run from a Windows Rust/MSVC development environment with the Windows SDK:

```powershell
./desktop/packaging/windows/build-msix.ps1 -OutDir C:/build/pneu
```

This builds the GUI `Pneu.exe` and console `pneud.exe` from one entry point,
embeds their icon, version resources and application manifest, statically links
the C runtime, indexes the logo variants, validates the manifest, and produces an
unsigned x64 package. The GUI binary is opt-in (`windows-package`); ordinary
Linux/macOS builds keep the existing binary and packaging.

Both executables declare PerMonitorV2 DPI awareness (`true/pm` fallback), Windows
10/11 compatibility and `asInvoker` execution. The manifest sets awareness before
any native windows are created; the tray inherits it and the Slint/winit child
skips winit's process-awareness setter while retaining per-window DPI scaling.
Common Controls v6 is not required: muda/tray-icon use USER32 menus/windows,
muda's `common-controls-v6` feature is disabled, and pickers use `IFileOpenDialog`.

The share target declares `StorageItems` and `Bitmap`, as recommended for image
handlers in Microsoft's [Share receive guidance](https://learn.microsoft.com/windows/apps/develop/windows-integration/integrate-sharesheet-receive).
Image sources can use Bitmap when filtering targets. Activation still reads only
original `StorageItems` files, retaining their names and bytes; bitmap-only shares
report an error asking the user to save the image and share its file.

For sideload testing, pass `-CertificateThumbprint` for a code-signing certificate
in CurrentUser/My whose subject exactly matches `-Publisher`. Trust the test
certificate on the disposable machine first. The script never creates or exports
keys, changes trust, or installs the package. Install from an interactive Windows
session; SSH-only deployment can fail at PLM initialization.

The reserved Microsoft Store product **Pneu** uses these Partner Center values:

- `-IdentityName MatthewStanton.Pneu`: Package/Identity/Name.
- `-Publisher 'CN=68A11A39-FF28-48D5-9C3E-D4758423D785'`: Package/Identity/Publisher.
- `-PublisherDisplayName 'Matthew Stanton'`: Package/Properties/PublisherDisplayName.
- `-Version`: four 16-bit components, nonzero major, revision zero; defaults to
  the workspace version plus `.0`. Increase it for every submitted package.

The Store package family name is `MatthewStanton.Pneu_6es1gykrn3d1y`; the Store
ID is `9N48FLMGSDDV`. Pass the identity values above when building for submission.
The script defaults (`StantonMatt.Pneu.Dev`, `CN=Pneu Dev`, `Matthew Stanton`)
remain for local testing. The application ID is `App`; Windows derives its AUMID
and family from the supplied identity. Keep the toast CLSID in sync with
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

The release workflow passes the Store identity values above and retains the
**unsigned** MSIX as the `Pneu-Windows-Store-msix` workflow artifact only. Its
version defaults to the workspace version plus `.0` (2.2.1.0 for this release).
Partner Center accepts unsigned packages and signs them for distribution. The
workflow does not attach the MSIX to the public GitHub release, sign it locally,
or submit to Partner Center. Logo assets are reproducible via
`generate-assets.py` with Pillow; no image tooling is needed in the build job.
