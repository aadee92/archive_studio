import math, os, random, struct, sys, time, wave
from multiprocessing import Pool
from PIL import Image, ImageDraw, ImageFilter

OUT = sys.argv[1]
N = int(sys.argv[2]) if len(sys.argv) > 2 else 2000
FOLDERS = ["1984_summer_lake", "1987_christmas", "1990_grandma_house", "1993_school_photos",
           "1996_road_trip", "1999_wedding", "box_01_scans", "box_02_scans", "box_03_scans",
           "loose_prints", "slides_carousel_a", "unsorted"]
SIZES = [(640, 480), (480, 640), (800, 533), (533, 800), (600, 600), (720, 480)]
FORMATS = [("jpg", "JPEG", 70), ("png", "PNG", 20), ("tif", "TIFF", 10)]

def pick_format(r):
    x = r.uniform(0, 100)
    for ext, fmt, w in FORMATS:
        if x < w: return ext, fmt
        x -= w
    return FORMATS[0][:2]

def rgb(r, lo=0, hi=255):
    return tuple(r.randint(lo, hi) for _ in range(3))

def scene(r, w, h):
    img = Image.new("RGB", (w, h))
    d = ImageDraw.Draw(img)
    kind = r.choice(["landscape", "landscape", "abstract", "portrait", "stripes"])
    top, bottom = rgb(r, 60, 230), rgb(r, 20, 200)
    for y in range(h):  # vertical gradient background
        t = y / h
        d.line([(0, y), (w, y)], fill=tuple(int(a + (b - a) * t) for a, b in zip(top, bottom)))
    if kind == "landscape":
        horizon = int(h * r.uniform(0.45, 0.7))
        pts = [(0, h)] + [(x, horizon + int(math.sin(x / r.uniform(30, 90)) * r.uniform(5, 30))) for x in range(0, w + 20, 20)] + [(w, h)]
        d.polygon(pts, fill=rgb(r, 20, 140))
        sx, sy, sr = r.randint(0, w), r.randint(0, horizon // 2), r.randint(20, 60)
        d.ellipse([sx - sr, sy - sr, sx + sr, sy + sr], fill=rgb(r, 200, 255))
        for _ in range(r.randint(2, 8)):  # trees / houses
            x, bw, bh = r.randint(0, w), r.randint(15, 60), r.randint(30, 120)
            d.rectangle([x, horizon - bh, x + bw, horizon + 10], fill=rgb(r, 10, 120))
    elif kind == "portrait":
        cx, cy = w // 2, int(h * 0.42)
        hr = min(w, h) // 5
        d.ellipse([cx - hr * 2, cy + hr, cx + hr * 2, h + hr], fill=rgb(r, 30, 160))  # shoulders
        d.ellipse([cx - hr, cy - hr, cx + hr, cy + int(hr * 1.2)], fill=(r.randint(150, 240), r.randint(110, 190), r.randint(80, 160)))
    elif kind == "stripes":
        step = r.randint(20, 80)
        for i in range(0, w + h, step * 2):
            d.polygon([(i, 0), (i + step, 0), (i + step - h, h), (i - h, h)], fill=rgb(r))
    else:
        for _ in range(r.randint(5, 25)):
            x, y, s = r.randint(0, w), r.randint(0, h), r.randint(15, 150)
            shape = r.choice([d.ellipse, d.rectangle])
            shape([x - s, y - s, x + s, y + s], fill=rgb(r), outline=rgb(r))
    img = img.filter(ImageFilter.GaussianBlur(r.uniform(0.5, 2.0)))
    # Faded old-print look on some
    if r.random() < 0.4:
        sepia = Image.new("RGB", (w, h), (r.randint(150, 200), r.randint(120, 160), r.randint(70, 110)))
        img = Image.blend(img, sepia, r.uniform(0.2, 0.45))
    return img

def make(i):
    r = random.Random(i)
    folder = r.choice(FOLDERS)
    w, h = r.choice(SIZES)
    ext, fmt = pick_format(r)
    prefix = "scan" if folder.startswith(("box", "loose", "slides")) else "IMG"
    path = os.path.join(OUT, folder, f"{prefix}_{i:05d}.{ext}")
    img = scene(r, w, h)
    ImageDraw.Draw(img).text((8, h - 18), f"#{i:05d}", fill=(255, 255, 255))
    kw = {"quality": r.randint(70, 92)} if fmt == "JPEG" else {}
    img.save(path, fmt, **kw)
    ts = r.uniform(time.mktime((1985, 1, 1, 0, 0, 0, 0, 0, -1)), time.time())
    os.utime(path, (ts, ts))

def make_wav(i):
    r = random.Random(100000 + i)
    path = os.path.join(OUT, "audio_tapes", f"tape_side_{'AB'[i % 2]}_{i // 2 + 1:02d}.wav")
    rate, secs, freq = 22050, r.uniform(1, 4), r.uniform(220, 660)
    with wave.open(path, "wb") as wf:
        wf.setnchannels(1); wf.setsampwidth(2); wf.setframerate(rate)
        wf.writeframes(b"".join(struct.pack("<h", int(8000 * math.sin(2 * math.pi * freq * n / rate))) for n in range(int(rate * secs))))

if __name__ == "__main__":
    for f in FOLDERS + ["audio_tapes"]:
        os.makedirs(os.path.join(OUT, f), exist_ok=True)
    with Pool() as pool:
        pool.map(make, range(N), chunksize=16)
    for i in range(12):
        make_wav(i)
