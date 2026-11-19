// SPDX-License-Identifier: EUPL-1.2
// Actual processor and shared buffers, with a deterministic clock and no devices.
import test from "node:test";
import assert from "node:assert/strict";

const classes=new Map();
globalThis.sampleRate=48000;
globalThis.AudioWorkletProcessor=class {
  constructor(){this.port={sent:[],postMessage(value){this.sent.push(value);}};}
};
globalThis.registerProcessor=(name,type)=>classes.set(name,type);
const {BlockRing,mixSpeaker,PLAYOUT_FRAME,PLAYOUT_BLOCKS,Lead}=await import("../web/app/src/audio-worklet.js");
const Processor=classes.get("nuschl-playback");
const SAMPLES=PLAYOUT_FRAME*2;

function fixture({epoch=1}={}) {
  const buffer=new SharedArrayBuffer(BlockRing.bytes(PLAYOUT_BLOCKS,SAMPLES,Float64Array));
  const policy=new Int32Array(new SharedArrayBuffer(Int32Array.BYTES_PER_ELEMENT));
  Atomics.store(policy,0,epoch);
  const producer=new BlockRing(buffer,PLAYOUT_BLOCKS,SAMPLES,Float64Array);
  const processor=new Processor({processorOptions:{buffer,policy}});
  const port={sent:[],closed:false,postMessage(value){this.sent.push(value);},start(){},close(){this.closed=true;}};
  processor.port.onmessage({data:{kind:"wake-port",port}});
  const configure=epoch=>Atomics.store(policy,0,epoch);
  const fill=(value=.25,tag=Atomics.load(policy,0))=>producer.push(tag,0,new Float64Array(SAMPLES).fill(value));
  const tick=(frames=128,mono=false)=>{
    const left=new Float32Array(frames),right=mono?null:new Float32Array(frames);
    processor.process([],[[left,...(right?[right]:[])]]);return {left,right};
  };
  return {producer,processor,port,configure,fill,tick};
}
const silence=out=>assert.ok(out.left.every(value=>value===0)&&(!out.right||out.right.every(value=>value===0)));

test("ring wraps without overwriting unread blocks",()=>{
  const f=fixture();
  let produced=0,consumed=0;
  for(let cycle=0;cycle<500;cycle++) {
    while(f.fill((produced+1)/65536))produced++;
    assert.equal(f.producer.size(),PLAYOUT_BLOCKS);
    const count=1+cycle%PLAYOUT_BLOCKS;
    const {left,right}=f.tick(count*PLAYOUT_FRAME);
    for(let i=0;i<left.length;i++)assert.equal(left[i],(consumed+1+Math.floor(i/PLAYOUT_FRAME))/65536);
    assert.deepEqual(left,right);
    assert.equal(f.producer.size(),PLAYOUT_BLOCKS-count);
    consumed+=count;
  }
});

test("revoke drops a partially rendered old block and keeps the next epoch for playback",()=>{
  const f=fixture();
  assert.ok(f.fill(.25,1));
  assert.ok(f.tick().left.every(value=>value===.25));
  f.configure(0);
  silence(f.tick());
  f.configure(2);
  assert.ok(f.fill(.5,2));
  const out=f.tick(PLAYOUT_FRAME);
  assert.ok(out.left.every(value=>value===.5));
  assert.deepEqual(out.left,out.right);
});

test("future PCM waits while stale PCM is retired across several route changes",()=>{
  const f=fixture({epoch:4});
  for(const epoch of [1,2,3,5])assert.ok(f.fill(epoch/16,epoch));
  const ring=f.processor.playout.ring;
  const head=ring.head.bind(ring);
  ring.head=()=>{
    const slot=head();
    if(ring.tags[slot*2]===5)f.configure(5);
    return slot;
  };
  silence(f.tick());
  assert.equal(f.producer.size(),1);
  assert.ok(f.tick(PLAYOUT_FRAME).left.every(value=>value===5/16));
  assert.equal(f.producer.size(),0);
});

