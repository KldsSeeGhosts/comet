#!/usr/bin/env python3
"""Exercise the actual bundled CEF process, using only a loopback fixture."""
import argparse, base64, http.server, json, pathlib, queue, struct, subprocess, threading, time

HTML = b'''<!doctype html><meta charset="utf-8"><title>Noches browser fixture</title>
<style>body{font:20px sans-serif;background:#162b39;color:#fff;padding:40px}button,input,select{font:inherit;margin:12px;padding:12px}</style>
<h1>Shared Chromium page</h1><label>Name <input id="name" aria-label="Name"></label>
<button id="count" onclick="this.textContent='Clicked '+(++window.count)">Click me</button>
<select aria-label="Choice"><option value="a">Alpha</option><option value="b">Beta</option></select>
<a href="/next">Next page</a><div style="height:1400px">Scroll fixture</div>
<script>window.count=0;document.querySelector('input').addEventListener('input',e=>document.title=e.target.value);console.log('fixture ready')</script>'''
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = HTML if self.path != '/next' else b'<title>Next page</title><h1>Next page</h1>'
        self.send_response(200);self.send_header('Content-Type','text/html');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
    def log_message(self,*args): pass

def main():
    parser=argparse.ArgumentParser();parser.add_argument('binary');parser.add_argument('output');parser.add_argument('--chromium-arg',action='append',default=[]);args=parser.parse_args()
    output=pathlib.Path(args.output);output.mkdir(parents=True,exist_ok=True)
    server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=server.serve_forever,daemon=True).start()
    url=f'http://127.0.0.1:{server.server_port}/'
    log=(output/'chromium.log').open('w');process=subprocess.Popen([str(pathlib.Path(args.binary).resolve()),*args.chromium_arg],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=log)
    events=queue.Queue();last_frame={};sequence=0;history=[]
    def read_exact(n):
        result=bytearray()
        while len(result)<n:
            chunk=process.stdout.read(n-len(result))
            if not chunk:raise EOFError('Chromium pipe closed')
            result.extend(chunk)
        return bytes(result)
    def read():
        try:
            while True:
                kind,tab,length=struct.unpack('<cII',read_exact(9));body=read_exact(length)
                if kind==b'F':last_frame[tab]=body
                else:events.put((kind,tab,json.loads(body) if kind in (b'A',b'S',b'I',b'M') else body.decode()))
        except Exception as error:events.put(error)
    threading.Thread(target=read,daemon=True).start()
    def send(cmd,tab=1,**fields):
        data=json.dumps(dict(cmd=cmd,id=tab,**fields)).encode();process.stdin.write(struct.pack('<I',len(data))+data);process.stdin.flush()
    def wait(predicate):
        deadline=time.monotonic()+15
        while time.monotonic()<deadline:
            item=events.get(timeout=max(.01,deadline-time.monotonic()))
            if isinstance(item,Exception):raise item
            history.append(item)
            if predicate(*item):return item[2]
        raise TimeoutError('Browser response timed out')
    def cdp(method,params=None,tab=1):
        nonlocal sequence
        sequence+=1;ident=sequence
        send('cdp',tab,message=dict(id=ident,method=method,params=params or {}))
        message=wait(lambda kind,t,value:kind==b'A' and t==tab and value.get('id')==ident)
        if 'error' in message:raise AssertionError(message['error'])
        return message['result']
    def evaluate(expression,tab=1):
        result=cdp('Runtime.evaluate',dict(expression=expression,returnByValue=True,awaitPromise=True),tab)
        assert 'exceptionDetails' not in result,result
        return result['result'].get('value')
    def settle(expression,expected,timeout=3):
        # Native input reaches the renderer asynchronously; poll instead of sleeping.
        deadline=time.monotonic()+timeout
        while True:
            value=evaluate(expression)
            if value==expected or time.monotonic()>=deadline:return value
            time.sleep(.05)
    def click(rect):
        send('move',**rect);send('down',**rect,button=1);send('up',**rect,button=1)
    def loaded(tab,title):return wait(lambda k,t,v:k==b'S' and t==tab and not v['loading'] and v['title']==title)
    try:
        send('create');send('load',url=url);loaded(1,'Noches browser fixture')
        assert evaluate('location.href')==url
        page_script=(pathlib.Path(__file__).resolve().parents[1]/'crates/browser/src/page.js').read_text()
        def act(op):
            result=json.loads(evaluate('JSON.stringify(('+page_script+')('+json.dumps(op)+'))'))
            assert 'error' not in result,result
            return result
        snap=act(dict(kind='snapshot'));assert 'Shared Chromium page' in snap['text']
        name=next(e['reference'] for e in snap['elements'] if e['name']=='Name')
        button=next(e['reference'] for e in snap['elements'] if e['tag']=='button')
        act(dict(kind='fill',reference=name,text='Edited by agent'));assert evaluate('document.title')=='Edited by agent'
        act(dict(kind='click',reference=button));assert evaluate('window.count')==1
        old=button;act(dict(kind='snapshot'))
        stale=json.loads(evaluate('JSON.stringify(('+page_script+')('+json.dumps(dict(kind='click',reference=old))+'))'));assert 'error' in stale
        # Exercise native CEF input, independent of DOM-based agent actions.
        rect=evaluate("(()=>{const r=document.querySelector('input').getBoundingClientRect();return {x:r.x+10,y:r.y+10}})()")
        click(rect)
        assert settle("document.activeElement.id",'name')=='name'
        send('select-all');time.sleep(.1);send('commit',text='Native input');time.sleep(.1)
        assert evaluate("document.querySelector('input').value")=='Native input'
        send('key_down',key='BackSpace');send('key_up',key='BackSpace');time.sleep(.1)
        assert evaluate("document.querySelector('input').value")=='Native inpu'
        send('preedit',text='語');time.sleep(.1);send('commit',text='語');time.sleep(.1)
        assert evaluate("document.querySelector('input').value").endswith('語')
        rect=evaluate("(()=>{const r=document.querySelector('button').getBoundingClientRect();return {x:r.x+10,y:r.y+10}})()")
        click(rect)
        assert settle('window.count',2)==2
        evaluate("document.title='Edited by agent'")
        png=base64.b64decode(cdp('Page.captureScreenshot',dict(format='png'))['data']);assert png.startswith(b'\x89PNG');(output/'page.png').write_bytes(png)
        send('resize',width=800,height=500,scale=1)
        deadline=time.monotonic()+5
        while time.monotonic()<deadline and (1 not in last_frame or struct.unpack('<II',last_frame[1][:8])!=(800,500)):time.sleep(.05)
        assert struct.unpack('<II',last_frame[1][:8])==(800,500),'resize did not produce a matching frame'
        send('create',2);send('load',2,url=url+'next');loaded(2,'Next page')
        assert evaluate('document.title',1)=='Edited by agent';assert evaluate('document.title',2)=='Next page'
        send('load',url=url+'next');loaded(1,'Next page');send('back');loaded(1,'Edited by agent')
        assert evaluate('location.href')==url
        send('visible',value=0);assert evaluate('1+1')==2;send('visible',value=1)
        send('load',url='http://127.0.0.1:1/');error=wait(lambda k,t,v:k==b'S' and t==1 and v.get('error'));assert error['error']
        send('close',1);send('close',2)
        process.stdin.close();process.wait(timeout=10);assert process.returncode==0
        (output/'result.json').write_text(json.dumps(dict(passed=True,checks=['load','DOM snapshot','fill input events','native mouse keyboard and IME','click','stale references','PNG screenshot','resize frame','independent tabs','back history','hidden tab control','load error','clean shutdown']),indent=2))
        print((output/'result.json').read_text())
    except Exception:
        (output/'failure.json').write_text(json.dumps(dict(events=history[-20:],process=process.poll()),indent=2,default=lambda v:v.decode() if isinstance(v,bytes) else str(v)))
        raise
    finally:
        if process.poll() is None:process.kill();process.wait()
        server.shutdown();log.close()
if __name__=='__main__':main()
