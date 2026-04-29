#!/usr/bin/env python3
"""Generate README color swatches for Babel's harness support table."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
OUT_DIR = ROOT / "docs" / "swatches"


@dataclass(frozen=True)
class Swatch:
    slug: str
    label: str
    colors: tuple[str, ...]


PALETTE: tuple[Swatch, ...] = (
    Swatch("claude-code", "Claude Code", ("#D97757",)),
    Swatch("codex-cli", "Codex CLI", ("#10A37F",)),
    Swatch("factory-droid", "Factory Droid", ("#D15010",)),
    Swatch("qwen-code", "Qwen Code", ("#624BEA",)),
    Swatch("kimi-cli", "Kimi CLI", ("#7F1C10",)),
    Swatch("gemini-cli", "Gemini CLI", ("#4285F4",)),
    Swatch("crush", "Crush", ("#6B50FF",)),
    Swatch("cursor-agent", "Cursor Agent", ("#14120B", "#F7F7F4")),
    Swatch("cline", "Cline", ("#9663F0",)),
    Swatch("opencode", "OpenCode", ("#FAB283",)),
    Swatch("amp", "Amp", ("#F34E3F",)),
    Swatch("kiro", "Kiro", ("#C6A0FF",)),
    Swatch("github-copilot-cli", "GitHub Copilot CLI", ("#8250DF",)),
    Swatch("roo-code", "Roo Code", ("#D8F14B",)),
    Swatch("kilo-code", "Kilo Code", ("#FA483A",)),
    Swatch("aider", "Aider", ("#14B014",)),
    Swatch("antigravity", "Antigravity", ("#3186FF",)),
)


def svg_for(swatch: Swatch) -> str:
    width = 44
    height = 14
    colors = swatch.colors
    rects: list[str] = []
    step = width / len(colors)
    for index, color in enumerate(colors):
        x = round(index * step, 3)
        w = round(step, 3)
        rects.append(f'<rect x="{x}" y="0" width="{w}" height="{height}" fill="{color}"/>')

    border = '<rect x="0.5" y="0.5" width="43" height="13" fill="none" stroke="#555" stroke-opacity="0.65"/>'
    title = f"<title>{swatch.label}: {' / '.join(colors)}</title>"
    return "\n".join(
        [
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img">',
            title,
            *rects,
            border,
            "</svg>",
            "",
        ]
    )


def main() -> None:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    for swatch in PALETTE:
        (OUT_DIR / f"{swatch.slug}.svg").write_text(svg_for(swatch), encoding="utf-8")


if __name__ == "__main__":
    main()
