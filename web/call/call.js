// SPDX-License-Identifier: EUPL-1.2
// The call page (docs/video-plan.md §3): opened from a call link
// `https://<host>/call#v=1&k=<S>&h=<V>[&p=<P>]`, the creator's with P. The link's
// fragment goes to the stream worker, which alone holds S, P and the keys.
import { Call } from "./session.js";
import { StreamClient } from "./stream-client.js";
import { linkOf } from "./signal.js";
import { support } from "./support.js";
import { View } from "./view.js";

const found = support();
const { link, host, relay } = linkOf(location);
let call;
const stream = found.refusal ? null : new StreamClient(() => call?.end("lost"));
call = new Call(stream, { codecs: found.codecs ?? [], relay });
new View(document, call);
document.getElementById("end-call").addEventListener("click", () => call.endForEveryone());
if (found.refusal) call.end(found.refusal);
else call.start({ link, host });
