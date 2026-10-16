///<reference types="svelte" />
;
/**
   * The storage panel: what is on disk, what the cap is, and what the policy makes of it
   * (spec §8.1).
   *
   * It is READ-ONLY, and says so. The cleanup pass itself belongs to the recorder
   * (`localplay-cli`), which runs it at startup and on a timer while it is capturing; this
   * window reports the verdict that `plan_cleanup` reaches and never deletes anything on
   * its own initiative. A panel that appeared to manage storage without being able to
   * would be worse than one that explains who does.
   *
   * It is a disclosure, like the settings panel and the engine counters, because its
   * figures answer a question asked rarely while the two lists above it are the window's
   * working surface — five panels stacked in one column spend the lists' room on a
   * readout, and the lists are the thing that scrolls when the library is large. The
   * summary carries the two numbers and the cap's verdict, so nothing is hidden by
   * closing it: what the disclosure withholds is the arithmetic, not the state.
   */
import { formatBytes, percentOf } from '../time';
import type { StorageStats } from '../types';
;

  interface Props {
    stats: StorageStats;
  };function $$render() {

  
  
  


  let { stats }: Props = $props();

  const usedPercent = $derived(percentOf(stats.total_bytes, stats.cap_bytes));
  const favouritePercent = $derived(percentOf(stats.favourite_bytes, stats.cap_bytes));
;
async () => {

 { svelteHTML.createElement("section", { "class":`panel storage`,});
   { svelteHTML.createElement("h2", {});  }

   { svelteHTML.createElement("details", { "class":`body`,});
     { svelteHTML.createElement("summary", {});
       { svelteHTML.createElement("span", { "class":`figures`,});
        formatBytes(stats.total_bytes);  formatBytes(stats.cap_bytes);
       }
       { svelteHTML.createElement("span", {  "class":`state`,});!stats.cap_met;
        stats.cap_met ? 'within the cap' : 'over the cap';
       }
     }

     { svelteHTML.createElement("div", { "class":`stack`,});
       { svelteHTML.createElement("div", {     "class":`bar`,"role":`img`,"aria-label":`${usedPercent.toFixed(0)}% of the cap is used`,});
         { svelteHTML.createElement("span", {   "class":`favourites`,"style":`width: ${favouritePercent}%`,}); }
         { svelteHTML.createElement("span", {   "class":`used`,"style":`width: ${Math.max(0, usedPercent - favouritePercent)}%`,}); }
       }

       { svelteHTML.createElement("dl", {});
         { svelteHTML.createElement("div", {});
           { svelteHTML.createElement("dt", {});   }
           { svelteHTML.createElement("dd", {});stats.clip_count;  { svelteHTML.createElement("span", { "class":`muted`,}); stats.favourite_count;  } }
         }
        if(stats.missing_count > 0){
          
           { svelteHTML.createElement("div", { "class":`wide`,});
             { svelteHTML.createElement("dt", {});  }
             { svelteHTML.createElement("dd", {});
              stats.missing_count;
              stats.missing_count === 1 ? 'indexed clip' : 'indexed clips';    
               { svelteHTML.createElement("span", { "class":`muted`,});
                               
                 
               }
             }
           }
        }
         { svelteHTML.createElement("div", {});
           { svelteHTML.createElement("dt", {});  }
           { svelteHTML.createElement("dd", {});formatBytes(stats.total_bytes);  { svelteHTML.createElement("span", { "class":`muted`,}); formatBytes(stats.cap_bytes); } }
         }
         { svelteHTML.createElement("div", {});
           { svelteHTML.createElement("dt", {});  }
           { svelteHTML.createElement("dd", {});formatBytes(stats.favourite_bytes);  { svelteHTML.createElement("span", { "class":`muted`,});    } }
         }
         { svelteHTML.createElement("div", {});
           { svelteHTML.createElement("dt", {});  }
           { svelteHTML.createElement("dd", {});stats.max_age_days;  }
         }
         { svelteHTML.createElement("div", { "class":`wide`,});
           { svelteHTML.createElement("dt", {});  }
           { svelteHTML.createElement("dd", { "class":`mono path`,});stats.clips_dir; }
         }
       }

      if(stats.cap_met){
         { svelteHTML.createElement("p", { "class":`verdict ok`,});
           { svelteHTML.createElement("strong", {});     }
          if(stats.planned_deletions > 0){       stats.planned_deletions;
            stats.planned_deletions === 1 ? 'clip' : 'clips'; 
            formatBytes(stats.bytes_after); }else{         }
         }
      }else{
         { svelteHTML.createElement("p", { "class":`verdict bad`,});
           { svelteHTML.createElement("strong", {});     }
                 formatBytes(stats.over_cap_by_bytes);  
                         
                        
            
         }
      }

        for(let warning of __sveltets_2_ensureArray(stats.warnings)){
         { svelteHTML.createElement("p", { "class":`verdict bad`,});warning; }
      }

       { svelteHTML.createElement("p", { "class":`muted footnote`,});
                     
            
       }
     }
   }
 }


};
return { props: {} as any as Props, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}
const StoragePanel__SvelteComponent_ = __sveltets_2_fn_component($$render());
/*Ωignore_startΩ*/type StoragePanel__SvelteComponent_ = ReturnType<typeof StoragePanel__SvelteComponent_>;
/*Ωignore_endΩ*/export default StoragePanel__SvelteComponent_;