///<reference types="svelte" />
;
/**
   * The clip list: newest first, with a thumbnail, the three facts that identify a clip,
   * a favourite toggle and a confirmed delete.
   *
   * The list does not sort. `list_clips` returns the clips newest first, ordered by the
   * wall-clock instant the row was written, and that decision belongs to the Rust side
   * (see `list_clips`' own note on why `clips.started_at` cannot be used for it). Sorting
   * again here would be a second opinion that could silently disagree.
   */
import type { ClipView } from '../clips';
;

  interface Props {
    clips: ClipView[];
    /** Clip id → `asset:` URL of its cached thumbnail. Missing entries show a placeholder. */
    thumbnails: Record<number, string>;
    selectedId: number | null;
    onSelect: (id: number) => void;
    onFavourite: (id: number, favourite: boolean) => void;
    onDelete: (id: number) => void;
  };function $$render() {

  
  


  let { clips, thumbnails, selectedId, onSelect, onFavourite, onDelete }: Props = $props();

  /** The clip whose delete button is armed, if any. */
  let confirming = $state<number | null>(null);
;
async () => {

if(clips.length === 0){
   { svelteHTML.createElement("p", { "class":`empty`,});
               
     { svelteHTML.createElement("strong", {});  }      
   }
}else{
   { svelteHTML.createElement("ul", {});
       for(let clip of __sveltets_2_ensureArray(clips)){clip.id;
      const thumbnail = thumbnails[clip.id];
       { svelteHTML.createElement("li", { });clip.id === selectedId;
         { svelteHTML.createElement("button", {         "class":`row`,"type":`button`,"aria-current":clip.id === selectedId,"onclick":() => onSelect(clip.id),});
          if(thumbnail){
             { svelteHTML.createElement("img", {      "class":`thumb`,"src":thumbnail,"alt":"","loading":`lazy`,});}
          }else{
             { svelteHTML.createElement("span", {   "class":`thumb placeholder`,"aria-hidden":`true`,}); }
          }
           { svelteHTML.createElement("span", { "class":`meta`,});
             { svelteHTML.createElement("span", { "class":`name`,});clip.name; }
             { svelteHTML.createElement("span", { "class":`sub muted`,});clip.durationLabel;  clip.sizeLabel;  clip.codec;  clip.createdAtLabel; }
           }
         }

         { svelteHTML.createElement("div", { "class":`actions`,});
           { svelteHTML.createElement("button", {             "class":`star`,"type":`button`,"aria-pressed":clip.favourite,"aria-label":clip.favourite ? 'Remove from favourites' : 'Add to favourites',"title":`Favourites are exempt from the storage policy's cap and age rules`,"onclick":() => onFavourite(clip.id, !clip.favourite),});
            clip.favourite ? '★' : '☆';
           }

          if(confirming === clip.id){
             { svelteHTML.createElement("button", {         "class":`danger`,"type":`button`,"title":`The index row is deleted first, then the file`,"onclick":() => {
                onDelete(clip.id);
                confirming = null;
              },});
              
             }
             { svelteHTML.createElement("button", {   "type":`button`,"onclick":() => (confirming = null),});  }
          }else{
             { svelteHTML.createElement("button", {         "class":`danger`,"type":`button`,"title":`Delete this clip`,"onclick":() => (confirming = clip.id),});
              
             }
          }
         }
       }
    }
   }
}


};
return { props: {} as any as Props, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}
const ClipList__SvelteComponent_ = __sveltets_2_fn_component($$render());
/*Ωignore_startΩ*/type ClipList__SvelteComponent_ = ReturnType<typeof ClipList__SvelteComponent_>;
/*Ωignore_endΩ*/export default ClipList__SvelteComponent_;