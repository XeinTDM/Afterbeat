import hashlib
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch
from zipfile import ZipFile

from peer_assets import PEERS, cached, fetch, stage


class PeerAssets(unittest.TestCase):
    def test_cached_download_is_verified_before_staging(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source.zip"
            with ZipFile(source, "w") as package:
                package.writestr("./subdirectory/peer.exe", b"fixture")
            peer = {
                "name": "Fixture", "version": "1", "url": "https://example.invalid/source.zip",
                "file": "peer.zip", "entry": "subdirectory/peer.exe",
                "sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
            }
            cache = root / "cache"
            with patch.dict(PEERS, {"fixture": peer}), patch("peer_assets.urlopen", return_value=source.open("rb")):
                fetch(cache, ["fixture"])
                archive = cached(cache, "fixture")
                self.assertEqual(stage(archive, "fixture", root / "staged").read_bytes(), b"fixture")
                archive.write_bytes(b"modified")
                with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
                    cached(cache, "fixture")

    @unittest.skipUnless(sys.platform == "win32", "Windows peer profile handling")
    def test_preexisting_profile_is_refused_and_preserved(self):
        from peer_windows import require_clean_profile, reserve_profile

        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            profile = root / "existing"
            profile.mkdir()
            settings = profile / "settings.json"
            settings.write_text("existing data", encoding="utf-8")
            with patch("peer_windows.profile_paths", return_value={"known-local": profile}):
                with self.assertRaisesRegex(RuntimeError, "Existing peer data"):
                    require_clean_profile(["fixture"])
                with self.assertRaises(FileExistsError):
                    with reserve_profile("fixture", root):
                        self.fail("The existing profile was reserved.")
            self.assertEqual(settings.read_text(encoding="utf-8"), "existing data")
            self.assertFalse((root / "known-local").exists())
