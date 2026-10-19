"""Capture pixels of this app's real running Win32 window, never a mockup.

Build/verification utility only (Python + Pillow are not runtime dependencies).
Captures only windows belonging to the PID in this project's runtime snapshot.
"""
from pathlib import Path
from datetime import datetime, timezone
import argparse
import ctypes
from ctypes import wintypes as w
import hashlib
import json
import os
import time
from PIL import ImageGrab

ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
target=parser.add_mutually_exclusive_group()
target.add_argument('--menu',action='store_true')
target.add_argument('--tray',action='store_true')
parser.add_argument('--device-id',help='For --tray, choose one monitored device by identity')
parser.add_argument('--tray-id',type=int,help='For --tray, choose an ID reported by the running app')
parser.add_argument('--screen',action='store_true',help='Capture the visible desktop rectangle, including the real caption')
parser.add_argument('--output',type=Path,default=ROOT/'validation/current/actual-window.png')
args=parser.parse_args()
user=ctypes.WinDLL('user32',use_last_error=True)
user.SetProcessDpiAwarenessContext.argtypes=[w.HANDLE]
user.SetProcessDpiAwarenessContext(w.HANDLE(-4))
user.GetWindowThreadProcessId.argtypes=[w.HWND,ctypes.POINTER(w.DWORD)]
user.GetClassNameW.argtypes=[w.HWND,w.LPWSTR,ctypes.c_int]
user.IsWindowVisible.argtypes=[w.HWND]
user.GetWindowRect.argtypes=[w.HWND,ctypes.POINTER(w.RECT)]
user.GetDpiForWindow.argtypes=[w.HWND]
user.GetForegroundWindow.restype=w.HWND
user.WindowFromPoint.argtypes=[w.POINT]
user.WindowFromPoint.restype=w.HWND
user.GetAncestor.argtypes=[w.HWND,w.UINT]
user.GetAncestor.restype=w.HWND
state=json.loads((ROOT/'.build/runtime-status.json').read_text(encoding='utf-8'))
if not 0 <= time.time()-state['updated_unix'] <= 60:
 raise RuntimeError('Runtime snapshot is stale')
# Bind evidence to the running process image, not merely a file at dist/.
kernel=ctypes.WinDLL('kernel32',use_last_error=True)
kernel.OpenProcess.argtypes=[w.DWORD,w.BOOL,w.DWORD];kernel.OpenProcess.restype=w.HANDLE
kernel.QueryFullProcessImageNameW.argtypes=[w.HANDLE,w.DWORD,w.LPWSTR,ctypes.POINTER(w.DWORD)]
kernel.CloseHandle.argtypes=[w.HANDLE]
process=kernel.OpenProcess(0x1000,False,state['pid'])
if not process:raise ctypes.WinError(ctypes.get_last_error())
try:
 name=ctypes.create_unicode_buffer(32768);length=w.DWORD(len(name))
 if not kernel.QueryFullProcessImageNameW(process,0,name,ctypes.byref(length)):
  raise ctypes.WinError(ctypes.get_last_error())
 executable=Path(name.value)
finally:kernel.CloseHandle(process)
if os.path.normcase(str(executable.resolve()))!=os.path.normcase(str((ROOT/'dist/quiet-battery.exe').resolve())):
 raise RuntimeError(f'Running PID does not belong to the expected release: {executable}')
image_hash=hashlib.sha256(executable.read_bytes()).hexdigest()
expected=('QuietBattery.NativeTray.Window' if args.tray else '#32768' if args.menu else 'QuietBattery.NativeTray.Information')
matches=[]
callback_type=ctypes.WINFUNCTYPE(w.BOOL,w.HWND,w.LPARAM)
@callback_type
def visit(hwnd,_):
 pid=w.DWORD();user.GetWindowThreadProcessId(hwnd,ctypes.byref(pid))
 name=ctypes.create_unicode_buffer(256);user.GetClassNameW(hwnd,name,len(name))
 if pid.value==state['pid'] and name.value==expected and (args.tray or user.IsWindowVisible(hwnd)):matches.append(int(hwnd))
 return True
