#!/usr/bin/env python3
"""Create and sign the rolling feed, pointing at an immutable build asset.

Keys are passed only over stdin, never in command arguments or written to disk.
"""
import base64
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import xml.etree.ElementTree as ET

SPARKLE = "http://www.andymatuschak.org/xml-namespaces/sparkle"
ET.register_namespace("sparkle", SPARKLE)


def openssl():
    """macOS's /usr/bin/openssl may be LibreSSL without Ed25519 support."""
    candidates = [os.environ["OPENSSL"]] if "OPENSSL" in os.environ else [
        "/opt/homebrew/opt/openssl@3/bin/openssl",
        "/usr/local/opt/openssl@3/bin/openssl",
        shutil.which("openssl"),
    ]
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            version = subprocess.run([candidate, "version"], check=True, capture_output=True, text=True).stdout
            if version.startswith("OpenSSL 3."):
                return candidate
    raise RuntimeError("OpenSSL 3 is required (brew install openssl@3, or set OPENSSL)")


def create(dmg, url, plist, tool, output):
    private = os.environ["SPARKLE_PRIVATE_KEY"].strip()
    public = os.environ["SPARKLE_PUBLIC_KEY"].strip()
    seed = base64.b64decode(private, validate=True)
    if len(seed) != 32:
        raise ValueError("Expected a 32-byte Sparkle private seed; export with Sparkle 2.10 generate_keys")
    # Catch a mismatched repository variable/secret before publishing an update
    # which every installed app would reject. RFC 8410 Ed25519 PKCS#8 wrapper.
    derived = subprocess.run(
        [openssl(), "pkey", "-inform", "DER", "-pubout", "-outform", "DER"],
        input=bytes.fromhex("302e020100300506032b657004220420") + seed,
        check=True, capture_output=True,
    ).stdout[-32:]
    if derived != base64.b64decode(public, validate=True):
        raise ValueError("SPARKLE_PUBLIC_KEY does not match SPARKLE_PRIVATE_KEY")
    with open(plist, "rb") as f:
        info = plistlib.load(f)
    if info["SUPublicEDKey"] != public:
        raise ValueError("Bundle contains a different Sparkle public key")

    def sign(path):
        return subprocess.run(
            [str(tool), "--ed-key-file", "-", "-p", str(path)],
            input=private, text=True, check=True, capture_output=True,
        ).stdout.strip()

    rss = ET.Element("rss", version="2.0")
    channel = ET.SubElement(rss, "channel")
    ET.SubElement(channel, "title").text = "Gander updates"
    item = ET.SubElement(channel, "item")
    ET.SubElement(item, "title").text = f"Gander {info['CFBundleVersion']}"
    ET.SubElement(item, f"{{{SPARKLE}}}version").text = info["CFBundleVersion"]
    ET.SubElement(item, f"{{{SPARKLE}}}shortVersionString").text = info["CFBundleShortVersionString"]
    ET.SubElement(item, f"{{{SPARKLE}}}minimumSystemVersion").text = info["LSMinimumSystemVersion"]
    ET.SubElement(item, "enclosure", {
        "url": url,
        "length": str(Path(dmg).stat().st_size),
        "type": "application/octet-stream",
        f"{{{SPARKLE}}}edSignature": sign(dmg),
    })
    ET.ElementTree(rss).write(output, encoding="utf-8", xml_declaration=True)
    sign(output)  # Sparkle embeds the feed signature directly in XML.


if __name__ == "__main__":
    create(*sys.argv[1:])
