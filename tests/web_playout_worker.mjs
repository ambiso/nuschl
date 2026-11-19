// SPDX-License-Identifier: EUPL-1.2
// Production codec worker and WASM on a deterministic clock, no devices.
import test from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import {readFile} from "node:fs/promises";
import {resolve,join} from "node:path";
import {pathToFileURL} from "node:url";
import * as protocol from "../web/app/src/audio-protocol.js";
import * as worklet from "../web/app/src/audio-worklet.js";

const directory=resolve(process.argv[2]||"");
assert.ok(process.argv[2],"usage: node tests/web_playout_worker.mjs CODEC_DIRECTORY");
const codec=await import(pathToFileURL(join(directory,"nuschl_audio_wasm.js")));
codec.initSync({module:await readFile(join(directory,"nuschl_audio_wasm_bg.wasm"))});
const source=(await readFile(new URL("../web/app/src/audio-codec.worker.js",import.meta.url),"utf8")).replace(/^import .*;\n/gm,"");
const {BlockRing,CaptureFrames,RingPlayout,CAPTURE_BLOCKS,PLAYOUT_BLOCKS,PLAYOUT_FRAME}=worklet;
const {FRAME,MAX_CAPTURE,MAX_STREAMS}=protocol;
const WIDTH=PLAYOUT_FRAME*2;

async function fixture() {
  let time=0;
  const queue=[],messages=[];
  const context=vm.createContext({...protocol,...worklet,VoiceEncoder:codec.VoiceEncoder,VoicePlayout:codec.VoicePlayout,
    Float32Array,Float64Array,Uint8Array,SharedArrayBuffer,performance:{now:()=>time},init:async()=>{},
    onmessage:null,postMessage:data=>messages.push(data),setInterval:()=>0,close:()=>{}});
  await new vm.Script(`(async()=>{${source}\n})()`).runInContext(context);
  const send=data=>context.onmessage({data});
  const port={postMessage(){},start(){},close(){}};
  function ring(blocks,samples,Pcm=Float32Array) {
    const buffer=new SharedArrayBuffer(BlockRing.bytes(blocks,samples,Pcm));
    const policy=new Int32Array(new SharedArrayBuffer(Int32Array.BYTES_PER_ELEMENT));
    return {buffer,policy,ring:new BlockRing(buffer,blocks,samples,Pcm)};
  }
  const rx=ring(PLAYOUT_BLOCKS,WIDTH,Float64Array),tx=ring(CAPTURE_BLOCKS,FRAME);
  const capture=new CaptureFrames(tx.ring,tx.policy),output=new RingPlayout(rx.ring,rx.policy);
  output.port={postMessage:data=>queue.push(data)};
  send({kind:"rings",playout:rx.buffer,capture:tx.buffer});
  send({kind:"playout-port",port});
  const authorize=({receive=1,epoch=1,playback=true,capture=false}={})=>{
    Atomics.store(rx.policy,0,playback?receive:0);
    Atomics.store(tx.policy,0,capture?epoch:0);
    send({kind:"configure",epoch,capture,bitrate:128000,receive_epoch:receive,playback});
  };
  const packet=(opus,sequence=0,{volume=.25,speaker=1,receive=1,job=1,end=false}={})=>
    send({kind:"decode",epoch:receive,job,speaker,sequence:String(sequence),opus,end,volume,arrival:time});
  const pump=()=>{
    assert.ok(queue.length<=1,"worklet posts at most one wake");
    if(queue.length)port.onmessage({data:queue.shift()});
  };
  const tick=({stall=false}={})=>{
    time+=10;
    const left=new Float32Array(PLAYOUT_FRAME);
    output.render(left);
    if(!stall)pump();
    return left;
  };
  return {send,authorize,packet,tick,pump,capture,messages,close:()=>send({kind:"close"})};
}

const tone=Float32Array.from({length:FRAME},(_,n)=>Math.sin(2*Math.PI*997*n/48000)*.2);
const encoder=new codec.VoiceEncoder(),packet=encoder.encode(tone);
encoder.free();
const audible=samples=>samples.some(value=>Math.abs(value)>.001);

test("production capture retains every frame across a 90 ms codec stall",async()=>{
  const f=await fixture();
  let acknowledged=0;
  try {
    f.authorize({capture:true});
    for(let tick=0;tick<50;tick++) {
      if(tick%2===0)f.capture.append(tone);
      f.tick({stall:tick>=20&&tick<29});
      for(const message of f.messages.filter(m=>m.kind==="encoded").slice(acknowledged)) {
        assert.ok(message.opus.length>0);
        f.send({kind:"encoded-ack"});
        acknowledged++;
      }
    }
    assert.deepEqual(f.messages.filter(m=>m.kind==="encoded").map(m=>m.frame),Array.from({length:25},(_,n)=>n));
  } finally {f.close();}
});

test("encode replies retain one global credit budget across TX epochs",async()=>{
  const f=await fixture(),encoded=()=>f.messages.filter(m=>m.kind==="encoded");
  try {
    f.authorize({capture:true});
    for(let frame=0;frame<MAX_CAPTURE*2;frame++){f.capture.append(tone);f.tick();}
    assert.equal(encoded().length,MAX_CAPTURE);
    f.authorize({capture:true,epoch:2});
    f.capture.append(tone);f.tick();assert.equal(encoded().length,MAX_CAPTURE);
    f.send({kind:"encoded-ack"});
    f.capture.append(tone);f.tick();
    assert.equal(encoded().length,MAX_CAPTURE+1);
    assert.equal(encoded().at(-1).epoch,2);
    assert.equal(encoded().at(-1).frame,1);
  } finally {f.close();}
});

test("valid playback crosses the actual codec and ring while mute leaves RX intact",async()=>{
  const f=await fixture();
  try {
    f.authorize();
    let heard=0;
    for(let tick=0;tick<40;tick++) {
      if(tick===20)f.authorize({epoch:2});
      if(tick%2===0)f.packet(packet,tick,{job:1+tick});
      if(audible(f.tick()))heard++;
    }
    assert.ok(heard>28,`heard only ${heard} of 40 blocks`);
    f.authorize({receive:2,epoch:3,playback:false});
    assert.ok(!audible(f.tick()));
    f.packet(packet,40,{job:99});
    assert.equal(f.messages.at(-1).kind,"packet-queued");
    assert.equal(f.messages.at(-1).epoch,1);
    assert.ok(f.messages.every(m=>!["decoded","playout-error","media-error"].includes(m.kind)));
  } finally {f.close();}
});

test("invalid Opus frames cannot consume the speaker admission budget",async()=>{
  const f=await fixture();
  try {
    f.authorize();
    for(let speaker=0;speaker<MAX_STREAMS;speaker++)f.packet(new Uint8Array([9,0]),0,{speaker});
    assert.equal(f.messages.filter(m=>m.kind==="media-error").length,MAX_STREAMS);
    for(let speaker=0;speaker<MAX_STREAMS;speaker++)f.packet(packet,0,{speaker});
    assert.equal(f.messages.filter(m=>m.kind==="packet-queued").length,MAX_STREAMS);
    f.packet(packet,0,{speaker:MAX_STREAMS});
    assert.match(f.messages.at(-1).message,/Too many active speakers/);
  } finally {f.close();}
});
