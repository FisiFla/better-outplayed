<script lang="ts">
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

  interface Props {
    clips: ClipView[];
    /** Clip id → `asset:` URL of its cached thumbnail. Missing entries show a placeholder. */
    thumbnails: Record<number, string>;
    selectedId: number | null;
    onSelect: (id: number) => void;
    onFavourite: (id: number, favourite: boolean) => void;
    onDelete: (id: number) => void;
  }

  let { clips, thumbnails, selectedId, onSelect, onFavourite, onDelete }: Props = $props();

  /** The clip whose delete button is armed, if any. */
  let confirming = $state<number | null>(null);
</script>

{#if clips.length === 0}
  <p class="empty">
    No clips are indexed yet. Record one with <code class="mono">localplay-cli buffer</code>
    — this window reads the same index the recorder writes.
  </p>
{:else}
  <ul>
    {#each clips as clip (clip.id)}
      {@const thumbnail = thumbnails[clip.id]}
      <li class:selected={clip.id === selectedId}>
        <button
          class="row"
          type="button"
          aria-current={clip.id === selectedId}
          onclick={() => onSelect(clip.id)}
        >
          {#if thumbnail}
            <img class="thumb" src={thumbnail} alt="" loading="lazy" />
          {:else}
            <span class="thumb placeholder" aria-hidden="true"></span>
          {/if}
          <span class="meta">
            <span class="name">{clip.name}</span>
            <span class="sub muted">
              {clip.durationLabel} · {clip.sizeLabel} · {clip.codec}
            </span>
            <span class="sub muted">{clip.createdAtLabel}</span>
          </span>
        </button>

        <div class="actions">
          <button
            class="star"
            type="button"
            aria-pressed={clip.favourite}
            aria-label={clip.favourite ? 'Remove from favourites' : 'Add to favourites'}
            title="Favourites are exempt from the storage policy's cap and age rules"
            onclick={() => onFavourite(clip.id, !clip.favourite)}
          >
            {clip.favourite ? '★' : '☆'}
          </button>

          {#if confirming === clip.id}
            <button
              class="danger"
              type="button"
              title="The index row is deleted first, then the file"
              onclick={() => {
                onDelete(clip.id);
                confirming = null;
              }}
            >
              Delete
            </button>
            <button type="button" onclick={() => (confirming = null)}>Cancel</button>
          {:else}
            <button
              class="danger"
              type="button"
              title="Delete this clip"
              onclick={() => (confirming = clip.id)}
            >
              Delete
            </button>
          {/if}
        </div>
      </li>
    {/each}
  </ul>
{/if}

<style>
  ul {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    margin: 0;
    padding: 0 8px 8px;
    list-style: none;
  }

  li {
    display: grid;
    grid-template-columns: 1fr auto;
    align-items: center;
    gap: 4px;
    border-radius: 6px;
    border: 1px solid transparent;
  }

  li.selected {
    background: var(--panel-2);
    border-color: var(--accent-dim);
  }

  .row {
    display: grid;
    grid-template-columns: 64px 1fr;
    align-items: center;
    gap: 10px;
    width: 100%;
    padding: 8px;
    background: transparent;
    border: 0;
    border-radius: 6px;
    text-align: left;
  }

  .thumb {
    width: 64px;
    height: 36px;
    object-fit: cover;
    border-radius: 3px;
    border: 1px solid var(--line);
    background: #000;
  }

  .thumb.placeholder {
    display: block;
    background: repeating-linear-gradient(
      45deg,
      #16181f,
      #16181f 6px,
      #1d2029 6px,
      #1d2029 12px
    );
  }

  .meta {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }

  .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .sub {
    font-size: 11px;
  }

  .actions {
    display: flex;
    align-items: center;
    gap: 2px;
    padding-right: 6px;
  }

  .actions button {
    padding: 3px 6px;
    font-size: 11px;
  }

  .star {
    font-size: 13px;
    color: var(--muted);
  }

  .star[aria-pressed='true'] {
    color: var(--star);
    border-color: var(--star);
  }
</style>
