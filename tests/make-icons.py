#!/usr/bin/env python3
"""Fabrique les icônes de Verbatim (icons/ et ui/icon-512.png).

Tuile bleue vive et barres blanches : lisible sur une barre des tâches
sombre comme claire. Sous 32 px, trois barres épaisses calées sur la grille
de pixels au lieu de cinq.
"""
import os
from PIL import Image, ImageDraw

HAUT, BAS = (74, 104, 255), (30, 48, 200)
BLANC = (255, 255, 255, 255)
S = 8  # suréchantillonnage


def icone(n):
    big = n * S
    grad = Image.new("RGBA", (1, big))
    for y in range(big):
        t = y / (big - 1)
        grad.putpixel((0, y), tuple(round(HAUT[i] + (BAS[i] - HAUT[i]) * t) for i in range(3)) + (255,))
    grad = grad.resize((big, big))
    masque = Image.new("L", (big, big), 0)
    marge = 0 if n <= 32 else round(n * 0.03)
    ImageDraw.Draw(masque).rounded_rectangle(
        [marge * S, marge * S, big - 1 - marge * S, big - 1 - marge * S], radius=round(n * 0.22 * S), fill=255)
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    img.paste(grad, (0, 0), masque)

    if n < 32:
        hauteurs = [0.38, 0.62, 0.38]
    else:
        hauteurs = [0.26, 0.50, 0.68, 0.42, 0.26]
    k = len(hauteurs)
    # Largeur de barre et écart en pixels entiers de l'image finale.
    larg = max(2, round(n * (0.125 if k == 3 else 0.094)))
    ecart = max(2, round(n * (0.11 if k == 3 else 0.07)))
    total = k * larg + (k - 1) * ecart
    x0 = (n - total) // 2
    d = ImageDraw.Draw(img)
    for i, h in enumerate(hauteurs):
        haut = max(larg, round(n * h))
        if (n - haut) % 2:
            haut += 1
        x = x0 + i * (larg + ecart)
        y = (n - haut) // 2
        d.rounded_rectangle([x * S, y * S, (x + larg) * S - 1, (y + haut) * S - 1], radius=larg * S // 2, fill=BLANC)
    return img.resize((n, n), Image.LANCZOS)


def main():
    racine = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    ic = os.path.join(racine, "icons")
    icone(32).save(os.path.join(ic, "32x32.png"))
    icone(128).save(os.path.join(ic, "128x128.png"))
    icone(256).save(os.path.join(ic, "128x128@2x.png"))
    icone(512).save(os.path.join(ic, "icon.png"))
    icone(512).save(os.path.join(racine, "ui", "icon-512.png"))
    tailles = [16, 24, 32, 48, 64, 128, 256]
    imgs = [icone(t) for t in tailles]
    imgs[-1].save(os.path.join(ic, "icon.ico"), format="ICO", sizes=[(t, t) for t in tailles], append_images=imgs[:-1])


if __name__ == "__main__":
    main()
