// Local iOS companion fixture. No files or agents are modified.
// node scripts/fixtures/noches-mobile-host.mjs
import { createRequire } from 'node:module';
import http from 'node:http';
const require = createRequire(new URL('../../edge/package.json', import.meta.url));
const { WebSocketServer } = require('ws');
const raw = process.argv.includes('--engine');
const port = Number(process.env.NOCHES_FIXTURE_PORT || (raw ? 28778 : 28777));
const profile = {id:'mobile-fixture',name:'Studio · fixture',endpoint:`ws://127.0.0.1:${port}`,token:'a'.repeat(64),deviceId:'fixture-mac'};
let commands = [];
let approvalResolved = false;
let working = true;
const queued = [];
const chat = {id:'fixture-chat',deviceId:profile.deviceId,title:'Build the mobile companion',archived:false,spaceId:'fixture-space',branch:'companion/mobile',cwd:'/tmp/noches-fixture',config:{harness:'mock',sandbox:'workspace-write',modelOptions:{}},createdAt:'2026-09-20T12:00:00Z',lastMessagePreview:'The companion is ready for a closer look.'};
const initialChat = {...chat};
const examples = [
  {...chat,id:'fixture-approval',title:'Review the authentication flow',branch:'fix/session-auth',lastMessagePreview:'Ready to run the migration. Allow this command?'},
  {...chat,id:'fixture-working',title:'Polish the desktop sidebar',branch:'design/sidebar',lastMessagePreview:'Refining spacing and the session status indicators.'},
  {...chat,id:'fixture-done',title:'Add keyboard shortcuts',branch:'feat/shortcuts',lastMessagePreview:'All checks passed. Changes are ready to review.'},
  {...chat,id:'fixture-archived',title:'Explore connection options',archived:true,branch:'research/remote',lastMessagePreview:'Tailscale keeps the host private across networks.'},
];
const messages = [{id:'reply',role:'assistant',parts:[{id:'text',kind:'text',text:'Your computer is connected.\n\nStart a session, send a message, or review a request from here.'}]}];
const host = http.createServer((req,res) => {
  res.setHeader('content-type','application/json');
  if(req.url === '/reset' && req.method === 'POST') {commands=[];approvalResolved=false;working=true;queued.length=0;for(const key of Object.keys(chat)) delete chat[key];Object.assign(chat,initialChat);messages.splice(1);return res.end('{}');}
  if(req.url === '/health') return res.end(JSON.stringify({fixture:true}));
  if(req.url === '/commands') return res.end(JSON.stringify(commands));
  res.statusCode=404; res.end('{}');
});
const server = new WebSocketServer({server:host,verifyClient:({req})=>raw || req.headers.authorization === `Bearer ${profile.token}`});
server.on('connection',socket=>{
  let seq=0,received=0,welcomed=raw;
  const watches=new Map();
  const send=frame=>socket.readyState===1 && (!raw || !frame.t) && socket.send(JSON.stringify(frame));
  const reply=frame=>{
    if(raw) {send(frame);return;}
    const payload=JSON.stringify(frame);
    // Split at JS character boundaries; surrogate pairs stay together.
    let chunk='';
    for(const char of payload) {
      if(Buffer.byteLength(chunk+char)>32768) {send({t:'data',seq:++seq,payload:chunk,end:false});chunk='';}
      chunk+=char;
    }
    send({t:'data',seq:++seq,payload:chunk,end:true});
  };
  const approval = {id:'approval-message',role:'assistant',parts:[{id:'approval-part',kind:'input',requestId:'approval-request',questions:[{id:'approve-command',header:'Run migration',question:'Allow the migration command on your computer?',options:['Allow','Deny']}],resolved:false}]};
  const snapshots={WatchChats:()=>[chat,...examples],WatchSpaces:()=>[{id:'fixture-space',deviceId:profile.deviceId,path:'/tmp/noches-fixture',name:'Noches'}],WatchSessions:()=>[{chatId:chat.id,status:'idle'},{chatId:'fixture-approval',status:approvalResolved?'idle':'awaitingInput'},{chatId:'fixture-working',status:working?'working':'idle'},{chatId:'fixture-done',status:'completed'}],WatchDocMessages:(params)=>({reset:params?.chatId==='fixture-approval' ? [{...approval,parts:[{...approval.parts[0],resolved:approvalResolved}]}] : messages}),WatchQueue:(params)=>({items:queued.filter(item=>item.chatId===params?.chatId)})};
  socket.on('message',bytes=>{
    try {
      const frame=raw ? {t:'data',seq:received+1,payload:bytes.toString(),end:true} : JSON.parse(bytes);
      if(frame.t==='hello') {
        if(frame.version!==1 || frame.resume || frame.cursor!==0) return socket.close();
        welcomed=true; return send({t:'welcome',version:1,resumed:false,received:0});
      }
      if(!welcomed) return socket.close();
      if(frame.t==='ping') return send({t:'pong'});
      if(frame.t==='ack') return;
      if(frame.t!=='data' || frame.end!==true || frame.seq!==++received) return socket.close();
      const rpc=JSON.parse(frame.payload);
      if(rpc.cancel) {watches.delete(rpc.id);return send({t:'ack',seq:frame.seq});}
      if(rpc.method==='FixtureDropMutation') {commands.push(rpc);socket.terminate();return;}
      send({t:'ack',seq:frame.seq});
      if(rpc.method==='EngineInfo') return reply({id:rpc.id,ok:{deviceId:profile.deviceId,workspaceScope:'local'}});
      if(rpc.method==='ListModels') return reply({id:rpc.id,ok:[{id:'fixture-model',label:'Fixture model',reasoningLevels:['low','high']}]});
      if(rpc.method==='ListHarnesses') return reply({id:rpc.id,ok:[{id:'mock',name:'Fixture agent',installed:true,enabled:true}]});
      if(snapshots[rpc.method]) {watches.set(rpc.id,{method:rpc.method,params:rpc.params});return reply({id:rpc.id,item:snapshots[rpc.method](rpc.params)});}
      if(rpc.method==='ListWorkspaceDirectory') {
        commands.push(rpc);
        const directory=rpc.params.directory || '';
        return reply({id:rpc.id,ok:{directory,entries:directory ? [{path:'Sources/Client.swift',name:'Client.swift',kind:'file'}] : [{path:'Sources',name:'Sources',kind:'directory'},{path:'README.md',name:'README.md',kind:'file'}],truncated:false}});
      }
      if(rpc.method==='ReadWorkspaceFile') {
        commands.push(rpc);
        return reply({id:rpc.id,ok:{path:rpc.params.path,text:'// Noches companion fixture\nstruct Client {\n    let connected = true\n}\n',encoding:'utf8',truncated:false}});
      }
      if(rpc.method==='GetCheckoutDiff') {
        commands.push(rpc);
        return reply({id:rpc.id,ok:{files:[{path:'Sources/Client.swift',additions:1,deletions:1}],additions:1,deletions:1,truncated:false,patch:'diff --git a/Sources/Client.swift b/Sources/Client.swift\n--- a/Sources/Client.swift\n+++ b/Sources/Client.swift\n@@ -1 +1 @@\n-let connected = false\n+let connected = true'}});
      }
      if(rpc.method==='BigReply') return reply({id:rpc.id,ok:{text:'🌙'.repeat(15000)}});
      if(rpc.method==='FixtureCommands') return reply({id:rpc.id,ok:commands});
      if(['QueueCommand','QueueMessage','Mutate'].includes(rpc.method)) {
        commands.push(rpc);
        if(rpc.method==='Mutate' && ['renameChat','setChatArchived'].includes(rpc.params.op)) {
          const target=[chat,...examples].find(c=>c.id===rpc.params.chatId);
          if(!target) return reply({id:rpc.id,err:'Unknown chat'});
          if(rpc.params.op==='renameChat') target.title=rpc.params.title;
          else target.archived=rpc.params.archived;
        }
        if(rpc.method==='Mutate' && rpc.params.op==='createChat') Object.assign(chat,{id:rpc.params.chatId,config:rpc.params.config,spaceId:rpc.params.spaceId,title:'New mobile session'});
        if(rpc.method==='QueueCommand' && rpc.params.command.kind==='run') {
          const text=rpc.params.command.request.prompt;
          messages.push({id:rpc.params.command.messageId,role:'user',parts:[{id:`p${commands.length}`,kind:'text',text}]});
          messages.push({id:`a${commands.length}`,role:'assistant',parts:[{id:`t${commands.length}`,kind:'text',text:'Received on the fixture host. No real agent was started.'}]});
        }
        if(rpc.method==='QueueMessage') queued.push({id:`queued-${commands.length}`,chatId:rpc.params.chatId,text:rpc.params.text});
        if(rpc.method==='QueueCommand' && rpc.params.command.kind==='respondInput' && rpc.params.command.requestId==='approval-request') approvalResolved=true;
        if(rpc.method==='QueueCommand' && rpc.params.command.kind==='interrupt' && rpc.params.chatId==='fixture-working') working=false;
        reply({id:rpc.id,ok:{commandId:`fixture-${commands.length}`}});
        for(const [id,{method,params}] of watches) reply({id,item:snapshots[method](params)});
        return;
      }
      reply({id:rpc.id,err:`Unknown fixture method: ${rpc.method}`});
    } catch {socket.close();}
  });
});
host.listen(port,'127.0.0.1',()=>console.log(`Fixture listening on 127.0.0.1:${port}\nnoches-connect:${Buffer.from(JSON.stringify(profile)).toString('base64url')}`));
