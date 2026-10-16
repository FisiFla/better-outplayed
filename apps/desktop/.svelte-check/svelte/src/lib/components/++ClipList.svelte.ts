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
import { shouldStartDrag } from '../clips';
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
    onCopy: (id: number) => void;
    onReveal: (id: number) => void;
    /** The pointer dragged a row: hand the clip's file to a native drag. */
    onDragOut: (id: number) => void;
    /** A filter is hiding clips, so an empty list is not "no clips". */
    filtering?: boolean;
  };function $$render() {

  
  
  


  let {
    clips,
    thumbnails,
    selectedId,
    onSelect,
    onFavourite,
    onDelete,
    onCopy,
    onReveal,
    onDragOut,
    filtering = false,
  }: Props = $props();

  /** Where the primary button went down on a row, until it is released or becomes a drag. */
  let press: { id: number; x: number; y: number } | null = null;

  function pointerMove(event: PointerEvent) {
    if (press === null) return;
    if (shouldStartDrag(press, { x: event.clientX, y: event.clientY }, event.buttons)) {
      const id = press.id;
      press = null;
      onDragOut(id);
    }
  }

  /** The clip whose delete button is armed, if any. */
  let confirming = $state<number | null>(null);
;
async () => {

if(clips.length === 0 && filtering){
   { svelteHTML.createElement("p", { "class":`empty`,});        }
} else if (clips.length === 0){
   { svelteHTML.createElement("p", { "class":`empty`,});
               
     { svelteHTML.createElement("strong", {});  }      
   }
}else{
   { svelteHTML.createElement("ul", {});
       for(let clip of __sveltets_2_ensureArray(clips)){clip.id;
      const thumbnail = thumbnails[clip.id];
       { svelteHTML.createElement("li", { });clip.id === selectedId;
         { svelteHTML.createElement("button", {                   "class":`row`,"type":`button`,"aria-current":clip.id === selectedId,"onclick":() => onSelect(clip.id),"onpointerdown":(e) => {
            if (e.button === 0) press = { id: clip.id, x: e.clientX, y: e.clientY };
          },"onpointermove":pointerMove,"onpointerup":() => (press = null),"onpointerleave":pointerMove,"title":`Drag to copy this clip's file into another app`,});
          if(thumbnail){
             { svelteHTML.createElement("img", {        "class":`thumb`,"src":thumbnail,"alt":"","loading":`lazy`,"draggable":`false`,});}
          }else{
             { svelteHTML.createElement("span", {   "class":`thumb placeholder`,"aria-hidden":`true`,}); }
          }
           { svelteHTML.createElement("span", { "class":`meta`,});
             { svelteHTML.createElement("span", { "class":`name`,});clip.displayName; }
             { svelteHTML.createElement("span", { "class":`sub muted`,});
              if(clip.game){clip.game; }clip.durationLabel;  clip.sizeLabel;  clip.createdAtLabel;
             }
            if(clip.tags.length > 0){
               { svelteHTML.createElement("span", { "class":`tags`,});
                   for(let tag of __sveltets_2_ensureArray(clip.tags)){tag; { svelteHTML.createElement("span", { "class":`tag`,}); tag; }}
               }
            }
           }
         }

         { svelteHTML.createElement("div", { "class":`actions`,});
           { svelteHTML.createElement("button", {             "class":`star`,"type":`button`,"aria-pressed":clip.favourite,"aria-label":clip.favourite ? 'Remove from favourites' : 'Add to favourites',"title":`Favourites are exempt from the storage policy's cap and age rules`,"onclick":() => onFavourite(clip.id, !clip.favourite),});
            clip.favourite ? '★' : '☆';
           }

           { svelteHTML.createElement("button", {         "type":`button`,"title":`Copy the clip file to the clipboard`,"aria-label":`Copy clip file`,"onclick":() => onCopy(clip.id),});
            
           }
           { svelteHTML.createElement("button", {         "type":`button`,"title":`Show the clip file in the file manager`,"aria-label":`Show in folder`,"onclick":() => onReveal(clip.id),});
            
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