// SPDX-License-Identifier: EUPL-1.2
// The packaged web app connects its saved servers when the page loads, on a
// Mumble server with a channel named Lobby: when the page is opened again and
// reloaded while connected, the working server connects without a click and
// rejoins Lobby, the channel last joined, with voice and the microphone closed;
// a server that refuses ends disconnected with its notice. Run by nix/vm-web.nix.
//   node tests/web_autoconnect.browser.mjs APP_URL SERVER_HOST SERVER_PORT REFUSING_PORT
import assert from "node:assert/strict";
import {pathToFileURL} from "node:url";
const [app,host,port,refusing]=process.argv.slice(2);
assert.ok(app&&host&&port&&refusing,"app URL, server host, port and a refusing port required");
const {chromium}=await import(pathToFileURL(process.env.NUSCHL_PLAYWRIGHT).href);
const browser=await chromium.launch({headless:true,executablePath:process.env.NUSCHL_CHROMIUM,
  args:[`--unsafely-treat-insecure-origin-as-secure=${new URL(app).origin}`]});
try {
  const context=await browser.newContext({viewport:{width:1200,height:800}});
  // A loaded CI host steals most of the VM's time: allow minutes per step.
  context.setDefaultTimeout(180000);
  const page=await context.newPage(),errors=[];
  page.on("pageerror",error=>errors.push(error.message));
  await page.addInitScript(()=>{
    const proof=window.__proof={servers:[],voice:null,commands:[],attempts:{},microphoneCalls:0};
    // Playback goes to no device; a microphone request is counted and refused.
    const Context=window.AudioContext;
    if(Context)window.AudioContext=class extends Context{constructor(options){super({...options,sinkId:{type:"none"}});}};
    if(navigator.mediaDevices)navigator.mediaDevices.getUserMedia=()=>{proof.microphoneCalls++;return Promise.reject(new DOMException("refused","NotAllowedError"));};
    const Worker=window.Worker;
    window.Worker=class extends Worker{
      constructor(url,options){super(url,options);this.addEventListener("message",({data})=>{if(data.kind!=="state")return;
        // Count each server's entries into "connecting".
        for(const server of data.servers)if(server.status==="connecting"&&proof.servers.find(old=>old.id===server.id)?.status!=="connecting")proof.attempts[server.port]=(proof.attempts[server.port]||0)+1;
        Object.assign(proof,{servers:data.servers,voice:data.voice});});}
      postMessage(message,...rest){if(message?.method)proof.commands.push(message.method);return super.postMessage(message,...rest);}
    };
  });
  const status=port=>page.evaluate(port=>__proof.servers.find(server=>server.port===Number(port))?.status,port);
  const connected=port=>page.waitForFunction(port=>__proof.servers.find(server=>server.port===Number(port))?.status==="connected",port)
    .catch(async error=>{throw new Error(`${error.message} ${JSON.stringify(await page.evaluate(()=>__proof.servers.map(({port,status,notice})=>({port,status,notice}))))}`);});
  const disconnected=port=>page.waitForFunction(port=>__proof.servers.find(server=>server.port===Number(port))?.status==="disconnected",port);
  // Save both servers through server links.
  await page.goto(`${app}#s=${host}:${port}&u=autoconnect`);
  await page.getByRole("button",{name:"Trust this certificate",exact:true}).click();
  await connected(port);
  // Join Lobby's voice once, with the Join voice control.
  await page.getByRole("button",{name:"Lobby",exact:true}).click();
  await page.getByRole("button",{name:"Join voice",exact:true}).click();
  await page.waitForFunction(()=>__proof.voice?.joined);
  await page.waitForTimeout(1000);
  await page.goto("about:blank");
  await page.goto(`${app}#s=${host}:${refusing}&u=autoconnect`);
  await disconnected(refusing);
  // Open the app without a link, then reload it while connected: no connect click.
  for(const step of ["open","reload"]){
    if(step==="open"){await page.goto("about:blank");await page.goto(app);}else await page.reload();
    await connected(port);await disconnected(refusing);
    await page.waitForTimeout(3000);
    assert.equal(await status(refusing),"disconnected");
    assert.equal(await status(port),"connected");
    const proof=await page.evaluate(()=>({commands:__proof.commands,attempts:__proof.attempts,voice:__proof.voice,microphoneCalls:__proof.microphoneCalls}));
    assert.ok(proof.attempts[refusing]<=1,`${step}: the refusing server was tried ${proof.attempts[refusing]} times`);
    assert.deepEqual(proof.commands.filter(method=>method.startsWith("connect")),[],`${step}: the page sent a connect command`);
    assert.ok(!proof.voice?.joined,`${step}: connecting joined voice`);
    await page.waitForFunction(port=>{const server=__proof.servers.find(server=>server.port===Number(port));
      return server.users.find(user=>user.session===server.own_session)?.channel===server.channels.find(channel=>channel.name==="Lobby").id;},port);
    assert.equal(proof.microphoneCalls,0,`${step}: connecting requested the microphone`);
  }
  assert.deepEqual(errors,[]);
  console.log("auto-connect: saved servers connect on open and reload and rejoin Lobby; the refusing one is tried once; microphone closed");
} finally { await browser.close(); }
