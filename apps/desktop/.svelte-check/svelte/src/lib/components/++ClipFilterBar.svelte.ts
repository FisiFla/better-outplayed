///<reference types="svelte" />
;
/**
   * Search box plus game and tag filters for the clip list. Pure view: the filter value is
   * owned by App and the matching is `filterClips` in `../clips`.
   */
import { isFiltering, NO_FILTER } from '../clips';
import type { ClipFilter } from '../clips';
;

  interface Props {
    filter: ClipFilter;
    games: { value: string; count: number }[];
    tags: { value: string; count: number }[];
    onChange: (filter: ClipFilter) => void;
  };function $$render() {

  
  
  


  let { filter, games, tags, onChange }: Props = $props();
;
async () => {

 { svelteHTML.createElement("div", { "class":`bar`,});
   { svelteHTML.createElement("input", {           "type":`search`,"placeholder":`Search title, game, tag…`,"aria-label":`Search clips`,"value":filter.query,"oninput":(e) => onChange({ ...filter, query: e.currentTarget.value }),});}
   { svelteHTML.createElement("div", { "class":`selects`,});
     { svelteHTML.createElement("select", {       "aria-label":`Filter by game`,"value":filter.game ?? '',"onchange":(e) => onChange({ ...filter, game: e.currentTarget.value || null }),});
       { svelteHTML.createElement("option", {"value":"",});  }
         for(let game of __sveltets_2_ensureArray(games)){game.value;
         { svelteHTML.createElement("option", { "value":game.value,});game.value; game.count;  }
      }
     }
     { svelteHTML.createElement("select", {       "aria-label":`Filter by tag`,"value":filter.tag ?? '',"onchange":(e) => onChange({ ...filter, tag: e.currentTarget.value || null }),});
       { svelteHTML.createElement("option", {"value":"",});  }
         for(let tag of __sveltets_2_ensureArray(tags)){tag.value;
         { svelteHTML.createElement("option", { "value":tag.value,}); tag.value; tag.count;  }
      }
     }
    if(isFiltering(filter)){
       { svelteHTML.createElement("button", {   "type":`button`,"onclick":() => onChange(NO_FILTER),});  }
    }
   }
 }


};
return { props: {} as any as Props, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}
const ClipFilterBar__SvelteComponent_ = __sveltets_2_fn_component($$render());
/*Ωignore_startΩ*/type ClipFilterBar__SvelteComponent_ = ReturnType<typeof ClipFilterBar__SvelteComponent_>;
/*Ωignore_endΩ*/export default ClipFilterBar__SvelteComponent_;