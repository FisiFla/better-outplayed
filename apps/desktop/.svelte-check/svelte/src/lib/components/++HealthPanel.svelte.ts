///<reference types="svelte" />
;
/**
   * The health page: what the recorder, the encoder, the disk and the game-event sources are
   * doing, and the last problems the log holds. A readout only; the wording lives in
   * `lib/health.ts`.
   */
import {
    describeDisk,
    describeEncoder,
    describeRecorderState,
    diskIsLow,
    needsAttention,
  } from '../health';
import type { Health } from '../types';
;

  interface Props {
    health: Health;
  };function $$render() {

  
  
  


  let { health }: Props = $props();
  const attention = $derived(needsAttention(health));
;
async () => {

 { svelteHTML.createElement("section", { "class":`panel health`,});
   { svelteHTML.createElement("h2", {});  }
   { svelteHTML.createElement("details", {});
     { svelteHTML.createElement("summary", {});
      describeRecorderState(health.recorder_state, null);if(attention){
         { svelteHTML.createElement("span", { "class":`flag`,});   }}
     }
     { svelteHTML.createElement("dl", {});
       { svelteHTML.createElement("div", { "class":`wide`,});
         { svelteHTML.createElement("dt", {});  }
         { svelteHTML.createElement("dd", {});describeRecorderState(health.recorder_state, health.recorder_error); }
       }
       { svelteHTML.createElement("div", { "class":`wide`,});
         { svelteHTML.createElement("dt", {});  }
         { svelteHTML.createElement("dd", {});describeEncoder(health); }
       }
       { svelteHTML.createElement("div", { "class":`wide`,});
         { svelteHTML.createElement("dt", {});  }
         { svelteHTML.createElement("dd", { });diskIsLow(health.disk);
          describeDisk(health.disk);if(diskIsLow(health.disk)){ }
         }
       }
       { svelteHTML.createElement("div", { "class":`wide`,});
         { svelteHTML.createElement("dt", {});  }
        if(health.event_sources.length === 0){
           { svelteHTML.createElement("dd", { "class":`muted`,});            }
        }else{
             for(let src of __sveltets_2_ensureArray(health.event_sources)){src.source;
             { svelteHTML.createElement("dd", { });src.state === 'failed';
              src.source; src.state; { svelteHTML.createElement("span", { "class":`muted`,}); src.detail;  }
             }
          }
        }
       }
       { svelteHTML.createElement("div", { "class":`wide`,});
         { svelteHTML.createElement("dt", {});    }
        if(health.recent_errors.length === 0){
           { svelteHTML.createElement("dd", { "class":`muted`,});    }
        }else{
           { svelteHTML.createElement("dd", {});
             { svelteHTML.createElement("ul", { "class":`errors`,});
                  for(let line of __sveltets_2_ensureArray(health.recent_errors)){let i = 1;i;
                 { svelteHTML.createElement("li", {});line; }
              }
             }
           }
        }
       }
       { svelteHTML.createElement("div", { "class":`wide`,});
         { svelteHTML.createElement("dt", {});  }
         { svelteHTML.createElement("dd", { "class":`path`,});health.log_path; }
       }
     }
   }
 }


};
return { props: {} as any as Props, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}
const HealthPanel__SvelteComponent_ = __sveltets_2_fn_component($$render());
/*Ωignore_startΩ*/type HealthPanel__SvelteComponent_ = ReturnType<typeof HealthPanel__SvelteComponent_>;
/*Ωignore_endΩ*/export default HealthPanel__SvelteComponent_;