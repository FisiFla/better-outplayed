///<reference types="svelte" />
;
/**
   * One session: what it is, its timeline, and the range to cut a clip out of.
   *
   * This is a **range picker**, not a player. A session file is one long recording and
   * `ClipDetail` is where a clip is played; what a session's timeline is for is choosing the
   * part worth keeping, which is why the markers are drawn here — they are the reason to
   * scrub a session at all, and they arrive measured on the same media clock the range is.
   *
   * The cursor is not playback: it is where the last marker click landed, so the timeline
   * shows what a click did. Nothing here pretends to play, and there is no `<video>` to
   * drive.
   */
import Timeline from './Timeline.svelte';
import {
    canExtract,
    describeSession,
    extractionBlocker,
    sessionLengthLabel,
    sessionTitle,
  } from '../sessions';
import type { SessionView } from '../sessions';
import { formatBytes, formatPrecise } from '../time';
import type { SessionEvent, TrimRange } from '../types';
;

  interface Props {
    session: SessionView;
    /** The session's markers, in the store's media-time order. */
    events: SessionEvent[];
    busy: boolean;
    onExtract: (range: TrimRange) => void;
    onFavourite: (favourite: boolean) => void;
    onDelete: () => void;
  };function $$render() {

  
  
  
  
  
  


  let { session, events, busy, onExtract, onFavourite, onDelete }: Props = $props();

  /**
   * The range to cut out. The whole session until the user says otherwise.
   *
   * Seeded empty and set in the effect below rather than in this initialiser, which would
   * capture the session's length as it was on the first render and never notice it change —
   * and it does change: a session's length is only known once its segments have been
   * concatenated, so a view watching a recording finish sees it go from unknown to real.
   */
  let range = $state<TrimRange>({ startMs: 0, endMs: 0 });
  /** Where the last marker click put the cursor. Not playback — see the header note. */
  let cursorMs = $state(0);

  const blocker = $derived(extractionBlocker(session));
  const extractable = $derived(canExtract(session));

  // A change of axis resets both, so neither can be left sitting past the end of a shorter
  // recording. Reads only the session's length, so it cannot re-trigger itself.
  $effect(() => {
    const length = session.durationMs;
    range = { startMs: 0, endMs: length };
    cursorMs = 0;
  });
;
async () => {

 { svelteHTML.createElement("section", { "class":`detail`,});
   { svelteHTML.createElement("header", {});
     { svelteHTML.createElement("h2", {});sessionTitle(session); }
     { svelteHTML.createElement("span", { "class":`muted`,});describeSession(session); }
   }

   { svelteHTML.createElement("dl", { "class":`facts`,});
     { svelteHTML.createElement("div", {}); { svelteHTML.createElement("dt", {});  } { svelteHTML.createElement("dd", { "class":`mono`,});session.mode; } }
     { svelteHTML.createElement("div", {}); { svelteHTML.createElement("dt", {});  } { svelteHTML.createElement("dd", { "class":`mono`,});sessionLengthLabel(session); } }
     { svelteHTML.createElement("div", {}); { svelteHTML.createElement("dt", {});  } { svelteHTML.createElement("dd", { "class":`mono`,});formatBytes(session.sizeBytes); } }
     { svelteHTML.createElement("div", {}); { svelteHTML.createElement("dt", {});  } { svelteHTML.createElement("dd", { "class":`mono`,});events.length; } }
     { svelteHTML.createElement("div", { "class":`wide`,});
       { svelteHTML.createElement("dt", {});  }
       { svelteHTML.createElement("dd", { "class":`mono path`,});session.scratchDir; }
     }
   }

  if(session.hasLength){
     { const $$_enilemiT1C = __sveltets_2_ensureComponent(Timeline); new $$_enilemiT1C({ target: __sveltets_2_any(), props: {            "durationMs":session.durationMs,"playheadMs":cursorMs,range,"markers":events,"onSeek":(ms) => (cursorMs = ms),"onRangeChange":(next) => (range = next),}});}
  }else{
     { svelteHTML.createElement("p", { "class":`empty`,});
                      
                   
     }
  }

  if(events.length > 0){
     { svelteHTML.createElement("table", { "class":`markers`,});
       { svelteHTML.createElement("caption", { "class":`muted`,});        }
       { svelteHTML.createElement("thead", {});
         { svelteHTML.createElement("tr", {}); { svelteHTML.createElement("th", {});  } { svelteHTML.createElement("th", {});  } { svelteHTML.createElement("th", {});  } }
       }
       { svelteHTML.createElement("tbody", {});
           for(let event of __sveltets_2_ensureArray(events)){event.id;
           { svelteHTML.createElement("tr", {});
             { svelteHTML.createElement("td", {});event.kind; }
             { svelteHTML.createElement("td", { "class":`mono`,});formatPrecise(event.offset_ms); }
             { svelteHTML.createElement("td", { "class":`mono muted`,});event.clip_id === null ? '—' : `#${event.clip_id}`; }
           }
        }
       }
     }
  }

   { svelteHTML.createElement("div", { "class":`controls`,});
     { svelteHTML.createElement("button", {         "type":`button`,"class":`primary`,"disabled":!extractable || busy || range.endMs <= range.startMs,"onclick":() => onExtract(range),});
      busy ? 'Working…' : 'Extract clip';
     }
     { svelteHTML.createElement("span", { "class":`muted`,});
      formatPrecise(range.startMs); formatPrecise(range.endMs);
      formatPrecise(range.endMs - range.startMs);
     }
     { svelteHTML.createElement("button", {   "type":`button`,"onclick":() => onFavourite(!session.favourite),});
      session.favourite ? 'Un-favourite' : 'Favourite';
     }
     { svelteHTML.createElement("button", {     "type":`button`,"class":`danger`,"onclick":onDelete,});  }
   }

  if(blocker !== null){
     { svelteHTML.createElement("p", { "class":`blocked`,});blocker; }
  }else{
     { svelteHTML.createElement("p", { "class":`hint muted`,});
           { svelteHTML.createElement("code", { "class":`mono`,});  }      
                  
             
     }
  }
 }


};
return { props: {} as any as Props, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}
const SessionDetail__SvelteComponent_ = __sveltets_2_fn_component($$render());
/*Ωignore_startΩ*/type SessionDetail__SvelteComponent_ = ReturnType<typeof SessionDetail__SvelteComponent_>;
/*Ωignore_endΩ*/export default SessionDetail__SvelteComponent_;