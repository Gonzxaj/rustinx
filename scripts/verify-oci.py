#!/usr/bin/env python3
"""Verify the local Rustinx OCI index and package labels without loading an image."""

import argparse
import hashlib
import json
import tarfile

ELF_MACHINE = {("linux", "amd64"): 62, ("linux", "arm64"): 183}
INDEX = "application/vnd.oci.image.index.v1+json"
MANIFEST = "application/vnd.oci.image.manifest.v1+json"


def read_json(archive, name):
    member = archive.getmember(name)
    if not member.isfile():
        raise ValueError(f"Expected a regular file at {name}")
    with archive.extractfile(member) as stream:
        return json.load(stream)


def blob_member(archive, descriptor):
    digest = descriptor["digest"]
    algorithm, value = digest.split(":", 1)
    if algorithm != "sha256" or len(value) != 64 or any(c not in "0123456789abcdef" for c in value):
        raise ValueError(f"Invalid digest {digest}")
    member = archive.getmember(f"blobs/sha256/{value}")
    if not member.isfile() or member.size != descriptor["size"]:
        raise ValueError(f"Invalid blob at {digest}")
    return member, value


def read_blob(archive, descriptor):
    member, digest = blob_member(archive, descriptor)
    with archive.extractfile(member) as stream:
        payload = stream.read()
    if hashlib.sha256(payload).hexdigest() != digest:
        raise ValueError(f"Digest mismatch at {descriptor['digest']}")
    return json.loads(payload)


def verify_rustinx_binary(archive, layers, target):
    for descriptor in reversed(layers):
        member, _ = blob_member(archive, descriptor)
        with archive.extractfile(member) as stream, tarfile.open(fileobj=stream, mode="r|*") as layer:
            for entry in layer:
                if entry.name.removeprefix("./") != "usr/local/bin/rustinx":
                    continue
                if not entry.isfile():
                    raise ValueError(f"Expected a regular Rustinx executable for {target}")
                with layer.extractfile(entry) as executable:
                    header = executable.read(20)
                machine = int.from_bytes(header[18:20], "little")
                if not header.startswith(b"\x7fELF\x02\x01") or machine != ELF_MACHINE[target]:
                    raise ValueError(f"Rustinx executable does not match {target}")
                return
    raise ValueError(f"Missing Rustinx executable for {target}")


def verify(archive, version, revision):
    if read_json(archive, "oci-layout")["imageLayoutVersion"] != "1.0.0":
        raise ValueError("Unsupported OCI layout version")
    index = read_json(archive, "index.json")
    manifests = index["manifests"]
    # BuildKit may wrap the platform manifests in a tagged index.
    if len(manifests) == 1 and manifests[0]["mediaType"] == INDEX:
        manifests = read_blob(archive, manifests[0])["manifests"]

    platforms = set()
    for descriptor in manifests:
        if descriptor["mediaType"] != MANIFEST:
            raise ValueError(f"Unexpected descriptor {descriptor['mediaType']}")
        platform = descriptor["platform"]
        target = (platform["os"], platform["architecture"])
        if target not in ELF_MACHINE:
            raise ValueError(f"Unexpected platform {target}")
        if target in platforms:
            raise ValueError(f"Duplicate platform {target}")
        platforms.add(target)
        manifest = read_blob(archive, descriptor)
        config = read_blob(archive, manifest["config"])
        if (config["os"], config["architecture"]) != target:
            raise ValueError(f"Image config does not match index platform {target}")
        labels = config["config"]["Labels"]
        if labels["org.opencontainers.image.version"] != version:
            raise ValueError(f"Unexpected version for {target}")
        if labels["org.opencontainers.image.revision"] != revision:
            raise ValueError(f"Unexpected revision for {target}")
        for layer in manifest["layers"]:
            member, digest = blob_member(archive, layer)
            with archive.extractfile(member) as stream:
                if hashlib.file_digest(stream, "sha256").hexdigest() != digest:
                    raise ValueError(f"Digest mismatch at {layer['digest']}")

        verify_rustinx_binary(archive, manifest["layers"], target)

    if platforms != ELF_MACHINE.keys():
        raise ValueError(f"Expected AMD64 and ARM64, found {sorted(platforms)}")
    print(f"Validated OCI index and labels for {version} ({revision}): linux/amd64, linux/arm64")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive")
    parser.add_argument("version")
    parser.add_argument("revision")
    args = parser.parse_args()
    try:
        with tarfile.open(args.archive, "r:*") as archive:
            verify(archive, args.version, args.revision)
    except (KeyError, TypeError, ValueError, tarfile.TarError, OSError) as error:
        parser.exit(1, f"Invalid Rustinx OCI archive: {error}\n")


if __name__ == "__main__":
    main()
