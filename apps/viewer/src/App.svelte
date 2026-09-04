<script lang="ts">
  import {
    Clipboard,
    Keyboard,
    Monitor,
    Play,
    RefreshCw,
    Square,
  } from "@lucide/svelte";
  import { onMount } from "svelte";

  import Badge from "./lib/components/ui/badge/Badge.svelte";
  import Button from "./lib/components/ui/button/Button.svelte";
  import Card from "./lib/components/ui/card/Card.svelte";
  import { DesktopSession } from "./lib/vnc/session.svelte";

  const session = new DesktopSession();
  let viewport!: HTMLDivElement;

  const tone = $derived(
    session.state === "connected"
      ? ("healthy" as const)
      : session.state === "failed"
        ? ("danger" as const)
        : session.state === "offline"
          ? ("warning" as const)
          : ("neutral" as const),
  );

  onMount(() => {
    session.mount(viewport);
    return () => session.destroy();
  });
</script>

<main class="isolate grid h-dvh grid-rows-[auto_minmax(0,1fr)] bg-canvas">
  <header
    class="flex min-w-0 items-center justify-between gap-4 border-b border-edge px-4 py-3 sm:px-6"
  >
    <div class="flex min-w-0 items-center gap-3">
      <Monitor class="size-4 shrink-0 stroke-ink" aria-hidden="true" />
      <div class="min-w-0">
        <h1
          class="truncate text-sm/5 font-semibold tracking-[-0.01em] text-ink"
        >
          Sprite Desktop
        </h1>
        <p class="truncate font-mono text-xs/4 text-ink-tertiary">
          shared desktop
        </p>
      </div>
    </div>
    <Badge {tone}>{#snippet children()}{session.label}{/snippet}</Badge>
  </header>

  <section
    class="grid min-h-0 grid-rows-[minmax(0,1fr)_auto] gap-3 p-3 sm:gap-4 sm:p-4"
  >
    <Card class="relative min-h-0 overflow-hidden bg-well">
      <!-- svelte-ignore a11y_no_noninteractive_tabindex (noVNC needs a focusable canvas host) -->
      <div
        bind:this={viewport}
        role="application"
        tabindex="0"
        aria-label="Remote Linux desktop"
        class="h-full min-h-0 w-full overflow-hidden outline-none focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-action [&>canvas]:h-full [&>canvas]:w-full"
      ></div>

      {#if !session.connected}
        <div
          class="pointer-events-none absolute inset-0 grid place-items-center p-6"
          aria-hidden="true"
        >
          <div class="flex max-w-sm flex-col items-center gap-3 text-center">
            <Monitor class="size-4 shrink-0 stroke-ink-tertiary" />
            <div>
              <p class="text-base/6 font-medium text-ink sm:text-sm/5">
                {session.label}
              </p>
              <p class="pt-1 text-base/6 text-ink-tertiary sm:text-sm/5">
                {#if session.state === "failed"}
                  The desktop did not answer within 60 seconds.
                {:else if session.state === "offline"}
                  Reconnection will resume when this browser is online.
                {:else if session.state === "stopped-by-user"}
                  Reconnect when you are ready to control the shared desktop.
                {:else}
                  Waiting for the desktop to become ready.
                {/if}
              </p>
            </div>
          </div>
        </div>
      {/if}
    </Card>

    <nav
      aria-label="Desktop controls"
      class="flex min-w-0 flex-wrap items-center gap-2"
    >
      {#if session.state === "stopped-by-user"}
        <Button variant="primary" onclick={() => session.reconnect()}>
          {#snippet children()}
            <Play
              class="size-4 shrink-0 stroke-action-ink"
              aria-hidden="true"
            />
            Reconnect
          {/snippet}
        </Button>
      {:else if session.state === "failed"}
        <Button variant="primary" onclick={() => session.retry()}>
          {#snippet children()}
            <RefreshCw
              class="size-4 shrink-0 stroke-action-ink"
              aria-hidden="true"
            />
            Retry
          {/snippet}
        </Button>
        <Button onclick={() => session.reauthenticate()}>
          {#snippet children()}Re-authenticate{/snippet}
        </Button>
      {:else}
        <Button
          disabled={session.state === "booting"}
          onclick={() => session.disconnect()}
        >
          {#snippet children()}
            <Square
              class="size-4 shrink-0 stroke-ink-secondary"
              aria-hidden="true"
            />
            Disconnect
          {/snippet}
        </Button>
      {/if}

      <div class="hidden h-5 w-px bg-edge sm:block" aria-hidden="true"></div>

      <Button
        variant="ghost"
        disabled={!session.connected}
        onclick={() => session.sendCtrlAltDel()}
      >
        {#snippet children()}
          <Keyboard
            class="size-4 shrink-0 stroke-ink-secondary"
            aria-hidden="true"
          />
          Send Ctrl+Alt+Del
        {/snippet}
      </Button>
      <Button
        variant="ghost"
        disabled={!session.connected}
        onclick={() => void session.paste()}
      >
        {#snippet children()}
          <Clipboard
            class="size-4 shrink-0 stroke-ink-secondary"
            aria-hidden="true"
          />
          Paste
        {/snippet}
      </Button>
      {#if session.error}
        <p role="alert" class="text-sm/5 text-danger">{session.error}</p>
      {/if}
    </nav>
  </section>
</main>
