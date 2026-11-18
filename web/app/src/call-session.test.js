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
  return {window:{document,navigator:{wakeLock}},locks,setVisibility};
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
