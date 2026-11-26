#!/usr/bin/env python3
"""Opt-in clipboard integration: private KWin + real Wayland Chrome + CDP paste.

Needs kwin_wayland, dbus-run-session, google-chrome, Node 22+, and Python Pillow.
Build: cargo build -p pneud --example received_files_offer
Run: python3 crates/pneud/tests/linux/clipboard_files_chrome.py /ABS/TARGET/debug/examples/received_files_offer
Uses only a fresh compositor/socket/profile, never the session clipboard.
"""
import http.server
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

from PIL import Image

PAGE = b'''<!doctype html><meta charset="utf-8"><div contenteditable id="target">Paste here</div>
<script>
window.pastes=[];
document.addEventListener('paste', async e => {
 e.preventDefault();
 const types=Array.from(e.clipboardData.types);
 const files=await Promise.all(Array.from(e.clipboardData.files,async f=>({
  name:f.name,type:f.type,size:f.size,
  header:Array.from(new Uint8Array(await f.arrayBuffer()).slice(0,8))
 })));
 window.pastes.push({types,files});
});
</script>'''

DRIVER = r'''
const {spawn} = require('node:child_process');
const fs = require('node:fs');
const [wsUrl, probe, root] = process.argv.slice(2);
const ws = new WebSocket(wsUrl);
let seq=0, pending=new Map(), owner;
const delay=ms=>new Promise(r=>setTimeout(r,ms));
ws.onmessage=e=>{const m=JSON.parse(e.data);if(m.id){const p=pending.get(m.id);pending.delete(m.id);m.error?p.reject(Error(JSON.stringify(m.error))):p.resolve(m.result)}};
const call=(method,params={})=>new Promise((resolve,reject)=>{const id=++seq;pending.set(id,{resolve,reject});ws.send(JSON.stringify({id,method,params}))});
const value=async expression=>(await call('Runtime.evaluate',{expression,returnByValue:true,awaitPromise:true})).result.value;
async function stopOwner(){
 if(!owner)return;
 const child=owner;owner=null;
 child.kill('SIGINT');
 for(let i=0;i<100&&child.exitCode===null;i++)await delay(20);
 if(child.exitCode===null)child.kill('SIGKILL');
}
async function offer(mode, names){
 await stopOwner();
 owner=spawn(probe,[mode,...names.map(n=>root+'/'+n)],{env:process.env,stdio:['ignore','pipe','pipe']});
 const child=owner;
 await new Promise((resolve,reject)=>{
  let text='',err='';
  child.stdout.on('data',d=>{text+=d;if(text.includes('OFFERED'))resolve()});
  child.stderr.on('data',d=>err+=d);
  child.on('exit',code=>reject(Error(`offer exited ${code}: ${err}`)));
 });
 await delay(250);
}
async function paste(){
 await call('Page.bringToFront');
 const index=await value('window.pastes.length');
 await value("document.getElementById('target').focus()");
 await call('Input.dispatchKeyEvent',{type:'rawKeyDown',modifiers:2,key:'v',code:'KeyV',windowsVirtualKeyCode:86,commands:['paste']});
 await call('Input.dispatchKeyEvent',{type:'keyUp',modifiers:2,key:'v',code:'KeyV',windowsVirtualKeyCode:86});
 for(let i=0;i<100;i++){
  const result=await value(`window.pastes[${index}]`);
  if(result)return result;
  await delay(50);
 }
 throw Error('paste event timeout');
}
(async()=>{
 await new Promise((resolve,reject)=>{ws.onopen=resolve;ws.onerror=reject});
 await call('Page.enable');await call('Runtime.enable');
 for(let i=0;i<100 && !await value('!!window.pastes');i++)await delay(50);
 const results={};
 await offer('files',['one.png','two.jpg','three.webp']);
 results.multiple=await paste();
 const actual=results.multiple.files.map(f=>[f.name,f.type]);
 const expected=[['one.png','image/png'],['two.jpg','image/jpeg'],['three.webp','image/webp']];
 if(JSON.stringify(actual)!==JSON.stringify(expected))throw Error('wrong multiple files: '+JSON.stringify(results));
 await offer('image',['two.jpg']);
 results.single_image=await paste();
 if(results.single_image.files.length!==1 || results.single_image.files[0].type!=='image/png' || results.single_image.files[0].header.join(',')!=='137,80,78,71,13,10,26,10')throw Error('wrong single image: '+JSON.stringify(results));
 await offer('files',['document.pdf']);
 results.single_file=await paste();
 if(results.single_file.files.length!==1 || results.single_file.files[0].name!=='document.pdf' || results.single_file.files[0].type!=='application/pdf')throw Error('wrong single file: '+JSON.stringify(results));
 console.log(JSON.stringify(results,null,2));
})().catch(e=>{console.error(e);process.exitCode=1}).finally(async()=>{await stopOwner();ws.close()});
'''


