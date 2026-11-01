set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command"]

iscc := env("ISCC", "C:/Program Files/Inno Setup 7/ISCC.exe")

# Build LOKI in release mode, bundle it, and compile the Windows installer.
installer:
    cargo build --release --locked -p loki-server
    cargo xtask bundle
    & "{{iscc}}" packaging/loki.iss

# Re-render the committed icon files after editing packaging/icon/loki.syar.
# Builds use the committed files; run this by hand with Synarith's syar CLI.
icon syar="syar":
    cargo xtask icon --syar "{{syar}}"
