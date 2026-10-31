"""Make a portable LOKI bundle with a checksum-verified official Caddy binary."""
import argparse
import hashlib
import io
import platform
import shutil
import tarfile
import tomllib
import urllib.request
import zipfile
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=Path)
parser.add_argument("--output", type=Path, default=Path("target/bundle"))
parser.add_argument("--caddy-version", default="2.11.4")
args = parser.parse_args()
os_name = {"darwin": "mac"}.get(platform.system().lower(), platform.system().lower())
arch = {"amd64": "amd64", "x86_64": "amd64", "arm64": "arm64", "aarch64": "arm64"}[platform.machine().lower()]
extension = "zip" if os_name == "windows" else "tar.gz"
filename = f"caddy_{args.caddy_version}_{os_name}_{arch}.{extension}"
base = f"https://github.com/caddyserver/caddy/releases/download/v{args.caddy_version}/"


def download(name):
    request = urllib.request.Request(base + name, headers={"User-Agent": "LOKI-release-builder"})
    with urllib.request.urlopen(request, timeout=60) as response:
        return response.read()


checksums = download(f"caddy_{args.caddy_version}_checksums.txt").decode()
expected = next(line.split()[0] for line in checksums.splitlines() if line.split()[-1].lstrip("*") == filename)
archive = download(filename)
algorithm = {64: "sha256", 128: "sha512"}.get(len(expected))
assert algorithm is not None, "Unsupported Caddy checksum format"
assert hashlib.new(algorithm, archive).hexdigest() == expected, "Caddy archive checksum mismatch"
executable = "caddy.exe" if os_name == "windows" else "caddy"
license_bytes = None
if extension == "zip":
    with zipfile.ZipFile(io.BytesIO(archive)) as source:
        binary = source.read(executable)
        if "LICENSE" in source.namelist():
            license_bytes = source.read("LICENSE")
else:
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as source:
        binary = source.extractfile(executable).read()
        try:
            license_bytes = source.extractfile("LICENSE").read()
        except KeyError:
            pass
args.output.mkdir(parents=True, exist_ok=True)
path = args.output / executable
path.write_bytes(binary)
path.chmod(0o755)
if license_bytes:
    (args.output / "CADDY-LICENSE.txt").write_bytes(license_bytes)
(args.output / "CADDY-SOURCE.txt").write_text(f"Caddy v{args.caddy_version}\nhttps://github.com/caddyserver/caddy\nArchive {algorithm.upper()}: {expected}\n", encoding="utf-8")
if args.binary:
    shutil.copy2(args.binary, args.output / args.binary.name)
    tool = args.binary.with_name("cargo-loki.exe" if os_name == "windows" else "cargo-loki")
    if tool.is_file():
        shutil.copy2(tool, args.output / tool.name)
    root = Path(__file__).resolve().parents[1]
    shutil.copy2(root / "README.md", args.output / "README.md")
    shutil.copy2(root / "LICENSE", args.output / "LICENSE")
    shutil.copytree(root / "docs", args.output / "docs", dirs_exist_ok=True)
    shutil.copytree(root / "crates" / "loki-server" / "examples", args.output / "examples", dirs_exist_ok=True)
    shutil.copytree(root / "crates" / "loki-sdk" / "wit", args.output / "wit", dirs_exist_ok=True)
    # Keep source-documentation links usable in portable bundles.
    for crate in ("loki-sdk", "cargo-loki"):
        destination = args.output / "crates" / crate
        destination.mkdir(parents=True, exist_ok=True)
        shutil.copy2(root / "crates" / crate / "README.md", destination / "README.md")
    version = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    sdk = root / "target" / "package" / f"loki-sdk-{version}.crate"
    if not sdk.is_file():
        parser.error("Build the SDK archive first: cargo package -p loki-sdk --allow-dirty")
    shutil.copy2(sdk, args.output / sdk.name)
    # Cargo's normalized package is independent of workspace-inherited metadata.
    with tarfile.open(sdk, mode="r:gz") as package:
        for member in package.getmembers():
            parts = Path(member.name).parts
            if not member.isfile() or parts[0] != f"loki-sdk-{version}" or len(parts) < 2:
                continue
            assert all(part not in ("..", ".") and ":" not in part for part in parts), "Unsafe SDK archive path"
            destination = args.output / "crates" / "loki-sdk" / Path(*parts[1:])
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(package.extractfile(member).read())
    sdk_manifest = args.output / "crates" / "loki-sdk" / "Cargo.toml"
    sdk_manifest.write_text(sdk_manifest.read_text(encoding="utf-8") + "\n[workspace]\n", encoding="utf-8")
    shutil.copytree(root / "crates" / "loki-sdk" / "wit", args.output / "crates" / "loki-sdk" / "wit", dirs_exist_ok=True)
    shutil.copytree(root / "crates" / "loki-server" / "examples", args.output / "crates" / "loki-server" / "examples", dirs_exist_ok=True)
    shutil.copytree(root / "examples" / "storage-notes", args.output / "examples" / "storage-notes", dirs_exist_ok=True, ignore=shutil.ignore_patterns("target", "*.loki-app"))
print(path.resolve())
