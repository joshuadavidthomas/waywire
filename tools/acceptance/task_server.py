#!/usr/bin/env python3
"""Task pages for the dogfood acceptance tasks, from the dogfood harness.

Usage: task_server.py <log directory>

Every submission is appended to <log>/submissions.jsonl, the ground truth that
checks read; nothing a driver reports is trusted. Each page also posts its own
element boxes to <log>/layout.jsonl when it loads, and draws a small magenta
square at its top-left corner, so the runner can turn page coordinates into
screen coordinates (a Wayland client cannot know its screen position).
"""

import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

LOG = Path(sys.argv[1])
STYLE = """<style>body{font:18px sans-serif;margin:24px;background:#fafafa}
label{display:block;margin:10px 0 2px}input,select,textarea{font-size:18px;padding:4px;width:360px}
button{font-size:18px;padding:6px 18px;margin-top:14px}
#drop{border:3px dashed #888;width:520px;height:180px;display:flex;align-items:center;justify-content:center;margin-top:12px}
#drop.over{background:#def}</style>"""
LAYOUT = """<div id=anchor style="position:fixed;left:0;top:0;width:6px;height:6px;background:#ff00ff"></div>
<script>addEventListener('load',()=>{const r={};for(const e of document.querySelectorAll('[id]')){
const b=e.getBoundingClientRect();r[e.id]=[b.left,b.top,b.width,b.height]}
fetch('/layout',{method:'POST',body:JSON.stringify({page:location.pathname,ratio:devicePixelRatio,rects:r})})})</script>"""

FORM = f"""<!doctype html><title>Registration form</title>{STYLE}
<h1>Registration</h1>
<form method=post action=/submit>
<input type=hidden name=task value=form>
<label for=name>Full name</label><input id=name name=name autocomplete=off>
<label for=email>Email</label><input id=email name=email autocomplete=off>
<label for=phone>Phone</label><input id=phone name=phone autocomplete=off>
<label for=plan>Plan</label><select id=plan name=plan><option>Basic</option><option>Pro</option><option>Enterprise</option></select>
<label><input type=checkbox id=terms name=terms value=yes style="width:auto"> I accept the terms</label>
<label for=notes>Notes</label><textarea id=notes name=notes rows=2></textarea>
<button type=submit id=submit>Submit</button>
</form>
<script>
const p=new URLSearchParams(location.search);
if(p.get('interrupt')){{let fired=false;document.getElementById('phone').addEventListener('focus',()=>{{if(!fired){{fired=true;setTimeout(()=>alert('Session expiring soon. Click OK to continue.'),300)}}}})}}
</script>{LAYOUT}"""

PASTE = f"""<!doctype html><title>Paste target</title>{STYLE}
<h1>Token check</h1>
<form method=post action=/submit><input type=hidden name=task value=paste>
<label for=token>Token</label><input id=token name=token autocomplete=off>
<button type=submit id=check>Check</button></form>{LAYOUT}"""

UPLOAD = f"""<!doctype html><title>Upload</title>{STYLE}
<h1>Upload a file</h1>
<div id=drop>Drop a file here</div>
<p>Or choose: <input type=file id=pick style="width:auto"></p>
<p id=status>No file yet.</p>
<script>
const d=document.getElementById('drop'),s=document.getElementById('status');
function send(f){{f.text().then(t=>{{fetch('/submit',{{method:'POST',headers:{{'content-type':'application/x-www-form-urlencoded'}},body:new URLSearchParams({{task:'upload',name:f.name,size:f.size,head:t.slice(0,80)}})}});s.textContent='Received '+f.name+' ('+f.size+' bytes)';}})}}
d.ondragover=e=>{{e.preventDefault();d.classList.add('over')}};d.ondragleave=()=>d.classList.remove('over');
d.ondrop=e=>{{e.preventDefault();d.classList.remove('over');const f=e.dataTransfer.files[0];if(f)send(f);else s.textContent='Drop had no file: types='+[...e.dataTransfer.types].join(',')}};
document.getElementById('pick').onchange=e=>send(e.target.files[0]);
</script>{LAYOUT}"""

NOTES = f"""<!doctype html><title>Notes A</title>{STYLE}
<h1>Window A</h1><label for=a>Value from Window B</label><input id=a>
<form method=post action=/submit><input type=hidden name=task value=switch><input type=hidden id=h name=value>
<button id=save onclick="document.getElementById('h').value=document.getElementById('a').value">Save</button></form>{LAYOUT}"""

PAGES = {"/form": FORM, "/paste": PASTE, "/upload": UPLOAD, "/notes": NOTES}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def send_html(self, body):
        data = body.encode()
        self.send_response(200)
        self.send_header("content-type", "text/html; charset=utf-8")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        page = PAGES.get(urlparse(self.path).path)
        if page is None:
            self.send_error(404)
            return
        self.send_html(page)

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0))).decode()
        if self.path == "/layout":
            with open(LOG / "layout.jsonl", "a") as f:
                f.write(json.dumps(json.loads(body)) + "\n")
            self.send_response(204)
            self.end_headers()
            return
        fields = {k: v[0] for k, v in parse_qs(body).items()}
        fields["_t"] = time.time()
        with open(LOG / "submissions.jsonl", "a") as f:
            f.write(json.dumps(fields) + "\n")
        rows = "".join(
            f"<tr><td>{k}</td><td id=v-{k}>{v}</td></tr>"
            for k, v in fields.items()
            if k != "_t"
        )
        self.send_html(
            f"<!doctype html><title>Submitted</title>{STYLE}<h1>Thank you</h1><table>{rows}</table>"
        )


LOG.mkdir(parents=True, exist_ok=True)
ThreadingHTTPServer(("127.0.0.1", 18950), Handler).serve_forever()
