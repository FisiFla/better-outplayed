///<reference types="svelte" />
;
/**
   * The settings panel: what the recorder runs with, shown and edited in the window.
   *
   * Everything here persists to `config.toml` the moment it is applied — the file the
   * CLI reads too, so one edit means one thing to both front-ends. What applies *now*
   * versus on the next recording is the honest split the backend reports: the clips
   * directory moves future writes immediately (existing rows stay where their files
   * are), while fps, output size, mic and auto-record are baked in per recorder start.
   * The panel says which is which instead of pretending everything is live.
   */
import type { RecordingStatus, SettingsDto, SettingsUpdate } from '../types';
import { open } from '@tauri-apps/plugin-dialog';
;

  interface Props {
    /** `null` until the first read answers; the panel is hidden meanwhile. */
    settings: SettingsDto | null;
    /** The engine's live counters; `null` until the first poll answers. */
    recording: RecordingStatus | null;
    /** A command is in flight; the apply buttons are held while it is. */
    busy?: boolean;
    onApply: (update: SettingsUpdate) => void;
  };function $$render() {

  
  
  


  let { settings, recording = null, busy = false, onApply }: Props = $props();

  const RESOLUTIONS = [
    { label: 'Native (capture size)', value: '' },
    { label: '1920 × 1080', value: '1920x1080' },
    { label: '1280 × 720', value: '1280x720' },
    { label: '960 × 540', value: '960x540' },
  ];

  /** The drafts behind the three Apply buttons, reseeded whenever a fresh read lands. */
  let clipsDir = $state('');
  let fpsText = $state('60');
  let outputSize = $state('');
  let micEnabled = $state(false);
  let autoRecord = $state(false);

  $effect(() => {
    if (settings === null) return;
    clipsDir = settings.clips_dir;
    fpsText = String(settings.fps);
    outputSize = settings.output_size;
    micEnabled = settings.mic_enabled;
    autoRecord = settings.auto_record;
  });

  const fps = $derived(/^\d+$/.test(fpsText.trim()) ? Number(fpsText.trim()) : null);
  const fpsValid = $derived(fps !== null && fps >= 1 && fps <= 240);
  const recordingNow = $derived(recording !== null && recording.running);
  const isDefault = $derived(
    (key: string) => settings?.defaulted.includes(key) ?? false,
  );

  function applyLibrary() {
    onApply({ clips_dir: clipsDir });
  }

  /**
   * The native folder picker. Cancelling — or running somewhere with no Tauri
   * runtime, like the screenshot harness — resolves to no directory, and the draft
   * is left alone rather than cleared.
   */
  async function browseClipsDir() {
    const selected = await open({
      directory: true,
      multiple: false,
      title: 'Choose the clips directory',
    }).catch(() => null);
    if (typeof selected === 'string') clipsDir = selected;
  }

  function applyCapture() {
    if (!fpsValid || fps === null) return;
    onApply({ fps, output_size: outputSize, mic_enabled: micEnabled });
  }

  function applyAutomatic() {
    onApply({ auto_record: autoRecord });
  }
;
async () => {

 { svelteHTML.createElement("section", { "class":`panel settings`,});
   { svelteHTML.createElement("h2", {});  }

  if(settings !== null){
     { svelteHTML.createElement("details", { "class":`body`,});
       { svelteHTML.createElement("summary", {});    }

    if(!settings.config_exists){
       { svelteHTML.createElement("p", { "class":`muted footnote`,});
                   
       }
    }

     { svelteHTML.createElement("div", { "class":`group`,});
       { svelteHTML.createElement("h3", {});  }
       { svelteHTML.createElement("label", { "class":`field`,});
         { svelteHTML.createElement("span", {});  if(isDefault('storage.clips_dir')){ { svelteHTML.createElement("span", { "class":`muted`,});  }} }
         { svelteHTML.createElement("span", { "class":`dir-row`,});
           { svelteHTML.createElement("input", {           "type":`text`,"value":clipsDir,"oninput":(event) => (clipsDir = event.currentTarget.value),"disabled":busy,"spellcheck":false,});}
           { svelteHTML.createElement("button", {     "type":`button`,"onclick":browseClipsDir,"disabled":busy,});  }
         }
       }
       { svelteHTML.createElement("p", { "class":`muted footnote`,});          }
       { svelteHTML.createElement("button", {     "type":`button`,"onclick":applyLibrary,"disabled":busy,});  }
     }

     { svelteHTML.createElement("div", { "class":`group`,});
       { svelteHTML.createElement("h3", {});  }
       { svelteHTML.createElement("div", { "class":`row`,});
         { svelteHTML.createElement("label", { "class":`field`,});
           { svelteHTML.createElement("span", {}); if(isDefault('encode.fps')){ { svelteHTML.createElement("span", { "class":`muted`,});  }} }
           { svelteHTML.createElement("input", {             "type":`number`,"min":`1`,"max":`240`,"value":fpsText,"oninput":(event) => (fpsText = event.currentTarget.value),"disabled":busy,});}
         }
         { svelteHTML.createElement("label", { "class":`field`,});
           { svelteHTML.createElement("span", {}); if(isDefault('encode.output_size')){ { svelteHTML.createElement("span", { "class":`muted`,});  }} }
           { svelteHTML.createElement("select", {       "value":outputSize,"onchange":(event) => (outputSize = event.currentTarget.value),"disabled":busy,});
               for(let option of __sveltets_2_ensureArray(RESOLUTIONS)){option.value;
               { svelteHTML.createElement("option", { "value":option.value,});option.label; }
            }
           }
         }
       }
      if(!fpsValid){
         { svelteHTML.createElement("p", { "class":`invalid`,});    }
      }
       { svelteHTML.createElement("label", { "class":`check`,});
         { svelteHTML.createElement("input", {         "type":`checkbox`,"checked":micEnabled,"onchange":(event) => (micEnabled = event.currentTarget.checked),"disabled":busy,});}
         { svelteHTML.createElement("span", {});  if(isDefault('mic.enabled')){ { svelteHTML.createElement("span", { "class":`muted`,});  }} }
       }
      if(recordingNow){
         { svelteHTML.createElement("p", { "class":`muted footnote`,});      }
      }
       { svelteHTML.createElement("button", {     "type":`button`,"onclick":applyCapture,"disabled":busy || !fpsValid,});
         
       }
     }

     { svelteHTML.createElement("div", { "class":`group`,});
       { svelteHTML.createElement("h3", {});  }
       { svelteHTML.createElement("label", { "class":`check`,});
         { svelteHTML.createElement("input", {         "type":`checkbox`,"checked":autoRecord,"onchange":(event) => (autoRecord = event.currentTarget.checked),"disabled":busy,});}
         { svelteHTML.createElement("span", {});    if(isDefault('games.auto_record')){ { svelteHTML.createElement("span", { "class":`muted`,});  }} }
       }
       { svelteHTML.createElement("p", { "class":`muted footnote`,}); settings.watch_titles.join(', '); }
      if(recordingNow){
         { svelteHTML.createElement("p", { "class":`muted footnote`,});      }
      }
       { svelteHTML.createElement("button", {     "type":`button`,"onclick":applyAutomatic,"disabled":busy,});  }
     }
     }
  }
 }


};
return { props: {} as any as Props, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}
const SettingsPanel__SvelteComponent_ = __sveltets_2_fn_component($$render());
/*Ωignore_startΩ*/type SettingsPanel__SvelteComponent_ = ReturnType<typeof SettingsPanel__SvelteComponent_>;
/*Ωignore_endΩ*/export default SettingsPanel__SvelteComponent_;