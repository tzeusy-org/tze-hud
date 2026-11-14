"""Generate the stand-in fallback fonts used by the font-fallback unit tests.

Each face is a box-glyph font covering only the code points a test needs,
under a distinct family name, so the tests can tell which "system" family
shaped a glyph without shipping real CJK or emoji fonts (hud-ljhku.13).
Released under CC0 1.0. Regenerate with: python3 make_standins.py
(requires fontTools).
"""

from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen

UPM = 1000
FACES = {
    "TzeStandInEmoji.ttf": ("TzeStandIn Emoji", "✅"),
    "TzeStandInJa.ttf": ("TzeStandIn Ja", "完成かん"),
    "TzeStandInZh.ttf": ("TzeStandIn Zh", "完成"),
    "TzeStandInKo.ttf": ("TzeStandIn Ko", "테스트"),
}


def box(inset):
    pen = TTGlyphPen(None)
    pen.moveTo((inset, 0))
    pen.lineTo((inset, 800))
    pen.lineTo((UPM - inset, 800))
    pen.lineTo((UPM - inset, 0))
    pen.closePath()
    return pen.glyph()


def build(path, family, chars):
    names = [".notdef"] + [f"u{ord(c):04X}" for c in chars]
    fb = FontBuilder(UPM, isTTF=True)
    fb.setupGlyphOrder(names)
    fb.setupCharacterMap({ord(c): f"u{ord(c):04X}" for c in chars})
    fb.setupGlyf({n: box(100 if n == ".notdef" else 60) for n in names})
    fb.setupHorizontalMetrics({n: (UPM, 60) for n in names})
    fb.setupHorizontalHeader(ascent=880, descent=-120)
    ps = family.replace(" ", "") + "-Regular"
    fb.setupNameTable(
        {
            "familyName": family,
            "styleName": "Regular",
            "fullName": family + " Regular",
            "psName": ps,
            "uniqueFontIdentifier": ps,
        }
    )
    fb.setupOS2(sTypoAscender=880, sTypoDescender=-120, usWinAscent=880, usWinDescent=120)
    fb.setupPost()
    fb.save(path)


for file, (family, chars) in FACES.items():
    build(file, family, chars)
