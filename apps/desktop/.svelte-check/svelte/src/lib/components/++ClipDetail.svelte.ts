///<reference types="svelte" />
;
/**
   * One clip, with its player and its trim controls.
   *
   * Playback is an HTML5 `<video>` pointed at an `asset:` URL (spec §9) — the Tauri asset
   * protocol, scoped to the clips directory, serving range requests so the element's own
   * scrubbing works. There is no streaming code in this application and there must not be.
   */
import Timeline from './Timeline.svelte';
import type { ClipView } from '../clips';
import { formatDateTime } from '../time';
import { fullRange, rangeLabel, rangeLengthLabel, validateRange } from '../trim';
import type { TrimRange } from '../types';
;

  interface Props {
    clip: ClipView;
    /** A command is in flight; both actions are held while it is. */
    busy: boolean;
    onTrim: (range: TrimRange) => void;
    onFavourite: (favourite: boolean) => void;
  };function $$render() {

  
  
  
  
  
  


  let { clip, busy, onTrim, onFavourite }: Props = $props();

  let video = $state<HTMLVideoElement | null>(null);
  let playheadMs = $state(0);
  // svelte-ignore state_referenced_locally — deliberate: this component is keyed on the
  // clip id by App, so a different clip remounts it rather than re-rendering it with a new
  // prop. Reading the prop once here is therefore the whole clip, which is what this
  // initial value is meant to be.
  let range = $state<TrimRange>(fullRange(clip.durationMs));
  let playing = $state(false);

  const validation = $derived(validateRange(range, clip.durationMs));

  /**
   * The playhead follows the element, never a timer of this window's own: while the video
   * plays it is read once per animation frame, and `ontimeupdate` covers the frames in
   * between plus every seek and pause. A timer would drift away from the decoder — and the
   * number this produces is the number that gets sent to `trim_clip`, so drift would mean
   * cutting somewhere other than where the user pointed.
   */
  $effect(() => {
    if (!playing) return;
    let frame = 0;
    const tick = () => {
      playheadMs = (video?.currentTime ?? 0) * 1000;
      frame = requestAnimationFrame(tick);
    };
    frame = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(frame);
  });

  function syncPlayhead() {
    playheadMs = (video?.currentTime ?? 0) * 1000;
  }

  function seek(ms: number) {
    playheadMs = ms;
    if (video) video.currentTime = ms / 1000;
  }

  function trim() {
    if (validation.ok) onTrim(validation.range);
  }
;
async () => {

 { svelteHTML.createElement("header", { "class":`head`,});
   { svelteHTML.createElement("div", { "class":`titles`,});
     { svelteHTML.createElement("h1", {});clip.name; }
     { svelteHTML.createElement("p", { "class":`mono muted`,});clip.path; }
   }
   { svelteHTML.createElement("button", {             "class":`star`,"type":`button`,"aria-pressed":clip.favourite,"title":clip.favourite ? 'Remove from favourites' : 'Protect this clip from cleanup',"onclick":() => onFavourite(!clip.favourite),"disabled":busy,});
    clip.favourite ? '★ Favourite' : '☆ Favourite';
   }
 }

 { svelteHTML.createElement("div", { "class":`facts`,});
   { svelteHTML.createElement("span", { "class":`chip`,});clip.durationLabel; }
   { svelteHTML.createElement("span", { "class":`chip`,});clip.sizeLabel; }
   { svelteHTML.createElement("span", { "class":`chip`,});clip.codec; }
   { svelteHTML.createElement("span", { "class":`chip`,});formatDateTime(clip.createdAtMs); }
 }


  { const $$_video0 = svelteHTML.createElement("video", {                  "class":`player`,"src":clip.assetUrl,"controls":true,"preload":`metadata`,"ontimeupdate":syncPlayhead,"onseeked":syncPlayhead,"onplay":() => (playing = true),"onpause":() => (playing = false),"onended":() => (playing = false),});video = $$_video0; }

 { const $$_enilemiT0C = __sveltets_2_ensureComponent(Timeline); new $$_enilemiT0C({ target: __sveltets_2_any(), props: {         "durationMs":clip.durationMs,playheadMs,range,"onSeek":seek,"onRangeChange":(next) => (range = next),}});}

 { svelteHTML.createElement("section", { "class":`panel trim`,});
   { svelteHTML.createElement("h2", {});  }

   { svelteHTML.createElement("div", { "class":`selection`,});
     { svelteHTML.createElement("span", { "class":`mono`,});rangeLabel(range); }
     { svelteHTML.createElement("span", { "class":`muted`,}); rangeLengthLabel(range);  }
   }

   { svelteHTML.createElement("div", { "class":`actions`,});
     { svelteHTML.createElement("button", {         "class":`primary`,"type":`button`,"disabled":!validation.ok || busy,"onclick":trim,});
      busy ? 'Working…' : 'Trim to a new clip';
     }
     { svelteHTML.createElement("button", {       "type":`button`,"disabled":busy,"onclick":() => (range = fullRange(clip.durationMs)),});
       
     }
   }

  if(!validation.ok){
     { svelteHTML.createElement("p", { "class":`invalid`,});validation.reason; }
  }

   { svelteHTML.createElement("p", { "class":`lossless`,});
     { svelteHTML.createElement("strong", {});  }         
     { svelteHTML.createElement("code", {});  }             
                   
           
     { svelteHTML.createElement("code", {});clip.name.replace(/\.[^.]+$/, '');  } 
       
   }
 }


};
return { props: {} as any as Props, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}
const ClipDetail__SvelteComponent_ = __sveltets_2_fn_component($$render());
/*Ωignore_startΩ*/type ClipDetail__SvelteComponent_ = ReturnType<typeof ClipDetail__SvelteComponent_>;
/*Ωignore_endΩ*/export default ClipDetail__SvelteComponent_;