"""Build and package a native CLI release archive and its SHA-256 checksum."""

import argparse
import gzip
import hashlib
import json
import os
import shutil
import subprocess
import tarfile
import zipfile
from pathlib import Path

TARGETS = (
    "x86_64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-gnu",
    "aarch64-unknown-linux-musl",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
)


def checksum(archive):
    with archive.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    return f"{digest}  {archive.name}\n"


def archive_binary(binary, archive):
    if archive.suffix == ".zip":
        # Output paths and source mtimes are excluded from both archive formats.
        info = zipfile.ZipInfo(binary.name)
        info.compress_type = zipfile.ZIP_DEFLATED
        with zipfile.ZipFile(archive, "w") as package:
            with binary.open("rb") as source, package.open(info, "w") as target:
                shutil.copyfileobj(source, target)
    else:
        entry = tarfile.TarInfo(binary.name)
        entry.size = binary.stat().st_size
        entry.mode = 0o755
        with archive.open("wb") as raw, gzip.GzipFile(filename="", fileobj=raw, mtime=0) as compressed:
            with binary.open("rb") as source, tarfile.open(fileobj=compressed, mode="w") as package:
                package.addfile(entry, source)


def package(target, output):
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        encoding="utf-8",
    ))
    version = next(package["version"] for package in metadata["packages"] if package["name"] == "hermes-cli")
    if os.environ.get("GITHUB_REF_TYPE") == "tag":
        tag = os.environ.get("GITHUB_REF_NAME")
        if tag != f"v{version}":
            raise SystemExit(f"Expected tag v{version} for hermes-cli, got {tag}")

    windows = target.endswith("-msvc")
    build_env = os.environ.copy()
    if target.endswith("-musl"):
        compiler = f"CC_{target.replace('-', '_')}"
        build_env.setdefault(compiler, "musl-gcc")
    if windows:
        build_env.setdefault("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS", "-C target-feature=+crt-static")
    subprocess.run(["cargo", "build", "--release", "--locked", "-p", "hermes-cli", "--target", target],
                   env=build_env, check=True)
    executable = "hermes.exe" if windows else "hermes"
    binary = Path(metadata["target_directory"]) / target / "release" / executable
    output.mkdir(parents=True, exist_ok=True)
    extension = "zip" if windows else "tar.gz"
    archive = output / f"hermes-{version}-{target}.{extension}"
    archive_binary(binary, archive)
    archive.with_name(archive.name + ".sha256").write_text(checksum(archive), encoding="ascii", newline="\n")
    print(archive)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", choices=TARGETS)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    package(args.target, args.output)


if __name__ == "__main__":
    main()
