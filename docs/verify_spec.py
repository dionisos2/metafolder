#!/usr/bin/env python3
"""
Verify that all content from spec-daemon.org is preserved in the new spec files.
Compares paragraphs and code/table blocks after stripping org-mode meta lines and headings.
"""

import re
import sys
from pathlib import Path

NEW_FILES = [
    'docs/spec-main.org',
    'docs/spec-data-model.org',
    'docs/spec-query.org',
    'docs/spec-file-tracking.org',
    'docs/spec-event-log.org',
    'docs/spec-sync.org',
]

def extract_body(filepath):
    lines = Path(filepath).read_text().splitlines()
    out = []
    for line in lines:
        s = line.strip()
        # Skip org-mode directives (but keep #+BEGIN_*/#+END_* blocks)
        if re.match(r'#\+(?!BEGIN|END)', s, re.IGNORECASE):
            continue
        # Skip heading lines (but keep their text for block-splitting purposes)
        if re.match(r'^\*+ ', s):
            out.append('')  # treat headings as paragraph separators
            continue
        out.append(line)
    return '\n'.join(out)

def split_blocks(text):
    """Split into non-empty blocks separated by blank lines."""
    blocks = re.split(r'\n[ \t]*\n', text)
    result = []
    for b in blocks:
        b = b.strip()
        if b and len(b) >= 15:  # ignore very short/empty fragments
            result.append(b)
    return result

def normalize(text):
    return re.sub(r'[ \t]+', ' ', text).strip()

def main():
    original_path = Path('docs/spec-daemon.org')
    if not original_path.exists():
        print("spec-daemon.org not found — skipping (already deleted?)")
        return

    original_text = extract_body(original_path)
    original_blocks = split_blocks(original_text)

    combined_text = ''
    for f in NEW_FILES:
        p = Path(f)
        if p.exists():
            combined_text += '\n\n' + extract_body(p)
        else:
            print(f"WARNING: {f} does not exist yet")

    combined_norm = normalize(combined_text)

    missing = []
    for block in original_blocks:
        if normalize(block) not in combined_norm:
            missing.append(block)

    if missing:
        print(f"MISSING CONTENT — {len(missing)} block(s) not found in new files:\n")
        for i, m in enumerate(missing, 1):
            print(f"--- Block {i} ---")
            print(m[:300] + ('…' if len(m) > 300 else ''))
            print()
        sys.exit(1)
    else:
        print(f"OK — all {len(original_blocks)} content blocks from spec-daemon.org are present in the new files.")

if __name__ == '__main__':
    main()
