// SPDX-License-Identifier: EUPL-1.2
// No session, credentials, endpoint or storage capability crosses this boundary.
// Codec/WASM work stays here, never in the UI or AudioWorklet callback.
import init,{VoiceEncoder,VoicePlayout} from "./audio-codec/nuschl_audio_wasm.js";
import {FRAME,MAX_PACKET,MAX_STREAMS,MAX_CAPTURE,decimal} from "./audio-protocol.js";
import {BlockRing,Lead,mixSpeaker,PLAYOUT_BLOCKS,PLAYOUT_FRAME,CAPTURE_BLOCKS} from "./audio-worklet.js";
let encoder=null,epoch=0,enabled=false,receiveEpoch=0,playback=false,loaded=false;
let pendingConfigure=null,port=null,playout=null,capture=null,lastTick=null,encodedOutstanding=0;
const speakers=new Map(),lead=new Lead(),mix=new Float64Array(PLAYOUT_FRAME*2);
let burst=0,created=0,windowStart=performance.now();
const validEpoch=value=>Number.isInteger(value)&&value>0&&value<=0x7fffffff;
const positive=value=>Number.isSafeInteger(value)&&value>0;
function freeSpeakers() {
  for(const state of speakers.values())state.receiver.free();
  speakers.clear();
}
function expire(now) {
  for(const [speaker,state] of speakers)if(now-state.last>2000){state.receiver.free();speakers.delete(speaker);}
}
function configure(data) {
  if(!validEpoch(data.epoch)||!validEpoch(data.receive_epoch)||typeof data.capture!=="boolean"||typeof data.playback!=="boolean"||
      data.capture&&(!Number.isInteger(data.bitrate)||data.bitrate<6000||data.bitrate>256000))throw new Error("Invalid media configuration");
  if(!loaded){pendingConfigure=data;return;}
  // TX policy changes never destroy an independently authorized receive decoder.
  if(data.epoch!==epoch||data.capture!==enabled||enabled&&data.bitrate!==encoderBitrate) {
    encoder?.free();encoder=null;epoch=data.epoch;enabled=data.capture;
    if(enabled){encoder=new VoiceEncoder();encoder.set_bitrate(data.bitrate);}
    encoderBitrate=data.bitrate;
  }
  if(receiveEpoch!==data.receive_epoch||playback!==data.playback){freeSpeakers();lastTick=null;}
  receiveEpoch=data.receive_epoch;playback=data.playback;
}
let encoderBitrate=0;
// The worklet coalesces wakes. Each pass consumes a snapshot of capture
// and renders at most a ringful of playback, even as the audio clock advances.
function pump() {
  playout?.clearWake();
  if (!loaded || !playout) return;
  try {
    for (let left = Math.min(capture.size(), CAPTURE_BLOCKS); left > 0; left--) {
      const at = capture.head(), tag = capture.tags[at * 2];
      // A wake can arrive before the matching configuration on the page port.
      if (tag > epoch || tag === epoch && !enabled) break;
      const frame = capture.tags[at * 2 + 1] >>> 0;
      const age = (capture.ticks() - frame) >>> 0;
      if (tag === epoch && encodedOutstanding < MAX_CAPTURE && age > 0 && age <= MAX_CAPTURE) {
        const pcm = capture.data.subarray(at * FRAME, (at + 1) * FRAME);
        const deadline = performance.timeOrigin + performance.now() + (MAX_CAPTURE - age + 1) * 20;
        const opus = encoder.encode(pcm);
        encodedOutstanding++;
        postMessage({kind:"encoded", epoch, frame, deadline, opus}, [opus.buffer]);
      }
      capture.pop();
    }
    if (!playback) return;
    const tick = playout.ticks();
    const now = performance.now();
    if (lastTick !== null) lead.observe((tick - lastTick) >>> 0, now);
    lastTick = tick;
    expire(now);
    for (let left = lead.blocks - playout.size(); left > 0; left--) {
      mix.fill(0);
      for (const {receiver} of speakers.values()) {
        const samples = receiver.render();
        if (!samples.length) continue;
        if (!(samples instanceof Float32Array) || samples.length !== mix.length || !samples.every(Number.isFinite))
          throw new Error("Invalid playout PCM");
        const gain = receiver.gain();
        if (!Number.isFinite(gain) || gain < 0 || gain > 4) throw new Error("Invalid playout gain");
        mixSpeaker(mix, samples, gain);
      }
      if (!playout.push(receiveEpoch, 0, mix)) break;
    }
  } catch (error) {
    playback = false;
    freeSpeakers();
    postMessage({kind:"playout-error", epoch:receiveEpoch, message:String(error?.message || error).slice(0,200)});
  }
}
function packet(data) {
  if(!playback||data.epoch!==receiveEpoch) {
    if(validEpoch(data.epoch)&&positive(data.job))postMessage({kind:"packet-queued",epoch:data.epoch,job:data.job});
    return;
  }
  try {
    if(!Number.isInteger(data.speaker)||data.speaker<0||data.speaker>0xffffffff||!positive(data.job)||!Number.isFinite(data.arrival)||data.arrival<0||data.arrival>Number.MAX_SAFE_INTEGER/1000||
        !(data.opus instanceof Uint8Array)||data.opus.length<1||data.opus.length>MAX_PACKET||!decimal(data.sequence)||
        BigInt(data.sequence)>((1n<<63n)-1n)||typeof data.end!=="boolean"||!Number.isFinite(data.volume)||data.volume<0||data.volume>4)throw new Error("Invalid encoded packet");
    // Full packing validation precedes speaker creation and any timing/gain state.
    VoicePlayout.validate_packet(data.opus);
    const now=performance.now();expire(now);
    if(now-windowStart>=1000){windowStart=now;burst=0;created=0;}
    if(++burst>2000)throw new Error("Media admission budget exhausted");
    let state=speakers.get(data.speaker);
    if(!state) {
      if(speakers.size>=MAX_STREAMS||created>=MAX_STREAMS)throw new Error("Too many active speakers");
      created++;state={receiver:new VoicePlayout(),last:now,count:0,window:now};speakers.set(data.speaker,state);
    }
    if(now-state.window>=1000){state.window=now;state.count=0;}
    if(++state.count>200)throw new Error("Speaker admission budget exhausted");
    state.receiver.push(BigInt(data.sequence),data.opus,data.end,data.volume,BigInt(Math.floor(data.arrival*1000)));
    state.last=now;
    postMessage({kind:"packet-queued",epoch:receiveEpoch,job:data.job});
  } catch(error) {
    postMessage({kind:"media-error",operation:"decode",epoch:receiveEpoch,job:data.job,message:String(error?.message||error).slice(0,200)});
  }
}
// Register before asynchronous WASM initialization. Only the most recent
// configuration and one transferred port are retained while loading.
onmessage=({data})=>{
  if(!data||typeof data!=="object")return;
  try {
    if(data.kind==="rings") {
      if(playout||!(data.playout instanceof SharedArrayBuffer)||!(data.capture instanceof SharedArrayBuffer))return;
      playout=new BlockRing(data.playout,PLAYOUT_BLOCKS,PLAYOUT_FRAME*2,Float64Array);capture=new BlockRing(data.capture,CAPTURE_BLOCKS,FRAME);return;
    }
    if(data.kind==="playout-port") {
      if(port||!data.port||typeof data.port.postMessage!=="function"){data.port?.close();return;}
      port=data.port;port.onmessage=pump;port.start?.();return;
    }
    if(data.kind==="configure"){configure(data);return;}
    if(data.kind==="encoded-ack"){if(encodedOutstanding)encodedOutstanding--;return;}
    if(data.kind==="close"){encoder?.free();encoder=null;freeSpeakers();port?.close();port=null;close();return;}
    if(!loaded)return;
    if(data.kind==="decode")packet(data);
  } catch(error) {
    postMessage({kind:"media-error",operation:data.kind,epoch,job:data.job,message:String(error?.message||error).slice(0,200)});
  }
};
await init();loaded=true;
if(pendingConfigure){const pending=pendingConfigure;pendingConfigure=null;configure(pending);}
postMessage({kind:"ready"});
setInterval(()=>{expire(performance.now());postMessage({kind:"heartbeat"});},500);
