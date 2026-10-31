"""Convert the approved Frames artwork to native icon containers (requires Pillow)."""
from pathlib import Path
from PIL import Image

ROOT = Path(__file__).resolve().parents[1]


def main():
    source = Image.open(ROOT / "assets/branding/frames-icon-white-shadow.png").convert("RGBA")
    output = ROOT / "assets/icons"
    output.mkdir(parents=True, exist_ok=True)
    image = source.resize((1024, 1024), Image.Resampling.LANCZOS)
    image.save(output / "Frames.icns")
    image.save(output / "Frames.ico", sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)])
    image.resize((256, 256), Image.Resampling.LANCZOS).save(output / "Frames.png")


if __name__ == "__main__":
    main()