user.EnumWindows.argtypes=[callback_type,w.LPARAM]
user.EnumWindows(visit,0)
if len(matches)!=1:raise RuntimeError(f'Expected one visible app-owned {expected} window, found {matches}')
hwnd=matches[0]
rect=w.RECT();user.GetWindowRect(hwnd,ctypes.byref(rect))
if args.tray:
 class Guid(ctypes.Structure):
  _fields_=[('a',w.DWORD),('b',w.WORD),('c',w.WORD),('d',w.BYTE*8)]
 class NotifyIdentifier(ctypes.Structure):
  _fields_=[('size',w.DWORD),('hwnd',w.HWND),('id',w.UINT),('guid',Guid)]
 available={d['id']:d['tray_id'] for d in state.get('devices',[]) if d.get('tray_id') is not None}
 allowed=set(available.values())
 fallback=state.get('fallback_tray_id')
 if fallback is not None:allowed.add(fallback)
 if not state.get('schema'):allowed.add(1)  # Historical single-device release.
 if args.device_id:
  if args.device_id not in available:raise RuntimeError('Requested device has no registered tray identity')
  shell_id=available[args.device_id]
 elif args.tray_id is not None:
  if args.tray_id not in allowed:raise RuntimeError('Tray ID is not owned by the runtime snapshot')
  shell_id=args.tray_id
 elif len(allowed)==1:shell_id=next(iter(allowed))
 else:raise RuntimeError('Multiple or no tray identities; specify --device-id or --tray-id')
 identifier=NotifyIdentifier();identifier.size=ctypes.sizeof(identifier);identifier.hwnd=hwnd;identifier.id=shell_id
 shell=ctypes.WinDLL('shell32')
 shell.Shell_NotifyIconGetRect.argtypes=[ctypes.POINTER(NotifyIdentifier),ctypes.POINTER(w.RECT)]
 shell.Shell_NotifyIconGetRect.restype=ctypes.c_long
 result=shell.Shell_NotifyIconGetRect(ctypes.byref(identifier),ctypes.byref(rect))
 if result!=0 or rect.right<=rect.left or rect.bottom<=rect.top:raise RuntimeError(f'Tray icon rectangle unavailable: HRESULT {result}')
 args.screen=True
if args.screen:
 if not args.menu and not args.tray and int(user.GetForegroundWindow() or 0)!=hwnd:
  raise RuntimeError('Main window is not foreground; refusing a potentially obscured screen capture')
 # DWM bounds exclude invisible resize margins that could contain another app.
 frame=w.RECT()
 dwm=ctypes.WinDLL('dwmapi')
 dwm.DwmGetWindowAttribute.argtypes=[w.HWND,w.DWORD,ctypes.c_void_p,w.DWORD]
 if not args.tray and dwm.DwmGetWindowAttribute(hwnd,9,ctypes.byref(frame),ctypes.sizeof(frame))==0:
  rect=frame
 def point_visibility():
  checks=[]
  for fx,fy in ((.25,.25),(.75,.25),(.5,.5),(.25,.75),(.75,.75)):
   x=int(rect.left+(rect.right-rect.left)*fx);y=int(rect.top+(rect.bottom-rect.top)*fy)
   hit=user.WindowFromPoint(w.POINT(x,y));root=user.GetAncestor(hit,2)
   root_name=ctypes.create_unicode_buffer(256);user.GetClassNameW(root,root_name,len(root_name))
   visible=(root_name.value in {'Shell_TrayWnd','Shell_SecondaryTrayWnd','NotifyIconOverflowWindow'} if args.tray else int(root or 0)==hwnd)
   checks.append({'point':[x,y],'root_class':root_name.value,'target_unobscured_at_point':visible})
  if not all(x['target_unobscured_at_point'] for x in checks):
   raise RuntimeError(f'Target desktop rectangle is obscured at a sampled point: {checks}')
  return checks
 points_before=point_visibility()
 pixels=ImageGrab.grab(bbox=(rect.left,rect.top,rect.right,rect.bottom),all_screens=True,include_layered_windows=True)
 points_after=point_visibility()
else:
 points_before=points_after=[]
 pixels=ImageGrab.grab(window=hwnd)
after=json.loads((ROOT/'.build/runtime-status.json').read_text(encoding='utf-8'))
if after['pid']!=state['pid']:raise RuntimeError('Tray process changed during capture')
if pixels.getextrema()==((0,0),(0,0),(0,0)):raise RuntimeError('Windows returned an all-black capture')
args.output.parent.mkdir(parents=True,exist_ok=True)
pixels.save(args.output)
metadata={
 'method':('Desktop pixels at app-owned Shell_NotifyIconGetRect' if args.tray else 'Pillow ImageGrab desktop pixels at actual HWND rectangle' if args.screen else 'Pillow ImageGrab(window=HWND), actual native client capture'),
 'mockup':False,'utc':datetime.now(timezone.utc).isoformat(),
 'pid':state['pid'],'hwnd':hwnd,'window_class':expected,
 'tray_shell_id':shell_id if args.tray else None,
 'window_rect':[rect.left,rect.top,rect.right,rect.bottom],
 'dpi':user.GetDpiForWindow(hwnd),'pixel_dimensions':list(pixels.size),
 'executable_path':str(executable),'executable_sha256':image_hash,
 'snapshot_before':state,'snapshot_after':after,
 'snapshot_changed_during_capture':any(state.get(k)!=after.get(k) for k in ('reading','stale','busy','error')),
 'sampled_visibility_before':points_before,'sampled_visibility_after':points_after,
 'visual_inspection_required':True,
}
args.output.with_suffix('.json').write_text(json.dumps(metadata,ensure_ascii=False,indent=2)+'\n',encoding='utf-8')
print(json.dumps({'output':str(args.output),'dimensions':pixels.size,'pid':state['pid']}))
