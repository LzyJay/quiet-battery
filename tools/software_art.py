"""Approved v14 software symbol; dedicated 16/20 pixel geometry."""
from pathlib import Path
from PIL import Image, ImageDraw
ROOT=Path(__file__).resolve().parents[1]

def software_icon(n):
    if n>=24:
        source=ROOT/'assets/source/software'/f'icon-{n}.png'
        if not source.exists():
            source=ROOT/'assets/source/software/icon-256.png'
        return Image.open(source).convert('RGBA').resize((n,n),Image.Resampling.LANCZOS)
    # Draw each small grid at its own size. Battery stays left; equal transport
    # backplates stay on the right, with simplified but separate glyphs.
    im=Image.new('RGBA',(n,n));d=ImageDraw.Draw(im)
    ink='#292824';tile='#EEE3CF';clay='#B65F43'
    d.rounded_rectangle((0,0,n-1,n-1),radius=3,fill=tile)
    if n==16:
        d.rectangle((4,2,6,3),fill=ink);d.rounded_rectangle((2,4,8,13),radius=1,fill=ink);d.rectangle((3,5,7,12),fill=tile)
        for y in (6,8,10):d.rectangle((4,y,6,y+1),fill=clay)
        badges=((10,4,15,8),(10,10,15,14))
        for b in badges:d.rounded_rectangle(b,radius=1,fill=ink)
        d.line(((12,5),(14,6),(12,7),(12,5)),fill=tile,width=1)
        # Tiny RF badge retains a distinct 2.4 radio pattern, not unreadable text.
        d.point((11,12),fill=tile);d.line(((12,11),(13,11),(13,12),(12,13),(14,13)),fill=tile,width=1)
    else:
        d.rectangle((5,2,8,3),fill=ink);d.rounded_rectangle((2,4,10,16),radius=1,fill=ink);d.rectangle((3,5,9,15),fill=tile)
        for y in (6,9,12):d.rectangle((4,y,8,y+1),fill=clay)
        for y in (5,12):d.rounded_rectangle((12,y,19,y+5),radius=1,fill=ink)
        d.line(((14,6),(17,8),(15,9),(15,6),(17,7),(14,9)),fill=tile,width=1)
        d.line(((13,13),(15,13),(15,14),(13,16),(15,16)),fill=tile,width=1)
        d.line(((17,13),(17,15),(18,15),(18,13),(18,16)),fill=tile,width=1)
    return im
