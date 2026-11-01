# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.

"""Pure receipt-contract tests; never discover or copy a real browser profile."""
import copy
import json
import os
import pathlib
import tempfile
import types
import unittest
from unittest.mock import patch

import zen_provider_fixture as fixture


def receipt(profile):
    origins = ["http://localhost:38211", "http://127.0.0.1:38211"]
    nonce = "synthetic-receipt-only"
    return {
        "profile": str(profile), "nonce": nonce, "origins": origins,
        "browser_returncode": 0,
        "handshake": {"applicationType": "gecko", "marionetteProtocol": 3},
        "session": {"capabilities": {"browserName": "zen", "browserVersion": "1.24b",
                    "platformName": "linux", "moz:headless": True, "moz:profile": str(profile)}},
        "checks": dict.fromkeys(fixture.CHECKS, True),
        "cleanup": dict.fromkeys(fixture.CLEANUP, True),
        "page_states": [{"url": o + "/account", "storage": nonce, "text": "Signed in\n" + nonce} for o in origins],
        "chrome_tabs": [{"url": o + "/account"} for o in origins],
        "events": [e for o in origins for e in (
            {"kind": "login-post", "host": o[7:], "credentials_correct": True},
            {"kind": "page-callback", "host": o[7:], "query": {"storage": [nonce]}, "cookie": "session=" + nonce})],
        "saved_login_service": [{"hostname": origins[0], "passwordCorrect": True}],
    }


class ReceiptTests(unittest.TestCase):
    def test_complete_receipt_passes(self):
        profile = pathlib.Path("/synthetic-only/profile")
        fixture.validate_receipt(receipt(profile), profile)

    def test_empty_missing_nonboolean_and_wrong_receipts_refuse_before_inventory_or_copy(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = pathlib.Path(tmp).resolve()
            profile = base / "profile"
            profile.mkdir()
            good = receipt(profile)
            bad = [{}]
            for field in ("checks", "cleanup"):
                r = copy.deepcopy(good)
                r[field] = {}
                bad.append(r)
                for key in good[field]:
                    r = copy.deepcopy(good)
                    del r[field][key]
                    bad.append(r)
                    for value in (1, "true", [], None, False):
                        r = copy.deepcopy(good)
                        r[field][key] = value
                        bad.append(r)
                r = copy.deepcopy(good)
                r[field]["unreviewed_check"] = True
                bad.append(r)
            for field, value in (("profile", "/wrong/profile"), ("browser_returncode", False),
                                 ("browser_returncode", 1), ("nonce", ""), ("page_states", []),
                                 ("events", []), ("origins", []), ("saved_login_service", [])):
                r = copy.deepcopy(good)
                r[field] = value
                bad.append(r)
            for field, value in (("moz:profile", "/wrong/profile"), ("browserName", "firefox"),
                                 ("moz:headless", 1)):
                r = copy.deepcopy(good)
                r["session"]["capabilities"][field] = value
                bad.append(r)
            args = types.SimpleNamespace(source_profile=profile, source_receipt=base / "receipt.json",
                source_sha256=base / "hashes.json", evidence=base / "evidence")
            args.source_sha256.write_text("{}")
            # Deliberately absent evidence. Every refusal must precede even
            # source inventory, not merely fail later while copying.
            with patch.dict(os.environ, {"TMPDIR": str(base)}), patch.object(fixture, "inventory", side_effect=AssertionError("inventory reached")) as inventory:
                for r in bad:
                    with self.subTest(receipt=r):
                        args.source_receipt.write_text(json.dumps(r))
                        with self.assertRaisesRegex(RuntimeError, "receipt does not attest"):
                            fixture.prepare(args)
                        self.assertFalse(args.evidence.exists())
                inventory.assert_not_called()

    def test_regular_parentlock_is_not_an_omitted_marker(self):
        self.assertNotIn(".parentlock", fixture.LOCKS)
        self.assertEqual(fixture.LOCKS, {"lock", "parent.lock"})


if __name__ == "__main__":
    unittest.main()
