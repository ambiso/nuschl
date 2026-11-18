// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {CallSession} from "./call-session.js";

// A page with a fake Wake Lock API that releases its lock on hide, as browsers do.
function page() {
  const document=new EventTarget();document.visibilityState="visible";
  const locks=[];
  const wakeLock={request:async type=>{
    assert.equal(type,"screen");
    if(document.visibilityState!=="visible")throw new DOMException("hidden","NotAllowedError");
    const lock=new EventTarget();lock.released=false;
    lock.release=async()=>{if(!lock.released){lock.released=true;lock.dispatchEvent(new Event("release"));}};
    locks.push(lock);return lock;
  }};
  const setVisibility=state=>{
    document.visibilityState=state;
    if(state==="hidden")for(const lock of locks)lock.release();
    document.dispatchEvent(new Event("visibilitychange"));
  };
  const handlers=new Map(),media=[];
  const mediaSession={metadata:null,playbackState:"none",microphone:[],
    setActionHandler:(action,handler)=>{if(action==="hangup")throw new TypeError("unsupported");handler?handlers.set(action,handler):handlers.delete(action);},
    setMicrophoneActive(active){this.microphone.push(active);}};
  class Audio {
    constructor(){this.attributes=new Map();this.playing=false;this.plays=0;media.push(this);}
    set src(value){this.attributes.set("src",value);} get src(){return this.attributes.get("src")||"";}
    removeAttribute(name){this.attributes.delete(name);}
    play(){this.plays++;this.playing=Boolean(this.src);return this.playing?Promise.resolve():Promise.reject(new Error("no source"));}
    pause(){this.playing=false;} load(){}
  }
  class MediaMetadata {constructor(init){Object.assign(this,init);}}
  const window=Object.assign(new EventTarget(),{document,navigator:{wakeLock,mediaSession},Audio,MediaMetadata,URL:{createObjectURL:()=>"blob:silence"}});
  return {window,
    locks,setVisibility,handlers,media,mediaSession};
}
const settle=()=>new Promise(resolve=>setTimeout(resolve,0));
const call={channel:"Lobby",server:"Home",muted:true};

test("joining requests a screen wake lock and leaving releases it",async()=>{
  const {window,locks}=page(),session=new CallSession({window});session.attach();
  session.update(null);await settle();
  assert.equal(locks.length,0);
  session.update(call);session.update({...call,muted:false});await settle();
  assert.equal(locks.length,1);
  session.update(null);
  assert.equal(locks[0].released,true);
  session.update(call);await settle();
  assert.equal(locks.length,2);
  session.detach();
  assert.equal(locks[1].released,true);
});

test("the lock is requested again each time the page becomes visible",async()=>{
  const {window,locks,setVisibility}=page(),session=new CallSession({window});session.attach();
  session.update(call);await settle();
  setVisibility("hidden");await settle();
  assert.equal(locks.length,1);
  setVisibility("visible");await settle();
  assert.equal(locks.length,2);assert.equal(locks[1].released,false);
  setVisibility("visible");await settle();
  assert.equal(locks.length,2);
  session.update(null);
  setVisibility("hidden");setVisibility("visible");await settle();
  assert.equal(locks.length,2);
});

test("a lock granted after leaving is released at once",async()=>{
  const {window,locks}=page(),session=new CallSession({window});
  session.update(call);session.update(null);await settle();
  assert.equal(locks.length,1);assert.equal(locks[0].released,true);
});

test("a hidden page or a browser without the API joins without a lock",async()=>{
  const {window,locks}=page();window.document.visibilityState="hidden";
  new CallSession({window}).update(call);await settle();
  assert.equal(locks.length,0);
  const bare=new CallSession({window:{document:new EventTarget(),navigator:{}}});
  bare.update(call);bare.update(null);
});

test("in voice the page is a playing media session with the channel, Mute and Hang up",async()=>{
  const {window,handlers,media,mediaSession}=page(),calls=[];
  const session=new CallSession({window,onMute:()=>calls.push("mute"),onHangUp:()=>calls.push("hang up")});
  session.update({channel:"Lobby",server:"Home",muted:false});
  assert.deepEqual({...mediaSession.metadata},{title:"Lobby",artist:"Home"});
  assert.equal(mediaSession.playbackState,"playing");
  assert.deepEqual(mediaSession.microphone,[true]);
  assert.deepEqual([...handlers.keys()].sort(),["pause","stop","togglemicrophone"]);
  assert.equal(media.length,1);assert.equal(media[0].playing,true);assert.equal(media[0].loop,true);
  handlers.get("togglemicrophone")();handlers.get("pause")();handlers.get("stop")();
  assert.deepEqual(calls,["mute","mute","hang up"]);
  session.update({channel:"Lobby",server:"Home",muted:true});
  assert.deepEqual(mediaSession.microphone,[true,false]);
  session.update({channel:"Games",server:"Home",muted:true});
  assert.equal(mediaSession.metadata.title,"Games");
  session.update(null);
  assert.equal(mediaSession.metadata,null);assert.equal(mediaSession.playbackState,"none");
  assert.equal(handlers.size,0);
  assert.equal(media[0].playing,false);assert.equal(media[0].src,"");
});

test("the notification never unmutes, also when the page resumes",async()=>{
  const {window,handlers,setVisibility}=page(),calls=[];
  const session=new CallSession({window,onMute:()=>calls.push("mute"),onHangUp:()=>calls.push("hang up")});session.attach();
  session.update(call);
  for(const action of ["togglemicrophone","pause"])handlers.get(action)();
  setVisibility("hidden");setVisibility("visible");await settle();
  assert.deepEqual(calls,[]);
  assert.equal(session.call.muted,true);
});

test("the Join gesture primes the media element without playing outside a call",()=>{
  const {window,media}=page(),session=new CallSession({window});
  session.prime();
  assert.equal(media[0].plays,1);assert.equal(media[0].playing,false);assert.equal(media[0].src,"");
  session.update(call);
  assert.equal(media.length,1);assert.equal(media[0].playing,true);
});

test("a frozen or hidden-for-good page ends the call once",async()=>{
  for(const [target,type] of [["document","freeze"],["window","pagehide"]]) {
    const {window,locks,media,mediaSession}=page(),ends=[];
    const session=new CallSession({window,onEnd:()=>ends.push(session.call)});session.attach();
    (target==="document"?window.document:window).dispatchEvent(new Event(type));
    assert.deepEqual(ends,[]);
    session.update(call);await settle();
    (target==="document"?window.document:window).dispatchEvent(new Event(type));
    window.document.dispatchEvent(new Event("freeze"));window.dispatchEvent(new Event("pagehide"));
    assert.deepEqual(ends,[null]);
    assert.equal(locks[0].released,true);assert.equal(media[0].playing,false);assert.equal(mediaSession.playbackState,"none");
    session.detach();window.dispatchEvent(new Event("pagehide"));
    assert.equal(ends.length,1);
  }
});
