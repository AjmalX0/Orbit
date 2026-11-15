#!/usr/bin/env python3
"""Write sitemap.xml and robots.txt for the built site (target/site)."""
import datetime
import pathlib
import sys

BASE = "https://ajmalx0.github.io/Orbit/"
site = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "target/site")
today = datetime.date.today().isoformat()
skip = {"404.html", "print.html", "toc.html"}
pages = sorted(
    p.relative_to(site).as_posix()
    for p in site.rglob("*.html")
    if p.name not in skip and "/searchindex" not in p.as_posix()
)
urls = []
for rel in pages:
    loc = BASE + (rel[: -len("index.html")] if rel.endswith("index.html") else rel)
    priority = "1.0" if rel == "index.html" else ("0.8" if rel.count("/") <= 2 else "0.6")
    urls.append(f"  <url><loc>{loc}</loc><lastmod>{today}</lastmod><priority>{priority}</priority></url>")
(site / "sitemap.xml").write_text(
    '<?xml version="1.0" encoding="UTF-8"?>\n'
    '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n'
    + "\n".join(urls)
    + "\n</urlset>\n"
)
(site / "robots.txt").write_text(f"User-agent: *\nAllow: /\n\nSitemap: {BASE}sitemap.xml\n")
print(f"sitemap.xml: {len(urls)} pages")
