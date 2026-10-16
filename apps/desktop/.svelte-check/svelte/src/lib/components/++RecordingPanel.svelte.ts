///<reference types="svelte" />
;
/**
   * The recorder panel: start, stop, take a clip, and the live readout — plus the two facts
   * about this shell's *background* half that a user cannot otherwise discover: which chord
   * takes a clip (and whether it is really installed), and what closing the window does.
   *
   * This window OWNS nothing of the recording: the engine is `localplay-recorder` — the
   * same crate the CLI drives — and every number below comes from its status. What this
   * component decides is only what a person sees: three states (not recording, REC,
   * stopped with a reason), the counters while it runs, and two buttons.
   *
   * It deliberately has no controls for things that do not exist yet: no hotkey *binding*
   * (the chord is `[hotkeys] clip` in `config.toml`, and the panel names the file), no
   * encoder picker, no per-game event toggle. Those are settings (spec §10) or Phase 4, and
   * a disabled-looking control that cannot work is worse than no control.
   */
import {
    describeAutoRecord,
    describeConfig,
    describeFrames,
    describeHotkey,
    formatDrift,
    formatRate,
    recordingLabel,
    recordingState,
  } from '../recording';
import { formatBytes, formatDuration } from '../time';
import type { AppStatus, RecordingStatus } from '../types';
;

  interface Props {
    /** `null` until the first status has arrived. */
    status: RecordingStatus | null;
    /** The background half — the hotkey, and where the config came from. `null` until read. */
    app: AppStatus | null;
    /** A command is in flight; the buttons are held while it is. */
    busy?: boolean;
    onStart: () => void;
    onStop: () => void;
    onClip: () => void;
  };function $$render() {

  
  
  
  


  let { status, app = null, busy = false, onStart, onStop, onClip }: Props = $props();

  const state = $derived(recordingState(status));
  const frames = $derived(describeFrames(status));
  const hotkey = $derived(describeHotkey(app?.hotkey ?? null));
  const config = $derived(describeConfig(app));
  const autoRecord = $derived(describeAutoRecord(status));
;
async () => {

 { svelteHTML.createElement("section", { "class":`panel recorder`,});
   { svelteHTML.createElement("div", { "class":`head`,});
     { svelteHTML.createElement("h2", {});  }
     { svelteHTML.createElement("span", { "class":`badge ${state}`,});
      if(state === 'recording'){ { svelteHTML.createElement("span", {   "class":`dot`,"aria-hidden":`true`,}); }}
      recordingLabel(status);
     }
   }

  if(hotkey !== null){
     { svelteHTML.createElement("p", {  "class":`hotkey`,});hotkey.problem;hotkey.text; }
  }

   { svelteHTML.createElement("div", { "class":`controls`,});
    if(state === 'recording'){
       { svelteHTML.createElement("button", {       "class":`primary`,"type":`button`,"onclick":onStop,"disabled":busy,});  }
    }else{
       { svelteHTML.createElement("button", {       "class":`primary`,"type":`button`,"onclick":onStart,"disabled":busy,});  }
    }
     { svelteHTML.createElement("button", {     "type":`button`,"onclick":onClip,"disabled":busy || state !== 'recording',});  }
   }

  if(status !== null && status.error !== null){
     { svelteHTML.createElement("p", { "class":`verdict bad`,});
       { svelteHTML.createElement("strong", {});   }
      status.error;
     }
  }

  if(state === 'recording' && status !== null){
     { svelteHTML.createElement("p", { "class":`live`,});
      formatDuration(status.span_ms);    formatRate(status); 
      status.clips;
      status.clips === 1 ? 'clip' : 'clips'; 
     }
  }

  if(autoRecord !== null){
     { svelteHTML.createElement("p", { "class":`muted auto`,});autoRecord; }
  }

   { svelteHTML.createElement("details", { "class":`engine`,});
     { svelteHTML.createElement("summary", {});  }
    if(status !== null){
       { svelteHTML.createElement("dl", {});
         { svelteHTML.createElement("div", {});
           { svelteHTML.createElement("dt", {});  }
           { svelteHTML.createElement("dd", {});formatDuration(status.span_ms);  { svelteHTML.createElement("span", { "class":`muted`,});  } }
         }
         { svelteHTML.createElement("div", {});
           { svelteHTML.createElement("dt", {});  }
           { svelteHTML.createElement("dd", {});status.segments;  { svelteHTML.createElement("span", { "class":`muted`,}); formatBytes(status.bytes);  } }
         }
         { svelteHTML.createElement("div", {});
           { svelteHTML.createElement("dt", {});  }
           { svelteHTML.createElement("dd", {});status.frames; }
         }
         { svelteHTML.createElement("div", {});
           { svelteHTML.createElement("dt", {});  }
           { svelteHTML.createElement("dd", {});formatRate(status); }
         }
         { svelteHTML.createElement("div", {});
           { svelteHTML.createElement("dt", {});  }
           { svelteHTML.createElement("dd", {});status.clips; }
         }
         { svelteHTML.createElement("div", {});
           { svelteHTML.createElement("dt", {});  }
           { svelteHTML.createElement("dd", {});formatDrift(status); }
         }
       }

      if(frames !== null){
         { svelteHTML.createElement("p", { "class":`muted footnote`,});frames; }
      }
    }else{
       { svelteHTML.createElement("p", { "class":`muted footnote`,});
                       
       }
    }

    if(state !== 'recording'){
       { svelteHTML.createElement("p", { "class":`muted footnote`,});
                    
               
       }
    }

    if(app !== null){
       { svelteHTML.createElement("p", { "class":`muted footnote shell-notes`,});
        app.close_hint;if(config !== null){ { svelteHTML.createElement("span", { "class":`config`,});' ';config; }}
       }
    }
   }
 }


};
return { props: {} as any as Props, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}
const RecordingPanel__SvelteComponent_ = __sveltets_2_fn_component($$render());
/*Ωignore_startΩ*/type RecordingPanel__SvelteComponent_ = ReturnType<typeof RecordingPanel__SvelteComponent_>;
/*Ωignore_endΩ*/export default RecordingPanel__SvelteComponent_;