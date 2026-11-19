// SPDX-License-Identifier: EUPL-1.2
// The packaged web app's shortcuts on a Mumble server with a channel named
// Music: Ctrl+K opens the quick switcher, typing finds Music, Enter opens it and
// Escape closes the switcher; Ctrl+F opens the message search, which finds a
// sent message. Run by nix/vm-web.nix.
//   node tests/web_shortcuts.browser.mjs APP_URL SERVER_HOST SERVER_PORT
import assert from "node:assert/strict";
import {pathToFileURL} from "node:url";
const [app,host,port]=process.argv.slice(2);
assert.ok(app&&host&&port,"app URL, server host and port required");
const {chromium}=await import(pathToFileURL(process.env.NUSCHL_PLAYWRIGHT).href);
const browser=await chromium.launch({headless:true,executablePath:process.env.NUSCHL_CHROMIUM,
  args:[`--unsafely-treat-insecure-origin-as-secure=${new URL(app).origin}`]});
try {
  const context=await browser.newContext({viewport:{width:1200,height:800}});
  // A loaded CI host steals most of the VM's time: allow minutes per step.
  context.setDefaultTimeout(180000);
  const page=await context.newPage(),errors=[];
  page.on("pageerror",error=>errors.push(error.message));
  await page.goto(`${app}#s=${host}:${port}&u=shortcuts`);
  await page.getByRole("button",{name:"Trust this certificate",exact:true}).click();
  await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected");
  const heading=name=>page.waitForFunction(name=>document.querySelector(".chat-header h2")?.textContent===name,name);
  const switcher=page.getByRole("dialog",{name:"Quick switcher"});
  // Ctrl+K, Escape.
  await page.keyboard.press("Control+k");
  await switcher.waitFor();
  await page.keyboard.press("Escape");
  await switcher.waitFor({state:"detached"});
  // Ctrl+K, type, Enter.
  await page.keyboard.press("Control+k");
  const field=page.getByRole("combobox");
  await field.evaluate(node=>{if(node!==document.activeElement)throw new Error("the switcher's field has no focus");});
  await page.keyboard.type("mus");
  await page.waitForFunction(()=>document.querySelector(".finder-input")?.getAttribute("aria-label")?.endsWith("Music (channel) highlighted"));
  assert.match(await page.getByRole("option",{selected:true}).getAttribute("aria-label"),/^Music, channel, /);
  await page.keyboard.press("Enter");
  await switcher.waitFor({state:"detached"});
  await heading("Music");
  // Ctrl+F finds a sent message.
  const composer=page.getByLabel("Message",{exact:true});
  for(const text of ["shortcut probe kestrel","an unrelated line"]){
    await composer.fill(text);await page.getByRole("button",{name:"Send",exact:true}).click();
    await page.locator("article").filter({hasText:text}).waitFor();
  }
  await composer.focus();
  await page.keyboard.press("Control+f");
  const search=page.getByLabel("Search this loaded history");
  await search.evaluate(node=>{if(node!==document.activeElement)throw new Error("the search field has no focus");});
  await page.keyboard.type("kestrel");
  await page.waitForFunction(()=>document.querySelectorAll("article").length===1);
  assert.match(await page.locator("article").textContent(),/shortcut probe kestrel/);
  await page.keyboard.press("Escape");
  await search.waitFor({state:"detached"});
  assert.deepEqual(errors,[]);
  console.log("shortcuts: Ctrl+K switches to Music, Escape closes it; Ctrl+F finds a sent message");
} finally { await browser.close(); }
