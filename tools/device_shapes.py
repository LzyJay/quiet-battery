"""Canonical filled device geometry; used for window and tray assets."""
from PIL import Image, ImageDraw
KINDS = ("mouse", "keyboard", "numpad", "headset", "earbuds", "gamepad", "touchpad", "trackball", "tablet", "pen", "microphone", "speaker", "remote", "camera", "presenter", "generic")

class Vector:
    def __init__(self,size=64):
        self.size=size; self.commands=[]
    def rect(self,x,y,w,h,fill,r=0): self.commands.append(('rect',(x,y,w,h,fill,r)))
    def circle(self,x,y,r,fill): self.commands.append(('circle',(x,y,r,fill)))
    def line(self,points,fill,width): self.commands.append(('line',(points,fill,width)))
    def png(self,n):
        scale=n*4/self.size
        im=Image.new('RGBA',(n*4,n*4)); d=ImageDraw.Draw(im)
        for typ,args in self.commands:
            if typ=='rect':
                x,y,w,h,c,r=args
                d.rounded_rectangle(tuple(round(a*scale) for a in (x,y,x+w,y+h)),radius=round(r*scale),fill=c)
            elif typ=='circle':
                x,y,r,c=args
                d.ellipse(tuple(round(a*scale) for a in (x-r,y-r,x+r,y+r)),fill=c)
            else:
                pts,c,w=args
                d.line([(round(x*scale),round(y*scale)) for x,y in pts],fill=c,width=max(1,round(w*scale)))
                for x,y in (pts[0],pts[-1]):
                    rr=w*scale/2
                    d.ellipse((x*scale-rr,y*scale-rr,x*scale+rr,y*scale+rr),fill=c)
        return im.resize((n,n),Image.Resampling.LANCZOS)
    def svg(self):
        out=[f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {self.size} {self.size}">']
        for typ,args in self.commands:
            if typ=='rect':
                x,y,w,h,c,r=args
                out.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{r}" fill="{c}"/>')
            elif typ=='circle':
                x,y,r,c=args
                out.append(f'<circle cx="{x}" cy="{y}" r="{r}" fill="{c}"/>')
            else:
                pts,c,w=args
                out.append(f'<polyline points="{" ".join(f"{x},{y}" for x,y in pts)}" fill="none" stroke="{c}" stroke-width="{w}" stroke-linecap="round" stroke-linejoin="round"/>')
        return '\n'.join(out+['</svg>'])

def base_shape(kind):
    v=Vector(16)
    if kind=='mouse':
        v.rect(3,1,10,14,'white',4)
        v.rect(7.35,2.2,1.3,4.6,'black',.5)
        v.rect(4.5,7,7,1,'black',.4)
    elif kind=='keyboard':
        v.rect(.4,3,15.2,10,'white',2)
        for y in (5.1,7.8):
            for x in (2.2,5.4,8.6,11.8): v.rect(x,y,1.7,1.4,'black',.2)
        v.rect(4.5,10.2,7,1.2,'black',.3)
    elif kind=='headset':
        v.circle(8,7.8,7,'white'); v.circle(8,7.8,4.6,'black')
        v.rect(0,9.8,16,6.2,'black')
        v.rect(.7,7,3.6,7,'white',1.4); v.rect(11.7,7,3.6,7,'white',1.4)
    else:
        v.rect(2,4,12,8,'white',3)
        v.line([(3,8),(1.7,12.5)],'white',3)
        v.line([(13,8),(14.3,12.5)],'white',3)
        v.rect(3.2,6.2,1.1,4,'black',.15); v.rect(1.8,7.6,4,1.1,'black',.15)
        for x,y in ((11.5,6.2),(13.1,7.8),(11.5,9.4),(9.9,7.8)): v.circle(x,y,.62,'black')
    return v
