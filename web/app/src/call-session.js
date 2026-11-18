// SPDX-License-Identifier: EUPL-1.2
// The voice call as the phone sees it (docs/browser-audio.md, "Screen off"):
// a screen wake lock while the page is visible, requested again each time it
// becomes visible, and released on leave.
export class CallSession {
  constructor({window:win=globalThis}={}) {
    this.window=win;this.call=null;this.lock=null;this.locking=false;
    this.visible=()=>this.lockScreen();
  }
  attach() {this.window.document.addEventListener("visibilitychange",this.visible);}
  detach() {this.window.document.removeEventListener("visibilitychange",this.visible);this.update(null);}
  // call: {channel, server, muted} while joining or in voice, otherwise null.
  update(call) {
    const was=this.call;this.call=call;
    if(!call){if(was)this.unlockScreen();return;}
    if(!was)this.lockScreen();
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
}