test("an empty ring emits silence and keeps at most one pending wake during a stall",()=>{
  const f=fixture();
  for(let i=0;i<2000;i++)silence(f.tick());
  assert.equal(f.port.sent.length,1);
  assert.ok(f.fill());
  assert.ok(f.tick().left.every(value=>value===.25));
  assert.equal(f.port.sent.length,1);
  f.producer.clearWake();
  f.tick(PLAYOUT_FRAME);
  assert.equal(f.port.sent.length,2);
});

test("wide production mix preserves headroom, folds mono before clipping, and silences non-finite samples",()=>{
  const f=fixture(),mix=new Float64Array(SAMPLES);
  const max=3.4028234663852886e38;
  for(let i=0;i<16;i++)mixSpeaker(mix,new Float32Array(SAMPLES).fill(i%2?max:-max),4);
  assert.ok(f.producer.push(1,0,mix));
  silence(f.tick(PLAYOUT_FRAME));
  const samples=Float32Array.from({length:SAMPLES},(_,i)=>i%2===0?3:-2);
  mix.fill(0);
  mixSpeaker(mix,samples,.5);
  mixSpeaker(mix,Float32Array.from({length:SAMPLES},(_,i)=>i%2===0?-.25:.5),1);
  assert.ok(f.producer.push(1,0,mix));
  const stereo=f.tick(128);
  assert.ok(stereo.left.every(value=>value===1));
  assert.ok(stereo.right.every(value=>value===-.5));
  assert.ok(f.tick(128,true).left.every(value=>value===.375));
  f.tick(PLAYOUT_FRAME-256);
  for(const value of [NaN,Infinity,-Infinity]) {
    assert.ok(f.fill(value));
    silence(f.tick(PLAYOUT_FRAME));
  }
});

test("variable output quanta allocate no PCM scratch",()=>{
  const f=fixture();
  assert.ok(f.fill());
  const outputs=[1,127,480,960,4096].map(size=>[new Float32Array(size),new Float32Array(size)]);
  const F32=globalThis.Float32Array,F64=globalThis.Float64Array;
  globalThis.Float32Array=new Proxy(F32,{construct(){throw Error("callback allocated f32");}});
  globalThis.Float64Array=new Proxy(F64,{construct(){throw Error("callback allocated f64");}});
  try {for(const output of outputs)f.processor.process([],[output]);}
  finally {globalThis.Float32Array=F32;globalThis.Float64Array=F64;}
  assert.ok(f.port.sent.length<=1);
});

test("closing the worklet port retires its wake channel",()=>{
  const f=fixture();
  f.processor.port.onmessage({data:{kind:"close"}});
  assert.equal(f.port.closed,true);
  f.tick(PLAYOUT_FRAME);
  assert.equal(f.port.sent.length,0);
});

test("the worker lead covers recurring 90 ms stalls and relaxes after clean playback",()=>{
  const f=fixture(),lead=new Lead();
  let lastTick=f.producer.ticks(),wakes=0;
  function refill(now) {
    f.producer.clearWake();
    const current=f.producer.ticks();
    lead.observe(current-lastTick,now);lastTick=current;
    while(f.producer.size()<lead.blocks)assert.ok(f.fill());
  }
  refill(0);
  for(let callback=1;callback<=6000;callback++) {
    const offset=callback%700;
    const out=f.tick(PLAYOUT_FRAME);
    if(callback>=710)assert.ok(out.left.every(value=>value===.25),`gap at ${callback*10} ms`);
    if((offset>=9||callback<700)&&f.port.sent.length>wakes) {
      wakes=f.port.sent.length;
      refill(callback*10);
    }
  }
  assert.ok(lead.blocks>=9&&lead.blocks<=PLAYOUT_BLOCKS);
  for(let now=60010;now<=240000;now+=10)lead.observe(1,now);
  assert.equal(lead.blocks,3);
});
