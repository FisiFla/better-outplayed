///<reference types="svelte" />
;
/**
   * The session split into matches: one bar per session with each match as a span, then a
   * card per match with its length, markers and (League) end-of-game scoreboard.
   * Extracting a match is the same lossless cut as extracting a range.
   */
import { markerColor } from '../markers';
import {
    canExtractMatch,
    kda,
    matchCaveat,
    matchLength,
    matchSpan,
    matchTitle,
    scoreboardByTeam,
  } from '../matches';
import { formatPrecise } from '../time';
import type { MatchDto } from '../types';
;

  interface Props {
    matches: MatchDto[];
    /** Session media length, `0` when unknown. */
    durationMs: number;
    extractable: boolean;
    busy: boolean;
    onExtract: (index: number) => void;
  };function $$render() {

  
  
  
  
  


  let { matches, durationMs, extractable, busy, onExtract }: Props = $props();
;
async () => {

if(matches.length > 0){
   { svelteHTML.createElement("section", { "class":`matches`,});
     { svelteHTML.createElement("h3", {}); matches.length;  }
    if(durationMs > 0){
       { svelteHTML.createElement("div", {   "class":`axis`,"aria-hidden":`true`,});
           for(let m of __sveltets_2_ensureArray(matches)){m.index;
          const span = matchSpan(m, durationMs);
          if(span !== null){
             { svelteHTML.createElement("div", {   "class":`span`,});__sveltets_2_ensureType(String, Number, `${span.leftPct} `);__sveltets_2_ensureType(String, Number, `${span.widthPct} `);m.index + 1; }
          }
        }
       }
    }

       for(let m of __sveltets_2_ensureArray(matches)){m.index;
      const caveat = matchCaveat(m);
       { svelteHTML.createElement("article", { "class":`match`,});
         { svelteHTML.createElement("header", {});
           { svelteHTML.createElement("strong", {});matchTitle(m); }
           { svelteHTML.createElement("span", { "class":`muted mono`,});
            formatPrecise(m.start_ms); formatPrecise(m.end_ms); matchLength(m);
           }
           { svelteHTML.createElement("button", {       "type":`button`,"disabled":!extractable || busy || !canExtractMatch(m),"onclick":() => onExtract(m.index),});
              
           }
         }
        if(caveat !== null){ { svelteHTML.createElement("p", { "class":`muted caveat`,});caveat; }}
         { svelteHTML.createElement("div", { "class":`dots`,});
             for(let e of __sveltets_2_ensureArray(m.events)){e.id;
             { svelteHTML.createElement("span", {    "class":`dot`,"title":`${e.kind} · ${formatPrecise(e.offset_ms)}`,});__sveltets_2_ensureType(String, Number, markerColor(e.kind)); }
          }
         }
        if(m.scoreboard.length > 0){
           { svelteHTML.createElement("table", { "class":`score`,});
             { svelteHTML.createElement("caption", { "class":`muted`,});  }
             { svelteHTML.createElement("thead", {}); { svelteHTML.createElement("tr", {}); { svelteHTML.createElement("th", {});  } { svelteHTML.createElement("th", {});  } { svelteHTML.createElement("th", {});  } { svelteHTML.createElement("th", {});  } } }
               for(let group of __sveltets_2_ensureArray(scoreboardByTeam(m.scoreboard))){group.team;
               { svelteHTML.createElement("tbody", {});
                 { svelteHTML.createElement("tr", { "class":`team`,}); { svelteHTML.createElement("th", { "colspan":4,});group.team === '' ? 'Unassigned' : group.team; } }
                    for(let r of __sveltets_2_ensureArray(group.rows)){let i = 1;i;
                   { svelteHTML.createElement("tr", { });r.is_local;
                     { svelteHTML.createElement("td", {});r.name ?? '—'; }
                     { svelteHTML.createElement("td", {});r.champion ?? '—'; }
                     { svelteHTML.createElement("td", { "class":`mono`,});kda(r); }
                     { svelteHTML.createElement("td", { "class":`mono`,});r.cs; }
                   }
                }
               }
            }
           }
        }
       }
    }
   }
}


};
return { props: {} as any as Props, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}
const MatchTimeline__SvelteComponent_ = __sveltets_2_fn_component($$render());
/*Ωignore_startΩ*/type MatchTimeline__SvelteComponent_ = ReturnType<typeof MatchTimeline__SvelteComponent_>;
/*Ωignore_endΩ*/export default MatchTimeline__SvelteComponent_;