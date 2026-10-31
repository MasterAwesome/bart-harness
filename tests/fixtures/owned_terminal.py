import pathlib,json,hashlib,subprocess,time,fcntl,termios,struct,shlex,http.server,threading,tempfile,sys
temporary=tempfile.TemporaryDirectory(prefix='bart-owned-terminal-');ROOT=pathlib.Path(temporary.name).resolve()
base=ROOT/'fixture';cwd=base/'project';cwd.mkdir(parents=True,exist_ok=True)
conf=base/'config/bart';conf.mkdir(parents=True,exist_ok=True)
release=threading.Event();started=threading.Event()
class Server(http.server.BaseHTTPRequestHandler):
 def log_message(self,*a):pass
 def do_GET(self):
  b=b'{"data":[]}';self.send_response(200);self.send_header('Content-Length',str(len(b)));self.end_headers();self.wfile.write(b)
 def do_POST(self):
  self.rfile.read(int(self.headers['Content-Length']));self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers()
  def emit(delta,**extra):
   self.wfile.write(('data: '+json.dumps({'choices':[{'index':0,'delta':delta,**extra}]})+'\n\n').encode());self.wfile.flush()
  body='\n\n'.join(f'LIVE_{i:03} '+('word '*14) for i in range(30))
  body=body.replace('LIVE_028', '| '+('header'*100)+' | value |\n| --- | --- |\n| item | done |\n\n---\n\nLIVE_028')
  emit({'content':body+'\n\nUnicode: 界 é 🙂\n\n'});started.set();assert release.wait(60)
  emit({'content':'LIVE_DONE'});emit({},finish_reason='stop');self.wfile.write(b'data: [DONE]\n\n');self.wfile.flush()
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Server);threading.Thread(target=server.serve_forever,daemon=True).start()
(conf/'providers.json').write_text(json.dumps({'fixture':{'protocol':'chat','url':f'http://127.0.0.1:{server.server_port}/v1','key':False}}))
(conf/'settings.json').write_text(json.dumps({'models': {'fixture': {'fixture': {'context_window': 100000}}}}))
sid='1-owned';project=cwd.name+'-'+hashlib.sha256(str(cwd).encode()).hexdigest()[:8]
folder=base/'state/bart/sessions'/project/sid;folder.mkdir(parents=True,exist_ok=True)
rows=[{'event':'session','data':{'version':1,'cwd':str(cwd),'provider':'fixture','model':'fixture','effort':None,'project':''},'time_ms':1},{'event':'messages','data':[{'role':'user','content':'OWNED_USER'},{'role':'assistant','content':{'protocol':'chat','provider':'fixture','model':'fixture','parts':[{'content':{'text':'\n\n'.join(f'COMMIT_{i:03} '+('word '*14) for i in range(50))},'replay':None}],'usage':None,'refused':False}}],'time_ms':2}]
(folder/'session.jsonl').write_text(''.join(json.dumps(r)+'\n' for r in rows))
sock=str(ROOT/'tmux.sock');binary=pathlib.Path(sys.argv[1])
def tm(*a):return subprocess.check_output(['tmux','-S',sock,*map(str,a)],text=True)
def capture(label,main=False):
 text=tm('capture-pane','-p',*(['-a']if main else []),'-S','-10000','-t','probe:0.0');(ROOT/(label+'.txt')).write_text(text);return text
def until(pred):
 end=time.monotonic()+10
 while time.monotonic()<end:
  text=tm('capture-pane','-p','-t','probe:0.0')
  if pred(text):return text
  time.sleep(.03)
 raise AssertionError(text)
def key(*keys):tm('send-keys','-t','probe:0.0',*keys)
def safe_main(label):
 text=capture(label,True);assert 'SHELL_OK' in text and not any(x in text for x in ['DRAFT_SENTINEL','COMMIT_','LIVE_']),text
