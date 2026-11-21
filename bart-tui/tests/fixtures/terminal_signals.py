import os,pathlib,json,subprocess,time,shlex,http.server,threading,tempfile,sys
binary=sys.argv[1]
owned=tempfile.TemporaryDirectory(prefix='bart-terminal-signals-');ROOT=pathlib.Path(owned.name)
base=ROOT/'fixture';cwd=base/'project';cwd.mkdir(parents=True);conf=base/'config/bart';conf.mkdir(parents=True)
marker='bart-fixture-job-'+str(os.getpid())
class Server(http.server.BaseHTTPRequestHandler):
 def log_message(self,*a):pass
 def do_GET(self):
  b=b'{"data":[]}';self.send_response(200);self.send_header('Content-Length',str(len(b)));self.end_headers();self.wfile.write(b)
 def do_POST(self):
  messages=json.loads(self.rfile.read(int(self.headers['Content-Length'])))['messages']
  if any(m.get('role')=='tool' for m in messages):
   delta,reason={'content':'JOB_STARTED'},'stop'
  else:
   code=f'bg_run(["bash","-c","exec -a {marker} sleep 300"])'
   delta,reason={'tool_calls':[{'index':0,'id':'job','type':'function','function':{'name':'python','arguments':json.dumps({'code':code})}}]},'tool_calls'
  body=''.join('data: '+json.dumps({'choices':[{'index':0,'delta':d,**x}]})+'\n\n' for d,x in [(delta,{}),({},{'finish_reason':reason})])+'data: [DONE]\n\n'
  self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers();self.wfile.write(body.encode())
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Server);threading.Thread(target=server.serve_forever,daemon=True).start()
(conf/'providers.json').write_text(json.dumps({'fixture':{'protocol':'chat','url':f'http://127.0.0.1:{server.server_port}/v1','key':False}}))
(conf/'settings.json').write_text(json.dumps({'models': {'fixture': {'fixture': {'context_window': 100000}}}}))
sock=str(ROOT/'tmux.sock');pidfile=ROOT/'pid';status=ROOT/'status'
env=f'HOME={shlex.quote(str(base))} XDG_CONFIG_HOME={shlex.quote(str(base/"config"))} XDG_STATE_HOME={shlex.quote(str(base/"state"))} TERM=xterm-256color'
bart=f'env {env} {shlex.quote(binary)} --provider fixture --model fixture'
def tm(*a):return subprocess.check_output(['tmux','-S',sock,*map(str,a)],text=True)
def pane():return tm('capture-pane','-p','-t','p:0.0')
def modes():return tm('display-message','-p','-t','p:0.0','#{alternate_on}#{mouse_any_flag}#{mouse_sgr_flag}').strip()
def until(pred,what,limit=10):
 end=time.monotonic()+limit
 while time.monotonic()<end:
  if pred():return
  time.sleep(.05)
 raise AssertionError(what+'\n'+pane())
def start(prefix=''):
 inner=f'echo $$ > {pidfile}; exec {bart}'
 cmd=f'cd {shlex.quote(str(cwd))}; {prefix}bash --noprofile --norc -c {shlex.quote(inner)}; echo $? > {status}; read unused'
 for f in (pidfile,status):f.unlink(missing_ok=True)
 tm('new-session','-d','-s','p','-x','100','-y','30','-c',str(cwd),'bash --noprofile --norc -c '+shlex.quote(cmd))
 until(lambda:'ready' in pane(),'start');return int(pidfile.read_text())
def alive(pid):
 try:os.kill(pid,0)
 except ProcessLookupError:return False
 return True
def kill_server():
 # tmux keeps its socket file; a new session started before the old server exits fails.
 server=int(tm('display-message','-p','#{pid}'))
 subprocess.run(['tmux','-S',sock,'kill-server'],capture_output=True)
 end=time.monotonic()+5
 while alive(server) and time.monotonic()<end:time.sleep(.02)
def jobs():return subprocess.run(['pgrep','-f',marker],capture_output=True).returncode==0
try:
 pid=start();tm('send-keys','-t','p:0.0','-l','go');tm('send-keys','-t','p:0.0','Enter')
 until(lambda:'JOB_STARTED' in pane() and jobs(),'job started')
 os.kill(pid,15);until(status.exists,'exit after SIGTERM')
 assert status.read_text().strip()=='143',status.read_text()
 assert modes()=='000',modes()
 until(lambda:not jobs(),'job reaped',5)
 kill_server()
 pid=start('trap "" HUP; ');kill_server()
 until(lambda:not alive(pid),'exit after terminal loss',5)
 shell=f'cd {shlex.quote(str(cwd))}; PS1="PROMPT$ " exec bash --noprofile --norc -i'
 tm('new-session','-d','-s','p','-x','100','-y','30','-c',str(cwd),'bash --noprofile --norc -c '+shlex.quote(shell))
 until(lambda:'PROMPT$' in pane(),'shell')
 tm('send-keys','-t','p:0.0','-l',bart);tm('send-keys','-t','p:0.0','Enter');until(lambda:'ready' in pane(),'start in shell')
 tm('send-keys','-t','p:0.0','-l','DRAFT');until(lambda:'DRAFT' in pane(),'draft')
 tm('send-keys','-t','p:0.0','C-z');until(lambda:'Stopped' in pane(),'stopped')
 assert modes()=='000',modes()
 tm('send-keys','-t','p:0.0','-l','fg');tm('send-keys','-t','p:0.0','Enter')
 until(lambda:'DRAFT' in pane() and 'ready' in pane() and modes()=='111','redrawn after fg')
 tm('send-keys','-t','p:0.0','-l','MORE');until(lambda:'DRAFTMORE' in pane(),'raw input after fg')
 tm('send-keys','-t','p:0.0','C-c');tm('send-keys','-t','p:0.0','C-c');until(lambda:'PROMPT$' in pane() and modes()=='000','quit after fg')
 print('ok')
finally:
 subprocess.run(['tmux','-S',sock,'kill-server'],capture_output=True);subprocess.run(['pkill','-f',marker],capture_output=True);server.shutdown()
