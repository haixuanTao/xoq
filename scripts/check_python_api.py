#!/usr/bin/env python3
"""Check Python package APIs against baselines for semver compliance.

Extracts public API names from PyO3 Rust source files and compares
against committed baselines. Detects removed classes, methods, properties,
and constants — which are breaking changes.

Usage:
    python scripts/check_python_api.py           # Check against baselines
    python scripts/check_python_api.py --update   # Update baselines
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).parent.parent
BASELINES_DIR = ROOT / "api-baselines"

PACKAGES = {
    "serial": ROOT / "packages/serial/src/lib.rs",
    "can": ROOT / "packages/can/src/lib.rs",
    "cv2": ROOT / "packages/cv2/src/lib.rs",
}


def extract_api(rust_file):
    """Extract Python-visible API names from a PyO3 Rust source file."""
    content = rust_file.read_text()
    names = set()

    # Module name
    for m in re.finditer(r"#\[pymodule\]\s*fn\s+(\w+)", content):
        names.add(f"module:{m.group(1)}")

    # Classes from #[pyclass]
    for m in re.finditer(
        r"#\[pyclass[^\]]*\][\s\S]*?pub\s+struct\s+(\w+)", content
    ):
        names.add(f"class:{m.group(1)}")

    # Class field properties: #[pyo3(get)] / #[pyo3(get, set)]
    pyclass_re = re.compile(
        r"#\[pyclass[^\]]*\][\s\S]*?pub\s+struct\s+(\w+)\s*\{([\s\S]*?)\n\}",
    )
    for m in pyclass_re.finditer(content):
        cls = m.group(1)
        for f in re.finditer(
            r"#\[pyo3\((?:get|get,\s*set|set)\)\]\s*(?:pub\s+)?(\w+)", m.group(2)
        ):
            names.add(f"property:{cls}.{f.group(1)}")

    # Methods in #[pymethods] impl blocks
    impl_re = re.compile(r"#\[pymethods\]\s*impl\s+(\w+)\s*\{")
    for impl_match in impl_re.finditer(content):
        cls = impl_match.group(1)
        # Find matching closing brace (handle nested braces)
        start = impl_match.end()
        depth = 1
        pos = start
        while pos < len(content) and depth > 0:
            if content[pos] == "{":
                depth += 1
            elif content[pos] == "}":
                depth -= 1
            pos += 1
        block = content[start : pos - 1]

        # Find each fn and its preceding annotations
        for fn_match in re.finditer(r"\bfn\s+(\w+)\s*[\(<]", block):
            fn_name = fn_match.group(1)
            # Look at the preceding text up to the previous fn or block start
            pre_start = max(0, fn_match.start() - 800)
            preceding = block[pre_start : fn_match.start()]
            # Only look back to the last `fn ` boundary (previous method)
            last_fn = preceding.rfind("\n    fn ")
            if last_fn >= 0:
                preceding = preceding[last_fn:]
            preceding_lines = preceding.split("\n")

            is_getter = any("#[getter]" in l for l in preceding_lines)
            is_setter = any("#[setter]" in l for l in preceding_lines)
            is_new = any("#[new]" in l for l in preceding_lines)

            if is_getter or is_setter:
                names.add(f"property:{cls}.{fn_name}")
            elif is_new:
                names.add(f"constructor:{cls}")
            elif fn_name.startswith("__") and fn_name.endswith("__"):
                names.add(f"method:{cls}.{fn_name}")
            else:
                names.add(f"method:{cls}.{fn_name}")

    # Module constants: m.add("NAME", ...)?
    for m in re.finditer(r'm\.add\(\s*"(\w+)"', content):
        names.add(f"const:{m.group(1)}")

    # Exported classes: m.add_class::<ClassName>()?
    for m in re.finditer(r"m\.add_class::<(\w+)>\(\)", content):
        names.add(f"export:{m.group(1)}")

    # Submodules
    for m in re.finditer(r"add_submodule.*?(\w+)_mod", content):
        pass  # Only if explicitly named
    for m in re.finditer(r'PyModule::new_bound\([^,]+,\s*"(\w+)"', content):
        names.add(f"submodule:{m.group(1)}")

    return names


def format_api(names):
    return "\n".join(sorted(names)) + "\n"


def main():
    if "--update" in sys.argv:
        BASELINES_DIR.mkdir(exist_ok=True)
        for name, source in PACKAGES.items():
            api = extract_api(source)
            path = BASELINES_DIR / f"{name}.api"
            path.write_text(format_api(api))
            print(f"  Updated {path} ({len(api)} API entries)")
        return

    print("Checking Python API semver compliance...")
    ok = True

    for name, source in PACKAGES.items():
        current = extract_api(source)
        baseline_path = BASELINES_DIR / f"{name}.api"

        if not baseline_path.exists():
            print(f"  {name}: ERROR - no baseline found at {baseline_path}")
            print(f"    Run: python {sys.argv[0]} --update")
            ok = False
            continue

        baseline = set(baseline_path.read_text().strip().splitlines())
        removed = baseline - current
        added = current - baseline

        if not removed and not added:
            print(f"  {name}: OK")
            continue

        if removed:
            print(f"  {name}: BREAKING CHANGES - removed API members:")
            for item in sorted(removed):
                print(f"    - {item}")
            ok = False

        if added:
            print(f"  {name}: new additions (update baseline):")
            for item in sorted(added):
                print(f"    + {item}")
            ok = False

    if not ok:
        print(f"\nUpdate baselines: python {sys.argv[0]} --update")
        sys.exit(1)

    print("All Python API checks passed.")


if __name__ == "__main__":
    main()
