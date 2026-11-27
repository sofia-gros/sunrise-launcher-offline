import json
import tempfile
import unittest
from contextlib import redirect_stderr
from io import StringIO
from pathlib import Path

from tools import launcher_add_lang_keys as keys


class Catalogs(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.lang = Path(self.dir.name)
        self.write("en", {"Settings": "Settings", "Close": "Close", "Cancel": "Cancel",
                          "Hi {name}": "Hi {name}"})
        self.write("de", {"Settings": "Einstellungen", "Close": "Schließen",
                          "Cancel": "Abbrechen", "Hi {name}": "Hallo {name}"})

    def tearDown(self):
        self.dir.cleanup()

    def write(self, code, table):
        (self.lang / f"{code}.json").write_text(json.dumps(table, ensure_ascii=False, indent=2) + "\n")

    def read(self, code):
        return json.loads((self.lang / f"{code}.json").read_text())

    def run_tool(self, *argv):
        with redirect_stderr(StringIO()):
            return keys.main(list(argv), self.lang)

    def test_a_new_key_goes_into_the_source_only(self):
        self.assertEqual(self.run_tool("Brand new"), 0)
        self.assertEqual(self.read("en")["Brand new"], "Brand new")
        self.assertNotIn("Brand new", self.read("de"))

    def test_a_rename_keeps_translations_and_position(self):
        self.assertEqual(self.run_tool("--rename", "Settings", "Preferences"), 0)
        self.assertEqual(list(self.read("en"))[0], "Preferences")
        self.assertEqual(self.read("en")["Preferences"], "Preferences")
        self.assertEqual(self.read("de")["Preferences"], "Einstellungen")
        self.assertNotIn("Settings", self.read("de"))

    def test_renaming_a_key_to_itself_changes_nothing(self):
        before = {code: self.read(code) for code in ("en", "de")}
        self.assertEqual(self.run_tool("--rename", "Settings", "Settings"), 0)
        self.assertEqual({code: self.read(code) for code in ("en", "de")}, before)

    def test_a_rename_onto_an_existing_key_is_refused_and_writes_nothing(self):
        before = {code: self.read(code) for code in ("en", "de")}
        self.assertEqual(self.run_tool("Added", "--rename", "Cancel", "Close"), 1)
        self.assertEqual({code: self.read(code) for code in ("en", "de")}, before)

    def test_a_rename_that_changes_holes_is_refused(self):
        self.assertEqual(self.run_tool("--rename", "Hi {name}", "Hi {player}"), 1)
        self.assertEqual(self.read("de")["Hi {name}"], "Hallo {name}")

    def test_two_renames_onto_the_same_key_are_refused(self):
        self.assertEqual(self.run_tool("--rename", "Close", "Done", "--rename", "Cancel", "Done"), 1)
        self.assertIn("Close", self.read("en"))

    def test_a_rename_onto_a_key_only_a_translation_has_is_refused(self):
        self.write("fr", {"Settings": "Paramètres", "Preferences": "Préférences"})
        before = {code: self.read(code) for code in ("en", "de", "fr")}
        self.assertEqual(self.run_tool("--rename", "Settings", "Preferences"), 1)
        self.assertEqual({code: self.read(code) for code in ("en", "de", "fr")}, before)

    def test_removing_and_renaming_the_same_key_is_refused(self):
        before = {code: self.read(code) for code in ("en", "de")}
        self.assertEqual(self.run_tool("--remove", "Settings", "--rename", "Settings", "Preferences"), 1)
        self.assertEqual(self.run_tool("--remove", "Close", "--rename", "Settings", "Close"), 1)
        self.assertEqual({code: self.read(code) for code in ("en", "de")}, before)

    def test_a_rename_of_a_missing_key_is_refused(self):
        self.assertEqual(self.run_tool("--rename", "Nope", "Still nope"), 1)

    def test_remove_takes_a_key_out_everywhere(self):
        self.assertEqual(self.run_tool("--remove", "Close"), 0)
        self.assertNotIn("Close", self.read("en"))
        self.assertNotIn("Close", self.read("de"))


if __name__ == "__main__":
    unittest.main()
