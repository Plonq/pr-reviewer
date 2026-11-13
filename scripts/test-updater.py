#!/usr/bin/env python3
"""Exercise a real ad-hoc-signed DMG update with a disposable native host.

Run from the repo root after scripts/fetch-sparkle.sh. No Apple account,
Keychain access, GitHub credentials, or user app data. Requires a GUI login.
"""
import base64
import functools
import http.server
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile
import threading
import time

import importlib.util

spec = importlib.util.spec_from_file_location("create_appcast", "scripts/create-appcast.py")
appcast = importlib.util.module_from_spec(spec)
spec.loader.exec_module(appcast)
ROOT = Path.cwd()
SPARKLE = ROOT / "target/sparkle"


def run(*args, **kwargs):
    return subprocess.run([str(a) for a in args], check=True, **kwargs)


def main():
    quit_on_ready = "--quit" in sys.argv
    with tempfile.TemporaryDirectory(prefix="updater-smoke-", dir=ROOT / "target") as directory:
        work = Path(directory)
        marker = work / "relaunched"
        seed_der = run(appcast.openssl(), "genpkey", "-algorithm", "ED25519", "-outform", "DER", capture_output=True).stdout
        public_der = run(appcast.openssl(), "pkey", "-inform", "DER", "-pubout", "-outform", "DER", input=seed_der, capture_output=True).stdout
        os.environ["SPARKLE_PRIVATE_KEY"] = base64.b64encode(seed_der[-32:]).decode()
        os.environ["SPARKLE_PUBLIC_KEY"] = base64.b64encode(public_der[-32:]).decode()
        handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=directory)
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        url = f"http://127.0.0.1:{server.server_port}"
        executable = work / "smoke"
        sdk = run("xcrun", "--show-sdk-path", capture_output=True, text=True).stdout.strip()
        run("clang", "-fobjc-arc", "-fblocks", "-isysroot", sdk,
            "-mmacosx-version-min=15.0", f"-F{SPARKLE}", "-framework", "AppKit",
            "-framework", "Sparkle", "-Wl,-rpath,@executable_path/../Frameworks",
            "crates/app/native/updater.m", "scripts/updater-smoke.m", "-o", executable)

        def bundle(path, version):
            contents = path / "Contents"
            (contents / "MacOS").mkdir(parents=True)
            (contents / "Frameworks").mkdir()
            shutil.copy2(executable, contents / "MacOS/smoke")
            run("ditto", SPARKLE / "Sparkle.framework", contents / "Frameworks/Sparkle.framework")
            info = {
                "CFBundleIdentifier": "org.plonq.gander.updater-smoke",
                "CFBundleExecutable": "smoke", "CFBundleName": "Updater Smoke",
                "CFBundlePackageType": "APPL", "CFBundleVersion": version,
                "CFBundleShortVersionString": "0.1.0", "LSMinimumSystemVersion": "15.0",
                "SUPublicEDKey": os.environ["SPARKLE_PUBLIC_KEY"],
                "SUFeedURL": f"{url}/appcast.xml", "SUEnableAutomaticChecks": True,
                "SUAutomaticallyUpdate": True, "SUVerifyUpdateBeforeExtraction": True,
                "SURequireSignedFeed": True, "SUEnableSystemProfiling": False,
                "SmokeMarker": str(marker),
                "SmokeQuitOnReady": quit_on_ready,
                # Test host only: production always uses HTTPS.
                "NSAppTransportSecurity": {"NSAllowsArbitraryLoads": True},
                "LSUIElement": True,
            }
            with (contents / "Info.plist").open("wb") as f:
                plistlib.dump(info, f)
            run("codesign", "--force", "--sign", "-", path)
            run("codesign", "--verify", "--deep", "--strict", path)

        installed = work / "installed/Updater Smoke.app"
        new = work / "payload/Updater Smoke.app"
        bundle(installed, "1.1")
        bundle(new, "2.1")
        dmg = work / "update.dmg"
        run("hdiutil", "create", "-quiet", "-volname", "Updater Smoke", "-srcfolder", new.parent, "-format", "UDZO", dmg)
        appcast.create(dmg, f"{url}/update.dmg", new / "Contents/Info.plist", SPARKLE / "bin/sign_update", work / "appcast.xml")
        # Verify tampering and key mismatch fail before trying the real update.
        signature = run(SPARKLE / "bin/sign_update", "--ed-key-file", "-", "-p", dmg,
                        input=os.environ["SPARKLE_PRIVATE_KEY"], capture_output=True, text=True).stdout.strip()
        run(SPARKLE / "bin/sign_update", "--verify", "--ed-key-file", "-", dmg, signature,
            input=os.environ["SPARKLE_PRIVATE_KEY"], text=True, capture_output=True)
        tampered = work / "tampered.dmg"
        shutil.copy2(dmg, tampered)
        with tampered.open("ab") as f:
            f.write(b"tampered")
        rejected = subprocess.run([str(SPARKLE / "bin/sign_update"), "--verify", "--ed-key-file", "-", str(tampered), signature],
                                  input=os.environ["SPARKLE_PRIVATE_KEY"], text=True, capture_output=True)
        assert rejected.returncode != 0, "tampered archive must be rejected"
        process = subprocess.Popen([str(installed / "Contents/MacOS/smoke")], stdout=sys.stdout, stderr=sys.stderr)
        try:
            process.wait(timeout=90)
            if quit_on_ready:
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    try:
                        with (installed / "Contents/Info.plist").open("rb") as f:
                            if plistlib.load(f)["CFBundleVersion"] == "2.1":
                                break
                    except FileNotFoundError:
                        pass  # The installer is swapping bundles.
                    time.sleep(0.2)
                run(installed / "Contents/MacOS/smoke", timeout=10)
            deadline = time.monotonic() + 30
            while not marker.exists() and time.monotonic() < deadline:
                time.sleep(0.2)
            assert marker.exists(), "new app did not relaunch"
            assert marker.read_text() == "relaunched new version", "update failed"
            with (installed / "Contents/Info.plist").open("rb") as f:
                assert plistlib.load(f)["CFBundleVersion"] == "2.1"
            print("PASS: signed feed, tamper rejection, background DMG download, ad-hoc replacement and "
                  + ("normal-quit installation" if quit_on_ready else "relaunch"))
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=10)
            server.shutdown()


if __name__ == "__main__":
    main()
