#!/usr/bin/env python3
"""Run or replay the create-record demo.

The experiment's `target` block starts and stops the bundled API, so this
script only chooses the API mode and calls Nadir. It is equivalent to:

    nadir run examples/create-record/experiment.yaml
    NADIR_FIXTURE_MODE=title_regression nadir run examples/create-record/experiment.yaml
"""

import argparse
import os
from pathlib import Path
import subprocess
import sys

EXAMPLE = Path(__file__).resolve().parent
ROOT = EXAMPLE.parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--regression", action="store_true", help="enable the demo title regression")
    parser.add_argument("--case", type=Path, help="repeat a case JSON instead of running the experiment")
    args = parser.parse_args()

    environment = os.environ.copy()
    environment["NADIR_FIXTURE_MODE"] = "title_regression" if args.regression else "corrected"
    command = ["cargo", "run", "--quiet", "--manifest-path", str(ROOT / "Cargo.toml"), "--"]
    if args.case:
        command += ["replay", str(EXAMPLE / "experiment.yaml"), "--case", str(args.case)]
    else:
        command += ["run", str(EXAMPLE / "experiment.yaml")]

    try:
        return subprocess.run(command, env=environment, check=False).returncode
    except OSError as exc:
        print(f"Could not launch the demo: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
