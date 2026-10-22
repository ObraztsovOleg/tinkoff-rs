#!/usr/bin/env python3
"""Generate dark.css from style.css: the same rules under [data-theme="dark"].

python3 scripts/dashboard/theme.py          # rewrite dark.css
python3 scripts/dashboard/theme.py --check  # exit 1 when dark.css is stale

Colors are mapped by role: light surfaces and lines become dark, dark text and
strokes become light, accents brighten a little. Light text on colored buttons
and dark tooltip backgrounds keep their color. Theme tokens are set by hand.
"""
from __future__ import annotations

import colorsys
from pathlib import Path
import re
import sys

ASSETS = Path(__file__).resolve().parent
SOURCE, TARGET = ASSETS / "style.css", ASSETS / "dark.css"
DARK = ':root[data-theme="dark"]'
ROOT, SCOPE = 'html:where([data-theme="dark"])', ':where([data-theme="dark"])'
TOKENS = (DARK + "{color-scheme:dark;--ink:#dce6e3;--muted:#8fa19c;--line:#2a3b3f;--paper:#0f1a1d;"
          "--teal:#38a78e;--mint:#16302a;--amber:#d9a441;--red:#e2716a;--nav:#0b1517}")
COLOR = re.compile(r"#[0-9a-fA-F]{3,8}\b|(?<![\w-])white(?![\w-])|rgba?\([^)]*\)")
# The sidebar is dark in both themes and keeps its colors.
SIDEBAR = re.compile(r"\.sidebar|\.brand|\.nav-label|(^|\s)nav\b")
THEME_HUE, THEME_SATURATION = 190 / 360, 0.28
FOREGROUND = {"color", "caret-color", "text-decoration-color", "-webkit-text-fill-color", "stroke", "outline", "outline-color"}
BACKGROUND = {"background", "background-color", "background-image"}


def rgb(token):
    if token == "white":
        return (255, 255, 255), None
    if token.startswith("#"):
        hex_ = token[1:]
        if len(hex_) in (3, 4):
            hex_ = "".join(c * 2 for c in hex_)
        values = [int(hex_[i:i + 2], 16) for i in range(0, 6, 2)]
        alpha = int(hex_[6:8], 16) / 255 if len(hex_) == 8 else None
        return tuple(values), alpha
    parts = [p.strip() for p in token[token.index("(") + 1:-1].split(",")]
    return tuple(int(float(p)) for p in parts[:3]), (float(parts[3]) if len(parts) > 3 else None)


def render(values, alpha):
    r, g, b = (max(0, min(255, round(v))) for v in values)
    return f"rgba({r},{g},{b},{alpha:g})" if alpha is not None else f"#{r:02x}{g:02x}{b:02x}"


def convert(token, prop):
    values, alpha = rgb(token)
    h, l, s = colorsys.rgb_to_hls(*(v / 255 for v in values))
    if prop == "box-shadow" and l < 0.8:
        return f"rgba(0,0,0,{max(alpha or 0, 0.35):g})"  # shadows stay dark
    foreground = prop in FOREGROUND or prop.startswith("border") and l < 0.8
    if l >= 0.8:
        if prop == "color" or prop == "-webkit-text-fill-color":
            return token  # light text on a colored control stays light
        # Surfaces and lines: page 0.09–0.15, raised lines up to ~0.24. Neutral
        # white takes the theme tint so cards match the page.
        l, s = 0.09 + (1 - l) * 0.75, min(s * 0.7, 0.32)
        if s < 0.12:
            h, s = THEME_HUE, THEME_SATURATION
    elif prop in BACKGROUND:
        return token  # accent fills and dark tooltips already suit a dark page
    elif l <= 0.4 or foreground:
        l = 0.93 - l * 0.55 if l <= 0.4 else l + (0.66 - l) * 0.5
    else:
        l = l + (0.62 - l) * 0.5
    return render([v * 255 for v in colorsys.hls_to_rgb(h, l, s)], alpha)


def declarations(body):
    out = []
    for declaration in body.split(";"):
        if ":" not in declaration:
            continue
        prop, value = declaration.split(":", 1)
        prop = prop.strip().lower()
        if prop.startswith("--") or not COLOR.search(value):
            continue
        out.append(f"{prop}:{COLOR.sub(lambda m: convert(m.group(0), prop), value.strip())}")
    return ";".join(out)


def scope(selectors):
    """Prefix with :where() so a dark rule keeps the specificity of its source
    and variants such as .button.primary still win over .button."""
    scoped = []
    for selector in selectors.split(","):
        selector = selector.strip()
        if selector in {":root", "html"}:
            scoped.append(ROOT)
        elif selector.startswith("html"):
            scoped.append(ROOT + selector[4:])
        else:
            scoped.append(SCOPE + " " + selector)
    return ",".join(scoped)


def rules(css):
    """Top-level rules and one level of @media nesting, enough for style.css."""
    out, index = [], 0
    while index < len(css):
        start = css.find("{", index)
        if start < 0:
            break
        head = css[index:start].strip()
        if head.startswith("@media"):
            depth, cursor = 1, start + 1
            while depth:
                depth += {"{": 1, "}": -1}.get(css[cursor], 0)
                cursor += 1
            inner = rules(css[start + 1:cursor - 1])
            if inner:
                out.append(head + "{" + "".join(inner) + "}")
            index = cursor
            continue
        end = css.index("}", start)
        body = declarations(css[start + 1:end])
        head = re.sub(r"/\*.*?\*/", "", head, flags=re.S).strip()
        if body and head and not head.startswith("@") and not SIDEBAR.search(head):
            out.append(scope(head) + "{" + body + "}")
        index = end + 1
    return out


def build():
    css = re.sub(r"/\*.*?\*/", "", SOURCE.read_text(), flags=re.S)
    return "/* Generated by scripts/dashboard/theme.py from style.css; do not edit. */\n" + TOKENS + "\n" + "\n".join(rules(css)) + "\n"


if __name__ == "__main__":
    text = build()
    if "--check" in sys.argv:
        sys.exit(0 if TARGET.exists() and TARGET.read_text() == text else 1)
    TARGET.write_text(text)
    print(f"{TARGET.name}: {len(text)} bytes")
