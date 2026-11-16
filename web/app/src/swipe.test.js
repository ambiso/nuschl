// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {SWIPE,startSwipe,moveSwipe,endSwipe} from "./swipe.js";

// A straight drag from (x,y) by (dx,dy), sampled every 16 ms over `ms`.
function drag(direction,{x=100,y=300,dx,dy=0,ms=200,steps=Math.max(2,Math.round(ms/16))}) {
  let swipe=startSwipe({x,y,t:0},direction);
  for(let i=1;i<=steps;i++)swipe=moveSwipe(swipe,{x:x+dx*i/steps,y:y+dy*i/steps,t:ms*i/steps});
  return swipe;
}

test("the axis is decided only after the slop, and a sideways drag must dominate",()=>{
  const start=startSwipe({x:0,y:0,t:0},1);
  assert.equal(moveSwipe(start,{x:6,y:2,t:16}).axis,"pending");
  assert.equal(moveSwipe(start,{x:12,y:3,t:16}).axis,"horizontal");
  // Diagonal (under 1.5:1) and vertical drags stay with scrolling.
  assert.equal(moveSwipe(start,{x:12,y:10,t:16}).axis,"rejected");
  assert.equal(moveSwipe(start,{x:1,y:14,t:16}).axis,"rejected");
  // The wrong direction rejects the swipe.
  assert.equal(moveSwipe(start,{x:-14,y:0,t:16}).axis,"rejected");
  assert.equal(moveSwipe(startSwipe({x:0,y:0,t:0},-1),{x:-14,y:0,t:16}).axis,"horizontal");
});

test("a decision is final: returning to the axis later does not revive it",()=>{
  let swipe=moveSwipe(startSwipe({x:0,y:0,t:0},1),{x:0,y:20,t:16});
  swipe=moveSwipe(swipe,{x:200,y:20,t:120});
  assert.equal(swipe.axis,"rejected");
  assert.equal(endSwipe(swipe),false);
  let horizontal=moveSwipe(startSwipe({x:0,y:0,t:0},1),{x:20,y:0,t:16});
  horizontal=moveSwipe(horizontal,{x:22,y:60,t:200});
  assert.equal(horizontal.axis,"horizontal");
});

test("a long press before moving is left to text selection",()=>{
  let swipe=startSwipe({x:0,y:0,t:0},1);
  swipe=moveSwipe(swipe,{x:2,y:0,t:SWIPE.hold+1});
  assert.equal(swipe.axis,"rejected");
  assert.equal(endSwipe(moveSwipe(swipe,{x:200,y:0,t:SWIPE.hold+200})),false);
});

test("a slow drag commits by distance, a short one does not",()=>{
  assert.equal(endSwipe(drag(1,{dx:SWIPE.distance+6,ms:900})),true);
  assert.equal(endSwipe(drag(1,{dx:SWIPE.distance-10,ms:900})),false);
  assert.equal(endSwipe(drag(-1,{dx:-(SWIPE.distance+6),ms:900})),true);
});

test("a swipe that drifts more vertically than sideways does not commit",()=>{
  let swipe=moveSwipe(startSwipe({x:0,y:0,t:0},1),{x:20,y:0,t:16});
  swipe=moveSwipe(swipe,{x:80,y:120,t:200});
  assert.equal(endSwipe(swipe),false);
});
