// SPDX-License-Identifier: EUPL-1.2
// The packaged web app in nix/vm-web.nix: alice watches the collapsed Lobby
// while bob joins voice in Games below it; Lobby's row counts him, live.
// Usage: node tests/web_tree.browser.mjs <chromium> <playwright-core> <output directory>
import assert from "node:assert/strict";
import {join} from "node:path";
import {pathToFileURL} from "node:url";
const [chromiumPath,playwright,output]=process.argv.slice(2);
assert.ok(chromiumPath&&playwright&&output,"usage: <chromium> <playwright-core> <output directory>");
const {chromium}=await import(pathToFileURL(join(playwright,"index.mjs")).href);
// Plain HTTP in the VM: treat the site as the HTTPS origin it is in production.
const browser=await chromium.launch({headless:true,executablePath:chromiumPath,
  args:["--unsafely-treat-insecure-origin-as-secure=http://web","--disable-audio-output"]});
const errors=[];
// Hang guards sized for a loaded host: within them, slowness is timing (#376).
const HANG=180000;
async function open(name) {
  const page=await browser.newPage({viewport:{width:1280,height:800}});
  page.setDefaultTimeout(HANG);page.setDefaultNavigationTimeout(HANG);
  page.on("pageerror",error=>errors.push(`${name}: ${error.message}`));
  await page.goto(`http://web/app/#s=murmur:64738&u=${name}`);
  await page.getByRole("button",{name:"Trust this certificate",exact:true}).click();
  await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected",null);
  return page;
}
try {
  const alice=await open("alice");
  const lobby=alice.locator(".channel-row").filter({has:alice.getByRole("button",{name:"Lobby",exact:true})});
  const count=lobby.getByRole("img",{name:"1 person inside",exact:true});
  await alice.getByRole("button",{name:"Expand Lobby",exact:true}).waitFor();
  assert.equal(await lobby.getByRole("img").count(),0);
  const bob=await open("bob");
  await bob.getByRole("button",{name:"Expand Lobby",exact:true}).click();
  await bob.getByRole("button",{name:"Games",exact:true}).click();
  await bob.getByRole("button",{name:"Join voice",exact:true}).click();
  await count.waitFor();
  assert.equal(await count.getAttribute("title"),"1 person in collapsed channels");
  assert.equal((await count.textContent()).trim(),"1");
  // Expanded, Lobby shows Games and bob in it instead of the count.
  await alice.getByRole("button",{name:"Expand Lobby",exact:true}).click();
  await alice.getByRole("button",{name:"Private message bob",exact:true}).waitFor();
  assert.equal(await lobby.getByRole("img").count(),0);
  await alice.getByRole("button",{name:"Collapse Lobby",exact:true}).click();
  await count.waitFor();
  await lobby.screenshot({path:join(output,"collapsed-row.png")});
  await alice.locator(".channels").screenshot({path:join(output,"channels.png")});
  // bob leaves voice for the root: the count goes.
  await bob.getByRole("button",{name:"Root",exact:true}).click();
  await bob.getByRole("button",{name:"Join voice",exact:true}).click();
  await count.waitFor({state:"detached"});
  assert.deepEqual(errors,[]);
  console.log("collapsed Lobby: \"1 person inside\" while bob is in Games, gone after he leaves");
} finally {
  await browser.close();
}
