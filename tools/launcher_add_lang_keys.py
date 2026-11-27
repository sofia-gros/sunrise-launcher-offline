#!/usr/bin/env python3
"""Add, rename or remove source-English keys in the launcher's catalogs.

`lang/en.json` is the source: it lists every key, each mapped to itself. The other catalogs hold
only real translations; a key a language lacks shows in English. New keys therefore go into
`en.json` alone, and Crowdin offers them to translators from there. Renaming a key carries its
translations along; removing one removes it everywhere.

Every rename is checked before any file is written: the old key must exist, the new one must not
exist in any catalog (that would silently drop one of the two sets of translations), both must have
the same `{holes}` (a carried translation would still say the old ones), and neither may also be
given to `--remove`. Renaming a key to itself does nothing.
"""

import argparse
import json
import re
import sys
from pathlib import Path

LANG = Path(__file__).resolve().parents[1] / "lang"
SOURCE = "en.json"


class RefusedRename(Exception):
    pass


def holes(text: str) -> set[str]:
    return set(re.findall(r"\{([^{}]*)\}", text))


def check_renames(
    tables: dict[str, dict[str, str]], remove: list[str], renames: list[tuple[str, str]]
) -> list[tuple[str, str]]:
    """The renames that change something, or `RefusedRename` naming the first that can't be done.

    `tables` maps each catalog's file name to its entries; `en.json` is the source."""
    source = tables[SOURCE]
    removed = set(remove)
    wanted = []
    targets = set()
    for old, new in renames:
        if old == new:
            continue
        if old in removed or new in removed:
            raise RefusedRename(f"{old!r} -> {new!r} is also given to --remove: do one or the other")
        if old not in source:
            raise RefusedRename(f"{old!r} is not a key in {SOURCE}")
        holder = next((name for name, table in tables.items() if new in table), None)
        if holder == SOURCE or new in targets:
            raise RefusedRename(
                f"{new!r} is already a key: renaming {old!r} onto it would drop one key's translations"
            )
        if holder:
            raise RefusedRename(
                f"{holder} already has {new!r}: renaming {old!r} onto it would drop one of its translations"
            )
        if holes(old) != holes(new):
            raise RefusedRename(
                f"{old!r} and {new!r} have different {{holes}}: its translations would keep the old ones"
            )
        targets.add(new)
        wanted.append((old, new))
    return wanted


def renamed(table: dict[str, str], old: str, new: str, source: bool) -> dict[str, str]:
    """`table` with `old` replaced by `new` in place, keeping the entry's position."""
    return {
        (new if key == old else key): (new if source and key == old else value)
        for key, value in table.items()
    }


def apply(lang: Path, keys: list[str], remove: list[str], renames: list[tuple[str, str]]) -> None:
    paths = sorted(lang.glob("*.json"))
    texts = {path: path.read_text(encoding="utf-8") for path in paths}
    tables = {path: json.loads(text) for path, text in texts.items()}
    renames = check_renames({path.name: table for path, table in tables.items()}, remove, renames)
    for path, table in tables.items():
        source = path.name == SOURCE
        for key in remove:
            table.pop(key, None)
        for old, new in renames:
            table = renamed(table, old, new, source)
        if source:
            for key in keys:
                table.setdefault(key, key)
        after = json.dumps(table, ensure_ascii=False, indent=2) + "\n"
        if after != texts[path]:
            path.write_text(after, encoding="utf-8")


def main(argv: list[str] | None = None, lang: Path = LANG) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("keys", nargs="*", help="New source-English strings used by tr/trf")
    parser.add_argument("--remove", action="append", default=[], help="Obsolete key, removed everywhere")
    parser.add_argument(
        "--rename",
        nargs=2,
        action="append",
        default=[],
        metavar=("OLD", "NEW"),
        help="Change a key's English, keeping its translations",
    )
    args = parser.parse_args(argv)
    if not (args.keys or args.remove or args.rename):
        parser.error("nothing to do: give keys to add, --remove or --rename")
    try:
        apply(lang, args.keys, args.remove, [tuple(r) for r in args.rename])
    except RefusedRename as refused:
        print(f"Nothing changed: {refused}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
