from PIL import Image, ImageDraw, ImageFilter

SRC = r"c:\Users\Administrator\WorkBuddy\2026-10-14-12-06-35\generated-images\Bright_youthful_software_appli_2026-10-16T03-23-34.png"
OUT = r"G:\InDesk\brand\indesk.ico"
PNG = r"G:\InDesk\brand\indesk.png"

img = Image.open(SRC).convert("RGBA")
w, h = img.size
cx, cy = w // 2, h // 2

def blue_extent(pixels):
    lo, hi = None, None
    for i in range(len(pixels)):
        r, g, b, a = pixels[i]
        if b > 60 and (b > r + 10 or b > 100):  # 深蓝到亮蓝
            if lo is None:
                lo = i
            hi = i
    return lo, hi

x0, x1 = blue_extent([img.getpixel((x, cy)) for x in range(w)])
y0, y1 = blue_extent([img.getpixel((cx, y)) for y in range(h)])
mx = int((x1 - x0) * 0.012); my = int((y1 - y0) * 0.012)
x0 += mx; x1 -= mx; y0 += my; y1 -= my
side = min(x1 - x0, y1 - y0)
ccx, ccy = (x0 + x1) // 2, (y0 + y1) // 2
x0 = ccx - side // 2; y0 = ccy - side // 2
sq = img.crop((x0, y0, x0 + side, y0 + side))

# ---- 修补右下角水印残影：用左侧同高度渐变镜像覆盖 + 羽化 ----
px0, px1 = int(side * 0.86), side            # 修补区 x
py0, py1 = int(side * 0.90), side            # 修补区 y
sx0 = px0 - (px1 - px0)                       # 采样区（左侧等宽）
patch = sq.crop((sx0, py0, px0, py1)).transpose(Image.FLIP_LEFT_RIGHT)
mask = Image.new("L", patch.size, 0)
md = ImageDraw.Draw(mask)
md.rectangle([0, 0, patch.size[0] - 1, patch.size[1] - 1], fill=255)
mask = mask.filter(ImageFilter.GaussianBlur(10))  # 边缘羽化融合
sq.paste(patch, (px0, py0), mask)

# ---- 圆角掩膜 ----
alpha = Image.new("L", (side, side), 0)
ImageDraw.Draw(alpha).rounded_rectangle(
    [0, 0, side - 1, side - 1], radius=int(side * 0.20), fill=255)
sq.putalpha(alpha)

sq.save(PNG)
sizes = [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]
sq.save(OUT, format="ICO", sizes=sizes)
print("OK", side, "->", OUT)
