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
const chat = {id:'fixture-chat',deviceId:profile.deviceId,title:'Build the mobile companion',archived:false,cwd:'/tmp/noches-fixture',config:{harness:'mock',sandbox:'workspace-write',modelOptions:{}},createdAt:'2026-09-20T12:00:00Z',lastMessagePreview:'Your host is connected.'};
const initialChat = {...chat};
const messages = [{id:'reply',role:'assistant',parts:[{id:'text',kind:'text',text:'Your computer is connected.\n\nStart a session, send a message, or review a request from here.'}]}];
const host = http.createServer((req,res) => {
  res.setHeader('content-type','application/json');
  if(req.url === '/reset' && req.method === 'POST') {commands=[];for(const key of Object.keys(chat)) delete chat[key];Object.assign(chat,initialChat);messages.splice(1);return res.end('{}');}
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
  const snapshots={WatchChats:()=>[chat],WatchSpaces:()=>[{id:'fixture-space',deviceId:profile.deviceId,path:'/tmp/noches-fixture',name:'Noches'}],WatchSessions:()=>[{chatId:chat.id,deviceId:profile.deviceId,status:'idle',updatedAt:'2026-09-20T12:00:00Z'}],WatchDocMessages:()=>({reset:messages}),WatchQueue:()=>({items:[]})};
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
      if(rpc.method==='ListHarnesses') return reply({id:rpc.id,ok:[{id:'mock',name:'Fixture agent',installed:true,enabled:true}]});
      if(snapshots[rpc.method]) {watches.set(rpc.id,rpc.method);return reply({id:rpc.id,item:snapshots[rpc.method]()});}
      if(rpc.method==='BigReply') return reply({id:rpc.id,ok:{text:'🌙'.repeat(15000)}});
      if(rpc.method==='FixtureCommands') return reply({id:rpc.id,ok:commands});
      if(['QueueCommand','QueueMessage','Mutate'].includes(rpc.method)) {
        commands.push(rpc);
        if(rpc.method==='Mutate' && rpc.params.op==='createChat') Object.assign(chat,{id:rpc.params.chatId,config:rpc.params.config,spaceId:rpc.params.spaceId,title:'New mobile session'});
        if(rpc.method==='QueueCommand' && rpc.params.command.kind==='run') {
          const text=rpc.params.command.request.prompt;
          messages.push({id:rpc.params.command.messageId,role:'user',parts:[{id:`p${commands.length}`,kind:'text',text}]});
          messages.push({id:`a${commands.length}`,role:'assistant',parts:[{id:`t${commands.length}`,kind:'text',text:'Received on the fixture host. No real agent was started.'}]});
        }
        reply({id:rpc.id,ok:{commandId:`fixture-${commands.length}`}});
        for(const [id,method] of watches) reply({id,item:snapshots[method]()});
        return;
      }
      reply({id:rpc.id,err:`Unknown fixture method: ${rpc.method}`});
    } catch {socket.close();}
  });
});
host.listen(port,'127.0.0.1',()=>console.log(`Fixture listening on 127.0.0.1:${port}\nnoches-connect:${Buffer.from(JSON.stringify(profile)).toString('base64url')}`));