cmd='cd '+shlex.quote(str(cwd))+'; printf "SHELL_OK\n"; '+f'HOME={shlex.quote(str(base))} XDG_CONFIG_HOME={shlex.quote(str(base/"config"))} XDG_STATE_HOME={shlex.quote(str(base/"state"))} COLORTERM= TERM=xterm-256color '+shlex.quote(str(binary))+' --session '+sid+'; printf "EXIT_SENTINEL\n"; read unused'
results=[]
try:
 tm('new-session','-d','-s','probe','-x','100','-y','30','bash --noprofile --norc -c '+shlex.quote(cmd));until(lambda s:'ready' in s and 'COMMIT_049' in s);safe_main('startup-main')
 key('-l','go');key('Enter');assert started.wait(8);live=until(lambda s:'LIVE_028' in s)
 assert sum(bool(line.strip()) and set(line.strip()) <= {'─'} for line in live.splitlines()) <= 3,live
 key('-l','DRAFT_SENTINEL');until(lambda s:'DRAFT_SENTINEL' in s)
 key('PPage');inspecting=until(lambda s:'history' in s);first=next((l for l in inspecting.splitlines() if 'LIVE_' in l or 'COMMIT_' in l),'')
 capture('inspecting');safe_main('inspecting-main')
 for label,w,h in [('wide',160,40),('width-shrink',70,30),('standard',80,24),('compact',44,20),('narrow',24,14),('width-restore',100,30),('height-shrink',100,18),('height-restore',100,30),('extreme',12,12),('restored',100,30)]:
  tm('resize-window','-t','probe:0','-x',w,'-y',h)
  time.sleep(.35)
  tty=tm('display-message','-p','-t','probe:0.0','#{pane_tty}').strip()
  deadline=time.monotonic()+3
  while True:
   with open(tty,'rb',buffering=0)as f:size=struct.unpack('HHHH',fcntl.ioctl(f,termios.TIOCGWINSZ,b'\0'*8))
   if size[:2]==(h,w):break
   assert time.monotonic()<deadline,size
   time.sleep(.03)
  text=capture(label);safe_main(label+'-main');results.append({'case':label,'size':[h,w],'main_screen_preserved':True})
  assert 'DRAFT' in text or w==12,text
 key('C-End');until(lambda s:'LIVE_028' in s and 'DRAFT_SENTINEL' in s)
 key('C-o');until(lambda s:'No details yet' in s);capture('details');safe_main('details-main');key('Escape');until(lambda s:'DRAFT_SENTINEL' in s)
 release.set();until(lambda s:'LIVE_DONE' in s and 'ready' in s)
 for w,h in [(160,40),(100,30),(80,24),(44,20),(24,14),(12,12),(100,30)]:
  tm('resize-window','-t','probe:0','-x',w,'-y',h);time.sleep(.35)
  text=capture('idle-'+str(w));safe_main('idle-'+str(w)+'-main')
  assert 'ready' in text,text
  assert 'DRAFT' in text or w==12,text
  results.append({'case':'idle','size':[h,w],'ready_visible':True,'main_screen_preserved':True})
 seen=set()
 for i in range(25):
  text=capture('page-'+str(i))
  import re
  seen.update(re.findall(r'COMMIT_\d{3}',text));key('PPage');time.sleep(.03)
 assert seen=={f'COMMIT_{i:03}'for i in range(50)},sorted({f'COMMIT_{i:03}'for i in range(50)}-seen)
 key('C-End');until(lambda s:'LIVE_DONE' in s)
 key('C-u');key('-l','/settings');key('Enter');until(lambda s:'Permissions' in s);capture('settings');safe_main('settings-main');key('Escape')
 until(lambda s:'ready' in s);key('C-c');key('C-c');final=until(lambda s:'EXIT_SENTINEL' in s)
 assert 'SHELL_OK' in final and 'DRAFT_SENTINEL' not in final and 'COMMIT_' not in final,final
 assert 'bart --resume' not in final and 'To resume' not in final,final
 assert f'$ bart --session {sid}' in final,final
 results.append({'case':'exit','shell_history_preserved':True,'draft_not_archived':True,'all_50_markers_browsable':True})
 (ROOT/'results.json').write_text(json.dumps(results,indent=2));print(json.dumps(results,indent=2))
finally:
 release.set();server.shutdown();server.server_close();subprocess.run(['tmux','-S',sock,'kill-server'],capture_output=True)
