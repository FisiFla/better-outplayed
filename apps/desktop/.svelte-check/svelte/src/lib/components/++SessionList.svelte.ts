///<reference types="svelte" />
;
/**
   * The session list: newest first, with the facts that identify a recording, a favourite
   * toggle and a confirmed delete.
   *
   * The list does not sort. `list_sessions` returns rows newest first on the store's own
   * order (`started_at DESC, id DESC`), which is the same order the retention rules use;
   * sorting again here would be a second opinion that could silently disagree with it.
   */
import { sessionLengthLabel } from '../sessions';
import type { SessionView } from '../sessions';
import { formatDateTime } from '../time';
;

  interface Props {
    sessions: SessionView[];
    selectedId: number | null;
    onSelect: (id: number) => void;
    onFavourite: (id: number, favourite: boolean) => void;
    onDelete: (id: number) => void;
  };function $$render() {

  
  
  
  


  let { sessions, selectedId, onSelect, onFavourite, onDelete }: Props = $props();

  /** The session whose delete button is armed, if any. */
  let confirming = $state<number | null>(null);
;
async () => {

if(sessions.length === 0){
   { svelteHTML.createElement("p", { "class":`empty`,});        }
}else{
   { svelteHTML.createElement("ul", {});
       for(let session of __sveltets_2_ensureArray(sessions)){session.id;
       { svelteHTML.createElement("li", { });session.id === selectedId;
         { svelteHTML.createElement("button", {     "class":`row`,"type":`button`,"onclick":() => onSelect(session.id),});
           { svelteHTML.createElement("span", { "class":`title`,});session.title; }
           { svelteHTML.createElement("span", { "class":`when muted`,});formatDateTime(session.startedAtMs); }
           { svelteHTML.createElement("span", { "class":`facts mono muted`,});
            sessionLengthLabel(session);
            if(session.running){ }
           }
         }

         { svelteHTML.createElement("div", { "class":`actions`,});
           { svelteHTML.createElement("button", {           "type":`button`,"class":`star`,"aria-pressed":session.favourite,"title":session.favourite
              ? 'Favourite: protected from both session retention rules'
              : 'Protect this session from the retention rules',"onclick":() => onFavourite(session.id, !session.favourite),});
            session.favourite ? '★' : '☆';
           }

          if(confirming === session.id){
             { svelteHTML.createElement("button", {     "type":`button`,"class":`danger`,"onclick":() => { confirming = null; onDelete(session.id); },});
              
             }
             { svelteHTML.createElement("button", {   "type":`button`,"onclick":() => (confirming = null),});  }
          }else{
             { svelteHTML.createElement("button", {   "type":`button`,"onclick":() => (confirming = session.id),});  }
          }
         }
       }
    }
   }
}


};
return { props: {} as any as Props, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}
const SessionList__SvelteComponent_ = __sveltets_2_fn_component($$render());
/*Ωignore_startΩ*/type SessionList__SvelteComponent_ = ReturnType<typeof SessionList__SvelteComponent_>;
/*Ωignore_endΩ*/export default SessionList__SvelteComponent_;