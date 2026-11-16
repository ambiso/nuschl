<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { artUrl } from '../api.js';
  import Icon from './Icon.svelte';
  import { store } from '../store.svelte.js';

  /** The bot answers with PNGs the content worker made; 400 pixels at most. */
  const MAX_EDGE = 400;

  let { library, art, size = 300, px = null, round = false } = $props();
  const bound = $derived(Math.max(32, Math.min(MAX_EDGE, Math.trunc(size))));
  // Covers load only while this browser's link works.
  const allowed = $derived(store.connection === 'open' && !store.invalid);
  const src = $derived(art && allowed ? artUrl(library, art, bound, store.snapshot?.library_keys?.[library]) : null);
  let loaded = $state(null);
  const ready = $derived(src !== null && loaded === src);
</script>

<div class="cover" class:fill={px === null} class:round style:width={px === null ? null : `${px}px`} style:height={px === null ? null : `${px}px`}>
  {#if src}
    <img {src} alt="" aria-hidden="true" loading="lazy" decoding="async" class:ready
         onload={() => (loaded = src)} onerror={() => (loaded = null)} />
  {/if}
  {#if !ready}<div class="placeholder"><Icon name="note" size={px ? Math.max(16, Math.round(px * 0.4)) : 48} /></div>{/if}
</div>

<style>
  .cover { position: relative; flex: none; overflow: hidden; border-radius: 6px; background: var(--placeholder); }
  .cover.fill { width: 100%; aspect-ratio: 1; }
  .cover.round { border-radius: 50%; }
  img { display: block; width: 100%; height: 100%; object-fit: cover; opacity: 0; }
  img.ready { opacity: 1; }
  .placeholder { position: absolute; inset: 0; display: grid; place-items: center; color: var(--muted); opacity: 0.6; }
</style>
