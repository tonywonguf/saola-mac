"""Tests for libexec/saola-configure-handy. Run: python3 -m unittest discover tests"""

import importlib.machinery
import importlib.util
import json
import pathlib
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
loader = importlib.machinery.SourceFileLoader(
    "configure", str(ROOT / "libexec/saola-configure-handy")
)
spec = importlib.util.spec_from_loader("configure", loader)
configure = importlib.util.module_from_spec(spec)
loader.exec_module(configure)

# The shape of a new Handy 0.10 settings file on macOS (only the keys that matter here).
FRESH = {
    "settings": {
        "selected_model": "parakeet-tdt-0.6b-v3",
        "paste_method": "ctrl_v",
        "post_process_enabled": False,
        "post_process_provider_id": "openai",
        "post_process_providers": [
            {
                "id": "openai",
                "label": "OpenAI",
                "base_url": "https://api.openai.com/v1",
            },
            {
                "id": "custom",
                "label": "Custom",
                "base_url": "http://localhost:11434/v1",
                "allow_base_url_edit": True,
                "models_endpoint": "/models",
            },
        ],
        "post_process_models": {"openai": ""},
        "post_process_prompts": [
            {"id": "default_improve_transcriptions", "name": "Improve", "prompt": "x"}
        ],
    }
}


class ConfigureTest(unittest.TestCase):
    def run_on(self, data, *flags):
        with tempfile.TemporaryDirectory() as d:
            p = pathlib.Path(d) / "settings_store.json"
            p.write_text(json.dumps(data))
            code = configure.main(["x", str(p), *flags])
            return code, json.loads(p.read_text())

    def test_sets_cleanup_and_keeps_other_settings(self):
        code, out = self.run_on(FRESH)
        s = out["settings"]
        self.assertEqual(code, 0)
        self.assertTrue(s["post_process_enabled"])
        self.assertEqual(s["post_process_provider_id"], "custom")
        custom = next(p for p in s["post_process_providers"] if p["id"] == "custom")
        self.assertEqual(custom["base_url"], "http://localhost:11436/v1")
        self.assertEqual(s["post_process_models"]["custom"], "s1-mini")
        self.assertEqual(s["post_process_selected_prompt_id"], "s1_mini_cleanup")
        self.assertEqual(len(s["post_process_prompts"]), 2)
        # Handy's own choices stay.
        self.assertEqual(s["selected_model"], "parakeet-tdt-0.6b-v3")
        self.assertEqual(s["paste_method"], "ctrl_v")
        self.assertEqual(
            s["post_process_providers"][0]["base_url"], "https://api.openai.com/v1"
        )

    def test_second_run_changes_nothing(self):
        _, once = self.run_on(FRESH)
        _, twice = self.run_on(once)
        self.assertEqual(once, twice)
        self.assertEqual(len(twice["settings"]["post_process_prompts"]), 2)

    def test_adds_custom_provider_when_missing(self):
        data = {"settings": {"post_process_providers": []}}
        _, out = self.run_on(data)
        self.assertEqual(out["settings"]["post_process_providers"][0]["id"], "custom")

    def test_dry_run_writes_nothing(self):
        code, out = self.run_on(FRESH, "--dry-run")
        self.assertEqual(code, 0)
        self.assertEqual(out, FRESH)

    def test_file_without_settings_fails(self):
        code, _ = self.run_on({"other": 1})
        self.assertEqual(code, 1)


if __name__ == "__main__":
    unittest.main()