class Page(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.end_headers()
        self.wfile.write(PAGE)

    def log_message(self, *_):
        pass


def stop_group(child):
    try:
        os.killpg(child.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        child.wait(timeout=3)
    except subprocess.TimeoutExpired:
        os.killpg(child.pid, signal.SIGKILL)
        child.wait(timeout=3)
    # The leader may exit before an owned helper. Kill remaining descendants too.
    try:
        os.killpg(child.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def main():
    probe = Path(sys.argv[1]).resolve(strict=True)
    cache = Path.home() / '.cache/agent-scratch/pneu'
    cache.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='clipboard-live-', dir=cache) as temporary:
        root = Path(temporary)
        runtime = root / 'runtime'
        runtime.mkdir(mode=0o700)
        for name, fmt in [('one.png', 'PNG'), ('two.jpg', 'JPEG'), ('three.webp', 'WEBP')]:
            Image.new('RGB', (5, 7), (12, 34, 56)).save(root / name, fmt)
        (root / 'document.pdf').write_bytes(b'%PDF-1.4\n% synthetic clipboard fixture\n%%EOF\n')
        (root / 'driver.cjs').write_text(DRIVER)
        env = os.environ.copy()
        for key in ['DISPLAY', 'WAYLAND_DISPLAY', 'SESSION_MANAGER', 'DBUS_SESSION_BUS_ADDRESS',
                    'DBUS_SESSION_BUS_PID', 'XDG_SESSION_DESKTOP', 'XDG_CURRENT_DESKTOP']:
            env.pop(key, None)
        env.update(XDG_RUNTIME_DIR=str(runtime), XDG_CONFIG_HOME=str(root / 'config'),
                   XDG_CACHE_HOME=str(root / 'cache'), XDG_DATA_HOME=str(root / 'data'),
                   QT_QPA_PLATFORM='offscreen', KWIN_COMPOSE='Q', LIBGL_ALWAYS_SOFTWARE='1',
                   PNEU_PRIVATE_CLIPBOARD_PROBE='1', PROBE_CACHE=str(root / 'probe-cache'))
        children = []
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Page)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            with (root / 'kwin.log').open('w') as log:
                children.append(subprocess.Popen(['dbus-run-session', '--', 'kwin_wayland', '--virtual',
                    '--socket', 'pneu-files-test', '--width', '800', '--height', '600',
                    '--no-lockscreen', '--no-global-shortcuts', '--no-kactivities'],
                    env=env, stdout=log, stderr=log, start_new_session=True))
            for _ in range(200):
                if (runtime / 'pneu-files-test').exists():
                    break
                if children[-1].poll() is not None:
                    raise RuntimeError((root / 'kwin.log').read_text())
                time.sleep(.05)
            else:
                raise RuntimeError('private Wayland socket timeout')
            env['WAYLAND_DISPLAY'] = 'pneu-files-test'
            url = f'http://127.0.0.1:{server.server_port}/'
            with (root / 'chrome.log').open('w') as log:
                children.append(subprocess.Popen(['google-chrome', '--ozone-platform=wayland',
                    f'--user-data-dir={root / "chrome-profile"}', '--remote-debugging-address=127.0.0.1',
                    '--remote-debugging-port=0', '--no-first-run', '--no-default-browser-check',
                    '--disable-background-networking', '--disable-component-update', '--disable-gpu', url],
                    env=env, stdout=log, stderr=log, start_new_session=True))
            port_file = root / 'chrome-profile/DevToolsActivePort'
            for _ in range(200):
                if port_file.exists():
                    break
                if children[-1].poll() is not None:
                    raise RuntimeError((root / 'chrome.log').read_text())
                time.sleep(.05)
            else:
                raise RuntimeError('private Chrome DevTools timeout')
            port = int(port_file.read_text().splitlines()[0])
            for _ in range(100):
                pages = json.load(urllib.request.urlopen(f'http://127.0.0.1:{port}/json/list', timeout=2))
                page = next((p for p in pages if p.get('url') == url), None)
                if page:
                    break
                time.sleep(.05)
            else:
                raise RuntimeError('local paste test page unavailable')
            with (root / 'driver.log').open('w') as log:
                child = subprocess.Popen(['node', str(root / 'driver.cjs'), page['webSocketDebuggerUrl'],
                    str(probe), str(root)], env=env, stdout=log, stderr=log, start_new_session=True)
                children.append(child)
                child.wait(timeout=35)
            output = (root / 'driver.log').read_text()
            print(output)
            if child.returncode:
                raise RuntimeError(f'private paste check failed ({child.returncode})')
        finally:
            for child in reversed(children):
                stop_group(child)
            # KWin's private D-Bus may activate a document portal. Unmount its
            # task-owned FUSE mount after stopping it, before deleting the root.
            doc = runtime / 'doc'
            if f' {doc} ' in Path('/proc/self/mountinfo').read_text():
                subprocess.run(['fusermount3', '-u', str(doc)], check=True, timeout=5)
            server.shutdown()
            server.server_close()
    print('CLEAN: private compositor, Chrome, offers and server stopped; profile/fixtures removed')


if __name__ == '__main__':
    main()
