// SPDX-License-Identifier: EUPL-1.2
// The voice call as the phone sees it (docs/browser-audio.md, "Screen off"):
// a screen wake lock while the page is visible, requested again each time it
// becomes visible, and released on leave; a media session with the channel,
// Mute and Hang up; and a silent looping <audio> element, the media Chrome on
// Android needs for its notification and to keep the page playing. Its
// handlers only mute: only the user's Unmute button opens the microphone.
export class CallSession {
  constructor({onMute=()=>{},onHangUp=()=>{},window:win=globalThis}={}) {
    this.onMute=onMute;this.onHangUp=onHangUp;this.window=win;
    this.call=null;this.lock=null;this.locking=false;this.media=null;
    this.visible=()=>this.lockScreen();
    const mute=()=>{if(this.call&&!this.call.muted)this.onMute();},hangUp=()=>{if(this.call)this.onHangUp();};
    this.actions={togglemicrophone:mute,pause:mute,hangup:hangUp,stop:hangUp};
  }
  attach() {this.window.document.addEventListener("visibilitychange",this.visible);}
  detach() {this.window.document.removeEventListener("visibilitychange",this.visible);this.update(null);}
  // From the Join gesture: Safari plays an element later only after one play() in a gesture.
  prime() {this.play();if(!this.call)this.stopMedia();}
  // call: {channel, server, muted} while joining or in voice, otherwise null.
  update(call) {
    const was=this.call,session=this.window.navigator.mediaSession;this.call=call;
    if(!call) {
      if(!was)return;
      this.unlockScreen();this.stopMedia();
      if(session){session.metadata=null;session.playbackState="none";this.setActions(session,false);}
      return;
    }
    if(!was){this.lockScreen();this.play();}
    if(!session)return;
    if(!was)this.setActions(session,true);
    if(was?.channel!==call.channel||was?.server!==call.server)
      session.metadata=new this.window.MediaMetadata({title:call.channel,artist:call.server});
    session.playbackState="playing";
    try{session.setMicrophoneActive?.(!call.muted);}catch{/* Unsupported. */}
  }
  setActions(session,active) {
    for(const [action,handler] of Object.entries(this.actions))
      try{session.setActionHandler(action,active?handler:null);}catch{/* Unsupported action. */}
  }
  async lockScreen() {
    const wakeLock=this.window.navigator.wakeLock;
    if(!this.call||this.lock||this.locking||!wakeLock||this.window.document.visibilityState!=="visible")return;
    this.locking=true;
    try {
      const lock=await wakeLock.request("screen");
      if(!this.call){lock.release().catch(()=>{});return;}
      // The browser releases the lock when the page is hidden.
      this.lock=lock;lock.addEventListener("release",()=>{if(this.lock===lock)this.lock=null;});
    } catch { /* Refused: hidden, low battery or blocked by policy. */ }
    finally {this.locking=false;}
  }
  unlockScreen() {this.lock?.release().catch(()=>{});this.lock=null;}
  play() {
    if(!this.window.Audio)return;
    this.media??=Object.assign(new this.window.Audio(),{loop:true});
    this.media.src=silence(this.window);
    this.media.play().catch(()=>{});
  }
  // Removing the source ends the player, and with it the notification.
  stopMedia() {if(this.media){this.media.pause();this.media.removeAttribute("src");this.media.load();}}
}

// Ten seconds of 8 kHz 8-bit silence as a WAV blob: Chrome shows a media
// notification only for media longer than five seconds.
let silent=null;
function silence(win) {
  if(silent)return silent;
  const rate=8000,length=10*rate,view=new DataView(new ArrayBuffer(44+length));
  // "RIFF", size, "WAVE", "fmt ", 16, PCM mono, rate, bytes/s, 1-byte frames of 8 bits, "data", size.
  [0x46464952,36+length,0x45564157,0x20746d66,16,0x00010001,rate,rate,0x00080001,0x61746164,length]
    .forEach((value,index)=>view.setUint32(index*4,value,true));
  new Uint8Array(view.buffer,44).fill(128);
  return silent=win.URL.createObjectURL(new Blob([view.buffer],{type:"audio/wav"}));
}
