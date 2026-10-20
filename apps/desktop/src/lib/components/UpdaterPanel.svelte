<script lang="ts">
  /**
   * The updates section: the one control in this application that reaches another machine.
   *
   * The panel is deliberately quiet about it. There is no check at startup, no polling and
   * no background work: the button below is the whole of the feature. Pressing it makes one
   * request, to one fixed GitHub URL, and nothing else in this application ever leaves the
   * machine.
   *
   * What comes back is not trusted because it came over TLS. It is trusted because the
   * artifact is checked against the minisign public key pinned in `tauri.conf.json` before
   * it is installed, and the panel says so rather than implying that a download is safe.
   *
   * The states are the ones a person can act on, and none of them is a promise the code
   * cannot keep:
   *
   * - **up to date** — the endpoint answered and there is nothing newer;
   * - **an update is offered** — its version, its date and its notes, and two buttons;
   * - **downloading** — the bytes so far, and the fact that the signature is being checked;
   * - **installed** — the application must be restarted to finish, and it says so.
   */
  import { checkForUpdate, formatUpdateDate, type UpdateOffer } from '../updater';
  import { errorMessage } from '../ipc';
  import { formatBytes } from '../time';

  /** What the panel is doing right now. */
  type Phase = 'idle' | 'checking' | 'offered' | 'downloading' | 'installed';

  let phase = $state<Phase>('idle');
  /** The update the endpoint offered, while one is on offer. */
  let offer = $state<UpdateOffer | null>(null);
  /** The refusal or failure, in words the user can act on. */
  let error = $state<string | null>(null);
  /** The bytes downloaded so far, while an update is downloading. */
  let downloaded = $state(0);
  /** The size of the download, when the server said what it is. */
  let total = $state<number | null>(null);
  /** True while a request is in flight; the buttons are held while it is. */
  let busy = $state(false);

  const day = $derived(offer === null ? '' : formatUpdateDate(offer.date));
  const progress = $derived(
    total === null ? formatBytes(downloaded) : `${formatBytes(downloaded)} of ${formatBytes(total)}`,
  );

  /**
   * Ask the endpoint whether there is a newer release.
   *
   * The only request this application makes, and it is made because the button was pressed.
   */
  async function check(): Promise<void> {
    busy = true;
    error = null;
    offer = null;
    phase = 'checking';
    try {
      const found = await checkForUpdate();
      if (found === null) {
        phase = 'idle';
      } else {
        offer = found;
        phase = 'offered';
      }
    } catch (err) {
      // A failed check is not a failed update: nothing was downloaded and nothing was
      // installed. The reason is shown, and the button stays available.
      error = errorMessage(err);
      phase = 'idle';
    } finally {
      busy = false;
    }
  }

  /**
   * Download the offered update and install it.
   *
   * The download is verified against the pinned signing key before anything is installed;
   * a mismatch fails the install and nothing is run.
   */
  async function install(): Promise<void> {
    if (offer === null) return;
    busy = true;
    error = null;
    downloaded = 0;
    total = null;
    phase = 'downloading';
    try {
      await offer.install((done, size) => {
        downloaded = done;
        total = size;
      });
      phase = 'installed';
    } catch (err) {
      error = errorMessage(err);
      phase = 'offered';
    } finally {
      busy = false;
    }
  }

  /** Leave the update alone: the offer is released and the panel goes back to idle. */
  async function dismiss(): Promise<void> {
    const pending = offer;
    offer = null;
    phase = 'idle';
    error = null;
    if (pending !== null) await pending.discard().catch(() => {});
  }
</script>

<section class="group" id="settings-updates">
  <h3>Updates</h3>

  {#if phase === 'checking'}
    <p class="muted footnote" role="status">Checking for a newer release…</p>
  {:else if phase === 'downloading'}
    <p class="muted footnote" role="status">
      Downloading {progress} — the signature is checked against the pinned signing key before
      anything is installed.
    </p>
  {:else if phase === 'installed'}
    <p class="saved" role="status">
      The update is installed. Restart replayed to finish.
    </p>
    <button type="button" onclick={() => void check()} disabled={busy}>Check again</button>
  {:else if offer !== null}
    <p class="chosen">
      <strong>{offer.version}</strong> is available — you are running {offer.currentVersion}.
      {#if day !== ''}Published {day}.{/if}
    </p>
    {#if offer.notes !== ''}
      <pre class="notes">{offer.notes}</pre>
    {:else}
      <p class="muted footnote">This release carries no notes.</p>
    {/if}
    <div class="actions">
      <button type="button" class="primary" onclick={() => void install()} disabled={busy}>
        Download and install
      </button>
      <button type="button" onclick={() => void dismiss()} disabled={busy}>Not now</button>
    </div>
  {:else}
    <p class="muted footnote">
      Nothing here checks for updates on its own. Pressing the button makes one request, to
      this project's own release page on GitHub, and nothing else in replayed ever talks to the
      network. What it downloads is checked against the signing key this build was made with,
      and a file that does not match is refused.
    </p>
    <button type="button" onclick={() => void check()} disabled={busy}>
      Check for updates
    </button>
  {/if}

  {#if error !== null}
    <p class="invalid" role="alert">{error}</p>
  {/if}
</section>

<style>
  .group {
    display: flex;
    flex-direction: column;
    gap: var(--s-2);
    padding: var(--s-4);
    background: var(--surface-2);
    border: 1px solid var(--border);
    border-radius: var(--r-lg);
    scroll-margin-top: var(--s-2);
  }

  h3 {
    margin: 0;
    font: var(--t-heading);
    color: var(--text);
  }

  .chosen {
    font: var(--t-body);
    color: var(--text);
  }

  .notes {
    margin: 0;
    padding: var(--s-3);
    font: var(--t-small);
    color: var(--text-muted);
    background: var(--surface-1);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    max-height: 220px;
    overflow-y: auto;
  }

  .invalid {
    color: var(--warn);
    font: var(--t-small);
  }

  .saved {
    color: var(--ok);
    font: var(--t-small);
  }

  .footnote {
    font: var(--t-small);
  }

  .actions {
    display: flex;
    gap: var(--s-2);
    flex-wrap: wrap;
  }

  .actions button {
    align-self: flex-start;
  }
</style>
