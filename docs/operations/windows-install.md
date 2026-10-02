# Installing tze_hud on Windows

CI builds `tze_hud.exe` (`x86_64-pc-windows-msvc`, static CRT, one file) and
publishes it from `.github/workflows/windows.yml`:

- **`dev`**: a rolling prerelease rebuilt from every merge to `main`.
- **`v*`**: a release per tag.

Each release carries `tze_hud.exe`, `tze_hud.exe.sha256`, `tze_hud.exe.minisig`,
and `tze_hud.pdb` (symbols for WPA, PIX, and crash dumps) when emitted.

## Download and verify

```powershell
gh release download dev -R tzeusy-org/tze-hud -p "tze_hud.exe*"
minisign -Vm tze_hud.exe -P RWTpCWtkNWD3YEwR2XCS2cwutGmd/fJCVuq9a99frgpLfTinnjWPsuvE
```

The public key is also committed at `app/tze_hud_app/minisign.pub`. The
signing key lives only in the `MINISIGN_SECRET_KEY` repository secret; the
release job refuses to publish without it and checks every signature against
the committed public key.

## Run

Until first-run install and pairing land (T6 in `docs/scope.md`), run it with
a config and a PSK:

```powershell
$env:TZE_HUD_PSK = "<a long random value>"
.\tze_hud.exe --config tze_hud.toml --window-mode overlay
```

`app/tze_hud_app/config/production.toml` is the reference config.
