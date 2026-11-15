#!/usr/bin/env python3
"""Fails if any page in the built site links to a local file or anchor that does not exist."""
import sys, re, pathlib
from html.parser import HTMLParser
from urllib.parse import urlsplit, unquote

root = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "target/site").resolve()

class Page(HTMLParser):
    def __init__(self):
        super().__init__()
        self.links, self.ids = [], set()
    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        if "id" in a: self.ids.add(a["id"])
        if tag == "a" and a.get("name"): self.ids.add(a["name"])
        for k in ("href", "src"):
            if k in a and a[k]: self.links.append(a[k])

pages = {}
for f in root.rglob("*.html"):
    p = Page(); p.feed(f.read_text(encoding="utf-8", errors="replace")); pages[f] = p

errors = []
for f, p in pages.items():
    if f.name in ("print.html", "404.html") or "toc" in f.name: continue
    for link in p.links:
        u = urlsplit(link)
        if u.scheme or u.netloc or link.startswith(("mailto:", "javascript:", "#")) and not link.startswith("#"):
            continue
        if link.startswith("#"):
            target, frag = f, u.fragment
        else:
            path = unquote(u.path)
            target = (f.parent / path).resolve()
            if path.endswith("/") or target.is_dir(): target = target / "index.html"
            frag = u.fragment
            if not target.exists():
                errors.append(f"{f.relative_to(root)}: missing {link}"); continue
        if frag and target.suffix == ".html":
            tp = pages.get(target)
            if tp is not None and frag not in tp.ids:
                errors.append(f"{f.relative_to(root)}: missing anchor {link}")

for e in sorted(set(errors)): print(e)
print(f"checked {len(pages)} pages, {len(set(errors))} broken links")
sys.exit(1 if errors else 0)
